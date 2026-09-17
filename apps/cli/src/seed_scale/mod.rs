//! `seed-scale` — deterministic, production-shaped scale data (TASK-019,
//! FR-1101).
//!
//! The same seed produces identical row counts and checksums; the
//! distribution (which tenants, how much of what) lives in [`distribution`]
//! and must not be simplified. The command refuses any target database
//! whose name does not match the staging pattern, checked against the
//! database the connection *actually resolves to* (`current_database()`),
//! never against an environment variable: no variable in this file turns
//! the guard off, because no variable is read.

pub mod distribution;

use std::sync::Arc;

use anyhow::bail;
use chrono::{DateTime, Datelike, TimeZone, Utc};
use sanvi_kernel::{RawIdGenerator, SnowflakeId};
use sanvi_persistence::TxManager;

use distribution::DistributionPlan;

/// Deterministic Snowflake range for seeded rows. Real generators mint ids
/// from the current epoch (≈7.4e15 today); this range sits far above it, so
/// seeded rows can never collide with runtime-minted ids, and the same seed
/// always mints the same ids — which the audit chain's hash covers.
const ID_BASE: i64 = 7_000_000_000_000_000_000;

/// All timestamps derive from this fixed instant (2026-01-01T00:00:00Z),
/// never from the wall clock: determinism is the whole value.
const TIME_BASE_SECS: i64 = 1_767_225_600;

/// Database names a scale seed may target: the staging pattern. Anything
/// else — `sanvi` itself, `sanvi_production`, a restored copy of a
/// production snapshot — is refused.
const STAGING_NAME_PREFIXES: &[&str] = &[
    "sanvi_stage",
    "sanvi_staging",
    "sanvi_scale",
    "sanvi_seed",
    "sanvi_dev",
    "sanvi_template",
    // The harness's throwaway databases.
    "sanvi_t_",
    "sanvi_b_",
];

fn is_staging_database_name(name: &str) -> bool {
    STAGING_NAME_PREFIXES
        .iter()
        .any(|prefix| name.starts_with(prefix))
}

/// Mint deterministic ids for one seeded run.
struct SeededIds {
    next: i64,
}

impl SeededIds {
    fn new() -> Self {
        Self { next: ID_BASE }
    }

    fn next_id(&mut self) -> i64 {
        self.next += 1;
        self.next
    }
}

fn time_at(step: i64) -> DateTime<Utc> {
    Utc.timestamp_opt(TIME_BASE_SECS + step * 60, 0)
        .single()
        .expect("fixed base plus whole minutes is a real instant")
}

fn base_date() -> chrono::NaiveDate {
    time_at(0).date_naive()
}

fn currency_for(region: &str) -> &'static str {
    match region {
        "jp" => "JPY",
        "de" => "EUR",
        _ => "USD",
    }
}

/// A stable pseudo-random-but-deterministic amount for row `n`.
fn amount_minor(n: usize) -> i64 {
    1_000 + ((n as i64 * 7_919) % 49_000)
}

/// `seed-scale --seed N [--dry-run]`: plan the distribution, print its row
/// counts and checksum, and (without `--dry-run`) write it.
pub async fn run(
    environment: &str,
    seed: u64,
    dry_run: bool,
    ids: Arc<dyn RawIdGenerator>,
) -> anyhow::Result<()> {
    let config = crate::config::load(environment)?;
    let pool = sanvi_persistence::build_pool(&config.database).await?;
    run_on_pool(pool, seed, dry_run, ids).await
}

