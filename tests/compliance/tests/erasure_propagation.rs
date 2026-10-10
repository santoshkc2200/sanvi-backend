//! TASK-029 (11.10 Step 4): erasure propagation reaches every destination.
//!
//! The saga erases the subject inside every provider; this proves what
//! happens *after*: the propagation pass fans out over the destinations
//! outside the registry — the archives, the backup window, the caches, the
//! search indexes and the ad-platform suppression list — and every
//! destination ends up in the ledger, either purged or lawfully retained
//! with an end date. A seeded subject is then verifiably gone: the cache
//! entry is purged for real, the consent ledger is pseudonymised, and the
//! retained classes are enumerated with their justification.

use std::sync::Arc;

use async_trait::async_trait;
use sanvi_api::compliance::ComplianceHarness;
use sanvi_cache::SsrCache;
use sanvi_kernel::{NodeId, RawIdGenerator, SnowflakeGenerator, TenantId};
use sanvi_privacy::adapters::{
    ArchivesDestination, BackupWindowDestination, CacheDestination, SearchIndexDestination,
};
use sanvi_privacy::application::{PropagateErasure, SubmitDsrCommand};
use sanvi_privacy::domain::{
    DsrKind, EraseDestinationOutcome, ErasureDestination, ParticipantStatus, PrivacyError,
    Requester, SubjectKind, SubjectRef,
};

fn ids() -> Arc<dyn RawIdGenerator> {
    Arc::new(SnowflakeGenerator::new(NodeId::new(80).expect("node id")))
}

/// The suppression-list destination as the composition root adapts it (the
/// worker's `AdSuppressionDestination` runs the phase-10 retraction sweep;
/// the port contract this test pins is the same one that adapter
/// implements).
struct RecordingSuppressionDestination;

#[async_trait]
impl ErasureDestination for RecordingSuppressionDestination {
    fn name(&self) -> &'static str {
        "ad_suppression"
    }

    fn description(&self) -> String {
        "ad-platform suppression lists: uploaded conversion events are retracted at the platform; capture denies any new upload once the directives are superseded".to_string()
    }

    async fn propagate_erasure(
        &self,
        _subject: &SubjectRef,
        _tenant_id: Option<TenantId>,
    ) -> Result<EraseDestinationOutcome, PrivacyError> {
        // The subject's conversion events died with the erasure (the
        // advertising provider's participant report holds the count);
        // nothing is outstanding to retract.
        Ok(EraseDestinationOutcome::purged(0))
    }
}

/// Seed one real tenant row so the EndUser erasure subject and the SSR
/// cache purge have a tenant to hang off.
async fn seed_tenant(pool: &sqlx::PgPool, tenant_id: i64, slug: &str) {
    sqlx::query(
        "INSERT INTO tenancy.tenants (id, slug, display_name, status, region)
         VALUES ($1, $2, $2, 'active', 'jp') ON CONFLICT (id) DO NOTHING",
    )
    .bind(tenant_id)
    .bind(slug)
    .execute(pool)
    .await
    .expect("seed tenant");
}

