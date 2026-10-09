//! TASK-025 (FR-1119): single-tenant restore.
//!
//! The far more common real request: one tenant's data returns to a
//! snapshot state *inside the live system*, without touching anyone else's
//! rows. The snapshot is a restored backup addressable by DSN (e.g. the
//! restore container from `scripts/db/restore.sh --keep`).
//!
//! Shape: every statement is tenant-scoped or id-scoped; the plan is
//! enumerated from the snapshot's own catalog (every base table carrying a
//! `tenant_id` column, in FK topological order — referenced tables before
//! referencing ones); and the whole apply runs in one live transaction in
//! two passes over that order:
//!
//!   1. children first — delete the tenant's live rows the snapshot lacks,
//!      so no parent row is ever removed while a post-snapshot child still
//!      references it;
//!   2. parents first — upsert the snapshot rows byte-exactly, so every
//!      snapshot child lands after its snapshot parent.
//!
//! The operation writes its own audit entry in the same transaction, so a
//! restore that happened is always a restore that was audited.
//! `audit.entries` is deliberately not restored: it is append-only by
//! construction and a restore must never rewrite history — it joins the
//! history instead.
//!
//! `--dry-run` prints the per-table plan and writes nothing.

use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;
use std::time::Instant;

use sanvi_audit::adapters::SqlxAuditWriter;
use sanvi_audit::domain::{ActorType, AuditEntryData, AuditWriter};
use sanvi_kernel::RawIdGenerator;
use sanvi_persistence::{PgPool, TxManager};
use sqlx::postgres::PgConnectOptions;
use std::str::FromStr;

#[derive(Debug, Clone)]
pub struct TenantRestoreArgs {
    pub tenant_slug: String,
    pub from_dsn: String,
    pub dry_run: bool,
}

/// Schemas the enumeration covers. Deliberately enumerated rather than
/// "everything with a tenant_id": a future schema lands here in review.
const APP_SCHEMAS: &[&str] = &[
    "tenancy",
    "identity",
    "access",
    "platform_admin",
    "billing",
    "payments",
    "privacy",
    "localization",
    "theming",
    "domains",
    "advertising",
    "media",
    "notifications",
];

/// Tables the enumeration finds but a tenant restore must never touch,
/// with the reason each is here.
const EXCLUDED_TABLES: &[(&str, &str)] = &[
    // Append-only by trigger; a restore joins the history (its own audit
    // entry), it never rewrites one.
    ("audit", "entries"),
];

/// The tenant row itself: `tenancy.tenants` has no `tenant_id` column (its
/// id *is* the tenant), so it is the fixed first table of every plan.
const ANCHOR_QUALIFIED: &str = "tenancy.tenants";

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct TableRef {
    schema: String,
    name: String,
}

impl TableRef {
    fn qualified(&self) -> String {
        format!("{}.{}", self.schema, self.name)
    }

    /// SQL-safe: the catalog carries columns like `limit` (a reserved
    /// word), so every generated identifier is quoted.
    fn sql(&self) -> String {
        format!("\"{}\".\"{}\"", self.schema, self.name)
    }
}

fn q(column: &str) -> String {
    format!("\"{column}\"")
}

#[derive(Debug, Clone)]
struct TablePlan {
    table: TableRef,
    /// All columns, snapshot catalog order.
    columns: Vec<String>,
    /// Primary-key columns, key order. The plan handles any non-empty key:
    /// `tenancy.tenant_settings` is keyed `(tenant_id, key)`, not `id`.
    pk: Vec<String>,
}

#[derive(Debug, Default)]
struct Outcomes {
    upserted: BTreeMap<String, u64>,
    deleted: BTreeMap<String, u64>,
}

