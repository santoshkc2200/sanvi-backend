//! TASK-028 (FR-1130) tests for `retention-report`: the report is
//! generated from live data (rules + run ledger + oldest surviving
//! rows), passes while every executor-backed class runs inside its
//! window, and fails loudly naming a deliberately stalled class — the
//! `RetentionClassStalled` alert's own predicate.

use std::process::Command;

/// Every class with a scheduled executor — the alert's list.
const EXECUTOR_CLASSES: &[&str] = &[
    "consent_record",
    "preference",
    "advertising_conversion_event",
    "advertising_metric",
    "invoice",
    "transaction_record",
];

struct CliOutcome {
    code: i32,
    text: String,
}

async fn run_retention_report(db: &sanvi_testsupport::TestDb, database: &str) -> CliOutcome {
    let repo_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("apps/cli sits two levels below the repository root")
        .to_path_buf();

    let output = Command::new(env!("CARGO_BIN_EXE_sanvi-cli"))
        .args(["--env", "local", "retention-report"])
        .env("SANVI__DATABASE__HOST", "localhost")
        .env("SANVI__DATABASE__PORT", db.port().to_string())
        .env("SANVI__DATABASE__NAME", database)
        .env("SANVI__DATABASE__USER", "sanvi_app")
        .env("SANVI__DATABASE__PASSWORD", "app")
        .current_dir(&repo_root)
        .output()
        .expect("the retention-report binary spawns");

    CliOutcome {
        code: output.status.code().unwrap_or(-1),
        text: format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ),
    }
}

/// Record a fresh run for every executor-backed class.
async fn seed_runs(db: &sanvi_testsupport::TestDb, database: &str) {
    let pool = sqlx::PgPool::connect(&format!(
        "postgres://sanvi_app:app@localhost:{}/{}",
        db.port(),
        database
    ))
    .await
    .expect("connect");
    for class in EXECUTOR_CLASSES {
        sqlx::query(
            "INSERT INTO privacy.retention_runs (data_class, action, rows_affected, executor)
             VALUES ($1, 'delete', 0, 'test')",
        )
        .bind(class)
        .execute(&pool)
        .await
        .expect("seed run");
    }
}

#[tokio::test]
async fn report_passes_when_every_executor_class_ran_and_fails_naming_a_stalled_one() {
    let db = sanvi_testsupport::test_db().await;
    let database = db.name().to_string();

    // 1. Nothing has run for invoice/transaction_record (the archival
    //    sweep is flag-off by default) — the report must fail loudly,
    //    naming them: this is the alert's predicate, from live data.
    let stalled = run_retention_report(&db, &database).await;
    assert_ne!(stalled.code, 0, "a stalled class must fail the report");
    assert!(
        stalled.text.contains("invoice"),
        "the stalled list must name invoice; got: {}",
        stalled.text
    );
    assert!(
        stalled.text.contains("RetentionClassStalled"),
        "the failure must name its alert"
    );

    // 2. Every executor-backed class inside the window: the report
    //    passes and carries the per-class facts.
    seed_runs(&db, &database).await;
    let healthy = run_retention_report(&db, &database).await;
    assert_eq!(
        healthy.code, 0,
        "all classes running inside the window must pass; got: {}",
        healthy.text
    );
    let parsed: serde_json::Value =
        serde_json::from_str(healthy.text.trim()).expect("the report is one JSON document");
    let classes = parsed["classes"].as_array().expect("classes array");
    assert!(
        classes.len() >= 15,
        "every registered rule is reported (found {})",
        classes.len()
    );
    let consent = classes
        .iter()
        .find(|c| c["data_class"] == "consent_record")
        .expect("consent_record reported");
    assert_eq!(consent["executor"], "privacy.retention_sweep");
    assert_eq!(consent["stalled"], false);
    assert!(
        consent["last_run"]["rows_affected"].is_number(),
        "the last run carries rows affected"
    );

    // 3. A deliberately stalled class: backdate one class past the
    //    window and the report fails naming exactly it. (The ledger is
    //    INSERT-only for the app role; the test wields the owner role.)
    let pool = db.migrator_pool().await;
    sqlx::query("DELETE FROM privacy.retention_runs WHERE data_class = 'preference'")
        .execute(&pool)
        .await
        .expect("clear preference runs");
    sqlx::query(
        "INSERT INTO privacy.retention_runs (data_class, run_at, action, rows_affected, executor)
         VALUES ('preference', now() - interval '72 hours', 'delete', 0, 'test')",
    )
    .execute(&pool)
    .await
    .expect("backdate");
    let breached = run_retention_report(&db, &database).await;
    assert_ne!(
        breached.code, 0,
        "the backdated class must stall the report"
    );
    assert!(
        breached.text.contains("preference"),
        "the stalled list must name preference"
    );
}

/// The enforcement inventory is honest: classes without a scheduled
/// executor say how they are enforced instead of pretending.
#[tokio::test]
async fn report_names_the_enforcement_kind_of_unscheduled_classes() {
    let db = sanvi_testsupport::test_db().await;
    let database = db.name().to_string();
    seed_runs(&db, &database).await;

    let outcome = run_retention_report(&db, &database).await;
    assert_eq!(
        outcome.code, 0,
        "executor classes all running: {}",
        outcome.text
    );
    let parsed: serde_json::Value =
        serde_json::from_str(outcome.text.trim()).expect("the report is one JSON document");
    let classes = parsed["classes"].as_array().expect("classes array");
    let find = |class: &str| {
        classes
            .iter()
            .find(|c| c["data_class"] == class)
            .unwrap_or_else(|| panic!("{class} reported"))
            .clone()
    };
    let audit_log = find("audit_log");
    assert_eq!(audit_log["stalled"], serde_json::Value::Null);
    assert!(
        audit_log["enforcement"]
            .as_str()
            .expect("text")
            .contains("by-construction"),
        "audit_log's enforcement is by construction"
    );
    let membership = find("membership");
    assert_eq!(membership["enforcement"], "tenant-lifecycle");
}
