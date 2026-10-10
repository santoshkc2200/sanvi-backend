//! TASK-028 step 3 verification — `just archival-roundtrip`.
//!
//! The one gate the spec names: archive a range, query it (and get the
//! archived-range answer, never a silent partial), restore it, and
//! assert **identical rows including ordering and checksums**. A
//! neighbour tenant's rows in the same range must be untouched, and the
//! archived tenant's rows outside the range must stay in the table.
//!
//! The cipher here is a fixed-key AES-256-GCM stand-in for the phase-05
//! envelope scheme: the composition roots (CLI, worker) bridge the same
//! port to `EnvelopeCrypto::encrypt_with_passphrase`, and the crypto
//! itself is proven by the privacy crate's own tests. What this suite
//! proves is the mover: ranges, ordering, checksums, per-tenant scope,
//! and the read-path answer.

use std::sync::Arc;

use aes_gcm::Aes256Gcm;
use aes_gcm::aead::{Aead, KeyInit, Payload};
use chrono::{Duration, TimeZone, Utc};
use sanvi_archival::storage::{ArchiveCipher, LocalEncryptedVolume, SealedStorage};
use sanvi_archival::{ArchivalError, mover::ArchiveTarget, ranges};
use sqlx::Connection;

/// Fixed-key AES-256-GCM with a deterministic nonce — test-only, and
/// fine for that: determinism here makes failures reproducible.
struct TestCipher(Aes256Gcm);

impl TestCipher {
    fn new() -> Self {
        Self(Aes256Gcm::new_from_slice(&[7u8; 32]).expect("fixed key length"))
    }
}

impl ArchiveCipher for TestCipher {
    fn seal(&self, plaintext: &[u8]) -> Result<Vec<u8>, ArchivalError> {
        self.0
            .encrypt((&[9u8; 12]).into(), Payload::from(plaintext))
            .map_err(|_| ArchivalError::Cipher("seal failed".to_string()))
    }

    fn open(&self, sealed: &[u8]) -> Result<Vec<u8>, ArchivalError> {
        self.0
            .decrypt((&[9u8; 12]).into(), Payload::from(sealed))
            .map_err(|_| ArchivalError::Cipher("open failed".to_string()))
    }
}

/// Two tenants + one platform-scope writer, fixed ids in this suite's
/// own range (910_288_xxx — nothing else may mint these).
const TENANT_A: i64 = 910_288_001;
const TENANT_B: i64 = 910_288_002;

fn range_anchor() -> chrono::DateTime<Utc> {
    // A fixed past month, independent of today.
    Utc.with_ymd_and_hms(2026, 1, 10, 0, 0, 0).unwrap()
}

/// Mint the anchor month's audit partition (audit.entries is partitioned
/// since TASK-028). Backfill semantics: exactly that month, no horizon.
async fn ensure_anchor_partition(pool: &sqlx::PgPool) {
    sqlx::query("SELECT platform.ensure_monthly_partitions('2026-01-10')")
        .execute(pool)
        .await
        .expect("mint the anchor month's partitions");
}

/// Seed under the audited platform scope, exactly like every platform
/// writer does (`audit.entries` is FORCE RLS since phase 03).
async fn seed_audit_rows(
    pool: &sqlx::PgPool,
    rows: &[(i64, Option<i64>, chrono::DateTime<Utc>, &str)],
) {
    let mut conn = pool.acquire().await.expect("seed connection");
    let mut tx = conn.begin().await.expect("seed transaction");
    sqlx::query("SELECT set_config('sanvi.platform_scope', 'on', true)")
        .execute(&mut *tx)
        .await
        .expect("platform scope");
    for (id, tenant_id, occurred_at, action) in rows {
        sqlx::query(
            "INSERT INTO audit.entries
               (id, occurred_at, actor_type, actor_id, on_behalf_of, tenant_id,
                action, resource_type, resource_id, before, after, request_id,
                ip, user_agent, prev_hash, hash)
             VALUES ($1, $2, 'system', NULL, NULL, $3, $4, 'test', NULL, NULL, NULL, NULL,
                     NULL, NULL, $5, $6)",
        )
        .bind(id)
        .bind(occurred_at)
        .bind(tenant_id)
        .bind(action)
        .bind([0u8; 32])
        .bind([1u8; 32])
        .execute(&mut *tx)
        .await
        .expect("seed audit row");
    }
    tx.commit().await.expect("seed commit");
}

async fn mover(
    db: &sanvi_testsupport::TestDb,
    volume_root: &std::path::Path,
) -> sanvi_archival::mover::ArchiveMover {
    let ids = Arc::new(sanvi_kernel::SnowflakeGenerator::new(
        sanvi_kernel::NodeId::new(29).expect("node id 29"),
    ));
    let storage = Arc::new(SealedStorage::new(
        Arc::new(LocalEncryptedVolume::new(volume_root)),
        Arc::new(TestCipher::new()),
    ));
    // The mover connects as the migrator: lifting the append-only guard
    // for the delete is an owner operation (production: the CLI's
    // archive commands run with migrator credentials, like `migrate`).
    let tx = sanvi_persistence::TxManager::new(db.migrator_pool().await, ids.clone());
    sanvi_archival::mover::ArchiveMover::new(tx, ids, storage)
}

