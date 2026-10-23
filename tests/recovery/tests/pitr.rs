//! Point-in-time recovery, mid-transaction-stream (TASK-025 step 4,
//! FR-1119).
//!
//! The timed, scale-data drill lives in `scripts/db/backup.sh` +
//! `restore.sh` (`just dr-pitr --rehearse`). This test is the same drill at
//! test scale, in CI, against a real Postgres with WAL archiving on: a base
//! backup is taken mid-stream, the stream continues, and recovery is pinned
//! to a timestamp between the two. The restored database must land exactly
//! on the target and stay consistent in the three places a torn restore
//! shows up first:
//!
//! 1. the audit chain — the real `VerifyAuditChain` use case verifies every
//!    hash link in the restored prefix;
//! 2. the outbox — every event committed at or before the target is present
//!    exactly once, nothing carries a delivery stamp from after it, and the
//!    consumer dedupe ledger never names an event the restored outbox does
//!    not contain (so replay after recovery re-delivers only events whose
//!    handlers are idempotent by the dependency matrix);
//! 3. payment records — the pre-target rows, byte-for-byte their recorded
//!    state, with external ids still unique.
//!
//! The control half restores the *same* backup without a target: the
//! post-backup transaction arrives, the checks now require it. That proves
//! the checks can tell a pinned restore from a full one — a check that
//! passes both ways proves nothing.

use std::sync::Arc;
use std::time::Duration;

use sanvi_audit::{
    adapters::{SqlxAuditReader, SqlxAuditWriter},
    application::VerifyAuditChain,
    domain::{ActorType, AuditEntryData, AuditWriter},
};
use sanvi_kernel::{RawIdGenerator, TenantId, UserId};
use sanvi_persistence::{Migrator, TxManager};
use sanvi_testsupport::{migrations_dir, test_ids};
use sqlx::{PgPool as SqlxPool, postgres::PgConnectOptions};
use testcontainers::{
    ImageExt,
    core::{ContainerPort, ExecCommand},
    runners::AsyncRunner,
};
use testcontainers_modules::postgres::Postgres;

/// Host port the in-container restored instance (5433) is published on.
/// Overridable; the default avoids the dev/test ranges.
const RESTORED_PORT_ENV: &str = "SANVI_PITR_PORT";
const DEFAULT_RESTORED_PORT: u16 = 45433;

struct Sh {
    code: Option<i64>,
    stdout: String,
    stderr: String,
}

impl Sh {
    fn ok(self, what: &str) -> Self {
        assert_eq!(self.code, Some(0), "{what} failed: {}", self.stderr);
        self
    }
}

async fn sh(node: &testcontainers::ContainerAsync<Postgres>, script: &str) -> Sh {
    let env = [("PGPASSWORD", "postgres")];
    let mut result = node
        .exec(ExecCommand::new(["sh", "-c", script]).with_env_vars(env))
        .await
        .expect("exec into the drill container");
    // These readers finish only when the command exits. Sampling the exit
    // status before them can retain None when pg_ctl legitimately takes
    // longer than the brief inspect retry window to start PostgreSQL.
    let stdout =
        String::from_utf8_lossy(&result.stdout_to_vec().await.expect("stdout")).into_owned();
    let stderr =
        String::from_utf8_lossy(&result.stderr_to_vec().await.expect("stderr")).into_owned();
    // Docker's inspect status can still briefly lag the completed log stream.
    let mut code = result.exit_code().await.expect("exec exit code");
    for _ in 0..40 {
        if code.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        code = result.exit_code().await.expect("exec exit code");
    }
    Sh {
        code,
        stdout,
        stderr,
    }
}

/// One psql statement inside the container, trimmed stdout back.
async fn psql(
    node: &testcontainers::ContainerAsync<Postgres>,
    port: u16,
    db: &str,
    sql: &str,
) -> String {
    let script = format!("psql -h 127.0.0.1 -p {port} -U postgres -d {db} -qtAc \"{sql}\"");
    sh(node, &script).await.ok(sql).stdout.trim().to_string()
}

