//! Audit integration tests (real Postgres): the chain appends,
//! verifies, rejects UPDATE/DELETE at the database level, and isolates
//! tenant rows via RLS.

use sanvi_audit::{
    adapters::{SqlxAuditReader, SqlxAuditWriter},
    application::VerifyAuditChain,
    domain::{
        ActorType, AuditEntryData, AuditError, AuditReader, AuditWriter, GENESIS_HASH, verify_chain,
    },
};
use sanvi_kernel::TenantId;
use sanvi_persistence::PlatformScope;
use sanvi_testsupport::TestDb;

/// Fresh database with all migrations applied; returns app-role and
/// owner-role pools, on a database private to the caller.
async fn prepare() -> (sanvi_persistence::PgPool, sanvi_persistence::PgPool, TestDb) {
    let db = sanvi_testsupport::test_db().await;
    let app = db.app_pool();
    let migrator = db.migrator_pool().await;
    (app, migrator, db)
}

fn data(action: &str, tenant_id: Option<TenantId>) -> AuditEntryData {
    AuditEntryData {
        occurred_at: chrono::Utc::now(),
        actor_type: ActorType::PlatformAdmin,
        actor_id: Some(sanvi_kernel::UserId::from_snowflake(
            sanvi_testsupport::test_ids().next_id().unwrap(),
        )),
        on_behalf_of: None,
        tenant_id,
        action: action.to_string(),
        resource_type: Some("tenant".to_string()),
        resource_id: tenant_id.map(|id| id.to_string()),
        before: None,
        after: Some(serde_json::json!({ "status": "suspended" })),
        request_id: None,
        ip: None,
        user_agent: None,
    }
}

#[tokio::test]
async fn appends_form_a_verifiable_chain() {
    let (app, _migrator, _node) = prepare().await;
    let tx_manager = sanvi_persistence::TxManager::new(app.clone(), sanvi_testsupport::test_ids());
    let writer = SqlxAuditWriter::new(sanvi_testsupport::test_ids());
    let reader = SqlxAuditReader::new();

    let mut scope: PlatformScope<'_> = tx_manager
        .begin_unscoped("test_append", "test")
        .await
        .expect("unscoped tx");
    let first = writer
        .append(&mut scope, &data("tenancy.suspend", None))
        .await
        .unwrap();
    let second = writer
        .append(&mut scope, &data("tenancy.resume", None))
        .await
        .unwrap();
    let third = writer
        .append(&mut scope, &data("tenancy.suspend", None))
        .await
        .unwrap();
    scope.commit().await.unwrap();
    assert!(first < second && second < third, "seq must increase");

    // The chain verifies through the use case (paged).
    let verify = VerifyAuditChain::new(
        sanvi_persistence::TxManager::new(app.clone(), sanvi_testsupport::test_ids()),
        std::sync::Arc::new(SqlxAuditReader::new()),
        1, // batch size 1: page boundaries must continue the chain
    );
    let report = verify.execute().await.unwrap();
    assert!(
        report.verified,
        "chain must verify: {:?}",
        report.first_mismatch
    );
    assert_eq!(report.entries_checked, 3);

    // And as a pure function over the rows.
    let mut scope: PlatformScope<'_> = tx_manager
        .begin_unscoped("test_read", "test")
        .await
        .expect("unscoped tx");
    let (entries, _) = reader
        .list(&mut scope, &Default::default(), None, 100, false)
        .await
        .unwrap();
    scope.commit().await.unwrap();
    assert_eq!(entries.len(), 3);
    let mut ordered = entries;
    ordered.reverse();
    assert!(verify_chain(&ordered).is_ok());
    assert_eq!(ordered[0].prev_hash, GENESIS_HASH);
}

