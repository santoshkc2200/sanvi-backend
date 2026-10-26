//! Regression proofs for the minimum-compliance fixes, using the real provider registry.
use sanvi_api::compliance::ComplianceHarness;
use sanvi_privacy::{
    PrivacyConfig,
    application::{EvidenceInput, RecordConsentChangeCommand, SubjectInput, SubmitDsrCommand},
    domain::{
        DirectiveState, DsrKind, DsrStatus, EnvelopeCrypto, ProcessingPurpose, Requester,
        SubjectKeyRepository, SubjectRef,
    },
};
use std::sync::Arc;
fn command(kind: DsrKind, country: &str) -> SubmitDsrCommand {
    SubmitDsrCommand {
        kind,
        requester: Requester::Subject,
        agent: None,
        note: None,
        subject: SubjectInput {
            kind: "tenant_user".to_string(),
            email: None,
            external_id: None,
            age_signal: None,
            evidence: vec![EvidenceInput {
                kind: "declared_residency".to_string(),
                value: country.to_string(),
            }],
        },
    }
}
async fn setup() -> (
    sanvi_testsupport::TestDb,
    ComplianceHarness,
    Arc<dyn sanvi_kernel::RawIdGenerator>,
) {
    let db = sanvi_testsupport::test_db().await;
    let ids = sanvi_testsupport::test_ids();
    let harness = ComplianceHarness::build(db.app_pool(), ids.clone());
    (db, harness, ids)
}
#[tokio::test]
async fn restriction_and_objection_survive_new_consent_until_reviewed_release() {
    let (_db, harness, _) = setup().await;
    for kind in [DsrKind::Restriction, DsrKind::Objection] {
        let subject = harness.drill_subject();
        let reference = SubjectRef::user(subject.user_id);
        let request = harness
            .wiring
            .state
            .submit_dsr
            .execute(Some(subject.user_id), None, None, command(kind, "DE"))
            .await
            .unwrap();
        harness.process_dsr.execute(50).await.unwrap();
        harness.process_dsr.execute(50).await.unwrap();
        assert_eq!(
            harness.request_status(request.request_id).await.unwrap(),
            DsrStatus::OnHold
        );
        let mut scope = harness
            .tx
            .begin_unscoped("test_restriction_propagation", "test")
            .await
            .unwrap();
        let queued: i64 = sqlx::query_scalar("SELECT count(*) FROM privacy.consents WHERE granted = false AND evidence->>'request_id' = $1")
            .bind(request.request_id.to_string()).fetch_one(&mut **scope.tx_mut().as_inner()).await.unwrap();
        assert!(queued > 0, "a restriction must queue downstream refusals");
        scope.commit().await.unwrap();
        let grant = || RecordConsentChangeCommand {
            purpose: ProcessingPurpose::MarketingEmail,
            granted: true,
            notice_version: "2026-08-19".to_string(),
            method: "consent".to_string(),
            locale: None,
            signal_source: None,
        };
        assert!(
            harness
                .wiring
                .state
                .record_consent
                .execute(&reference, grant())
                .await
                .is_err()
        );
        assert_eq!(
            harness
                .wiring
                .resolver
                .resolve(&reference, ProcessingPurpose::MarketingEmail)
                .await
                .unwrap()
                .state,
            DirectiveState::Denied
        );
        harness.wiring.state.patch_request.fulfill(subject.user_id, request.request_id,
            "Processing restricted; essential-processing exceptions assessed and downstream action verified.",
            "case R-1 reviewed decision and delivery evidence").await.unwrap();
        harness
            .wiring
            .state
            .patch_request
            .release_restriction(
                subject.user_id,
                request.request_id,
                "Subject instruction verified; notice delivered; case R-1",
            )
            .await
            .unwrap();
        assert!(
            harness
                .wiring
                .state
                .record_consent
                .execute(&reference, grant())
                .await
                .is_ok()
        );
    }
}
#[tokio::test]
async fn corrections_and_other_rights_wait_for_an_actual_recorded_response() {
    let (_db, harness, _) = setup().await;
    for kind in [DsrKind::Rectification, DsrKind::Other] {
        let subject = harness.drill_subject();
        let request = harness
            .wiring
            .state
            .submit_dsr
            .execute(Some(subject.user_id), None, None, command(kind, "GB"))
            .await
            .unwrap();
        harness.process_dsr.execute(50).await.unwrap();
        assert_eq!(
            harness.request_status(request.request_id).await.unwrap(),
            DsrStatus::InProgress
        );
        if kind == DsrKind::Rectification {
            assert!(
                harness
                    .wiring
                    .state
                    .patch_request
                    .execute(
                        subject.user_id,
                        request.request_id,
                        sanvi_privacy::domain::RectifyPatch {
                            corrections: vec![sanvi_privacy::domain::Correction {
                                context: "domains".to_string(),
                                field: "registrant.name".to_string(),
                                value: "Corrected name".to_string(),
                            }]
                        }
                    )
                    .await
                    .is_err(),
                "registrar changes need the owning-service workflow, not a no-op projection update"
            );
            assert_eq!(
                harness.request_status(request.request_id).await.unwrap(),
                DsrStatus::InProgress
            );
        }
        assert!(
            harness
                .wiring
                .state
                .patch_request
                .fulfill(subject.user_id, request.request_id, "", "")
                .await
                .is_err()
        );
        harness
            .wiring
            .state
            .patch_request
            .fulfill(
                subject.user_id,
                request.request_id,
                "The requested information was supplied through the verified case channel.",
                "case-123",
            )
            .await
            .unwrap();
        let status = harness
            .wiring
            .state
            .get_request_status
            .execute(request.request_id, Some(subject.user_id), None)
            .await
            .unwrap();
        assert_eq!(status.status, DsrStatus::Completed);
        assert!(status.fulfillment_summary.unwrap().contains("supplied"));
    }
}
#[tokio::test]
async fn known_teens_and_unreviewed_markets_cannot_enable_high_risk_processing() {
    let (_db, harness, _) = setup().await;
    for country in ["US-CO", "US-OR"] {
        let subject = harness.drill_subject();
        let reference = SubjectRef::user(subject.user_id);
        let mut request = command(DsrKind::Other, country);
        if country == "US-CO" {
            request.subject.age_signal = Some(sanvi_privacy::application::AgeSignal {
                birth_year: Some(chrono::Datelike::year(&chrono::Utc::now()) - 17),
                is_minor: false,
            });
        }
        harness
            .wiring
            .state
            .submit_dsr
            .execute(Some(subject.user_id), None, None, request)
            .await
            .unwrap();
        harness
            .wiring
            .state
            .record_consent
            .execute(
                &reference,
                RecordConsentChangeCommand {
                    purpose: ProcessingPurpose::TargetedAdvertising,
                    granted: true,
                    notice_version: "test".to_string(),
                    method: "consent".to_string(),
                    locale: None,
                    signal_source: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(
            harness
                .wiring
                .resolver
                .resolve(&reference, ProcessingPurpose::TargetedAdvertising)
                .await
                .unwrap()
                .state,
            DirectiveState::Denied
        );
    }
}

#[tokio::test]
async fn a_database_mapping_adds_a_jurisdiction_without_a_code_change() {
    let (_db, harness, _) = setup().await;
    let subject = harness.drill_subject();
    let mut scope = harness
        .tx
        .begin_unscoped("test_mapping", "test")
        .await
        .unwrap();
    harness
        .jurisdictions
        .set_mappings(&mut scope, "uk", &["ZZ".to_string()])
        .await
        .unwrap();
    scope.commit().await.unwrap();
    let request = harness
        .wiring
        .state
        .submit_dsr
        .execute(
            Some(subject.user_id),
            None,
            None,
            command(DsrKind::Other, "ZZ"),
        )
        .await
        .unwrap();
    assert_eq!(request.jurisdiction, "uk");
    let another = harness.drill_subject();
    let request = harness
        .wiring
        .state
        .submit_dsr
        .execute(
            Some(another.user_id),
            None,
            None,
            command(DsrKind::Other, "US-OR"),
        )
        .await
        .unwrap();
    assert_eq!(request.jurisdiction, "global");
}
#[tokio::test]
async fn replica_and_restored_key_cannot_bypass_a_durable_erasure_tombstone() {
    use sanvi_privacy::adapters::{SqlxEnvelopeCrypto, SqlxSubjectKeyRepository, erasure_ledger};
    let (db, harness, ids) = setup().await;
    let subject = harness.drill_subject();
    harness
        .wiring
        .state
        .submit_dsr
        .execute(
            Some(subject.user_id),
            None,
            None,
            command(DsrKind::Other, "DE"),
        )
        .await
        .unwrap();
    let mut scope = harness
        .tx
        .begin_unscoped("test_subject", "test")
        .await
        .unwrap();
    let stored = harness
        .subjects
        .find_by_key(&mut scope, subject.subject_ref.canonical_key())
        .await
        .unwrap()
        .unwrap();
    scope.commit().await.unwrap();
    let root = std::env::temp_dir().join(format!("sanvi-restore-test-{}", uuid::Uuid::now_v7()));
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join(".sanvi-erasure-ledger-v1"), b"v1").unwrap();
    let keys = Arc::new(SqlxSubjectKeyRepository::new());
    let crypto = SqlxEnvelopeCrypto::new(
        harness.tx.clone(),
        keys.clone(),
        [7; 32],
        "test".to_string(),
    )
    .with_erasure_ledger(root.to_string_lossy().to_string());
    let replica = SqlxEnvelopeCrypto::new(
        harness.tx.clone(),
        keys.clone(),
        [7; 32],
        "test".to_string(),
    )
    .with_erasure_ledger(root.to_string_lossy().to_string());
    crypto.create_subject_key(stored.id).await.unwrap();
    let encrypted = crypto
        .encrypt_for_subject(stored.id, b"private")
        .await
        .unwrap();
    replica
        .decrypt_for_subject(stored.id, &encrypted)
        .await
        .unwrap();
    let mut scope = harness
        .tx
        .begin_unscoped("test_key_backup", "test")
        .await
        .unwrap();
    let before = keys.find(&mut scope, stored.id).await.unwrap().unwrap();
    scope.commit().await.unwrap();
    crypto.destroy_subject_key(stored.id).await.unwrap();
    let migrator = db.migrator_pool().await;
    sqlx::query("UPDATE privacy.subject_keys SET wrapped_dek = $2, destroyed_at = NULL WHERE subject_id = $1").bind(stored.id.as_i64()).bind(before.wrapped_dek).execute(&migrator).await.unwrap();
    assert!(
        replica
            .decrypt_for_subject(stored.id, &encrypted)
            .await
            .is_err()
    );
    let mut config = PrivacyConfig {
        erasure_ledger_dir: root.to_string_lossy().to_string(),
        ..PrivacyConfig::default()
    };
    config.kek = serde_json::from_value(serde_json::json!(
        "BwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwc="
    ))
    .unwrap();
    assert!(
        erasure_ledger::require_replayed(&harness.tx, &config)
            .await
            .is_err()
    );
    let audit = sanvi_audit::adapters::PrivacyAuditWriterAdapter::new(Arc::new(
        sanvi_audit::adapters::SqlxAuditWriter::new(ids.clone()),
    ));
    assert_eq!(
        erasure_ledger::queue_restore_replay(&harness.tx, &config, ids, &audit)
            .await
            .unwrap(),
        1
    );
    for _ in 0..5 {
        harness.process_dsr.execute(50).await.unwrap();
    }
    erasure_ledger::require_replayed(&harness.tx, &config)
        .await
        .unwrap();
    std::fs::remove_dir_all(root).unwrap();
}