/// The canonical serialization the checksums are taken over — same
/// statement shape as the mover, exposed for the assertion. Read under
/// the audited platform scope (the table is FORCE RLS; a bare app-pool
/// read would silently see nothing).
async fn canonical(
    pool: &sqlx::PgPool,
    tenant_id: Option<i64>,
    start: chrono::DateTime<Utc>,
    end: chrono::DateTime<Utc>,
) -> serde_json::Value {
    let tenant_clause = match tenant_id {
        Some(tenant) => format!(" AND tenant_id = {tenant}"),
        None => " AND tenant_id IS NULL".to_string(),
    };
    let sql = format!(
        r#"SELECT COALESCE(jsonb_agg(row), '[]'::jsonb)
           FROM (SELECT * FROM audit.entries
                 WHERE occurred_at >= '{start}' AND occurred_at < '{end}'{tenant_clause}
                 ORDER BY occurred_at, id) row"#
    );
    let mut conn = pool.acquire().await.expect("canonical connection");
    let mut tx = conn.begin().await.expect("canonical transaction");
    sqlx::query("SELECT set_config('sanvi.platform_scope', 'on', true)")
        .execute(&mut *tx)
        .await
        .expect("platform scope");
    let value = sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
        .fetch_one(&mut *tx)
        .await
        .expect("canonical serialization");
    tx.commit().await.expect("canonical commit");
    value
}

/// Tenant-visible row count under the same platform scope.
async fn tenant_count(pool: &sqlx::PgPool, tenant_id: i64) -> i64 {
    let mut conn = pool.acquire().await.expect("count connection");
    let mut tx = conn.begin().await.expect("count transaction");
    sqlx::query("SELECT set_config('sanvi.platform_scope', 'on', true)")
        .execute(&mut *tx)
        .await
        .expect("platform scope");
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM audit.entries WHERE tenant_id = $1")
        .bind(tenant_id)
        .fetch_one(&mut *tx)
        .await
        .expect("count");
    tx.commit().await.expect("count commit");
    count
}

#[tokio::test]
async fn archival_round_trips_with_identical_rows_ordering_and_checksums() {
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();

    let start = range_anchor();
    let end = start + Duration::days(10);
    ensure_anchor_partition(&pool).await;

    // Tenant A: three rows inside the range (two share a timestamp, so
    // the id tiebreak in the canonical ordering is exercised) and one
    // outside it, which must never move. Tenant B: two rows inside the
    // range, which must never move either.
    seed_audit_rows(
        &pool,
        &[
            (910_288_101, Some(TENANT_A), start, "a.first"),
            (910_288_102, Some(TENANT_A), start, "a.same-instant"),
            (
                910_288_103,
                Some(TENANT_A),
                start + Duration::days(9),
                "a.last",
            ),
            (
                910_288_104,
                Some(TENANT_A),
                end + Duration::days(1),
                "a.outside",
            ),
            (
                910_288_201,
                Some(TENANT_B),
                start + Duration::days(1),
                "b.first",
            ),
            (
                910_288_202,
                Some(TENANT_B),
                end - Duration::seconds(1),
                "b.last",
            ),
        ],
    )
    .await;

    let before = canonical(&pool, Some(TENANT_A), start, end).await;

    let volume = tempfile();
    let mover = mover(&db, &volume).await;

    // --- archive -----------------------------------------------------------
    let outcome = mover
        .archive(&ArchiveTarget {
            schema_name: "audit".to_string(),
            table_name: "entries".to_string(),
            key_column: "occurred_at".to_string(),
            tenant_id: Some(TENANT_A),
            range_start: start,
            range_end: end,
        })
        .await
        .expect("archive tenant A's range");
    assert_eq!(
        outcome.row_count, 3,
        "three in-range rows of tenant A moved"
    );
    assert!(outcome.archive_id > 0);

    // In-range rows are gone for A, present for B; out-of-range A rows
    // stay. A silently-partial neighbour effect is the failure mode this
    // must rule out.
    let a_in_range: i64 = {
        let mut conn = pool.acquire().await.unwrap();
        let mut tx = conn.begin().await.unwrap();
        sqlx::query("SELECT set_config('sanvi.platform_scope', 'on', true)")
            .execute(&mut *tx)
            .await
            .unwrap();
        let count: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM audit.entries
             WHERE tenant_id = $1 AND occurred_at >= $2 AND occurred_at < $3",
        )
        .bind(TENANT_A)
        .bind(start)
        .bind(end)
        .fetch_one(&mut *tx)
        .await
        .unwrap();
        tx.commit().await.unwrap();
        count
    };
    assert_eq!(a_in_range, 0, "archived rows left the table");
    let a_outside = tenant_count(&pool, TENANT_A).await;
    assert_eq!(a_outside, 1, "out-of-range rows stay");
    let b_count = tenant_count(&pool, TENANT_B).await;
    assert_eq!(b_count, 2, "the neighbour tenant is untouched");

    // --- the read path answers archived-range, never a silent partial ------
    let mut conn = pool.acquire().await.unwrap();
    let overlapping_a = ranges::overlapping_reads(
        &mut conn,
        "audit",
        "entries",
        Some(TENANT_A),
        Some(start),
        Some(end),
    )
    .await
    .unwrap();
    assert_eq!(overlapping_a.len(), 1, "tenant A's read window is archived");
    assert_eq!(overlapping_a[0].id, outcome.archive_id);

    let overlapping_b = ranges::overlapping_reads(
        &mut conn,
        "audit",
        "entries",
        Some(TENANT_B),
        Some(start),
        Some(end),
    )
    .await
    .unwrap();
    assert!(overlapping_b.is_empty(), "the neighbour reads normally");

    let overlapping_platform =
        ranges::overlapping_reads(&mut conn, "audit", "entries", None, Some(start), Some(end))
            .await
            .unwrap();
    assert_eq!(
        overlapping_platform.len(),
        1,
        "a platform-wide read over the window is also archived-range"
    );

    // --- re-archiving the same range is refused loudly ---------------------
    let re = mover
        .archive(&ArchiveTarget {
            schema_name: "audit".to_string(),
            table_name: "entries".to_string(),
            key_column: "occurred_at".to_string(),
            tenant_id: Some(TENANT_A),
            range_start: start + Duration::days(1),
            range_end: end - Duration::days(1),
        })
        .await;
    assert!(
        matches!(re, Err(ArchivalError::RangeArchived { archive_id, .. }) if archive_id == outcome.archive_id),
        "an overlapping archive must be refused, got {re:?}"
    );

    // --- restore, and prove identical rows, ordering, checksums -----------
    let restore = mover.restore(outcome.archive_id).await.expect("restore");
    assert_eq!(restore.row_count, 3);
    assert_eq!(
        restore.checksum, outcome.checksum,
        "the restored canonical serialization checksums to the archived one"
    );

    let after = canonical(&pool, Some(TENANT_A), start, end).await;
    assert_eq!(before, after, "identical rows in identical order");

    // Restored means readable again: the read path no longer answers
    // archived-range over this window.
    let cleared = ranges::overlapping_reads(
        &mut conn,
        "audit",
        "entries",
        Some(TENANT_A),
        Some(start),
        Some(end),
    )
    .await
    .unwrap();
    assert!(cleared.is_empty());

    // The neighbour is still exactly itself.
    let b_after = tenant_count(&pool, TENANT_B).await;
    assert_eq!(b_after, 2);
}