pub async fn run(
    environment: &str,
    args: TenantRestoreArgs,
    ids: Arc<dyn RawIdGenerator>,
) -> anyhow::Result<()> {
    let config = crate::config::load(environment)?;
    let live_pool = sanvi_persistence::build_pool(&config.database).await?;
    let snapshot_pool = PgPool::connect(&args.from_dsn).await?;

    let live_tx = TxManager::new(live_pool.clone(), ids.clone());
    let snapshot_tx = TxManager::new(snapshot_pool.clone(), ids.clone());

    // A restore from the live database itself is a no-op wearing a
    // procedure's clothes. Identity is the connection target, not the
    // database name: two restored servers legitimately run same-named
    // databases.
    let live_target = format!(
        "{}:{}/{}",
        live_pool.connect_options().get_host(),
        live_pool.connect_options().get_port(),
        sqlx::query_scalar::<_, String>("SELECT current_database()")
            .fetch_one(&live_pool)
            .await?
    );
    let snapshot_options = PgConnectOptions::from_str(&args.from_dsn)?;
    let snapshot_target = format!(
        "{}:{}/{}",
        snapshot_options.get_host(),
        snapshot_options.get_port(),
        sqlx::query_scalar::<_, String>("SELECT current_database()")
            .fetch_one(&snapshot_pool)
            .await?
    );
    if live_target == snapshot_target {
        anyhow::bail!(
            "the --from-dsn points at the live database ({live_target}); \
             a restore needs a restored snapshot to read from"
        );
    }
    let snapshot_db = snapshot_options
        .get_database()
        .unwrap_or_default()
        .to_string();

    let tenant_id = resolve_tenant(&live_tx, &args.tenant_slug).await?;
    let snapshot_tenant_id = resolve_tenant(&snapshot_tx, &args.tenant_slug).await?;
    if tenant_id != snapshot_tenant_id {
        anyhow::bail!(
            "snapshot tenant id {snapshot_tenant_id} does not match live tenant id {tenant_id} \
             for slug '{}': refusing a mismatched restore",
            args.tenant_slug
        );
    }

    let plans = build_plans(&snapshot_tx).await?;
    let started = Instant::now();

    if args.dry_run {
        println!(
            "tenant-restore: DRY RUN — tenant '{}' (id {tenant_id}), snapshot '{snapshot_db}': nothing is written",
            args.tenant_slug
        );
        println!("{:<40}{:>12}{:>12}", "table", "snapshot", "live");
        for plan in &plans {
            if plan.table.qualified() == ANCHOR_QUALIFIED {
                println!("{:<40}{:>12}{:>12}", plan.table.qualified(), 1, 1);
                continue;
            }
            let snapshot_count = tenant_row_count(&snapshot_tx, &plan.table, tenant_id).await?;
            let live_count = tenant_row_count(&live_tx, &plan.table, tenant_id).await?;
            println!(
                "{:<40}{:>12}{:>12}",
                plan.table.qualified(),
                snapshot_count,
                live_count
            );
        }
        println!(
            "tenant-restore: {} table(s) planned in {} ms — re-run without --dry-run to apply",
            plans.len(),
            started.elapsed().as_millis()
        );
        live_pool.close().await;
        snapshot_pool.close().await;
        return Ok(());
    }

    // --- the apply: one transaction, audited ------------------------------
    let mut scope = live_tx
        .begin_unscoped("tenant_restore", "cli:tenant-restore")
        .await?;

    // Snapshot rows travel as one jsonb array per table; staging and
    // unstaging happen per table inside the live transaction.
    let mut snapshot_rows: BTreeMap<String, serde_json::Value> = BTreeMap::new();
    for plan in &plans {
        let by_id = plan.table.qualified() == ANCHOR_QUALIFIED;
        snapshot_rows.insert(
            plan.table.qualified(),
            fetch_snapshot_rows(&snapshot_tx, plan, tenant_id, by_id).await?,
        );
    }

    let mut outcomes = Outcomes::default();

    // Phase 1, children first: drop what the snapshot does not have. The
    // anchor is upsert-only — the tenant row exists on both sides (it was
    // resolved by slug) and has no tenant_id column to scope a delete by.
    for plan in plans.iter().rev() {
        if plan.table.qualified() == ANCHOR_QUALIFIED {
            continue;
        }
        stage_rows(&mut scope, plan, &snapshot_rows[&plan.table.qualified()]).await?;
        let key_match = plan
            .pk
            .iter()
            .map(|c| format!("s.{} = t.{}", q(c), q(c)))
            .collect::<Vec<_>>()
            .join(" AND ");
        // Identifiers come from the snapshot catalog, never free input.
        let statement = format!(
            "DELETE FROM {} t
             WHERE t.{} = $1
               AND NOT EXISTS (SELECT 1 FROM tr_restore_src s WHERE {key_match})",
            plan.table.sql(),
            q("tenant_id")
        );
        let deleted = sqlx::query(sqlx::AssertSqlSafe(statement))
            .bind(tenant_id)
            .execute(&mut **scope.tx_mut().as_inner())
            .await?
            .rows_affected();
        unstage_rows(&mut scope).await?;
        if deleted > 0 {
            outcomes.deleted.insert(plan.table.qualified(), deleted);
        }
    }

    // Phase 2, parents first: the tenant row itself, then every snapshot
    // row, byte-exactly.
    for plan in plans.iter() {
        stage_rows(&mut scope, plan, &snapshot_rows[&plan.table.qualified()]).await?;
        let column_list = plan
            .columns
            .iter()
            .map(|c| q(c))
            .collect::<Vec<_>>()
            .join(", ");
        let update_list = plan
            .columns
            .iter()
            .filter(|c| !plan.pk.contains(c))
            .map(|c| format!("{} = EXCLUDED.{}", q(c), q(c)))
            .collect::<Vec<_>>()
            .join(", ");
        let pk = plan.pk.iter().map(|c| q(c)).collect::<Vec<_>>().join(", ");
        // A key-only table (every column is the key, e.g.
        // identity.membership_roles) has nothing to update: an existing row
        // with the same key is already the snapshot's row.
        let conflict_action = if update_list.is_empty() {
            "DO NOTHING".to_string()
        } else {
            format!("DO UPDATE SET {update_list}")
        };
        // Identifiers come from the snapshot catalog, never free input.
        let statement = format!(
            "INSERT INTO {table} ({column_list})
             SELECT {column_list} FROM tr_restore_src
             ON CONFLICT ({pk}) {conflict_action}",
            table = plan.table.sql(),
        );
        let upserted = sqlx::query(sqlx::AssertSqlSafe(statement))
            .execute(&mut **scope.tx_mut().as_inner())
            .await?
            .rows_affected();
        unstage_rows(&mut scope).await?;
        outcomes.upserted.insert(plan.table.qualified(), upserted);
    }

    let writer = Arc::new(SqlxAuditWriter::new(ids.clone()));
    writer
        .append(
            &mut scope,
            &AuditEntryData {
                occurred_at: chrono::Utc::now(),
                actor_type: ActorType::PlatformAdmin,
                actor_id: None,
                on_behalf_of: None,
                tenant_id: Some(sanvi_kernel::TenantId::try_from(tenant_id)?),
                action: "tenant.restore".to_string(),
                resource_type: Some("tenant".to_string()),
                resource_id: Some(tenant_id.to_string()),
                before: None,
                after: Some(serde_json::json!({
                    "slug": args.tenant_slug,
                    "snapshot_database": snapshot_db,
                    "dry_run": false,
                    "tables_upserted": outcomes.upserted,
                    "tables_deleted": outcomes.deleted,
                })),
                request_id: None,
                ip: None,
                user_agent: None,
            },
        )
        .await?;
    scope.commit().await?;

    println!(
        "tenant-restore: tenant '{}' (id {tenant_id}) restored from '{snapshot_db}' in {} ms",
        args.tenant_slug,
        started.elapsed().as_millis()
    );
    for plan in &plans {
        println!(
            "{:<44}{:>10} upserted{:>10} deleted",
            plan.table.qualified(),
            outcomes
                .upserted
                .get(&plan.table.qualified())
                .copied()
                .unwrap_or(0),
            outcomes
                .deleted
                .get(&plan.table.qualified())
                .copied()
                .unwrap_or(0),
        );
    }
    println!("tenant-restore: audited as tenant.restore (audit.entries)");

    live_pool.close().await;
    snapshot_pool.close().await;
    Ok(())
}