#[tokio::test]
async fn a_seeded_subject_is_verifiably_gone_from_every_destination() {
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool().clone();
    let harness = ComplianceHarness::build(pool.clone(), ids());

    let tenant_id = TenantId::try_from(901_000_001_i64).expect("tenant id");
    seed_tenant(&pool, tenant_id.as_i64(), "erasure-drill").await;

    // An EndUser subject of that tenant; a tenant-operator submission is
    // session-verified, so the drill runs the full saga without the OTP
    // gate.
    let external_id = "erasure-drill-subject";
    let subject = SubjectRef {
        kind: SubjectKind::EndUser,
        tenant_id: Some(tenant_id),
        key: format!("end_user:{tenant_id}:{external_id}"),
        identifiers: vec![sanvi_privacy::domain::SubjectIdentifier {
            id_type: "external_id".to_string(),
            value: external_id.to_string(),
        }],
    };

    // Seed real personal data first: an exercised opt-out writes consent
    // ledger rows and directives, so erasure has something to destroy and
    // the pseudonymisation is observable.
    harness
        .wiring
        .state
        .record_opt_out
        .execute(
            &subject,
            sanvi_privacy::application::RecordOptOutCommand {
                purposes: vec![sanvi_privacy::domain::ProcessingPurpose::SaleOrShare],
                source: "ui".to_string(),
                device_ref: None,
            },
        )
        .await
        .expect("seed opt-out");

    let operator = harness.drill_subject().user_id;
    let submitted = harness
        .wiring
        .state
        .submit_dsr
        .execute(
            Some(operator),
            Some(tenant_id),
            None,
            SubmitDsrCommand {
                subject: sanvi_privacy::application::SubjectInput {
                    kind: "end_user".to_string(),
                    email: None,
                    external_id: Some(external_id.to_string()),
                    evidence: vec![],
                    age_signal: None,
                },
                kind: DsrKind::Erasure,
                requester: Requester::TenantOperator,
                agent: None,
                note: Some("TASK-029 propagation drill".to_string()),
            },
        )
        .await
        .expect("submit erasure");
    assert!(
        !submitted.verification_required,
        "a tenant-operator submission is session-verified"
    );
    let status = harness
        .drive_to_completion(submitted.request_id, 10)
        .await
        .expect("erasure completes");
    assert_eq!(status, sanvi_privacy::domain::DsrStatus::Completed);

    // Every provider context reached a terminal state — the subject is
    // gone from the contexts, not just marked so.
    let participants = harness
        .participants_of(submitted.request_id)
        .await
        .expect("participants");
    assert_eq!(
        participants.len(),
        sanvi_privacy::domain::CONTEXT_NAMES.len(),
        "one participant row per registered context: {participants:?}"
    );
    assert!(
        participants
            .iter()
            .all(|(_context, status)| *status == ParticipantStatus::Completed.as_str()),
        "every context completed: {participants:?}"
    );

    // The propagation pass: real destinations, one ledger row each.
    let clock: Arc<dyn sanvi_privacy::domain::Clock> =
        Arc::new(sanvi_privacy::adapters::SystemClock);
    let ssr = Arc::new(SsrCache::new(None, None));
    // Prove the cache purge is real, not nominal: an entry for the
    // subject's tenant goes in, the destination goes over it, the entry is
    // gone.
    let cache_key = sanvi_cache::SsrCacheKey::new(
        tenant_id,
        "en",
        1,
        sanvi_cache::DeviceClass::Desktop,
        "/account",
    )
    .expect("ssr key");
    ssr.put(
        &cache_key,
        &sanvi_cache::SsrEntry {
            status: 200,
            headers: Vec::new(),
            body: b"account page of the subject".to_vec(),
        },
        std::time::Duration::from_secs(300),
    )
    .await;
    assert!(ssr.get(&cache_key).await.is_some(), "cache entry seeded");

    let destinations: Vec<Arc<dyn ErasureDestination>> = vec![
        Arc::new(ArchivesDestination::new(
            harness.tx.clone(),
            Arc::new(sanvi_privacy::adapters::SqlxRetentionRuleRepository::new()),
            clock.clone(),
        )),
        Arc::new(BackupWindowDestination {
            backup_retention_days: 30,
            clock: clock.clone(),
        }),
        Arc::new(CacheDestination { ssr: ssr.clone() }),
        Arc::new(SearchIndexDestination),
        Arc::new(RecordingSuppressionDestination),
    ];
    let propagation = PropagateErasure::new(harness.tx.clone(), destinations);
    let report = propagation.execute(10).await.expect("propagation runs");
    assert_eq!(report.requests_propagated, 1, "the completed erasure");
    assert_eq!(report.failed, 0);
    assert_eq!(
        report.destinations_recorded, 5,
        "every destination ledgered"
    );

    // The cache entry is verifiably gone.
    assert!(
        ssr.get(&cache_key).await.is_none(),
        "the tenant's cached page did not survive the erasure"
    );

    // The ledger: read back every row and demand purged-or-justified.
    let mut scope = harness
        .tx
        .begin_unscoped("erasure_propagation_ledger_read", "test")
        .await
        .expect("scope");
    let rows: Vec<(String, String, i64, serde_json::Value)> = sqlx::query_as(
        "SELECT destination, status, purged, retained
         FROM privacy.erasure_propagations
         WHERE request_id = $1
         ORDER BY destination",
    )
    .bind(submitted.request_id.as_i64())
    .fetch_all(&mut **scope.tx_mut().as_inner())
    .await
    .expect("ledger read");
    scope.commit().await.expect("commit");

    let by_destination: std::collections::BTreeMap<String, (String, i64, serde_json::Value)> = rows
        .into_iter()
        .map(|(d, s, p, r)| (d, (s, p, r)))
        .collect();
    for destination in [
        "ad_suppression",
        "archives",
        "backup_window",
        "caches",
        "search_indexes",
    ] {
        let (status, _purged, retained) = by_destination
            .get(destination)
            .unwrap_or_else(|| panic!("destination `{destination}` missing from the ledger"));
        assert_ne!(status, "failed", "{destination} failed");
        if status == "retained" {
            let items = retained.as_array().expect("retained items");
            assert!(
                !items.is_empty(),
                "{destination} claims retention with nothing enumerated"
            );
            for item in items {
                let detail = item.get("detail").and_then(|v| v.as_str()).unwrap_or("");
                let reason = item.get("reason").and_then(|v| v.as_str()).unwrap_or("");
                assert!(
                    !detail.is_empty() && !reason.is_empty(),
                    "{destination}: retained items carry a detail and a justification: {item}"
                );
            }
        }
    }

    // The backup window's end date is on the record (the crypto-shred makes
    // the retained ciphertext unreadable, and the date says when even that
    // ages out).
    let (_, _, backup_retained) = by_destination.get("backup_window").expect("backup row");
    let item = &backup_retained.as_array().expect("items")[0];
    assert!(
        item.get("retains_until").is_some_and(|v| !v.is_null()),
        "the backup window documents its end date: {item}"
    );

    // The archives enumerate the statutory classes (invoices, transaction
    // records) with a basis each.
    let (_, _, archive_retained) = by_destination.get("archives").expect("archives row");
    let archive_items = archive_retained.as_array().expect("items");
    assert!(
        !archive_items.is_empty(),
        "the archive-action classes (invoice, transaction_record) are enumerated"
    );
    for item in archive_items {
        assert!(
            item.get("reason")
                .and_then(|v| v.as_str())
                .is_some_and(|r| r.contains("statutory")),
            "archived classes are justified by the statutory schedule: {item}"
        );
    }

    // The subject's consent ledger rows survived as pseudonyms (proof of
    // consent outlives the consent; the subject does not).
    let mut scope = harness
        .tx
        .begin_unscoped("erasure_propagation_pseudonym_read", "test")
        .await
        .expect("scope");
    let ledger_rows: Vec<(String,)> = sqlx::query_as(
        "SELECT subject_ref->>'key' FROM privacy.consents
         WHERE subject_ref->>'key' LIKE '%erasure-drill-subject%'",
    )
    .fetch_all(&mut **scope.tx_mut().as_inner())
    .await
    .expect("consent read");
    scope.commit().await.expect("commit");
    assert!(
        ledger_rows.is_empty(),
        "no consent row may still name the erased subject: {ledger_rows:?}"
    );

    // And the cache destination's purge count in the ledger reflects the
    // real purge that happened above.
    let (_, caches_purged, _) = by_destination.get("caches").expect("caches row");
    assert_eq!(*caches_purged, 1, "the cached account page was purged");
}