#[tokio::test]
async fn update_and_delete_are_rejected_by_the_trigger() {
    let (app, migrator, _node) = prepare().await;
    let tx_manager = sanvi_persistence::TxManager::new(app.clone(), sanvi_testsupport::test_ids());
    let writer = SqlxAuditWriter::new(sanvi_testsupport::test_ids());
    let mut scope: PlatformScope<'_> = tx_manager
        .begin_unscoped("test_append", "test")
        .await
        .expect("unscoped tx");
    writer
        .append(&mut scope, &data("tenancy.suspend", None))
        .await
        .unwrap();
    scope.commit().await.unwrap();

    // The app role has no UPDATE/DELETE grants at all (permission denied
    // without any trigger involvement).
    for statement in [
        "UPDATE audit.entries SET action = 'tenancy.tampered'",
        "DELETE FROM audit.entries",
    ] {
        let result = sqlx::query(statement).execute(&app).await;
        assert!(
            result.is_err(),
            "app role must not hold UPDATE/DELETE on audit.entries"
        );
    }

    // An elevated role that somehow holds UPDATE grants is stopped by the
    // BEFORE trigger: set the RLS escape hatch (any role may set_config)
    // so the UPDATE actually reaches a row, then the trigger raises.
    // Each statement gets its own transaction (the trigger aborts the
    // current one).
    for statement in [
        "UPDATE audit.entries SET action = 'tenancy.tampered'",
        "DELETE FROM audit.entries",
    ] {
        let mut tx = migrator.begin().await.expect("owner tx");
        sqlx::query("SELECT set_config('sanvi.platform_scope', 'on', true)")
            .execute(&mut *tx)
            .await
            .unwrap();
        let result = sqlx::query(statement).execute(&mut *tx).await;
        let err = result.expect_err("append-only trigger must reject modifications");
        assert!(
            err.to_string().contains("append-only"),
            "unexpected error: {err}"
        );
        tx.rollback().await.unwrap();
    }

    // The row is intact and still verifies.
    let verify = VerifyAuditChain::new(
        sanvi_persistence::TxManager::new(app.clone(), sanvi_testsupport::test_ids()),
        std::sync::Arc::new(SqlxAuditReader::new()),
        100,
    );
    assert!(verify.execute().await.unwrap().verified);
}

#[tokio::test]
async fn a_tampered_chain_is_detected() {
    let (app, _migrator, _node) = prepare().await;
    let tx_manager = sanvi_persistence::TxManager::new(app.clone(), sanvi_testsupport::test_ids());
    let writer = SqlxAuditWriter::new(sanvi_testsupport::test_ids());
    let mut scope: PlatformScope<'_> = tx_manager
        .begin_unscoped("test_append", "test")
        .await
        .expect("unscoped tx");
    writer
        .append(&mut scope, &data("tenancy.suspend", None))
        .await
        .unwrap();
    writer
        .append(&mut scope, &data("tenancy.resume", None))
        .await
        .unwrap();
    scope.commit().await.unwrap();

    let verify = VerifyAuditChain::new(
        sanvi_persistence::TxManager::new(app.clone(), sanvi_testsupport::test_ids()),
        std::sync::Arc::new(SqlxAuditReader::new()),
        100,
    );
    assert!(verify.execute().await.unwrap().verified);

    // Simulate tampering by a hostile actor who somehow bypassed the
    // trigger: rewrite the hash of the newest row (e.g. after an offline
    // page-level edit). We emulate it by flipping the stored hash through
    // a *fresh* trigger-free table... which the schema forbids; instead we
    // prove the verifier's core on in-memory rows (the database layer is
    // proven append-only by the test above).
    let entries = {
        let mut scope: PlatformScope<'_> = tx_manager
            .begin_unscoped("test_read", "test")
            .await
            .expect("unscoped tx");
        let (entries, _) = SqlxAuditReader::new()
            .list(&mut scope, &Default::default(), None, 100, false)
            .await
            .unwrap();
        scope.commit().await.unwrap();
        entries
    };
    let mut tampered = entries;
    tampered.reverse();
    tampered[1].action = "tenancy.tampered".to_string();
    let err = verify_chain(&tampered).unwrap_err();
    assert!(matches!(err, AuditError::ChainMismatch { position: 1, .. }));

    // Sanity: the untouched database still verifies.
    assert!(verify.execute().await.unwrap().verified);
}