/// The snapshot's catalog decides the plan: every base table with a
/// `tenant_id` column in an app schema, minus the exclusions, plus the
/// anchor tenant row — in FK topological order.
async fn build_plans(snapshot_tx: &TxManager) -> anyhow::Result<Vec<TablePlan>> {
    let mut scope = snapshot_tx
        .begin_unscoped("tenant_restore: plan", "cli:tenant-restore")
        .await?;
    let conn = &mut **scope.tx_mut().as_inner();

    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT c.table_schema, c.table_name
         FROM information_schema.columns c
         JOIN information_schema.tables t
           ON t.table_schema = c.table_schema AND t.table_name = c.table_name
          AND t.table_type = 'BASE TABLE'
         WHERE c.column_name = 'tenant_id' AND c.table_schema = ANY($1)
         GROUP BY 1, 2
         ORDER BY 1, 2",
    )
    .bind(
        APP_SCHEMAS
            .iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>(),
    )
    .fetch_all(&mut *conn)
    .await?;

    let mut tables: Vec<TableRef> = rows
        .into_iter()
        .map(|(schema, name)| TableRef { schema, name })
        .filter(|t| {
            !EXCLUDED_TABLES
                .iter()
                .any(|(s, n)| t.schema == *s && t.name == *n)
        })
        .collect();
    let anchor = TableRef {
        schema: ANCHOR_QUALIFIED.split('.').next().unwrap().to_string(),
        name: ANCHOR_QUALIFIED.split('.').nth(1).unwrap().to_string(),
    };
    tables.push(anchor.clone());

    // FK edges between selected tables, then Kahn's algorithm. A cycle is a
    // schema fact the drill has not met; refuse loudly rather than guess.
    let all_fks: Vec<(String, String)> = sqlx::query_as(
        "SELECT conrelid::regclass::text, confrelid::regclass::text
         FROM pg_constraint WHERE contype = 'f'",
    )
    .fetch_all(&mut *conn)
    .await?;
    let selected: std::collections::BTreeSet<String> =
        tables.iter().map(|t| t.qualified()).collect();
    let mut edges: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut indegree: BTreeMap<String, usize> = selected.iter().map(|t| (t.clone(), 0)).collect();
    for (child, parent) in &all_fks {
        if child == parent || !selected.contains(child) || !selected.contains(parent) {
            continue;
        }
        edges.entry(parent.clone()).or_default().push(child.clone());
        *indegree.get_mut(child).expect("selected table") += 1;
    }
    let mut queue: VecDeque<String> = indegree
        .iter()
        .filter(|(_, d)| **d == 0)
        .map(|(t, _)| t.clone())
        .collect();
    let mut ordered: Vec<String> = Vec::with_capacity(selected.len());
    while let Some(parent) = queue.pop_front() {
        ordered.push(parent.clone());
        for child in edges.get(&parent).cloned().unwrap_or_default() {
            let degree = indegree.get_mut(&child).expect("known node");
            *degree -= 1;
            if *degree == 0 {
                queue.push_back(child);
            }
        }
    }
    if ordered.len() != selected.len() {
        let unordered: Vec<&String> = selected.iter().filter(|t| !ordered.contains(t)).collect();
        anyhow::bail!(
            "tenant-restore: foreign-key cycle across tenant tables — the plan \
             needs a manual ordering for: {unordered:?}"
        );
    }

    let mut plans = Vec::with_capacity(ordered.len());
    for qualified in ordered {
        let (schema, name) = qualified.split_once('.').expect("qualified name");
        // Key order comes from indkey itself, not attribute order.
        let pk: Vec<String> = sqlx::query_scalar(
            "SELECT a.attname
             FROM pg_index i
             JOIN LATERAL unnest(i.indkey) WITH ORDINALITY AS k(attnum, ord) ON true
             JOIN pg_attribute a ON a.attrelid = i.indrelid AND a.attnum = k.attnum
             WHERE i.indrelid = format('%I.%I', $1, $2)::regclass AND i.indisprimary
             ORDER BY k.ord",
        )
        .bind(schema)
        .bind(name)
        .fetch_all(&mut *conn)
        .await?;
        if pk.is_empty() {
            anyhow::bail!(
                "tenant-restore: {qualified} has no primary key — \
                 add it to the plan by hand before restoring"
            );
        }
        let columns: Vec<(String, String)> = sqlx::query_as(
            "SELECT column_name, is_generated
             FROM information_schema.columns
             WHERE table_schema = $1 AND table_name = $2
             ORDER BY ordinal_position",
        )
        .bind(schema)
        .bind(name)
        .fetch_all(&mut *conn)
        .await?;
        plans.push(TablePlan {
            table: TableRef {
                schema: schema.to_string(),
                name: name.to_string(),
            },
            columns: columns
                .into_iter()
                .filter(|(_, generated)| generated != "ALWAYS")
                .map(|(name, _)| name)
                .collect(),
            pk,
        });
    }
    scope.commit().await?;
    Ok(plans)
}