/// The guard, the plan and the write, on an existing pool. The guard reads
/// the database the pool is actually connected to, so no configuration
/// path can name one database and seed another.
async fn run_on_pool(
    pool: sqlx::PgPool,
    seed: u64,
    dry_run: bool,
    ids: Arc<dyn RawIdGenerator>,
) -> anyhow::Result<()> {
    let actual_db: String = sqlx::query_scalar("SELECT current_database()")
        .fetch_one(&pool)
        .await?;
    if !is_staging_database_name(&actual_db) {
        bail!(
            "seed-scale refuses to run against database '{actual_db}': the name does not match \
             the staging pattern ({}).\n\
             There is no bypass variable: point config at a staging-named database instead \
             (e.g. `createdb sanvi_scale` + SANVI__DATABASE__NAME=sanvi_scale).",
            STAGING_NAME_PREFIXES.join(", ")
        );
    }

    let plan = distribution::plan(seed);
    let counts = plan.row_counts();
    let checksum = plan.checksum();

    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "seed": seed,
            "dry_run": dry_run,
            "database": actual_db,
            "tenants": counts.tenants,
            "row_counts": counts,
            "total_rows": counts.total(),
            "checksum": checksum,
        }))?
    );

    if dry_run {
        return Ok(());
    }

    let tx_manager = TxManager::new(pool, ids);

    if database_already_seeded(&tx_manager).await? {
        bail!(
            "this database already holds scale data (a tenant with a `scale-` slug exists); \
             seed-scale targets a fresh staging database — drop and recreate it, or pick a \
             database that has not been seeded"
        );
    }

    let mut seeded = SeededIds::new();
    let tenant_ids = write_core_entities(&tx_manager, &plan, &mut seeded).await?;
    write_audit_chain(&tx_manager, &plan, &tenant_ids, &mut seeded).await?;
    write_advertising(&tx_manager, &plan, &tenant_ids, &mut seeded).await?;
    write_payments(&tx_manager, &plan, &tenant_ids, &mut seeded).await?;

    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "seed": seed,
            "written": true,
            "total_rows": counts.total(),
            "checksum": checksum,
        }))?
    );
    Ok(())
}

/// Scale data is identified by the `scale-` slug prefix: one lookup, and a
/// re-run against the same database refuses instead of half-colliding.
async fn database_already_seeded(tx: &TxManager) -> anyhow::Result<bool> {
    let mut platform = tx
        .begin_unscoped("seed-scale: pre-flight check", "cli:seed-scale")
        .await?;
    let hit: Option<i64> = sqlx::query_scalar(
        "SELECT 1::bigint FROM tenancy.tenants WHERE slug LIKE 'scale-%' LIMIT 1",
    )
    .fetch_optional(&mut **platform.tx_mut().as_inner())
    .await?;
    platform.commit().await?;
    Ok(hit.is_some())
}

// -- core entities -----------------------------------------------------------

