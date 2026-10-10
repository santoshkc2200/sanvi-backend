//! The scheduled archival sweep (TASK-028, FR-1130): the retention
//! classes registered `action = 'archive'` — invoices, transaction
//! records — age out of the hot database through the mover, monthly
//! steps per tenant, and every pass lands in `privacy.retention_runs`
//! so the report can prove the machinery executed the rules.
//!
//! Boundary tracking is the archives themselves: the sweep resumes from
//! the newest archived range end for that table and tenant, so a missed
//! day is caught up on the next run and no range is ever archived twice
//! (the registry's unique index is the backstop).

use std::sync::Arc;

use chrono::{DateTime, Datelike, Duration, TimeZone, Utc};
use sanvi_persistence::TxManager;
use sqlx::PgPool;

use crate::mover::{ArchiveMover, ArchiveTarget};
use crate::storage::SealedStorage;

/// One table of an archive-action class.
pub struct ClassTable {
    pub schema: &'static str,
    pub table: &'static str,
    pub key_column: &'static str,
}

/// The archive-action classes' tables, in dependency order (children
/// before parents: `payments.payments` is referenced by refunds and
/// disputes, so they archive first). A registered archive class with no
/// entry here is skipped loudly — an unknown class must never be
/// archived by guesswork.
pub fn archive_class_tables(data_class: &str) -> Option<&'static [ClassTable]> {
    match data_class {
        "invoice" => Some(&[ClassTable {
            schema: "billing",
            table: "invoices",
            key_column: "created_at",
        }]),
        "transaction_record" => Some(&[
            ClassTable {
                schema: "payments",
                table: "refunds",
                key_column: "created_at",
            },
            ClassTable {
                schema: "payments",
                table: "disputes",
                key_column: "created_at",
            },
            ClassTable {
                schema: "payments",
                table: "payments",
                key_column: "created_at",
            },
        ]),
        _ => None,
    }
}

/// What one sweep pass did, per class.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SweepClassReport {
    pub data_class: String,
    pub action: String,
    pub rows_archived: i64,
    pub archives_created: i64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct SweepReport {
    pub started_at: DateTime<Utc>,
    pub classes: Vec<SweepClassReport>,
}

/// How many monthly steps one tenant-table pass may take: a bounded
/// catch-up, so a long backlog cannot monopolize the job (the next pass
/// continues from the boundary).
const MAX_STEPS_PER_TENANT: usize = 6;

/// Run one sweep pass. Reads the archive-action rules from
/// `privacy.retention_rules` (the single source of truth phase 05
/// established), moves aged rows, and records the run-ledger rows.
pub async fn archive_aged_classes(
    pool: &PgPool,
    mover: &ArchiveMover,
) -> Result<SweepReport, crate::ArchivalError> {
    let started = Utc::now();

    let rules: Vec<(String, String, i64)> = sqlx::query_as(
        "SELECT data_class, action, (EXTRACT(EPOCH FROM period) / 86400)::bigint AS days
         FROM privacy.retention_rules WHERE action = 'archive' ORDER BY data_class",
    )
    .fetch_all(pool)
    .await
    .map_err(crate::ArchivalError::storage)?;

    let mut classes = Vec::new();
    for (data_class, _action, period_days) in rules {
        let Some(tables) = archive_class_tables(&data_class) else {
            tracing::warn!(
                data_class = %data_class,
                "retention rule is action=archive but no table mapping exists; skipping loudly"
            );
            continue;
        };

        let cutoff = started - Duration::days(period_days);
        let mut rows_archived: i64 = 0;
        let mut archives_created: i64 = 0;

        // Tenants with rows aged past the cutoff in any table of the
        // class — the sweep is per-tenant so a neighbour's live history
        // never moves.
        let mut tenants: Vec<i64> = Vec::new();
        for class_table in tables {
            let found: Vec<i64> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                "SELECT DISTINCT tenant_id FROM {}.{} WHERE {} < $1 LIMIT 1000",
                class_table.schema, class_table.table, class_table.key_column
            )))
            .bind(cutoff)
            .fetch_all(pool)
            .await
            .map_err(crate::ArchivalError::storage)?;
            for tenant in found {
                if !tenants.contains(&tenant) {
                    tenants.push(tenant);
                }
            }
        }

        for tenant_id in tenants {
            // Children before parents (the map's order), so an FK never
            // blocks a parent's move.
            for class_table in tables {
                let moved = sweep_one_table(pool, mover, class_table, tenant_id, cutoff).await?;
                rows_archived += moved.0;
                archives_created += moved.1;
            }
        }

        // The run-ledger row: the class ran, whatever it found. This is
        // the row TASK-029's evidence packs and the staleness alert read.
        sqlx::query(
            "INSERT INTO privacy.retention_runs
               (data_class, action, rows_affected, executor, detail)
             VALUES ($1, 'archive', $2, 'platform.archival_sweep', $3::jsonb)",
        )
        .bind(&data_class)
        .bind(rows_archived)
        .bind(
            serde_json::to_string(&serde_json::json!({ "archives_created": archives_created }))
                .unwrap_or_else(|_| "{}".to_string()),
        )
        .execute(pool)
        .await
        .map_err(crate::ArchivalError::storage)?;

        classes.push(SweepClassReport {
            data_class,
            action: "archive".to_string(),
            rows_archived,
            archives_created,
        });
    }

    Ok(SweepReport {
        started_at: started,
        classes,
    })
}