async fn wait_promoted(node: &testcontainers::ContainerAsync<Postgres>, db: &str) {
    for _ in 0..120 {
        let state = psql(node, 5433, db, "SELECT pg_is_in_recovery()").await;
        if state == "f" {
            return;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    panic!("restored instance never left recovery within 60s");
}

/// The drill, both halves: recovery pinned to the recorded target, then the
/// same backup replayed to the archive end as the control.
#[tokio::test]
async fn pitr_to_a_timestamp_leaves_a_consistent_platform() {
    let restored_port = std::env::var(RESTORED_PORT_ENV)
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(DEFAULT_RESTORED_PORT);

    // Explicit mappings for both ports: with any mapped port set,
    // testcontainers disables publish-all-ports, so 5432 must be mapped too
    // (host port 0 = ephemeral).
    let node = Postgres::default()
        .with_tag("18-alpine")
        .with_mapped_port(0, ContainerPort::Tcp(5432))
        .with_mapped_port(restored_port, ContainerPort::Tcp(5433))
        .with_cmd([
            "postgres",
            "-c",
            "archive_mode=on",
            "-c",
            "archive_command=test ! -f /wal_archive/%f && cp %p /wal_archive/%f",
            "-c",
            "archive_timeout=5s",
        ])
        .start()
        .await
        .expect("drill container starts");
    let port = node.get_host_port_ipv4(5432).await.expect("mapped 5432");

    // The archive mountpoint must be writable by the postgres user before
    // any segment completes.
    sh(
        &node,
        "mkdir -p /wal_archive && chown postgres:postgres /wal_archive",
    )
    .await
    .ok("prepare archive dir");

    let ids: Arc<dyn RawIdGenerator> = test_ids();

    // --- roles, database, migrations -------------------------------------
    let admin = SqlxPool::connect_with(
        PgConnectOptions::new()
            .host("127.0.0.1")
            .port(port)
            .username("postgres")
            .password("postgres"),
    )
    .await
    .expect("admin pool");
    // Separately: a multi-statement simple query is one implicit
    // transaction, and CREATE DATABASE refuses to run inside one.
    let db = format!("pitr_{}", uuid::Uuid::now_v7().simple());
    for statement in [
        "CREATE ROLE sanvi_migrator LOGIN PASSWORD 'pitr' NOSUPERUSER NOCREATEDB NOCREATEROLE".to_string(),
        "CREATE ROLE sanvi_app LOGIN PASSWORD 'pitr' NOSUPERUSER NOCREATEDB NOCREATEROLE NOBYPASSRLS".to_string(),
        format!("CREATE DATABASE {db} OWNER sanvi_migrator"),
        format!("GRANT CONNECT ON DATABASE {db} TO sanvi_app"),
    ] {
        sqlx::raw_sql(sqlx::AssertSqlSafe(statement))
            .execute(&admin)
            .await
            .expect("drill roles and database");
    }
    admin.close().await;

    let source = SqlxPool::connect_with(
        PgConnectOptions::new()
            .host("127.0.0.1")
            .port(port)
            .database(&db)
            .username("sanvi_migrator")
            .password("pitr"),
    )
    .await
    .expect("source pool");
    Migrator::new(migrations_dir())
        .run(&source)
        .await
        .expect("migrations up");

    // --- the transaction stream, up to the target ------------------------
    let tx = TxManager::new(source.clone(), ids.clone());
    let writer = Arc::new(SqlxAuditWriter::new(ids.clone()));

    async fn audit_entry(
        tx: &TxManager,
        writer: &Arc<SqlxAuditWriter>,
        action: &str,
        tenant: Option<TenantId>,
    ) {
        let mut scope = tx
            .begin_unscoped("pitr: seed audit entry", "recovery-tests")
            .await
            .expect("audit scope");
        writer
            .append(
                &mut scope,
                &AuditEntryData {
                    occurred_at: chrono::Utc::now(),
                    actor_type: ActorType::System,
                    actor_id: None::<UserId>,
                    on_behalf_of: None,
                    tenant_id: tenant,
                    action: action.to_string(),
                    resource_type: Some("drill".to_string()),
                    resource_id: None,
                    before: None,
                    after: Some(serde_json::json!({ "drill": true })),
                    request_id: None,
                    ip: None,
                    user_agent: None,
                },
            )
            .await
            .expect("audit append");
        scope.commit().await.expect("audit commit");
    }

    let outbox_event = |tx: &TxManager, ids: &Arc<dyn RawIdGenerator>, event: &'static str| {
        let tx = tx.clone();
        let ids = ids.clone();
        async move {
            let id = ids.next_id().expect("snowflake id").as_i64();
            let mut scope = tx
                .begin_unscoped("pitr: seed outbox event", "recovery-tests")
                .await
                .expect("scope");
            sqlx::query(
                "INSERT INTO platform.outbox (id, aggregate, aggregate_id, event_type, payload)
                 VALUES ($1, 'drill', $1, $2, '{\"drill\": true}'::jsonb)",
            )
            .bind(id)
            .bind(event)
            .execute(&mut **scope.tx_mut().as_inner())
            .await
            .expect("outbox insert");
            scope.commit().await.expect("scope commit");
            id
        }
    };

    // Fixture writes go through the unscoped platform scope: the migrated
    // tables FORCE row-level security, so even the owner inserts need the
    // scope the application itself writes under.
    async fn scoped_insert(
        tx: &TxManager,
        what: &'static str,
        run: impl FnOnce(
            &mut sqlx::PgConnection,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = sqlx::Result<sqlx::postgres::PgQueryResult>> + '_>,
        >,
    ) {
        let mut scope = tx
            .begin_unscoped(what, "recovery-tests")
            .await
            .expect("scope");
        run(scope.tx_mut().as_inner())
            .await
            .unwrap_or_else(|err| panic!("{what}: {err}"));
        scope.commit().await.expect("scope commit");
    }

    // A tenant rows hang off (payments FK).
    let tenant_id = ids.next_id().expect("snowflake id").as_i64();
    {
        let (tenant_id, what) = (tenant_id, "tenant insert");
        scoped_insert(&tx, what, move |conn| {
            Box::pin(
                sqlx::query(
                    "INSERT INTO tenancy.tenants (id, slug, display_name, status, region)
                     VALUES ($1, 'pitr-drill', 'PITR drill', 'active', 'us')",
                )
                .bind(tenant_id)
                .execute(conn),
            )
        })
        .await;
    }
    let tenant = TenantId::try_from(tenant_id).expect("tenant id");

    for i in 0..5 {
        audit_entry(&tx, &writer, &format!("drill.pre.{i}"), Some(tenant)).await;
        outbox_event(&tx, &ids, "drill.pre").await;
    }
    let payment_id = ids.next_id().expect("snowflake id").as_i64();
    {
        let (payment_id, tenant_id) = (payment_id, tenant_id);
        scoped_insert(&tx, "payment insert", move |conn| {
            Box::pin(
                sqlx::query(
                    "INSERT INTO payments.payments (id, tenant_id, external_payment_id, status, amount_minor, currency)
                     VALUES ($1, $2, 'pi_drill_pre', 'succeeded', 1234, 'USD')",
                )
                .bind(payment_id)
                .bind(tenant_id)
                .execute(conn),
            )
        })
        .await;
    }

    // The marker the target pins to. Recovery stops between this commit and
    // the post-backup one.
    {
        let mut scope = tx
            .begin_unscoped("markers", "recovery-tests")
            .await
            .expect("scope");
        sqlx::raw_sql("CREATE SCHEMA IF NOT EXISTS drill;
            CREATE TABLE drill.markers(name text PRIMARY KEY, at timestamptz NOT NULL DEFAULT clock_timestamp());
            INSERT INTO drill.markers(name) VALUES ('A2_last_pre_target');")
            .execute(&mut **scope.tx_mut().as_inner())
            .await
            .expect("markers");
        scope.commit().await.expect("scope commit");
    }

    let pre = PreState::capture(&tx).await;

    // --- base backup ------------------------------------------------------
    sh(
        &node,
        "rm -rf /tmp/base && su postgres -c \"pg_basebackup -h 127.0.0.1 -U postgres -D /tmp/base -Fp -Xs --checkpoint=fast -w\"",
    )
    .await
    .ok("pg_basebackup");

    // --- the stream continues past the backup ------------------------------
    for i in 0..5 {
        audit_entry(&tx, &writer, &format!("drill.post.{i}"), Some(tenant)).await;
        outbox_event(&tx, &ids, "drill.post").await;
    }
    {
        let (payment_id, tenant_id) = (ids.next_id().expect("snowflake id").as_i64(), tenant_id);
        scoped_insert(&tx, "post payment insert", move |conn| {
            Box::pin(
                sqlx::query(
                    "INSERT INTO payments.payments (id, tenant_id, external_payment_id, status, amount_minor, currency)
                     VALUES ($1, $2, 'pi_drill_post', 'processing', 99, 'USD')",
                )
                .bind(payment_id)
                .bind(tenant_id)
                .execute(conn),
            )
        })
        .await;
    }
    {
        let mut scope = tx
            .begin_unscoped("marker B", "recovery-tests")
            .await
            .expect("scope");
        sqlx::raw_sql("INSERT INTO drill.markers(name) VALUES ('B_post_target');")
            .execute(&mut **scope.tx_mut().as_inner())
            .await
            .expect("marker B");
        scope.commit().await.expect("scope commit");
    }
    // Flush the post-backup stream into the archive.
    psql(&node, 5432, &db, "SELECT pg_switch_wal()").await;
    for _ in 0..40 {
        let segments = sh(&node, "ls /wal_archive 2>/dev/null | wc -l")
            .await
            .ok("ls archive");
        if segments.stdout.trim() != "0" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    let post = PreState::capture(&tx).await;
    let target: String = psql(
        &node,
        5432,
        &db,
        "SELECT at + interval '1 second' FROM drill.markers WHERE name = 'A2_last_pre_target'",
    )
    .await;
    let target = target.replace(' ', "T");

    // --- restore #1: pinned to the target ----------------------------------
    prepare_restore_dir(&node, Some(&target)).await;
    start_restored(&node).await;
    wait_promoted(&node, &db).await;

    let restored = SqlxPool::connect_with(
        PgConnectOptions::new()
            .host("127.0.0.1")
            .port(restored_port)
            .database(&db)
            .username("sanvi_migrator")
            .password("pitr"),
    )
    .await
    .expect("restored pool");

    // 1. The audit chain verifies — the real use case, every hash link.
    let report = VerifyAuditChain::new(
        TxManager::new(restored.clone(), ids.clone()),
        Arc::new(SqlxAuditReader::new()),
        1000,
    )
    .execute()
    .await
    .expect("chain verification runs");
    assert!(
        report.verified,
        "restored audit chain failed verification: {:?}",
        report.first_mismatch
    );
    assert_eq!(
        report.entries_checked, pre.audit_entries,
        "the restored chain must be exactly the pre-target prefix"
    );

    // 2. The outbox: pre-target set intact, no torn delivery, dedupe ledger
    //    never names an event the restored outbox does not hold. Reads run
    //    through the platform scope (forced RLS).
    let restored_tx = TxManager::new(restored.clone(), ids.clone());
    {
        let mut scope = restored_tx
            .begin_unscoped("pitr: restored outbox checks", "recovery-tests")
            .await
            .expect("scope");
        let conn = &mut **scope.tx_mut().as_inner();
        let outbox_ids: Vec<i64> = sqlx::query_scalar("SELECT id FROM platform.outbox ORDER BY id")
            .fetch_all(&mut *conn)
            .await
            .expect("restored outbox ids");
        assert_eq!(
            outbox_ids, pre.outbox_ids,
            "the restored outbox must be exactly the pre-target event set"
        );
        let torn: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM platform.outbox WHERE published_at IS NOT NULL AND published_at > $1::timestamptz",
        )
        .bind(&target)
        .fetch_one(&mut *conn)
        .await
        .expect("torn delivery query");
        assert_eq!(torn, 0, "delivery stamps from after the restore point");
        let orphan_ledger: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM platform.processed_events pe
             WHERE NOT EXISTS (SELECT 1 FROM platform.outbox o WHERE o.id = pe.event_id)",
        )
        .fetch_one(&mut *conn)
        .await
        .expect("dedupe-ledger query");
        assert_eq!(
            orphan_ledger, 0,
            "the consumer ledger names events the restored outbox does not contain"
        );
        scope.commit().await.expect("scope commit");
    }

    // 3. Payment records: the pre-target row, byte-for-byte its state.
    let payment: (String, String, i64) = {
        let mut scope = restored_tx
            .begin_unscoped("pitr: restored payment check", "recovery-tests")
            .await
            .expect("scope");
        let row = sqlx::query_as::<_, (String, String, i64)>(
            "SELECT external_payment_id, status, amount_minor FROM payments.payments",
        )
        .fetch_one(&mut **scope.tx_mut().as_inner())
        .await
        .expect("restored payment row");
        scope.commit().await.expect("scope commit");
        row
    };
    assert_eq!(
        payment,
        ("pi_drill_pre".to_string(), "succeeded".to_string(), 1234),
        "a post-target payment row leaked into the pinned restore"
    );
    restored.close().await;

    // --- control: the same backup, replayed to the archive end -------------
    // The checks must be able to tell the two restores apart, or they
    // cannot fail on a torn one.
    sh(
        &node,
        "su postgres -c \"pg_ctl -D /tmp/restore -m fast stop\" >/dev/null 2>&1 || true",
    )
    .await;
    prepare_restore_dir(&node, None).await;
    start_restored(&node).await;
    wait_promoted(&node, &db).await;

    let full = SqlxPool::connect_with(
        PgConnectOptions::new()
            .host("127.0.0.1")
            .port(restored_port)
            .database(&db)
            .username("sanvi_migrator")
            .password("pitr"),
    )
    .await
    .expect("full-replay pool");
    let full_tx = TxManager::new(full.clone(), ids.clone());
    let marker_b: String = {
        let mut scope = full_tx
            .begin_unscoped("pitr: full-replay marker check", "recovery-tests")
            .await
            .expect("scope");
        let name: String =
            sqlx::query_scalar("SELECT name FROM drill.markers WHERE name = 'B_post_target'")
                .fetch_one(&mut **scope.tx_mut().as_inner())
                .await
                .expect("the full replay must carry the post-backup marker");
        scope.commit().await.expect("scope commit");
        name
    };
    assert_eq!(marker_b, "B_post_target");
    let report = VerifyAuditChain::new(
        TxManager::new(full.clone(), ids.clone()),
        Arc::new(SqlxAuditReader::new()),
        1000,
    )
    .execute()
    .await
    .expect("chain verification runs");
    assert!(
        report.verified,
        "full-replay chain failed: {:?}",
        report.first_mismatch
    );
    assert_eq!(report.entries_checked, post.audit_entries);
    let full_outbox_ids: Vec<i64> = {
        let mut scope = full_tx
            .begin_unscoped("pitr: full-replay outbox check", "recovery-tests")
            .await
            .expect("scope");
        let ids: Vec<i64> = sqlx::query_scalar("SELECT id FROM platform.outbox ORDER BY id")
            .fetch_all(&mut **scope.tx_mut().as_inner())
            .await
            .expect("full-replay outbox ids");
        scope.commit().await.expect("scope commit");
        ids
    };
    assert_eq!(
        full_outbox_ids, post.outbox_ids,
        "the full replay must carry every event"
    );
    full.close().await;

    sh(
        &node,
        "su postgres -c \"pg_ctl -D /tmp/restore -m fast stop\" >/dev/null 2>&1 || true",
    )
    .await;

    // The two states must differ, or the pinning proved nothing.
    assert_ne!(pre.audit_entries, post.audit_entries);
    assert_ne!(pre.outbox_ids, post.outbox_ids);
}

/// The state snapshot the restore is checked against.
#[derive(Debug, Clone, PartialEq)]
struct PreState {
    audit_entries: u64,
    outbox_ids: Vec<i64>,
}

impl PreState {
    /// Through the platform scope: the migrated tables FORCE row-level
    /// security, so a bare owner connection sees none of the rows.
    async fn capture(tx: &TxManager) -> Self {
        let mut scope = tx
            .begin_unscoped("pitr: capture state", "recovery-tests")
            .await
            .expect("scope");
        let audit_entries: i64 = sqlx::query_scalar("SELECT count(*) FROM audit.entries")
            .fetch_one(&mut **scope.tx_mut().as_inner())
            .await
            .expect("audit count");
        let outbox_ids: Vec<i64> = sqlx::query_scalar("SELECT id FROM platform.outbox ORDER BY id")
            .fetch_all(&mut **scope.tx_mut().as_inner())
            .await
            .expect("outbox ids");
        scope.commit().await.expect("scope commit");
        Self {
            audit_entries: audit_entries as u64,
            outbox_ids,
        }
    }
}

/// Lay the base backup down as a recovery data directory. With a target,
/// recovery pins to it and promotes; without, it consumes the archive.
async fn prepare_restore_dir(
    node: &testcontainers::ContainerAsync<Postgres>,
    target: Option<&str>,
) {
    let target_lines = match target {
        Some(ts) => format!(
            "echo \"recovery_target_time = '{ts}'\" >> /tmp/restore/postgresql.auto.conf\n\
             echo \"recovery_target_action = 'promote'\" >> /tmp/restore/postgresql.auto.conf\n"
        ),
        None => String::new(),
    };
    let script = format!(
        "set -e\n\
         rm -rf /tmp/restore\n\
         cp -a /tmp/base /tmp/restore\n\
         chmod 700 /tmp/restore\n\
         touch /tmp/restore/recovery.signal\n\
         echo \"restore_command = 'cp /wal_archive/%f %p'\" >> /tmp/restore/postgresql.auto.conf\n\
         {target_lines}\
         chown -R postgres:postgres /tmp/restore"
    );
    sh(node, &script).await.ok("prepare restore dir");
}

/// Start the restored instance on 5433 and wait for read-write service.
async fn start_restored(node: &testcontainers::ContainerAsync<Postgres>) {
    sh(
        node,
        "su postgres -c \"pg_ctl -D /tmp/restore -o '-p 5433' -l /tmp/restore.log -w -t 60 start\"",
    )
    .await
    .ok("start restored instance");
}
