//! Integration tests for `seed-scale` (TASK-019 steps 3, FR-1101).
//!
//! These drive the real binary (the CLI is a binary crate, so the tests
//! spawn it via `CARGO_BIN_EXE_*`) against throwaway databases on the
//! shared test server. The refusal test points the command at a
//! production-shaped database name — created on the test server for the
//! purpose — with the would-be bypass variable set, and asserts the guard
//! refuses anyway: there is no bypass variable, and the guard reads the
//! name the connection resolves to, not one a caller can claim.

use std::process::Command;

/// Spawn `sanvi-cli seed-scale` against `name` on the shared test server.
/// `SANVI_SEED_SCALE_ALLOW` is set on every invocation: a plausible bypass
/// variable that must change nothing, because the command never reads it.
fn run_cli(db: &sanvi_testsupport::TestDb, database: &str, seed: &str, extra: &[&str]) -> Output {
    let repo_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("apps/cli sits two levels below the repository root")
        .to_path_buf();

    let output = Command::new(env!("CARGO_BIN_EXE_sanvi-cli"))
        .args(["--env", "local", "seed-scale", "--seed", seed])
        .args(extra)
        .env("SANVI__DATABASE__HOST", "localhost")
        .env("SANVI__DATABASE__PORT", db.port().to_string())
        .env("SANVI__DATABASE__NAME", database)
        .env("SANVI__DATABASE__USER", "sanvi_app")
        .env("SANVI__DATABASE__PASSWORD", "app")
        .env("SANVI_SEED_SCALE_ALLOW", "yes-i-know-what-i-am-doing")
        .current_dir(&repo_root)
        .output()
        .expect("the seed-scale binary spawns");

    Output {
        code: output.status.code().unwrap_or(-1),
        text: format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ),
    }
}

struct Output {
    code: i32,
    text: String,
}

/// The refusal test needs a database whose name is production-shaped —
/// no staging prefix, so the guard must refuse it — but that no real dev
/// database could ever carry and no concurrent run can collide on: unique
/// per test database (the harness's uuid token). Created here, dropped by
/// the test before it asserts, and the negative test's safety therefore
/// never depends on the guard it is testing. CREATE/DROP DATABASE cannot
/// run inside a transaction, so they go through plain statements.
async fn create_production_shaped_database(db: &sanvi_testsupport::TestDb) -> String {
    let name = format!("sanvi_prod_{}", db.name().trim_start_matches("sanvi_t_"));
    let admin = db.admin_pool().await;
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "DROP DATABASE IF EXISTS {name} WITH (FORCE)"
    )))
    .execute(&admin)
    .await
    .expect("test-server superuser drops a leftover database");
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!("CREATE DATABASE {name}")))
        .execute(&admin)
        .await
        .expect("test-server superuser creates a database");
    name
}

async fn drop_database(db: &sanvi_testsupport::TestDb, name: &str) {
    let admin = db.admin_pool().await;
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "DROP DATABASE IF EXISTS {name} WITH (FORCE)"
    )))
    .execute(&admin)
    .await
    .expect("test-server superuser drops the throwaway database");
}

#[tokio::test]
async fn refuses_a_production_shaped_database_even_with_a_bypass_variable_set() {
    let db = sanvi_testsupport::test_db().await;
    let name = create_production_shaped_database(&db).await;

    let out = run_cli(&db, &name, "1", &["--dry-run"]);

    // Drop before asserting: a failed assert must not leak the database.
    drop_database(&db, &name).await;

    assert_ne!(
        out.code, 0,
        "seed-scale must refuse a production-shaped name"
    );
    assert!(
        out.text.contains("staging pattern"),
        "refusal must name the staging pattern, got: {}",
        out.text
    );
}