#[tokio::test]
async fn audit_rows_are_rls_isolated_between_tenants() {
    let (app, _migrator, _node) = prepare().await;
    let tx_manager = sanvi_persistence::TxManager::new(app.clone(), sanvi_testsupport::test_ids());
    let writer = SqlxAuditWriter::new(sanvi_testsupport::test_ids());
    let tenant_a = TenantId::from_snowflake(sanvi_testsupport::test_ids().next_id().unwrap());
    let tenant_b = TenantId::from_snowflake(sanvi_testsupport::test_ids().next_id().unwrap());

    let mut scope: PlatformScope<'_> = tx_manager
        .begin_unscoped("test_append", "test")
        .await
        .expect("unscoped tx");
    writer
        .append(&mut scope, &data("tenancy.suspend", Some(tenant_a)))
        .await
        .unwrap();
    writer
        .append(&mut scope, &data("tenancy.suspend", Some(tenant_b)))
        .await
        .unwrap();
    scope.commit().await.unwrap();

    // A tenant-scoped transaction must see only its own rows (raw SQL:
    // the reader port is platform-scoped by design).
    let mut scoped = tx_manager
        .begin_for(tenant_a.as_i64())
        .await
        .expect("scoped tx");
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM audit.entries WHERE tenant_id = $1")
        .bind(tenant_a.as_i64())
        .fetch_one(&mut **scoped.tx_mut().as_inner())
        .await
        .expect("scoped read");
    assert_eq!(count, 1);
    // A query without the tenant filter still only sees the RLS-visible
    // rows.
    let all: i64 = sqlx::query_scalar("SELECT count(*) FROM audit.entries")
        .fetch_one(&mut **scoped.tx_mut().as_inner())
        .await
        .expect("scoped read");
    assert_eq!(all, 1, "RLS must hide tenant B's rows from tenant A");
    scoped.commit().await.unwrap();

    // The audited platform scope sees everything.
    let mut platform: PlatformScope<'_> = tx_manager
        .begin_unscoped("test_read_all", "test")
        .await
        .expect("unscoped tx");
    let (entries, _) = SqlxAuditReader::new()
        .list(&mut platform, &Default::default(), None, 100, false)
        .await
        .expect("platform read");
    assert_eq!(entries.len(), 2);
    platform.commit().await.unwrap();
}

// ---------------------------------------------------------------------------
// TASK-028 (FR-1129): the archived-range read path.
// ---------------------------------------------------------------------------

