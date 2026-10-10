//! TASK-028 (FR-1130): the retention report — proof from live data that
//! the phase-05 rules actually execute.
//!
//! For every class in `privacy.retention_rules`: the rule (period,
//! action, basis), its last recorded run (from `privacy.retention_runs`
//! — never a promise), the rows that run affected, and the oldest
//! surviving row in the class's table. Executor-backed classes (the
//! privacy sweep, the partition job's advertising enforcement, the
//! archival sweep) must have run inside the staleness window or the
//! report fails loudly — a stalled class is `RetentionClassStalled`, the
//! alert the worker evaluates from the same ledger.
//!
//! Classes without a scheduled executor are reported with their
//! enforcement kind (`by-construction`, `tenant-lifecycle`) rather than
//! silently omitted — an honest "who enforces this" inventory, which is
//! the input TASK-029's evidence packs consume.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::Serialize;

/// How a class's retention is enforced. `Executor` classes are the
/// staleness-alerted ones; the others are reported as facts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Enforcement {
    /// A scheduled job executes the rule; runs are ledgered.
    Executor(&'static str),
    /// Enforced by construction (e.g. append-only grants guarantee the
    /// minimum window; archival is the removal path).
    ByConstruction(&'static str),
    /// Enforced by the tenant lifecycle: the rows live and die with the
    /// tenant (offboarding is the executor).
    TenantLifecycle,
    /// Registered but nothing enforces it — the report says so.
    Unenforced,
}

/// One class's table facts, for the oldest-surviving-row probe.
struct ClassTable {
    schema: &'static str,
    table: &'static str,
    key_column: &'static str,
}

fn class_facts(data_class: &str) -> (Option<ClassTable>, Enforcement) {
    let table = |schema, table, key| {
        Some(ClassTable {
            schema,
            table,
            key_column: key,
        })
    };
    match data_class {
        "consent_record" => (
            table("privacy", "consents", "recorded_at"),
            Enforcement::Executor("privacy.retention_sweep"),
        ),
        "preference" => (
            table("privacy", "preference_signals", "observed_at"),
            Enforcement::Executor("privacy.retention_sweep"),
        ),
        "advertising_conversion_event" => (
            table("advertising", "conversion_events", "occurred_at"),
            Enforcement::Executor("platform.partition_maintenance"),
        ),
        "advertising_metric" => (
            table("advertising", "metrics_daily", "date"),
            Enforcement::Executor("platform.partition_maintenance"),
        ),
        "invoice" => (
            table("billing", "invoices", "created_at"),
            Enforcement::Executor("platform.archival_sweep"),
        ),
        "transaction_record" => (
            table("payments", "payments", "created_at"),
            Enforcement::Executor("platform.archival_sweep"),
        ),
        "audit_log" => (
            table("audit", "entries", "occurred_at"),
            Enforcement::ByConstruction(
                "append-only grants guarantee the >= 1 year minimum; archival is the removal path",
            ),
        ),
        // The bytes live in hitox-media-service, not this database; the
        // disclosure is contractual and deletion rides the service's
        // owner-reference lifecycle (offboarding calls it explicitly).
        "media_asset" => (
            None,
            Enforcement::ByConstruction(
                "bytes live in hitox-media-service; deletion rides the owner-reference lifecycle",
            ),
        ),
        "identity_profile" | "authentication" | "membership" | "contact" | "communication" => {
            (None, Enforcement::TenantLifecycle)
        }
        "billing" => (None, Enforcement::TenantLifecycle),
        "jurisdiction" | "request_record" => (None, Enforcement::TenantLifecycle),
        _ => (None, Enforcement::Unenforced),
    }
}

/// A class's report row.
#[derive(Debug, Clone, Serialize)]
pub struct ClassReport {
    pub data_class: String,
    pub sensitivity: String,
    pub basis: Option<String>,
    pub period_days: i64,
    pub action: String,
    pub enforcement: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub executor: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_run: Option<LastRun>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub oldest_surviving_row: Option<DateTime<Utc>>,
    /// `Some(true)` when an executor-backed class has not run inside the
    /// window; `None` when the question does not apply.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stalled: Option<bool>,
}

#[derive(Debug, Clone, Serialize)]
pub struct LastRun {
    pub run_at: DateTime<Utc>,
    pub action: String,
    pub rows_affected: i64,
    pub executor: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct RetentionReport {
    pub generated_at: DateTime<Utc>,
    /// The staleness window applied to executor-backed classes.
    pub staleness_window_hours: i64,
    pub classes: Vec<ClassReport>,
    pub stalled_classes: Vec<String>,
}

/// The alert's window: executors run daily; 48h tolerates one missed
/// pass without paging about it.
pub const STALENESS_WINDOW_HOURS: i64 = 48;

pub async fn build(pool: &sqlx::PgPool) -> anyhow::Result<RetentionReport> {
    let generated_at = Utc::now();

    // The oldest-row probes read tenant tables; run them under the
    // audited platform scope like every other platform maintenance read.
    let tx = sanvi_persistence::TxManager::new(
        pool.clone(),
        Arc::new(sanvi_kernel::SnowflakeGenerator::new(
            sanvi_kernel::NodeId::new(24)?,
        )),
    );
    let mut scope = tx
        .begin_unscoped("retention_report", "cli:retention-report")
        .await?;
    let conn = &mut **scope.tx_mut().as_inner();

    let rules: Vec<(String, String, Option<String>, i64, String)> = sqlx::query_as(
        "SELECT data_class, sensitivity, basis::text,
                (EXTRACT(EPOCH FROM period) / 86400)::bigint AS days, action
         FROM privacy.retention_rules ORDER BY data_class",
    )
    .fetch_all(&mut *conn)
    .await?;

    let mut classes = Vec::new();
    let mut stalled_classes = Vec::new();
    for (data_class, sensitivity, basis, period_days, action) in rules {
        let (table, enforcement) = class_facts(&data_class);

        let last_run: Option<(DateTime<Utc>, String, i64, String)> = sqlx::query_as(
            "SELECT run_at, action, rows_affected, executor
             FROM privacy.retention_runs
             WHERE data_class = $1
             ORDER BY run_at DESC LIMIT 1",
        )
        .bind(&data_class)
        .fetch_optional(&mut *conn)
        .await?;

        let oldest = match &table {
            Some(table) => sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                "SELECT min({}) FROM \"{}\".\"{}\"",
                table.key_column.replace('"', "\"\""),
                table.schema.replace('"', "\"\""),
                table.table.replace('"', "\"\""),
            )))
            .fetch_optional(&mut *conn)
            .await?
            .flatten(),
            None => None,
        };

        let stalled = match (&enforcement, &last_run) {
            (Enforcement::Executor(_), Some((run_at, ..))) => {
                Some(*run_at < generated_at - chrono::Duration::hours(STALENESS_WINDOW_HOURS))
            }
            (Enforcement::Executor(_), None) => Some(true),
            _ => None,
        };
        if stalled == Some(true) {
            stalled_classes.push(data_class.clone());
        }

        let enforcement_text = match &enforcement {
            Enforcement::Executor(name) => format!("executor:{name}"),
            Enforcement::ByConstruction(reason) => format!("by-construction ({reason})"),
            Enforcement::TenantLifecycle => "tenant-lifecycle".to_string(),
            Enforcement::Unenforced => "unenforced".to_string(),
        };

        classes.push(ClassReport {
            data_class,
            sensitivity,
            basis,
            period_days,
            action,
            enforcement: enforcement_text,
            executor: match &enforcement {
                Enforcement::Executor(name) => Some((*name).to_string()),
                _ => None,
            },
            last_run: last_run.map(|(run_at, action, rows_affected, executor)| LastRun {
                run_at,
                action,
                rows_affected,
                executor,
            }),
            oldest_surviving_row: oldest,
            stalled,
        });
    }
    scope.commit().await?;

    Ok(RetentionReport {
        generated_at,
        staleness_window_hours: STALENESS_WINDOW_HOURS,
        classes,
        stalled_classes,
    })
}

pub async fn run(environment: &str, write: Option<String>) -> anyhow::Result<()> {
    let cli_config = crate::config::load(environment)?;
    let pool = sanvi_persistence::build_pool(&cli_config.database).await?;
    let report = build(&pool).await?;
    let json = serde_json::to_string_pretty(&report)?;
    if let Some(path) = write {
        std::fs::write(&path, format!("{json}\n"))?;
        println!("retention-report: written to {path}");
    } else {
        println!("{json}");
    }
    if !report.stalled_classes.is_empty() {
        // Fail loudly: a stalled class is RetentionClassStalled firing.
        eprintln!(
            "retention-report: STALLED CLASSES (RetentionClassStalled): {}",
            report.stalled_classes.join(", ")
        );
        anyhow::bail!(
            "{} retention class(es) have not run inside the {}h window",
            report.stalled_classes.len(),
            STALENESS_WINDOW_HOURS
        );
    }
    Ok(())
}