#[tokio::test]
async fn seeds_a_staging_database_and_matches_the_plan() {
    let db = sanvi_testsupport::test_db().await;

    // Dry-run on a staging-named database: allowed, and no rows are written.
    let out = run_cli(&db, db.name(), "1", &["--dry-run"]);
    assert_eq!(
        out.code, 0,
        "dry-run on a staging-named database: {}",
        out.text
    );
    let plan: serde_json::Value =
        serde_json::from_str(out.text.trim()).expect("dry-run prints one JSON document");
    let total = plan["total_rows"].as_u64().expect("total_rows");
    assert!(total > 0);

    let tenants: i64 = sqlx::query_scalar("SELECT count(*) FROM tenancy.tenants")
        .fetch_one(&db.admin_pool().await)
        .await
        .expect("count tenants");
    assert_eq!(tenants, 0, "a dry-run writes nothing");

    // The real run: writes the full plan.
    let out = run_cli(&db, db.name(), "1", &[]);
    assert_eq!(out.code, 0, "seed-scale writes: {}", out.text);

    let admin = db.admin_pool().await;
    let tenants: i64 = sqlx::query_scalar("SELECT count(*) FROM tenancy.tenants")
        .fetch_one(&admin)
        .await
        .expect("count tenants");
    assert_eq!(
        u64::try_from(tenants).unwrap(),
        plan["tenants"].as_u64().unwrap()
    );

    // The three hot tenants exist, and the hot dimensions have their rows.
    let hot: Vec<String> = sqlx::query_scalar(
        "SELECT slug FROM tenancy.tenants WHERE slug LIKE 'scale-hot-%' ORDER BY slug",
    )
    .fetch_all(&admin)
    .await
    .expect("hot tenants");
    assert_eq!(
        hot,
        vec!["scale-hot-ads", "scale-hot-audit", "scale-hot-payments"]
    );

    let (audit_hot,): (i64,) = sqlx::query_as(
        "SELECT count(*) FROM audit.entries e JOIN tenancy.tenants t ON t.id = e.tenant_id \
         WHERE t.slug = 'scale-hot-audit'",
    )
    .fetch_one(&admin)
    .await
    .expect("audit rows");
    assert!(
        audit_hot >= 10_000,
        "the audit hot tenant is hot: {audit_hot}"
    );

    let (metrics_rows,): (i64,) = sqlx::query_as(
        "SELECT count(*) FROM advertising.metrics_daily m JOIN tenancy.tenants t ON t.id = m.tenant_id \
         WHERE t.slug = 'scale-hot-ads'",
    )
    .fetch_one(&admin)
    .await
    .expect("metrics rows");
    assert!(
        metrics_rows >= 2_000,
        "the ads hot tenant is hot: {metrics_rows}"
    );

    let (refunds,): (i64,) = sqlx::query_as(
        "SELECT count(*) FROM payments.refunds r JOIN tenancy.tenants t ON t.id = r.tenant_id \
         WHERE t.slug = 'scale-hot-payments'",
    )
    .fetch_one(&admin)
    .await
    .expect("refund rows");
    assert!(
        refunds >= 500,
        "the payments hot tenant carries many refunds: {refunds}"
    );

    // The written tables must match the plan's printed row counts — the
    // seed's contract is "same seed = identical row counts" (the payments
    // table once quietly wrote a row per checkout, 25 % more than planned).
    let counts = &plan["row_counts"];
    for table in ["checkouts", "payments", "refunds"] {
        let expected = counts[table].as_u64().expect("plan prints the row count");
        let (actual,): (i64,) = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT count(*) FROM payments.{table}"
        )))
        .fetch_one(&admin)
        .await
        .expect("payment-table row count");
        assert_eq!(
            u64::try_from(actual).unwrap(),
            expected,
            "{table} rows must match the plan"
        );
    }

    // And the state mix must be one production can produce: every payment
    // sits on a completed checkout, succeeded there, with a capture.
    let (inconsistent,): (i64,) = sqlx::query_as(
        "SELECT count(*) FROM payments.payments p \
         JOIN payments.checkouts c ON c.id = p.checkout_id \
         WHERE c.status <> 'complete' OR p.status <> 'succeeded' OR p.captured_at IS NULL",
    )
    .fetch_one(&admin)
    .await
    .expect("payment/checkout consistency count");
    assert_eq!(
        inconsistent, 0,
        "payments exist only as captures of completed checkouts"
    );

    // A second real run refuses instead of half-colliding on primary keys.
    let out = run_cli(&db, db.name(), "1", &[]);
    assert_ne!(out.code, 0, "a re-seed of the same database must refuse");
    assert!(
        out.text.contains("already holds scale data"),
        "{}",
        out.text
    );
}