/// One tenant's one table of a class: monthly steps from the last
/// archived boundary to the cutoff. Returns (rows moved, archives
/// created).
async fn sweep_one_table(
    pool: &PgPool,
    mover: &ArchiveMover,
    class_table: &ClassTable,
    tenant_id: i64,
    cutoff: DateTime<Utc>,
) -> Result<(i64, i64), crate::ArchivalError> {
    let (schema, table, key_column) = (
        class_table.schema,
        class_table.table,
        class_table.key_column,
    );

    // Resume from the newest archived (or restored) boundary for this
    // table+tenant; from the aged rows' own start when none.
    let boundary: Option<DateTime<Utc>> = sqlx::query_scalar(
        "SELECT max(range_end) FROM platform.archives
         WHERE schema_name = $1 AND table_name = $2 AND tenant_id = $3",
    )
    .bind(schema)
    .bind(table)
    .bind(tenant_id)
    .fetch_optional(pool)
    .await
    .map_err(crate::ArchivalError::storage)?
    .flatten();

    let oldest: Option<DateTime<Utc>> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT min({key_column}) FROM {schema}.{table} WHERE tenant_id = $1"
    )))
    .bind(tenant_id)
    .fetch_optional(pool)
    .await
    .map_err(crate::ArchivalError::storage)?
    .flatten();

    let Some(oldest) = oldest else {
        return Ok((0, 0));
    };
    let start_month = boundary.unwrap_or(oldest).max(oldest);

    let mut rows = 0i64;
    let mut archives = 0i64;
    let mut month = month_start(start_month);
    let mut steps = 0usize;
    while month < cutoff && steps < MAX_STEPS_PER_TENANT {
        let next = succ_month(month);
        // Only the aged part of the final month: the sweep never
        // archives rows younger than the cutoff.
        let end = next.min(cutoff);
        if end > month {
            let target = ArchiveTarget {
                schema_name: schema.to_string(),
                table_name: table.to_string(),
                key_column: key_column.to_string(),
                tenant_id: Some(tenant_id),
                range_start: month,
                range_end: end,
            };
            let overlap = {
                let mut conn = pool
                    .acquire()
                    .await
                    .map_err(crate::ArchivalError::storage)?;
                crate::ranges::overlapping_active(&mut conn, &target)
                    .await
                    .map_err(crate::ArchivalError::storage)?
            };
            if overlap.is_none() {
                let outcome = mover.archive(&target).await?;
                rows += outcome.row_count;
                if outcome.archive_id > 0 {
                    archives += 1;
                }
            }
        }
        month = next;
        steps += 1;
    }
    Ok((rows, archives))
}

fn month_start(at: DateTime<Utc>) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(at.year(), at.month(), 1, 0, 0, 0)
        .single()
        .unwrap_or(at)
}

fn succ_month(month: DateTime<Utc>) -> DateTime<Utc> {
    let (year, next_month) = if month.month() == 12 {
        (month.year() + 1, 1)
    } else {
        (month.year(), month.month() + 1)
    };
    Utc.with_ymd_and_hms(year, next_month, 1, 0, 0, 0)
        .single()
        .unwrap_or(month + Duration::days(28))
}

/// Convenience for the composition roots: a mover over the given pool.
pub fn mover_for(
    pool: &PgPool,
    ids: Arc<dyn sanvi_kernel::RawIdGenerator>,
    storage: Arc<SealedStorage>,
) -> ArchiveMover {
    ArchiveMover::new(TxManager::new(pool.clone(), ids.clone()), ids, storage)
}
