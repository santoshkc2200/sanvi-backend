//! Single-tenant restore, end to end through the shipped binary
//! (TASK-025, FR-1119/NFR-1107).
//!
//! The restore runs `sanvi-cli tenant-restore` against two real databases:
//! a live one and a restored snapshot. The assertions are the task's exit
//! bar verbatim: the target tenant's rows return to the expected state, a
//! neighbour tenant's rows are byte-identical before and after (asserted by
//! checksum across every tenant-scoped table), and the operation appears in
//! the audit log. The dry-run asserts the first half of that promise: it
//! prints the plan and writes nothing.

use std::process::Command;

use sha2::Digest;

use sanvi_persistence::{Migrator, PgPool};
use sanvi_testsupport::{migrations_dir, test_db};
use sqlx::postgres::PgConnectOptions;

/// Deterministic ids far above anything the suite mints.
const TARGET: i64 = 900_000_001;
const NEIGHBOUR: i64 = 900_000_002;

async fn admin(port: u16, db: &str) -> PgPool {
    PgPool::connect_with(
        PgConnectOptions::new()
            .host("127.0.0.1")
            .port(port)
            .database(db)
            .username("postgres")
            .password("postgres")
            .application_name("tenant-restore-tests"),
    )
    .await
    .expect("admin pool")
}

async fn migrate(port: u16, db: &str) {
    let pool = admin(port, db).await;
    Migrator::new(migrations_dir())
        .run(&pool)
        .await
        .expect("migrations up");
    pool.close().await;
}

async fn seed(port: u16, db: &str, variant: &str) {
    let pool = admin(port, db).await;
    for (id, slug, display) in [
        (
            TARGET,
            "restore-target",
            if variant == "live" {
                "Live Name"
            } else {
                "Snapshot Name"
            },
        ),
        (NEIGHBOUR, "restore-neighbour", "Neighbour"),
    ] {
        sqlx::query(
            "INSERT INTO tenancy.tenants (id, slug, display_name, status, region)
             VALUES ($1, $2, $3, 'active', 'us')",
        )
        .bind(id)
        .bind(slug)
        .bind(display)
        .execute(&pool)
        .await
        .expect("tenant seed");
    }
    // A composite-key table (PK (tenant_id, key)) and an id-keyed one with
    // a tenants FK — the two key shapes the plan handles.
    for (tenant, key, value) in [
        (
            TARGET,
            "theme",
            if variant == "live" {
                "live-theme"
            } else {
                "snapshot-theme"
            },
        ),
        (NEIGHBOUR, "theme", "neighbour-theme"),
    ] {
        sqlx::query(
            "INSERT INTO tenancy.tenant_settings (tenant_id, key, value)
             VALUES ($1, $2, to_jsonb($3::text))",
        )
        .bind(tenant)
        .bind(key)
        .bind(value)
        .execute(&pool)
        .await
        .expect("settings seed");
    }
    // id-keyed payments: the live database carries a post-snapshot row the
    // restore must remove; the snapshot carries a row the restore must add.
    let payments: Vec<(i64, i64, &str, &str, i64)> = match variant {
        "live" => vec![
            (910_000_001, TARGET, "pi_target_kept", "succeeded", 100),
            (910_000_002, TARGET, "pi_target_extra", "processing", 7),
            (910_000_003, NEIGHBOUR, "pi_neighbour", "succeeded", 42),
        ],
        _ => vec![
            (910_000_001, TARGET, "pi_target_kept", "succeeded", 555),
            (910_000_004, TARGET, "pi_target_missing", "succeeded", 99),
            (910_000_003, NEIGHBOUR, "pi_neighbour", "succeeded", 42),
        ],
    };
    for (id, tenant, external, status, amount) in payments {
        sqlx::query(
            "INSERT INTO payments.payments (id, tenant_id, external_payment_id, status, amount_minor, currency)
             VALUES ($1, $2, $3, $4, $5, 'USD')",
        )
        .bind(id)
        .bind(tenant)
        .bind(external)
        .bind(status)
        .bind(amount)
        .execute(&pool)
        .await
        .expect("payment seed");
    }
    pool.close().await;
}

/// Byte-identical, asserted by checksum: every tenant-scoped row of the
/// neighbour across the tables the plan touches, hashed in a stable order.
async fn neighbour_checksum(port: u16, db: &str) -> String {
    let pool = admin(port, db).await;
    let mut hash = sha2::Sha256::new();
    for statement in [
        format!(
            "SELECT row_to_json(t)::text FROM (SELECT * FROM tenancy.tenants WHERE id = {NEIGHBOUR}) t ORDER BY id"
        ),
        format!(
            "SELECT row_to_json(t)::text FROM (SELECT * FROM tenancy.tenant_settings WHERE tenant_id = {NEIGHBOUR}) t ORDER BY key"
        ),
        format!(
            "SELECT row_to_json(t)::text FROM (SELECT * FROM payments.payments WHERE tenant_id = {NEIGHBOUR}) t ORDER BY id"
        ),
    ] {
        let rows: Vec<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(statement))
            .fetch_all(&pool)
            .await
            .expect("checksum rows");
        for row in rows {
            use sha2::Digest;
            hash.update(row.as_bytes());
            hash.update(b"\n");
        }
    }
    {
        use sha2::Digest;
        let digest = hash.finalize();
        hex::encode(digest)
    }
}