async fn write_core_entities(
    tx: &TxManager,
    plan: &DistributionPlan,
    seeded: &mut SeededIds,
) -> anyhow::Result<Vec<i64>> {
    let mut tenant_ids = Vec::with_capacity(plan.tenants.len());
    let mut platform = tx
        .begin_unscoped("seed-scale: tenants, theming, domains", "cli:seed-scale")
        .await?;
    let db = platform.tx_mut().as_inner();

    for tenant in &plan.tenants {
        let tenant_id = seeded.next_id();
        tenant_ids.push(tenant_id);
        sqlx::query(
            "INSERT INTO tenancy.tenants (id, slug, display_name, status, region, default_locale, created_at, updated_at)
             VALUES ($1, $2, $3, 'active', $4, $5, $6, $6)",
        )
        .bind(tenant_id)
        .bind(&tenant.slug)
        .bind(&tenant.display_name)
        .bind(&tenant.region)
        .bind(&tenant.default_locale)
        .bind(time_at(0))
        .execute(&mut **db)
        .await?;

        sqlx::query(
            "INSERT INTO tenancy.tenant_settings (tenant_id, key, value, updated_at)
             VALUES ($1, 'storefront.tagline', $2::jsonb, $3),
                    ($1, 'support.email', $4::jsonb, $3)",
        )
        .bind(tenant_id)
        .bind(serde_json::json!({ "ja": "ようこそ", "en": "Welcome" }).to_string())
        .bind(time_at(1))
        .bind(serde_json::json!(format!("support@{}.example", tenant.slug)).to_string())
        .execute(&mut **db)
        .await?;

        sqlx::query(
            "INSERT INTO localization.tenant_locales (tenant_id, default_locale, enabled_locales, timezone, updated_at)
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(tenant_id)
        .bind(&tenant.default_locale)
        .bind(&tenant.enabled_locales)
        .bind(if tenant.region == "jp" { "Asia/Tokyo" } else { "UTC" })
        .bind(time_at(1))
        .execute(&mut **db)
        .await?;

        // The migrated-in `base` theme (id 10, version 1.0.0) at several
        // published revisions.
        sqlx::query(
            "INSERT INTO theming.tenant_themes (tenant_id, state, theme_id, semver, revision, updated_at)
             VALUES ($1, 'live', 10, '1.0.0', $2, $3)",
        )
        .bind(tenant_id)
        .bind(tenant.theme_revisions as i32)
        .bind(time_at(2 + i64::from(tenant.theme_revisions)))
        .execute(&mut **db)
        .await?;
        for revision in 1..=tenant.theme_revisions {
            sqlx::query(
                "INSERT INTO theming.tenant_theme_revisions (tenant_id, revision, snapshot, published_at)
                 VALUES ($1, $2, $3::jsonb, $4)",
            )
            .bind(tenant_id)
            .bind(revision as i32)
            .bind(
                serde_json::json!({
                    "theme_id": 10,
                    "semver": "1.0.0",
                    "token_overrides": {},
                    "layout_overrides": {},
                    "assets": {},
                    "custom_css": null,
                })
                .to_string(),
            )
            .bind(time_at(2 + i64::from(revision)))
            .execute(&mut **db)
            .await?;
        }

        for (d, domain) in tenant.domains.iter().enumerate() {
            let domain_id = seeded.next_id();
            let hostname = format!("{}-d{}.domains.example", tenant.slug, d);
            sqlx::query(
                "INSERT INTO domains.custom_domains
                   (id, tenant_id, hostname, normalized_hostname, kind, role, status,
                    challenge_token, verified_at, created_at, updated_at)
                 VALUES ($1, $2, $3, $3, $4, $5, $6, $7, $8, $9, $9)",
            )
            .bind(domain_id)
            .bind(tenant_id)
            .bind(&hostname)
            .bind(domain.kind)
            .bind(domain.role)
            .bind(domain.status)
            .bind(
                if domain.status == "pending_setup" || domain.status == "verifying" {
                    Some(format!("seed-challenge-{tenant_id}-{d}"))
                } else {
                    None
                },
            )
            .bind(if domain.projected {
                Some(time_at(10))
            } else {
                None
            })
            .bind(time_at(9))
            .execute(&mut **db)
            .await?;
            if domain.projected {
                sqlx::query(
                    "INSERT INTO domains.live_hostnames (normalized_hostname, tenant_id, status, updated_at)
                     VALUES ($1, $2, $3, $4)",
                )
                .bind(&hostname)
                .bind(tenant_id)
                .bind(domain.status)
                .bind(time_at(10))
                .execute(&mut **db)
                .await?;
            }
        }
    }

    platform.commit().await?;
    Ok(tenant_ids)
}

// -- audit chain -------------------------------------------------------------

/// The seeded audit history: hash-chained entries sharing the global chain
/// the audit writer maintains, verified in memory before a single row is
/// written, then bulk-inserted in chain order.
async fn write_audit_chain(
    tx: &TxManager,
    plan: &DistributionPlan,
    tenant_ids: &[i64],
    seeded: &mut SeededIds,
) -> anyhow::Result<()> {
    let mut platform = tx
        .begin_unscoped("seed-scale: audit history", "cli:seed-scale")
        .await?;
    let db = platform.tx_mut().as_inner();

    // Same lock and tail read as the audit writer: chained appends can
    // never fork, even against a concurrent writer.
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(0x5341_4E56_4941_5544i64) // "SANVIAUD"
        .execute(&mut **db)
        .await?;
    let tail: Option<(i64, Vec<u8>)> =
        sqlx::query_as("SELECT seq, hash FROM audit.entries ORDER BY seq DESC LIMIT 1")
            .fetch_optional(&mut **db)
            .await?;
    let mut running: [u8; 32] = match tail {
        Some((_, bytes)) => bytes
            .try_into()
            .map_err(|_| anyhow::anyhow!("audit tail hash is not 32 bytes"))?,
        None => sanvi_audit::domain::GENESIS_HASH,
    };

    let mut entries: Vec<sanvi_audit::domain::AuditEntry> = Vec::new();
    let mut ids: Vec<i64> = Vec::new();

    for (plan_tenant, tenant_id) in plan.tenants.iter().zip(tenant_ids) {
        for _ in 0..plan_tenant.audit_entries {
            let id = seeded.next_id();
            let step = (ids.len() % 500_000) as i64;
            let entry = sanvi_audit::domain::AuditEntry::new(
                SnowflakeId::try_from(id)?,
                sanvi_audit::domain::AuditEntryData {
                    occurred_at: time_at(100 + step),
                    actor_type: sanvi_audit::domain::ActorType::System,
                    actor_id: None,
                    on_behalf_of: None,
                    tenant_id: Some(sanvi_kernel::TenantId::from_snowflake(
                        SnowflakeId::try_from(*tenant_id)?,
                    )),
                    action: "seed.scale".to_string(),
                    resource_type: Some("scale".to_string()),
                    resource_id: Some(tenant_id.to_string()),
                    before: None,
                    after: None,
                    request_id: None,
                    ip: None,
                    user_agent: None,
                },
                running,
            )?;
            running = entry.hash;
            entries.push(entry);
            ids.push(id);
        }
    }

    sanvi_audit::domain::verify_chain(&entries)
        .map_err(|err| anyhow::anyhow!("generated audit chain does not verify: {err}"))?;

    for (start, chunk) in Chunker::new(&entries, 400) {
        insert_audit_chunk(db, chunk, &ids[start..start + chunk.len()]).await?;
    }

    platform.commit().await?;
    Ok(())
}

/// `(offset, &slice)` pairs over `items` in `chunk_size` pieces.
struct Chunker<'a, T> {
    items: &'a [T],
    chunk_size: usize,
    offset: usize,
}