async fn resolve_tenant(tx: &TxManager, slug: &str) -> anyhow::Result<i64> {
    let mut scope = tx
        .begin_unscoped("tenant_restore: resolve", "cli:tenant-restore")
        .await?;
    let tenant_id: Option<i64> =
        sqlx::query_scalar("SELECT id FROM tenancy.tenants WHERE slug = $1")
            .bind(slug)
            .fetch_optional(&mut **scope.tx_mut().as_inner())
            .await?;
    scope.commit().await?;
    tenant_id.ok_or_else(|| anyhow::anyhow!("tenant '{slug}' does not exist"))
}

async fn tenant_row_count(tx: &TxManager, table: &TableRef, tenant_id: i64) -> anyhow::Result<i64> {
    let mut scope = tx
        .begin_unscoped("tenant_restore: count", "cli:tenant-restore")
        .await?;
    let statement = format!(
        "SELECT count(*) FROM {} WHERE {} = $1",
        table.sql(),
        q("tenant_id")
    );
    let count: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(statement))
        .bind(tenant_id)
        .fetch_one(&mut **scope.tx_mut().as_inner())
        .await?;
    scope.commit().await?;
    Ok(count)
}

/// The tenant's snapshot rows as one jsonb array — the payload the apply
/// stage feeds to `jsonb_populate_recordset`, ordered by the table's key
/// for a deterministic payload. The anchor filters by id (its id *is* the
/// tenant id); every other table by tenant_id.
async fn fetch_snapshot_rows(
    tx: &TxManager,
    plan: &TablePlan,
    tenant_id: i64,
    by_id: bool,
) -> anyhow::Result<serde_json::Value> {
    let table = plan.table.qualified();
    let order_by = plan.pk.join(", ");
    let mut scope = tx
        .begin_unscoped("tenant_restore: read snapshot", "cli:tenant-restore")
        .await?;
    let filter = if by_id {
        format!("{} = $1", q("id"))
    } else {
        format!("{} = $1", q("tenant_id"))
    };
    let statement = format!(
        "SELECT COALESCE(jsonb_agg(row), '[]'::jsonb)
         FROM (SELECT * FROM {table} WHERE {filter} ORDER BY {order_by}) row"
    );
    let rows: serde_json::Value = sqlx::query_scalar(sqlx::AssertSqlSafe(statement))
        .bind(tenant_id)
        .fetch_one(&mut **scope.tx_mut().as_inner())
        .await?;
    scope.commit().await?;
    Ok(rows)
}

async fn stage_rows(
    scope: &mut sanvi_persistence::PlatformScope<'_>,
    plan: &TablePlan,
    rows: &serde_json::Value,
) -> anyhow::Result<()> {
    let statement = format!(
        "CREATE TEMP TABLE tr_restore_src AS
         SELECT * FROM jsonb_populate_recordset(NULL::{table}, $1::jsonb)",
        table = plan.table.sql(),
    );
    sqlx::query(sqlx::AssertSqlSafe(statement))
        .bind(rows)
        .execute(&mut **scope.tx_mut().as_inner())
        .await?;
    Ok(())
}

async fn unstage_rows(scope: &mut sanvi_persistence::PlatformScope<'_>) -> anyhow::Result<()> {
    sqlx::query("DROP TABLE IF EXISTS tr_restore_src")
        .execute(&mut **scope.tx_mut().as_inner())
        .await?;
    Ok(())
}