fn run_cli(port: u16, live_db: &str, snapshot_db: &str, extra_args: &[&str]) -> (String, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_sanvi-cli"))
        .env("SANVI_ENV", "local")
        .env("SANVI__DATABASE__HOST", "127.0.0.1")
        .env("SANVI__DATABASE__PORT", port.to_string())
        .env("SANVI__DATABASE__NAME", live_db)
        .env("SANVI__DATABASE__USER", "postgres")
        .env("SANVI__DATABASE__PASSWORD", "postgres")
        .args([
            "tenant-restore",
            "--tenant",
            "restore-target",
            "--from-dsn",
            &format!("postgres://postgres:postgres@127.0.0.1:{port}/{snapshot_db}"),
        ])
        .args(extra_args)
        .output()
        .expect("the CLI binary runs");
    (
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

#[tokio::test]
async fn tenant_restore_moves_only_the_target_and_audits_itself() {
    let live = test_db().await;
    let snapshot = test_db().await;
    let (port, live_db) = (live.port(), live.name().to_string());
    let snapshot_db = snapshot.name().to_string();

    migrate(port, &live_db).await;
    migrate(port, &snapshot_db).await;
    seed(port, &live_db, "live").await;
    seed(port, &snapshot_db, "snapshot").await;

    let before = neighbour_checksum(port, &live_db).await;
    let (stdout, stderr) = run_cli(port, &live_db, &snapshot_db, &[]);
    assert!(
        stderr.is_empty(),
        "restore failed: {stderr}\n--- stdout ---\n{stdout}"
    );
    assert!(stdout.contains("restored from"), "{stdout}");
    assert!(stdout.contains("audited as tenant.restore"), "{stdout}");

    let pool = admin(port, &live_db).await;
    // The target's rows are exactly the snapshot's state: the tenant row
    // and its composite-key setting restored, the post-snapshot payment
    // deleted, the snapshot-only payment present, the kept payment's
    // amount byte-identical to the snapshot.
    let display: String =
        sqlx::query_scalar("SELECT display_name FROM tenancy.tenants WHERE id = $1")
            .bind(TARGET)
            .fetch_one(&pool)
            .await
            .expect("target tenant");
    assert_eq!(display, "Snapshot Name");
    let theme: String = sqlx::query_scalar(
        "SELECT value #>> '{}' FROM tenancy.tenant_settings WHERE tenant_id = $1 AND key = 'theme'",
    )
    .bind(TARGET)
    .fetch_one(&pool)
    .await
    .expect("target theme");
    assert_eq!(theme, "snapshot-theme");
    let externals: Vec<String> =
        sqlx::query_scalar("SELECT external_payment_id FROM payments.payments WHERE tenant_id = $1 ORDER BY external_payment_id")
            .bind(TARGET)
            .fetch_all(&pool)
            .await
            .expect("target payments");
    assert_eq!(
        externals,
        vec![
            "pi_target_kept".to_string(),
            "pi_target_missing".to_string()
        ],
        "the live-only row must be gone and the snapshot-only row present"
    );
    let amount: i64 = sqlx::query_scalar(
        "SELECT amount_minor FROM payments.payments WHERE external_payment_id = 'pi_target_kept'",
    )
    .fetch_one(&pool)
    .await
    .expect("kept payment");
    assert_eq!(amount, 555, "the kept payment must be the snapshot's bytes");

    // The neighbour: byte-identical, by checksum.
    let after = neighbour_checksum(port, &live_db).await;
    assert_eq!(before, after, "the neighbour tenant's rows moved");

    // The operation is in the audit log, tenant-scoped.
    let audited: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit.entries WHERE action = 'tenant.restore' AND tenant_id = $1",
    )
    .bind(TARGET)
    .fetch_one(&pool)
    .await
    .expect("audit lookup");
    assert_eq!(audited, 1, "the restore must appear in the audit log");
    pool.close().await;
}

#[tokio::test]
async fn dry_run_writes_nothing() {
    let live = test_db().await;
    let snapshot = test_db().await;
    let (port, live_db) = (live.port(), live.name().to_string());
    let snapshot_db = snapshot.name().to_string();

    migrate(port, &live_db).await;
    migrate(port, &snapshot_db).await;
    seed(port, &live_db, "live").await;
    seed(port, &snapshot_db, "snapshot").await;

    let before = neighbour_checksum(port, &live_db).await;
    let (stdout, stderr) = run_cli(port, &live_db, &snapshot_db, &["--dry-run"]);
    assert!(stderr.is_empty(), "dry-run failed: {stderr}\n{stdout}");
    assert!(stdout.contains("DRY RUN"), "{stdout}");
    assert!(stdout.contains("payments.payments"), "{stdout}");

    let pool = admin(port, &live_db).await;
    let amount: i64 = sqlx::query_scalar(
        "SELECT amount_minor FROM payments.payments WHERE external_payment_id = 'pi_target_kept'",
    )
    .fetch_one(&pool)
    .await
    .expect("kept payment");
    assert_eq!(amount, 100, "a dry run changed the target");
    let display: String =
        sqlx::query_scalar("SELECT display_name FROM tenancy.tenants WHERE id = $1")
            .bind(TARGET)
            .fetch_one(&pool)
            .await
            .expect("target tenant");
    assert_eq!(display, "Live Name", "a dry run changed the tenant row");
    let audited: i64 =
        sqlx::query_scalar("SELECT count(*) FROM audit.entries WHERE action = 'tenant.restore'")
            .fetch_one(&pool)
            .await
            .expect("audit lookup");
    assert_eq!(audited, 0, "a dry run must not audit (it did not happen)");
    drop(pool);
    assert_eq!(before, neighbour_checksum(port, &live_db).await);
}