/// A read whose window overlaps an active archive of `audit.entries`
/// answers `ArchivedRange` — never a silent partial. After the restore,
/// the same query answers normally. This is the read-path half of the
/// `archival-roundtrip` gate, exercised through the exact composition
/// the api builds when `platform.archival` is on.
#[tokio::test]
async fn ranged_reads_refuse_archived_windows_and_recover_after_restore() {
    let (app, migrator, db) = prepare().await;

    let tenant = TenantId::from_snowflake(sanvi_testsupport::test_ids().next_id().unwrap());
    let tx_manager = sanvi_persistence::TxManager::new(app.clone(), sanvi_testsupport::test_ids());
    let writer = SqlxAuditWriter::new(sanvi_testsupport::test_ids());

    let start = chrono::Utc::now() - chrono::Duration::days(40);
    let end = start + chrono::Duration::days(5);

    // audit.entries is partitioned (TASK-028): every month the seeds and
    // the outside-row land in needs its child. Backfill semantics —
    // exactly those months, no horizon.
    {
        let mut conn = app.acquire().await.unwrap();
        let mut month = start.date_naive();
        let last = (end + chrono::Duration::days(2)).date_naive();
        while month <= last {
            sqlx::query("SELECT platform.ensure_monthly_partitions($1::date)")
                .bind(month)
                .execute(&mut *conn)
                .await
                .expect("mint a seed month's partition");
            // Aug 31 + 1 month is Sep 30, which would skip September:
            // step on month *starts*.
            month = sqlx::query_scalar::<_, chrono::NaiveDate>(
                "SELECT date_trunc('month', $1::date + interval '1 month')::date",
            )
            .bind(month)
            .fetch_one(&mut *conn)
            .await
            .unwrap();
        }
    }

    // Two entries inside the window for the tenant, one outside.
    {
        let mut scope: PlatformScope<'_> = tx_manager
            .begin_unscoped("archival_test_seed", "test")
            .await
            .unwrap();
        let mut inside = data("tenant.archival-probe", Some(tenant));
        inside.occurred_at = start + chrono::Duration::hours(1);
        writer.append(&mut scope, &inside).await.unwrap();
        let mut inside2 = data("tenant.archival-probe", Some(tenant));
        inside2.occurred_at = end - chrono::Duration::hours(1);
        writer.append(&mut scope, &inside2).await.unwrap();
        let mut outside = data("tenant.archival-probe-outside", Some(tenant));
        outside.occurred_at = end + chrono::Duration::days(1);
        writer.append(&mut scope, &outside).await.unwrap();
        scope.commit().await.unwrap();
    }

    // The composition the api builds with `platform.archival` on.
    let reader = SqlxAuditReader::new();
    let list = sanvi_audit::application::ListAuditLog::new(
        tx_manager.clone(),
        std::sync::Arc::new(reader),
    )
    .with_archive_check(std::sync::Arc::new(
        sanvi_audit::adapters::SqlxArchivedRangeCheck::new(),
    ));

    let filters = |since, until| sanvi_audit::domain::AuditFilters {
        tenant_id: Some(tenant),
        actor_id: None,
        action: None,
        since,
        until,
    };

    // Before any archive exists the read answers normally.
    let page = list
        .execute(filters(None, None), None, Some(50))
        .await
        .unwrap();
    assert_eq!(page.entries.len(), 3, "nothing archived yet");

    // Archive the tenant's window through the mover (migrator role: the
    // delete lifts the append-only guard, an owner operation).
    use sanvi_archival::mover::{ArchiveMover, ArchiveTarget};
    use sanvi_archival::storage::{ArchiveCipher, LocalEncryptedVolume, SealedStorage};

    struct PlainCipher;
    impl ArchiveCipher for PlainCipher {
        fn seal(&self, plaintext: &[u8]) -> Result<Vec<u8>, sanvi_archival::ArchivalError> {
            Ok(plaintext.to_vec())
        }
        fn open(&self, sealed: &[u8]) -> Result<Vec<u8>, sanvi_archival::ArchivalError> {
            Ok(sealed.to_vec())
        }
    }

    let volume = std::env::temp_dir().join(format!("sanvi-audit-archival-{}", std::process::id()));
    std::fs::create_dir_all(&volume).unwrap();
    let mover_ids = std::sync::Arc::new(sanvi_kernel::SnowflakeGenerator::new(
        sanvi_kernel::NodeId::new(31).unwrap(),
    ));
    let mover = ArchiveMover::new(
        sanvi_persistence::TxManager::new(migrator.clone(), mover_ids.clone()),
        mover_ids,
        std::sync::Arc::new(SealedStorage::new(
            std::sync::Arc::new(LocalEncryptedVolume::new(&volume)),
            std::sync::Arc::new(PlainCipher),
        )),
    );
    let outcome = mover
        .archive(&ArchiveTarget {
            schema_name: "audit".to_string(),
            table_name: "entries".to_string(),
            key_column: "occurred_at".to_string(),
            tenant_id: Some(tenant.as_i64()),
            range_start: start,
            range_end: end,
        })
        .await
        .expect("archive the window");
    assert_eq!(outcome.row_count, 2);

    // The refused read: any window overlapping the archive — including a
    // boundless one — is an ArchivedRange error naming the window.
    let refused = list.execute(filters(None, None), None, Some(50)).await;
    assert!(
        matches!(&refused, Err(AuditError::ArchivedRange(None, None))),
        "a boundless tenant read must refuse, got {refused:?}"
    );
    let refused = list
        .execute(filters(Some(start), Some(end)), None, Some(50))
        .await;
    assert!(
        matches!(&refused, Err(AuditError::ArchivedRange(Some(_), Some(_)))),
        "the exact window must refuse, got {refused:?}"
    );

    // The neighbour scope (another tenant) reads normally: their window
    // was never archived.
    let other = list
        .execute(
            sanvi_audit::domain::AuditFilters {
                tenant_id: Some(TenantId::from_snowflake(
                    sanvi_testsupport::test_ids().next_id().unwrap(),
                )),
                actor_id: None,
                action: None,
                since: Some(start),
                until: Some(end),
            },
            None,
            Some(50),
        )
        .await
        .unwrap();
    assert!(other.entries.is_empty());

    // Restore, and the same reads answer normally again.
    mover.restore(outcome.archive_id).await.expect("restore");
    let page = list
        .execute(filters(Some(start), Some(end)), None, Some(50))
        .await
        .unwrap();
    assert_eq!(page.entries.len(), 2, "the restored window reads again");
    let _ = db;
}
