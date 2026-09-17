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

/// The test server may not carry a database literally named `sanvi` (the
/// production-shaped name from `configs/base.toml`); create it if absent.
/// CREATE DATABASE cannot run inside a transaction, so it goes through a
/// plain connection.
async fn ensure_database_named_sanvi(db: &sanvi_testsupport::TestDb) {
    let admin = db.admin_pool().await;
    let exists: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_database WHERE datname = 'sanvi')")
            .fetch_one(&admin)
            .await
            .expect("pg_database is readable");
    if !exists {
        sqlx::query("CREATE DATABASE sanvi")
            .execute(&admin)
            .await
            .expect("test-server superuser creates a database");
    }
}

#[tokio::test]
async fn refuses_a_production_shaped_database_even_with_a_bypass_variable_set() {
    let db = sanvi_testsupport::test_db().await;
    ensure_database_named_sanvi(&db).await;

    let out = run_cli(&db, "sanvi", "1", &["--dry-run"]);
    assert_ne!(out.code, 0, "seed-scale must refuse the name 'sanvi'");
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

    // A second real run refuses instead of half-colliding on primary keys.
    let out = run_cli(&db, db.name(), "1", &[]);
    assert_ne!(out.code, 0, "a re-seed of the same database must refuse");
    assert!(
        out.text.contains("already holds scale data"),
        "{}",
        out.text
    );
}