impl<'a, T> Chunker<'a, T> {
    fn new(items: &'a [T], chunk_size: usize) -> Self {
        Self {
            items,
            chunk_size,
            offset: 0,
        }
    }
}

impl<'a, T> Iterator for Chunker<'a, T> {
    type Item = (usize, &'a [T]);

    fn next(&mut self) -> Option<Self::Item> {
        if self.offset >= self.items.len() {
            return None;
        }
        let end = (self.offset + self.chunk_size).min(self.items.len());
        let item = (self.offset, &self.items[self.offset..end]);
        self.offset = end;
        Some(item)
    }
}

async fn insert_audit_chunk(
    db: &mut sqlx::PgConnection,
    chunk: &[sanvi_audit::domain::AuditEntry],
    ids: &[i64],
) -> anyhow::Result<()> {
    let occurred: Vec<DateTime<Utc>> = chunk.iter().map(|e| e.occurred_at).collect();
    let actor_types: Vec<&str> = chunk.iter().map(|e| e.actor_type.as_str()).collect();
    let actor_ids: Vec<Option<i64>> = chunk
        .iter()
        .map(|e| e.actor_id.map(|v| v.as_i64()))
        .collect();
    let on_behalf: Vec<Option<i64>> = chunk
        .iter()
        .map(|e| e.on_behalf_of.map(|v| v.as_i64()))
        .collect();
    let tenant_ids: Vec<Option<i64>> = chunk
        .iter()
        .map(|e| e.tenant_id.map(|v| v.as_i64()))
        .collect();
    let actions: Vec<&str> = chunk.iter().map(|e| e.action.as_str()).collect();
    let resource_types: Vec<Option<&str>> =
        chunk.iter().map(|e| e.resource_type.as_deref()).collect();
    let resource_ids: Vec<Option<&str>> = chunk.iter().map(|e| e.resource_id.as_deref()).collect();
    let befores: Vec<Option<serde_json::Value>> = chunk.iter().map(|e| e.before.clone()).collect();
    let afters: Vec<Option<serde_json::Value>> = chunk.iter().map(|e| e.after.clone()).collect();
    let request_ids: Vec<Option<&str>> = chunk.iter().map(|e| e.request_id.as_deref()).collect();
    let ips: Vec<Option<std::net::IpAddr>> = chunk.iter().map(|e| e.ip).collect();
    let user_agents: Vec<Option<&str>> = chunk.iter().map(|e| e.user_agent.as_deref()).collect();
    let prev_hashes: Vec<Vec<u8>> = chunk.iter().map(|e| e.prev_hash.to_vec()).collect();
    let hashes: Vec<Vec<u8>> = chunk.iter().map(|e| e.hash.to_vec()).collect();

    sqlx::query(
        "INSERT INTO audit.entries
           (id, occurred_at, actor_type, actor_id, on_behalf_of, tenant_id,
            action, resource_type, resource_id, before, after, request_id,
            ip, user_agent, prev_hash, hash)
         SELECT * FROM UNNEST(
           $1::bigint[], $2::timestamptz[], $3::text[], $4::bigint[], $5::bigint[], $6::bigint[],
           $7::text[], $8::text[], $9::text[], $10::jsonb[], $11::jsonb[], $12::text[],
           $13::inet[], $14::text[], $15::bytea[], $16::bytea[])",
    )
    .bind(ids)
    .bind(&occurred)
    .bind(&actor_types)
    .bind(&actor_ids)
    .bind(&on_behalf)
    .bind(&tenant_ids)
    .bind(&actions)
    .bind(&resource_types)
    .bind(&resource_ids)
    .bind(&befores)
    .bind(&afters)
    .bind(&request_ids)
    .bind(&ips)
    .bind(&user_agents)
    .bind(&prev_hashes)
    .bind(&hashes)
    .execute(&mut *db)
    .await?;
    Ok(())
}

// -- advertising -------------------------------------------------------------

/// Connections, campaigns and the heavy `metrics_daily` rollup. The
/// monthly partitions must exist before any metrics row is written, so the
/// same partition-maintenance function the worker uses runs first.
async fn write_advertising(
    tx: &TxManager,
    plan: &DistributionPlan,
    tenant_ids: &[i64],
    seeded: &mut SeededIds,
) -> anyhow::Result<()> {
    let mut platform = tx
        .begin_unscoped(
            "seed-scale: advertising connections and metrics",
            "cli:seed-scale",
        )
        .await?;
    let db = platform.tx_mut().as_inner();

    let platforms = ["google_ads", "meta"];

    // (tenant_id, connection_id, campaign_id, days) per metrics row batch.
    let mut metric_keys: Vec<(i64, String, i64, i64, i64)> = Vec::new();

    for (plan_tenant, tenant_id) in plan.tenants.iter().zip(tenant_ids) {
        for c in 0..plan_tenant.ad_connections {
            let connection_id = seeded.next_id();
            let platform_name = platforms[((usize::try_from(*tenant_id).unwrap_or(0))
                + usize::try_from(c).unwrap_or(0))
                % 2];
            sqlx::query(
                "INSERT INTO advertising.connections
                   (id, tenant_id, platform, external_account_id, account_name, currency,
                    timezone, status, token_ref, connected_at, created_at, updated_at)
                 VALUES ($1, $2, $3, $4, $5, $6, 'UTC', 'active', 'seed-scale', $7, $7, $7)",
            )
            .bind(connection_id)
            .bind(tenant_id)
            .bind(platform_name)
            .bind(format!("act_seed_{connection_id}"))
            .bind(format!("{} — connection {c}", plan_tenant.slug))
            .bind(currency_for(&plan_tenant.region))
            .bind(time_at(20))
            .execute(&mut **db)
            .await?;

            for k in 0..plan_tenant.ad_campaigns_per_connection {
                let campaign_id = seeded.next_id();
                sqlx::query(
                    "INSERT INTO advertising.campaigns
                       (id, tenant_id, connection_id, platform, external_id, name, objective,
                        status, budget, created_at, updated_at)
                     VALUES ($1, $2, $3, $4, $5, $6, 'OUTCOME_SALES', 'ENABLED', $7::jsonb, $8, $8)",
                )
                .bind(campaign_id)
                .bind(tenant_id)
                .bind(connection_id)
                .bind(platform_name)
                .bind(format!("seed-{campaign_id}"))
                .bind(format!("Seed Campaign {k}"))
                .bind(
                    serde_json::json!({
                        "amount_minor": 100_000,
                        "currency": currency_for(&plan_tenant.region),
                    })
                    .to_string(),
                )
                .bind(time_at(21))
                .execute(&mut **db)
                .await?;

                for day in 0..plan_tenant.metrics_days {
                    metric_keys.push((
                        *tenant_id,
                        platform_name.to_string(),
                        connection_id,
                        campaign_id,
                        i64::from(day),
                    ));
                }
            }
        }
    }

    // Partitions first: one call per *calendar* month the metrics span —
    // stepping 30 days can skip a month (Jan 31 + 30 days is Mar 2).
    let days = metric_keys
        .iter()
        .map(|(_, _, _, _, d)| *d)
        .max()
        .unwrap_or(0);
    let mut month_start = base_date().with_day(1).expect("day 1 exists");
    let last_month_start = (base_date() + chrono::Duration::days(days))
        .with_day(1)
        .expect("day 1 exists");
    while month_start <= last_month_start {
        sqlx::query("SELECT advertising.ensure_monthly_partitions($1::date)")
            .bind(month_start)
            .execute(&mut **db)
            .await?;
        month_start = month_start
            .checked_add_months(chrono::Months::new(1))
            .expect("month steps stay in range");
    }

    // The rollup rows themselves, chunked.
    const CHUNK: usize = 400;
    let currency_by_tenant: std::collections::HashMap<i64, &str> = plan
        .tenants
        .iter()
        .zip(tenant_ids)
        .map(|(t, id)| (*id, currency_for(&t.region)))
        .collect();

    for start in (0..metric_keys.len()).step_by(CHUNK) {
        let end = (start + CHUNK).min(metric_keys.len());
        let slice = &metric_keys[start..end];
        let tenant_col: Vec<i64> = slice.iter().map(|r| r.0).collect();
        let platform_col: Vec<&str> = slice.iter().map(|r| r.1.as_str()).collect();
        let connection_col: Vec<i64> = slice.iter().map(|r| r.2).collect();
        let campaign_col: Vec<i64> = slice.iter().map(|r| r.3).collect();
        let date_col: Vec<chrono::NaiveDate> = slice
            .iter()
            .map(|r| base_date() + chrono::Duration::days(r.4))
            .collect();
        let impressions: Vec<i64> = (start..end)
            .map(|n| 500 + ((n as i64 * 104_729) % 9_500))
            .collect();
        let clicks: Vec<i64> = (start..end)
            .map(|n| 20 + ((n as i64 * 1_577) % 480))
            .collect();
        let spend: Vec<i64> = (start..end)
            .map(|n| 1_000 + ((n as i64 * 3_511) % 24_000))
            .collect();
        let currencies: Vec<&str> = tenant_col.iter().map(|t| currency_by_tenant[t]).collect();
        let conversions: Vec<String> = (start..end).map(|n| format!("{}", n % 40)).collect();
        let conversion_value: Vec<i64> = (start..end).map(|n| (n as i64 * 977) % 30_000).collect();

        sqlx::query(
            "INSERT INTO advertising.metrics_daily
               (tenant_id, platform, connection_id, campaign_id, date, impressions, clicks,
                spend_minor, currency, conversions, conversion_value_minor)
             SELECT * FROM UNNEST(
               $1::bigint[], $2::text[], $3::bigint[], $4::bigint[], $5::date[], $6::bigint[],
               $7::bigint[], $8::bigint[], $9::text[], $10::numeric[], $11::bigint[]) ",
        )
        .bind(&tenant_col)
        .bind(&platform_col)
        .bind(&connection_col)
        .bind(&campaign_col)
        .bind(&date_col)
        .bind(&impressions)
        .bind(&clicks)
        .bind(&spend)
        .bind(&currencies)
        .bind(&conversions)
        .bind(&conversion_value)
        .execute(&mut **db)
        .await?;
    }

    platform.commit().await?;
    Ok(())
}

// -- payments ----------------------------------------------------------------

/// One checkout or payment row (the two tables share most columns).
struct PaymentRow {
    id: i64,
    tenant_id: i64,
    parent_id: i64,
    external: String,
    amount: i64,
    currency: &'static str,
    completed: bool,
    step: i64,
}

/// Checkouts, payments and refunds. The payments hot tenant exists for
/// TASK-025's single-tenant restore and TASK-028's bloat measurement, so
/// the refund ratio is part of the plan, not an afterthought.
async fn write_payments(
    tx: &TxManager,
    plan: &DistributionPlan,
    tenant_ids: &[i64],
    seeded: &mut SeededIds,
) -> anyhow::Result<()> {
    let mut platform = tx
        .begin_unscoped("seed-scale: payments history", "cli:seed-scale")
        .await?;
    let db = platform.tx_mut().as_inner();

    let mut checkout_rows: Vec<PaymentRow> = Vec::new();
    let mut payment_rows: Vec<PaymentRow> = Vec::new();
    let mut refund_rows: Vec<RefundRow> = Vec::new();

    for (plan_tenant, tenant_id) in plan.tenants.iter().zip(tenant_ids) {
        if plan_tenant.payments == 0 {
            continue;
        }
        let currency = currency_for(&plan_tenant.region);
        let connection_id = seeded.next_id();
        sqlx::query(
            "INSERT INTO payments.provider_connections
               (id, tenant_id, provider, external_account_id, status, capabilities,
                requirements, country, default_currency, connected_at, last_synced_at,
                created_at, updated_at)
             VALUES ($1, $2, 'stripe_connect', $3, 'active', '{}'::jsonb, '{}'::jsonb, $4, $5, $6, $6, $6, $6)",
        )
        .bind(connection_id)
        .bind(tenant_id)
        .bind(format!("acct_seed_{tenant_id}"))
        .bind(plan_tenant.region.to_uppercase())
        .bind(currency)
        .bind(time_at(30))
        .execute(&mut **db)
        .await?;

        let n_checkouts = plan_tenant.checkouts() as usize;
        let mut tenant_checkout_ids = Vec::with_capacity(n_checkouts);
        for i in 0..n_checkouts {
            let checkout_id = seeded.next_id();
            tenant_checkout_ids.push(checkout_id);
            checkout_rows.push(PaymentRow {
                id: checkout_id,
                tenant_id: *tenant_id,
                parent_id: connection_id,
                external: format!("cs_scale_{checkout_id}"),
                amount: amount_minor(i),
                currency,
                completed: i < plan_tenant.payments as usize,
                step: 40 + (i % 100_000) as i64,
            });
        }

        let mut succeeded_ids = Vec::with_capacity(plan_tenant.payments as usize);
        for (i, &checkout_id) in tenant_checkout_ids.iter().enumerate() {
            let payment_id = seeded.next_id();
            let succeeded = (i * 13) % 100 < 85;
            if succeeded {
                succeeded_ids.push(payment_id);
            }
            payment_rows.push(PaymentRow {
                id: payment_id,
                tenant_id: *tenant_id,
                parent_id: checkout_id,
                external: format!("pi_scale_{payment_id}"),
                amount: amount_minor(i),
                currency,
                completed: succeeded,
                step: 41 + (i % 100_000) as i64,
            });
        }

        // Refund the first `refunds()` succeeded payments, deterministically.
        for refund in succeeded_ids.iter().take(plan_tenant.refunds() as usize) {
            let refund_id = seeded.next_id();
            let amount = payment_rows
                .iter()
                .find(|p| p.id == *refund)
                .map(|p| p.amount / 2)
                .unwrap_or(100);
            refund_rows.push(RefundRow {
                id: refund_id,
                tenant_id: *tenant_id,
                payment_id: *refund,
                external: format!("re_scale_{refund_id}"),
                amount,
                currency,
            });
        }
    }

    for (_, chunk) in Chunker::new(&checkout_rows, 400) {
        insert_checkout_chunk(db, chunk).await?;
    }
    for (_, chunk) in Chunker::new(&payment_rows, 400) {
        insert_payment_chunk(db, chunk).await?;
    }
    for (_, chunk) in Chunker::new(&refund_rows, 400) {
        insert_refund_chunk(db, chunk).await?;
    }

    platform.commit().await?;
    Ok(())
}

struct RefundRow {
    id: i64,
    tenant_id: i64,
    payment_id: i64,
    external: String,
    amount: i64,
    currency: &'static str,
}

async fn insert_checkout_chunk(
    db: &mut sqlx::PgConnection,
    chunk: &[PaymentRow],
) -> anyhow::Result<()> {
    let ids: Vec<i64> = chunk.iter().map(|r| r.id).collect();
    let tenants: Vec<i64> = chunk.iter().map(|r| r.tenant_id).collect();
    let parents: Vec<i64> = chunk.iter().map(|r| r.parent_id).collect();
    let references: Vec<String> = chunk.iter().map(|r| r.external.clone()).collect();
    let amounts: Vec<i64> = chunk.iter().map(|r| r.amount).collect();
    let currencies: Vec<&str> = chunk.iter().map(|r| r.currency).collect();
    let statuses: Vec<&str> = chunk
        .iter()
        .map(|r| if r.completed { "complete" } else { "expired" })
        .collect();
    let created: Vec<DateTime<Utc>> = chunk.iter().map(|r| time_at(r.step)).collect();
    let completed: Vec<Option<DateTime<Utc>>> = chunk
        .iter()
        .map(|r| {
            if r.completed {
                Some(time_at(r.step + 1))
            } else {
                None
            }
        })
        .collect();
    let updated: Vec<DateTime<Utc>> = chunk
        .iter()
        .map(|r| time_at(if r.completed { r.step + 1 } else { r.step }))
        .collect();

    sqlx::query(
        "INSERT INTO payments.checkouts
           (id, tenant_id, connection_id, reference, amount_minor, currency, status,
            external_session_id, completed_at, created_at, updated_at)
         SELECT * FROM UNNEST(
           $1::bigint[], $2::bigint[], $3::bigint[], $4::text[], $5::bigint[], $6::text[],
           $7::text[], $8::text[], $9::timestamptz[], $10::timestamptz[], $11::timestamptz[])",
    )
    .bind(&ids)
    .bind(&tenants)
    .bind(&parents)
    .bind(&references)
    .bind(&amounts)
    .bind(&currencies)
    .bind(&statuses)
    .bind(&references)
    .bind(&completed)
    .bind(&created)
    .bind(&updated)
    .execute(&mut *db)
    .await?;
    Ok(())
}

async fn insert_payment_chunk(
    db: &mut sqlx::PgConnection,
    chunk: &[PaymentRow],
) -> anyhow::Result<()> {
    let ids: Vec<i64> = chunk.iter().map(|r| r.id).collect();
    let tenants: Vec<i64> = chunk.iter().map(|r| r.tenant_id).collect();
    let checkouts: Vec<i64> = chunk.iter().map(|r| r.parent_id).collect();
    let externals: Vec<String> = chunk.iter().map(|r| r.external.clone()).collect();
    let amounts: Vec<i64> = chunk.iter().map(|r| r.amount).collect();
    let currencies: Vec<&str> = chunk.iter().map(|r| r.currency).collect();
    let statuses: Vec<&str> = chunk
        .iter()
        .map(|r| if r.completed { "succeeded" } else { "failed" })
        .collect();
    let created: Vec<DateTime<Utc>> = chunk.iter().map(|r| time_at(r.step)).collect();
    let captured: Vec<Option<DateTime<Utc>>> = chunk
        .iter()
        .map(|r| {
            if r.completed {
                Some(time_at(r.step + 1))
            } else {
                None
            }
        })
        .collect();
    let updated: Vec<DateTime<Utc>> = chunk
        .iter()
        .map(|r| time_at(if r.completed { r.step + 1 } else { r.step }))
        .collect();

    sqlx::query(
        "INSERT INTO payments.payments
           (id, tenant_id, checkout_id, external_payment_id, status, amount_minor, currency,
            captured_at, created_at, updated_at)
         SELECT * FROM UNNEST(
           $1::bigint[], $2::bigint[], $3::bigint[], $4::text[], $5::text[], $6::bigint[],
           $7::text[], $8::timestamptz[], $9::timestamptz[], $10::timestamptz[])",
    )
    .bind(&ids)
    .bind(&tenants)
    .bind(&checkouts)
    .bind(&externals)
    .bind(&statuses)
    .bind(&amounts)
    .bind(&currencies)
    .bind(&captured)
    .bind(&created)
    .bind(&updated)
    .execute(&mut *db)
    .await?;
    Ok(())
}

async fn insert_refund_chunk(
    db: &mut sqlx::PgConnection,
    chunk: &[RefundRow],
) -> anyhow::Result<()> {
    let ids: Vec<i64> = chunk.iter().map(|r| r.id).collect();
    let tenants: Vec<i64> = chunk.iter().map(|r| r.tenant_id).collect();
    let payments: Vec<i64> = chunk.iter().map(|r| r.payment_id).collect();
    let externals: Vec<String> = chunk.iter().map(|r| r.external.clone()).collect();
    let amounts: Vec<i64> = chunk.iter().map(|r| r.amount).collect();
    let currencies: Vec<&str> = chunk.iter().map(|r| r.currency).collect();
    let created: Vec<DateTime<Utc>> = chunk.iter().map(|_| time_at(50)).collect();
    let updated: Vec<DateTime<Utc>> = chunk.iter().map(|_| time_at(51)).collect();

    sqlx::query(
        "INSERT INTO payments.refunds
           (id, tenant_id, payment_id, external_refund_id, amount_minor, currency,
            reason, status, created_at, updated_at)
         SELECT * FROM UNNEST(
           $1::bigint[], $2::bigint[], $3::bigint[], $4::text[], $5::bigint[], $6::text[],
           $7::text[], $8::text[], $9::timestamptz[], $10::timestamptz[])",
    )
    .bind(&ids)
    .bind(&tenants)
    .bind(&payments)
    .bind(&externals)
    .bind(&amounts)
    .bind(&currencies)
    .bind(vec!["requested_by_customer".to_string(); chunk.len()])
    .bind(vec!["succeeded"; chunk.len()])
    .bind(&created)
    .bind(&updated)
    .execute(&mut *db)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::is_staging_database_name;

    #[test]
    fn staging_pattern_accepts_the_staging_and_harness_names() {
        for name in [
            "sanvi_stage",
            "sanvi_staging_eu1",
            "sanvi_scale",
            "sanvi_seed_2",
            "sanvi_dev",
            "sanvi_template_20260816010000",
            "sanvi_t_1234_7",
            "sanvi_b_42",
        ] {
            assert!(is_staging_database_name(name), "{name} must be allowed");
        }
    }

    #[test]
    fn staging_pattern_refuses_production_shaped_names() {
        for name in [
            "sanvi",
            "sanvi_production",
            "sanvi_prod",
            "SANVI_scale",
            "postgres",
            "sanvi-scale", // the pattern matches name prefixes, punctuation differs
        ] {
            assert!(!is_staging_database_name(name), "{name} must be refused");
        }
    }
}