/// A purged archive: the cold object is gone, the registry row remains as
/// evidence, and the read path answers normally (the rows are not coming
/// back — this is the offboarding end state, not a restore candidate).
#[tokio::test]
async fn purged_archives_stop_gating_reads_and_keep_their_evidence_row() {
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();

    let start = range_anchor();
    let end = start + Duration::days(5);
    ensure_anchor_partition(&pool).await;
    seed_audit_rows(&pool, &[(910_288_301, Some(TENANT_A), start, "a.purge-me")]).await;

    let volume = tempfile();
    let mover = mover(&db, &volume).await;
    let outcome = mover
        .archive(&ArchiveTarget {
            schema_name: "audit".to_string(),
            table_name: "entries".to_string(),
            key_column: "occurred_at".to_string(),
            tenant_id: Some(TENANT_A),
            range_start: start,
            range_end: end,
        })
        .await
        .expect("archive");

    // The offboarding sweep: delete the cold object, mark the row purged.
    let record = ranges::by_id(&pool, outcome.archive_id)
        .await
        .unwrap()
        .unwrap();
    let sealed = std::fs::read(volume.join(&record.storage_path)).expect("cold object exists");
    assert!(!sealed.is_empty());
    assert!(
        &sealed != b"[]",
        "the cold object is sealed ciphertext, not the plaintext payload"
    );
    std::fs::remove_file(volume.join(&record.storage_path)).expect("delete cold object");
    let mut conn = pool.acquire().await.unwrap();
    ranges::mark_purged(&mut conn, outcome.archive_id)
        .await
        .unwrap();
    drop(conn);

    let after = ranges::by_id(&pool, outcome.archive_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(after.status, "purged");
    assert!(after.purged_at.is_some(), "the evidence row keeps the when");

    let mut conn = pool.acquire().await.unwrap();
    let overlapping = ranges::overlapping_reads(
        &mut conn,
        "audit",
        "entries",
        Some(TENANT_A),
        Some(start),
        Some(end),
    )
    .await
    .unwrap();
    assert!(
        overlapping.is_empty(),
        "a purged range no longer claims reads"
    );

    // And the restore path refuses it: purged is terminal.
    let refused = mover.restore(outcome.archive_id).await;
    assert!(
        matches!(&refused, Err(ArchivalError::WrongStatus { status, .. }) if status == "purged"),
        "restoring a purged archive must refuse, got {refused:?}"
    );
}

fn tempfile() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "sanvi-archival-test-{}",
        std::process::id() as u128 ^ rand_suffix()
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

fn rand_suffix() -> u128 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u128)
        .unwrap_or(0)
}
