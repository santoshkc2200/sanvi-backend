//! FR-1108 (TASK-021, slice 11.2d): EXPLAIN (ANALYZE, BUFFERS) plan gate for
//! the profiled endpoints' read models.
//!
//! 11.2b made `WHERE tenant_id = $1 … ORDER BY … LIMIT $n` the read shape of
//! every collection endpoint; 11.2d measured what those pages cost before and
//! after the keyset indexes landed (`benchmarks/explain-2026-10-06.md`): a
//! page that walks the primary key and filters by tenant reads every newer
//! row on the platform — waste that grows with platform size, not tenant
//! size. This gate pins the resulting access paths so the flip back fails CI
//! instead of surfacing as a page.
//!
//! Each rule names one read-model query, the table it must not scan, and the
//! index the plan must use. Pinning the *index name* — not merely the absence
//! of a Seq Scan — is the point: measured on the 11.2d scratch seed, dropping
//! `invoices_tenant_idx` does not produce a Seq Scan, it produces
//! `Index Scan Backward using invoices_pkey` with the tenant filter riding on
//! top — exactly the silent regression that made pages O(platform). The
//! failure message names both sides of the flip.
//!
//! **The fixture.** The rules run in the steady-state posture production
//! reaches once autovacuum has vacuumed a table: the visibility map filled.
//! The map matters — measured on the 11.2d scratch seed, a freshly-loaded
//! table has an empty one, and until Postgres marks pages all-visible (it
//! refuses for a short aging window after the load commit, ~a minute here)
//! the planner legitimately prefers the primary-key walk over a
//! heap-fetching composite scan. A per-run clone therefore cannot pin these
//! plans: it would assert on the transient posture and fail spuriously. So
//! the fixture is a **shared database** built once per (migration set, seed)
//! pair under an advisory lock — cloned from the migrated template, seeded,
//! and vacuumed until a probe proves the map is filled — then read-only for
//! every run after. Its name carries the migrated template's fingerprint and
//! a hash of the seed, so either moving rebuilds it; the `scripts/test-db.sh`
//! reaper (`sanvi_t_*`, `sanvi_b_*`) never touches it.
//!
//! Every id lives in the 911_* range, owned by this suite.

use serde_json::Value;
use sha2::{Digest, Sha256};
use sqlx::postgres::PgConnectOptions;
use sqlx::{Connection, PgConnection, PgPool, Row};

// ---------------------------------------------------------------------------
// Fixed identities. The 911_* range is owned by this suite; nothing else may
// mint ids in it.
// ---------------------------------------------------------------------------

/// Every tenant-scoped rule plans for the *lowest*-id tenant, the one whose
/// pages the pkey walk serves last.
const TENANT: i64 = 911_000_001;

/// Roles of the shared test server (`crates/platform/testsupport`).
const APP_ROLE: &str = "sanvi_app";
const APP_PASSWORD: &str = "app";

/// Advisory lock owned by this suite (testsupport's bootstrap lock is
/// `0x5AE7_1DB0`; do not reuse). Serializes fixture builds across the
/// processes that share one server.
const FIXTURE_LOCK: i64 = 0x5AE7_9EF7;

/// The audited-SQL convention (`rls_matrix::audited`): the statements this
/// suite interpolates only compile-time constants and names read back from
/// the Postgres catalog — never client input.
fn audited(sql: &str) -> sqlx::AssertSqlSafe<&str> {
    sqlx::AssertSqlSafe(sql)
}

/// One plan rule: the read-model query (as the adapter issues it), the table
/// whose access path is pinned, and the index the plan must use.
struct PlanRule {
    name: &'static str,
    sql: &'static str,
    binds: Vec<Bind>,
    schema: &'static str,
    table: &'static str,
    index: &'static str,
}

enum Bind {
    I64(i64),
    OptI64(Option<i64>),
    OptText(Option<&'static str>),
    Ts(chrono::DateTime<chrono::Utc>),
    OptTs(Option<chrono::DateTime<chrono::Utc>>),
}

/// Seconds after the Unix epoch — a fixed instant, not wall-clock arithmetic.
fn ts(secs: i64) -> chrono::DateTime<chrono::Utc> {
    use chrono::TimeZone as _;
    chrono::Utc.timestamp_opt(secs, 0).unwrap()
}

fn rules() -> Vec<PlanRule> {
    use Bind::*;
    vec![
        // -- keyset pages whose index 11.2d created or realigned -------------
        PlanRule {
            name: "billing invoices keyset (first page)",
            sql: "SELECT id, tenant_id, stripe_invoice_id, number, status, total_minor, currency,
                         hosted_url, pdf_url, period_start, period_end, created_at, paid_at
                  FROM billing.invoices
                  WHERE tenant_id = $1 AND ($2::bigint IS NULL OR id < $2)
                  ORDER BY id DESC LIMIT $3",
            binds: vec![I64(TENANT), OptI64(None), I64(200)],
            schema: "billing",
            table: "invoices",
            index: "invoices_tenant_idx",
        },
        PlanRule {
            name: "billing invoices keyset (page 2)",
            sql: "SELECT id FROM billing.invoices
                  WHERE tenant_id = $1 AND ($2::bigint IS NULL OR id < $2)
                  ORDER BY id DESC LIMIT $3",
            binds: vec![I64(TENANT), OptI64(Some(911_031_001_000)), I64(200)],
            schema: "billing",
            table: "invoices",
            index: "invoices_tenant_idx",
        },
        PlanRule {
            name: "privacy DSR tenant keyset",
            sql: "SELECT id FROM privacy.data_subject_requests
                  WHERE tenant_id = $1 AND ($2::text IS NULL OR status = $2)
                    AND ($4::bigint IS NULL OR id < $4)
                  ORDER BY id DESC LIMIT $3",
            binds: vec![
                I64(TENANT),
                OptText(None),
                I64(100),
                OptI64(Some(911_071_000_500)),
            ],
            schema: "privacy",
            table: "data_subject_requests",
            index: "dsr_tenant_idx",
        },
        PlanRule {
            name: "identity invitations keyset",
            sql: "SELECT id FROM identity.invitations
                  WHERE tenant_id = $1 AND ($2::bigint IS NULL OR id < $2)
                  ORDER BY id DESC LIMIT $3",
            binds: vec![I64(TENANT), OptI64(Some(911_081_000_250)), I64(50)],
            schema: "identity",
            table: "invitations",
            index: "invitations_tenant_idx",
        },
        PlanRule {
            name: "audit tenant seq keyset",
            sql: "SELECT seq FROM audit.entries
                  WHERE ($1::bigint IS NULL OR tenant_id = $1)
                    AND ($2::bigint IS NULL OR actor_id = $2)
                    AND ($3::text IS NULL OR action = $3)
                    AND ($4::timestamptz IS NULL OR occurred_at >= $4)
                    AND ($5::timestamptz IS NULL OR occurred_at < $5)
                    AND ($6::bigint IS NULL OR seq < $6)
                  ORDER BY seq DESC LIMIT $7",
            binds: vec![
                OptI64(Some(TENANT)),
                OptI64(None),
                OptText(None),
                OptTs(None),
                OptTs(None),
                OptI64(Some(3_000)),
                I64(50),
            ],
            schema: "audit",
            table: "entries",
            index: "audit_entries_tenant_seq_idx",
        },
        PlanRule {
            name: "domains custom_domains keyset",
            sql: "SELECT id FROM domains.custom_domains
                  WHERE tenant_id = $1 AND ($2::bigint IS NULL OR id < $2)
                  ORDER BY id DESC LIMIT $3",
            binds: vec![I64(TENANT), OptI64(Some(911_041_000_450)), I64(200)],
            schema: "domains",
            table: "custom_domains",
            index: "custom_domains_tenant_idx",
        },
        PlanRule {
            name: "domains orders keyset",
            sql: "SELECT id FROM domains.orders
                  WHERE tenant_id = $1 AND ($2::bigint IS NULL OR id < $2)
                  ORDER BY id DESC LIMIT $3",
            binds: vec![I64(TENANT), OptI64(Some(911_051_000_300)), I64(50)],
            schema: "domains",
            table: "orders",
            index: "orders_tenant_idx",
        },
        PlanRule {
            name: "payments disputes keyset",
            sql: "SELECT id FROM payments.disputes
                  WHERE tenant_id = $1 AND (created_at, id) < ($2::timestamptz, $3)
                  ORDER BY created_at DESC, id DESC LIMIT 101",
            binds: vec![I64(TENANT), Ts(ts(300)), I64(911_023_000_300)],
            schema: "payments",
            table: "disputes",
            index: "disputes_tenant_idx",
        },
        // -- keyset pages whose index predates 11.2d: guard the good shape ---
        PlanRule {
            name: "payments page keyset",
            sql: "SELECT p.id FROM payments.payments p
                  WHERE p.tenant_id = $1 AND (p.created_at, p.id) < ($2::timestamptz, $3)
                  ORDER BY p.created_at DESC, p.id DESC LIMIT 201",
            binds: vec![I64(TENANT), Ts(ts(1_000)), I64(911_021_001_000)],
            schema: "payments",
            table: "payments",
            index: "payments_tenant_created_id_idx",
        },
        PlanRule {
            name: "advertising campaigns keyset",
            sql: "SELECT id FROM advertising.campaigns
                  WHERE tenant_id = $1 AND ($2::bigint IS NULL OR id < $2)
                  ORDER BY id DESC LIMIT $3",
            binds: vec![I64(TENANT), OptI64(Some(911_101_000_250)), I64(50)],
            schema: "advertising",
            table: "campaigns",
            index: "campaigns_tenant_id_id_key",
        },
        PlanRule {
            name: "identity members keyset",
            sql: "SELECT m.tenant_id, m.user_id, u.primary_email
                  FROM identity.memberships m
                  JOIN identity.users u ON u.id = m.user_id
                  WHERE m.tenant_id = $1 AND ($2::bigint IS NULL OR m.user_id < $2)
                  ORDER BY m.user_id DESC LIMIT $3",
            binds: vec![I64(TENANT), OptI64(Some(911_001_100_300)), I64(50)],
            schema: "identity",
            table: "memberships",
            index: "memberships_pkey",
        },
        // -- the resolution and slug lookups in front of every request -------
        PlanRule {
            name: "live-hostname resolution",
            sql: "SELECT normalized_hostname, tenant_id, status
                  FROM domains.live_hostnames
                  WHERE normalized_hostname = $1",
            binds: vec![OptText(Some("perf-live-1-750.example.test"))],
            schema: "domains",
            table: "live_hostnames",
            index: "live_hostnames_pkey",
        },
        PlanRule {
            name: "public slug availability",
            sql: "SELECT EXISTS(SELECT 1 FROM tenancy.tenants WHERE slug = $1)",
            binds: vec![OptText(Some("perf-tenant-2"))],
            schema: "tenancy",
            table: "tenants",
            index: "tenants_slug_unique",
        },
        PlanRule {
            name: "platform tenant browse keyset",
            sql: "SELECT id, slug, created_at FROM tenancy.tenants
                  WHERE ($1::text IS NULL
                         OR slug ILIKE '%' || $1 || '%'
                         OR display_name ILIKE '%' || $1 || '%')
                    AND ($2::text IS NULL OR status = $2)
                    AND ($3::timestamptz IS NULL
                         OR created_at < $3
                         OR (created_at = $3 AND id < $4))
                  ORDER BY created_at DESC, id DESC LIMIT $5",
            binds: vec![
                OptText(None),
                OptText(None),
                OptTs(None),
                OptI64(None),
                I64(50),
            ],
            schema: "tenancy",
            table: "tenants",
            index: "tenants_created_idx",
        },
    ]
}

// ---------------------------------------------------------------------------
// Seed: the same shape the 11.2d measurement ran (three page-sized tenants on
// the read models, thousands of background tenants for the platform
// surfaces, ANALYZE), so every planner choice below is decisive. The ids and
// cursors in `rules()` are sized against these volumes.
// ---------------------------------------------------------------------------

const SEED_SQL: &str = r#"
INSERT INTO tenancy.tenants (id, slug, display_name, status, region, default_locale)
SELECT 911_000_000 + g, 'perf-tenant-' || g, 'Perf Tenant ' || g, 'active', 'us', 'en'
FROM generate_series(1, 3) g;
INSERT INTO tenancy.tenants (id, slug, display_name, status, region, default_locale)
SELECT 911_200_000_000 + g, 'perf-scale-' || g, 'Scale Tenant ' || g, 'active', 'us', 'en'
FROM generate_series(4, 3000) g;

INSERT INTO identity.users (id, kratos_identity_id, primary_email, email_verified, status)
SELECT 911_001_000_000 + (t * 100000) + g, gen_random_uuid(),
       'perf-user-' || t || '-' || g || '@example.test', TRUE, 'active'
FROM generate_series(1, 600) g, generate_series(1, 3) t;
INSERT INTO identity.memberships (tenant_id, user_id, status)
SELECT 911_000_000 + t, 911_001_000_000 + (t * 100000) + g, 'active'
FROM generate_series(1, 600) g, generate_series(1, 3) t;
INSERT INTO identity.invitations (id, tenant_id, email, role_ids, token_hash, expires_at)
SELECT 911_080_000_000 + (t * 1000000) + g, 911_000_000 + t,
       'perf-invite-' || t || '-' || g || '@example.test', ARRAY[1::bigint],
       'tokhash-' || t || '-' || g, now() + make_interval(days => 7)
FROM generate_series(1, 500) g, generate_series(1, 3) t;

INSERT INTO payments.provider_connections (id, tenant_id, provider, external_account_id, status)
SELECT 911_005_000_000 + t, 911_000_000 + t, 'stripe_connect', 'acct_perf_' || t, 'active'
FROM generate_series(1, 3) t;
INSERT INTO payments.checkouts (id, tenant_id, connection_id, reference, amount_minor, currency, status, created_at)
SELECT 911_010_000_000 + (t * 1000000) + g, 911_000_000 + t, 911_005_000_000 + t,
       'perf-co-' || t || '-' || g, 1000 + g, 'USD', 'complete',
       '2026-01-01'::timestamptz + make_interval(secs => g)
FROM generate_series(1, 2000) g, generate_series(1, 3) t;
INSERT INTO payments.payments (id, tenant_id, checkout_id, external_payment_id, status, amount_minor, currency, created_at)
SELECT 911_020_000_000 + (t * 1000000) + g, 911_000_000 + t,
       911_010_000_000 + (t * 1000000) + g, 'pay_perf_' || t || '_' || g,
       'succeeded', 1000 + g, 'USD', '2026-01-01'::timestamptz + make_interval(secs => g)
FROM generate_series(1, 2000) g, generate_series(1, 3) t;
INSERT INTO payments.disputes (id, tenant_id, payment_id, external_dispute_id, status, amount_minor, currency, created_at)
SELECT 911_022_000_000 + (t * 1000000) + g, 911_000_000 + t,
       911_020_000_000 + (t * 1000000) + g, 'dp_perf_' || t || '_' || g,
       'warning_needs_response', 1000 + g, 'USD',
       '2026-01-01'::timestamptz + make_interval(secs => g)
FROM generate_series(1, 600) g, generate_series(1, 3) t;

INSERT INTO billing.invoices (id, tenant_id, stripe_invoice_id, status, total_minor, currency, created_at)
SELECT 911_030_000_000 + (t * 1000000) + g, 911_000_000 + t,
       'in_perf_' || t || '_' || g, 'paid', 900 + g, 'usd',
       '2026-01-01'::timestamptz + make_interval(secs => g)
FROM generate_series(1, 2000) g, generate_series(1, 3) t;

INSERT INTO domains.custom_domains (id, tenant_id, hostname, normalized_hostname, kind, role, status)
SELECT 911_040_000_000 + (t * 1000000) + g, 911_000_000 + t,
       'perf-domain-' || t || '-' || g || '.example.test',
       'perf-domain-' || t || '-' || g || '.example.test',
       'connected', 'alias', 'pending_setup'
FROM generate_series(1, 900) g, generate_series(1, 3) t;
INSERT INTO domains.live_hostnames (normalized_hostname, tenant_id, status)
SELECT 'perf-live-' || t || '-' || g || '.example.test', 911_000_000 + t, 'live'
FROM generate_series(1, 1500) g, generate_series(1, 2) t;
INSERT INTO domains.orders (id, tenant_id, hostname, normalized_hostname, term_years, price_minor, currency, status, registrant_contact)
SELECT 911_050_000_000 + (t * 1000000) + g, 911_000_000 + t,
       'perf-order-' || t || '-' || g || '.example.test',
       'perf-order-' || t || '-' || g || '.example.test',
       1, 1200, 'usd', 'active', '{}'::jsonb
FROM generate_series(1, 600) g, generate_series(1, 3) t;

INSERT INTO privacy.data_subjects (id, tenant_id, kind, canonical_key, jurisdiction)
SELECT 911_060_000_000 + (t * 1000000) + g, 911_000_000 + t, 'end_user',
       'perf-subject-' || t || '-' || g || '@example.test', 'eu'
FROM generate_series(1, 300) g, generate_series(1, 3) t;
INSERT INTO privacy.data_subject_requests (id, tenant_id, subject_id, subject_ref, kind, status, jurisdiction, submitted_by, due_at)
SELECT 911_070_000_000 + (t * 1000000) + g, 911_000_000 + t,
       911_060_000_000 + (t * 1000000) + (1 + (g % 300)),
       jsonb_build_object('email', 'perf-subject-' || t || '-' || g || '@example.test'),
       (ARRAY['access','export','erasure','rectification'])[1 + (g % 4)],
       (ARRAY['in_progress','completed','awaiting_verification'])[1 + (g % 3)],
       'eu', 'tenant_operator', now() + make_interval(days => 30)
FROM generate_series(1, 1000) g, generate_series(1, 3) t;

INSERT INTO audit.entries (id, actor_type, tenant_id, action, prev_hash, hash)
SELECT 911_090_000_000 + (t * 1000000) + g, 'system', 911_000_000 + t,
       'perf.seed', '\x00'::bytea, '\x00'::bytea
FROM generate_series(1, 2000) g, generate_series(1, 3) t;

INSERT INTO advertising.connections (id, tenant_id, platform, external_account_id, currency, timezone, status, token_ref)
SELECT 911_110_000_000 + (t * 10) + p, 911_000_000 + t,
       (ARRAY['google_ads','meta'])[p], 'ext-' || t || '-' || p,
       'USD', 'UTC', 'active', 'vault-ref'
FROM generate_series(1, 2) p, generate_series(1, 3) t;
INSERT INTO advertising.campaigns (id, tenant_id, connection_id, platform, name, objective, status, budget)
SELECT 911_100_000_000 + (t * 1000000) + g, 911_000_000 + t,
       911_110_000_000 + (t * 10) + 1 + (g % 2),
       (ARRAY['google_ads','meta'])[1 + (g % 2)],
       'Perf Campaign ' || t || '-' || g, 'conversions', 'enabled', '{}'::jsonb
FROM generate_series(1, 500) g, generate_series(1, 3) t;

ANALYZE tenancy.tenants, identity.users, identity.memberships, identity.invitations,
        payments.provider_connections, payments.checkouts, payments.payments,
        payments.disputes, billing.invoices, domains.custom_domains,
        domains.live_hostnames, domains.orders, privacy.data_subjects,
        privacy.data_subject_requests, audit.entries,
        advertising.connections, advertising.campaigns;
"#;

// ---------------------------------------------------------------------------
// The fixture database.
// ---------------------------------------------------------------------------

/// Port of the shared test server. `test_db()` converges on it whether it was
/// started by `scripts/test-db.sh` (the fast path) or by testcontainers; the
/// clone it makes is dropped immediately — only the port is needed.
async fn server_port() -> u16 {
    sanvi_testsupport::test_db().await.port()
}

async fn admin_conn(port: u16, database: &str) -> PgConnection {
    PgConnection::connect_with(
        &PgConnectOptions::new()
            .host("127.0.0.1")
            .port(port)
            .database(database)
            .username("postgres")
            .password("postgres")
            .application_name("sanvi-perf-fixture"),
    )
    .await
    .expect("fixture admin connection")
}

/// Name of the migrated template `testsupport` maintains — exactly one is
/// alive (bootstrap reaps the rest), and its name carries the migration
/// fingerprint.
async fn base_template(admin: &mut PgConnection) -> String {
    sqlx::query_scalar(
        "SELECT datname FROM pg_database
         WHERE datistemplate AND datname LIKE 'sanvi\\_template\\_%' LIMIT 1",
    )
    .fetch_one(admin)
    .await
    .expect("the migrated template exists")
}

/// The fixture database's pool, building the fixture first if needed.
///
/// The fixture is seeded, vacuumed, and then used **read-only**: every rule
/// runs `EXPLAIN (ANALYZE)` over SELECTs. Concurrent gate runs therefore
/// share one fixture safely.
async fn fixture_pool(port: u16) -> PgPool {
    let mut admin = admin_conn(port, "postgres").await;
    let base = base_template(&mut admin).await;
    // The name binds the fixture to one migration set and one seed: either
    // moving makes this name miss and the fixture rebuild.
    let seed_hash = hex::encode(&Sha256::digest(SEED_SQL.as_bytes())[..4]);
    let base_fp = base.trim_start_matches("sanvi_template_").to_string();
    let fixture = format!("sanvi_perf_{base_fp}_{seed_hash}");

    if let Some(app) = filled_fixture(&mut admin, port, &fixture).await {
        let _ = admin.close().await;
        return app;
    }

    sqlx::query("SELECT pg_advisory_lock($1)")
        .bind(FIXTURE_LOCK)
        .execute(&mut admin)
        .await
        .expect("fixture lock");
    // Re-check under the lock: another process may have finished building
    // while this one probed an unfilled map.
    let app = match filled_fixture(&mut admin, port, &fixture).await {
        Some(app) => {
            let _ = sqlx::query("SELECT pg_advisory_unlock($1)")
                .bind(FIXTURE_LOCK)
                .execute(&mut admin)
                .await;
            let _ = admin.close().await;
            return app;
        }
        None => rebuild_fixture(&mut admin, port, &base, &fixture).await,
    };
    sqlx::query("SELECT pg_advisory_unlock($1)")
        .bind(FIXTURE_LOCK)
        .execute(&mut admin)
        .await
        .expect("fixture unlock");
    let _ = admin.close().await;

    assert!(
        vm_filled(port, &fixture).await,
        "the perf fixture was rebuilt but its visibility map never filled — \
         the gate cannot pin steady-state plans on this host"
    );
    app
}

/// The fixture's pool when the database exists **and** its visibility map is
/// filled; `None` otherwise. Safe to call without the lock.
async fn filled_fixture(admin: &mut PgConnection, port: u16, fixture: &str) -> Option<PgPool> {
    let exists: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_database WHERE datname = $1)")
            .bind(fixture)
            .fetch_one(&mut *admin)
            .await
            .expect("fixture lookup");
    if !exists || !vm_filled(port, fixture).await {
        return None;
    }
    Some(build_app_pool(port, fixture).await)
}

async fn build_app_pool(port: u16, fixture: &str) -> PgPool {
    build_fixture_pool(port, fixture, APP_ROLE, APP_PASSWORD).await
}

async fn build_fixture_pool(port: u16, fixture: &str, user: &str, password: &str) -> PgPool {
    let config = sanvi_persistence::DatabaseConfig {
        host: "127.0.0.1".to_string(),
        port,
        name: fixture.to_string(),
        user: user.to_string(),
        password: password.to_string(),
        max_connections: 2,
        min_connections: 0,
        migrations_dir: sanvi_testsupport::migrations_dir(),
        statement_timeout_ms: 0,
        ..sanvi_persistence::DatabaseConfig::default()
    };
    sanvi_persistence::build_pool(&config)
        .await
        .expect("fixture pool")
}

/// Clone and seed the fixture, then hand back its pool once the visibility
/// map has filled.
///
/// The map-fill is polled rather than forced: Postgres declines to mark
/// freshly-committed pages all-visible under a manual VACUUM on this seed
/// (measured on the 11.2d scratch databases — repeated manual vacuums over
/// three minutes marked nothing, and autovacuum's first pass filled the map
/// moments later), so the loop gives autovacuum its head and waits for the
/// probe to say the steady-state posture has been reached.
async fn rebuild_fixture(admin: &mut PgConnection, port: u16, base: &str, fixture: &str) -> PgPool {
    let drop = format!("DROP DATABASE IF EXISTS {fixture} WITH (FORCE)");
    sqlx::raw_sql(audited(&drop))
        .execute(&mut *admin)
        .await
        .expect("drop stale fixture");
    let create = format!("CREATE DATABASE {fixture} TEMPLATE {base} STRATEGY = wal_log");
    sqlx::raw_sql(audited(&create))
        .execute(&mut *admin)
        .await
        .expect("create fixture from the migrated template");
    let grant = format!("GRANT CONNECT ON DATABASE {fixture} TO {APP_ROLE}");
    sqlx::raw_sql(audited(&grant))
        .execute(&mut *admin)
        .await
        .expect("grant fixture connect");

    // Seed as superuser: rows for every tenant, RLS bypassed the way the
    // migrator (not the application) writes.
    let mut seed_conn = admin_conn(port, fixture).await;
    sqlx::raw_sql(audited(SEED_SQL))
        .execute(&mut seed_conn)
        .await
        .expect("seed the fixture");

    // Stats first (the seed's own ANALYZE covers this too); the map itself
    // is autovacuum's to fill.
    sqlx::raw_sql(audited("ANALYZE"))
        .execute(&mut seed_conn)
        .await
        .expect("analyze the fixture");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(360);
    loop {
        if vm_filled(port, fixture).await {
            let _ = seed_conn.close().await;
            return build_app_pool(port, fixture).await;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the fixture visibility map never filled within the budget —              is autovacuum enabled on the test server?"
        );
        tokio::time::sleep(std::time::Duration::from_secs(10)).await;
    }
}

/// Steady-state probe: the invoices keyset page must be served index-only
/// with zero heap fetches — true exactly when the visibility map covers the
/// table (a count would not discriminate: the planner prefers a seq scan for
/// a full count even when index-only is available). Run as the superuser:
/// under the application role the RLS predicate reads `tenant_id` from the
/// heap on every plan this probe could otherwise take.
async fn vm_filled(port: u16, fixture: &str) -> bool {
    let app = build_fixture_pool(port, fixture, "postgres", "postgres").await;
    let plan = explain_json(
        &app,
        "SELECT id FROM billing.invoices
         WHERE tenant_id = 911000001 AND (911031001000::bigint IS NULL OR id < 911031001000)
         ORDER BY id DESC LIMIT 200",
        &[],
        false,
    )
    .await;
    app.close().await;
    scan_nodes(&plan).iter().any(|node| {
        node.index.as_deref() == Some("invoices_tenant_idx") && node.heap_fetches == Some(0)
    })
}

// ---------------------------------------------------------------------------
// The gate.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn profiled_read_models_stay_on_their_indexes() {
    let port = server_port().await;
    let app = fixture_pool(port).await;

    let mut failures = Vec::new();
    let mut names = std::collections::BTreeSet::new();

    for rule in rules() {
        assert!(
            names.insert(rule.name),
            "duplicate plan rule name: {}",
            rule.name
        );
        let plan = explain_json(&app, rule.sql, &rule.binds, true).await;
        failures.extend(plan_violations(&plan, &rule));
    }
    assert!(
        !names.is_empty(),
        "the gate enumerated zero plan rules — the rule set is broken, \
         not the plans vacuously clean"
    );

    assert!(
        failures.is_empty(),
        "plan regressions on the profiled read models:\n  - {}",
        failures.join("\n  - ")
    );
}

/// One rule's plan violations, named so the fix is obvious from the message.
/// Exactly one violation per failed rule: the flip is the message.
fn plan_violations(plan: &Value, rule: &PlanRule) -> Vec<String> {
    let nodes = scan_nodes(plan);
    let mut violations = Vec::new();

    let seq_on_target = nodes
        .iter()
        .find(|n| n.is_seq_scan_on(rule.schema, rule.table));
    if let Some(node) = seq_on_target {
        violations.push(format!(
            "{}: plan flips to a Seq Scan on {} — the pinned index `{}` is gone \
             or no longer chosen; every page reads the whole table (found: {})",
            rule.name,
            qualified(rule.schema, rule.table),
            rule.index,
            node.describe()
        ));
        return violations;
    }

    if !nodes.iter().any(|n| n.index.as_deref() == Some(rule.index)) {
        let found = nodes
            .iter()
            .map(|n| n.describe())
            .collect::<Vec<_>>()
            .join("; ");
        violations.push(format!(
            "{}: expected the plan to use index `{}` on {}; found: {}. \
             The query or an index realignment moved the access path",
            rule.name,
            rule.index,
            qualified(rule.schema, rule.table),
            if found.is_empty() {
                "no scan nodes".to_string()
            } else {
                found
            }
        ));
    }

    violations
}

/// A scan-shaped plan node: relation or index access.
struct ScanNode {
    node_type: String,
    relation: Option<String>,
    schema: Option<String>,
    index: Option<String>,
    heap_fetches: Option<u64>,
}

impl ScanNode {
    fn is_seq_scan_on(&self, schema: &str, table: &str) -> bool {
        self.node_type == "Seq Scan"
            && self.relation.as_deref() == Some(table)
            && self.schema.as_deref() == Some(schema)
    }

    fn describe(&self) -> String {
        let relation = match (&self.schema, &self.relation) {
            (Some(s), Some(t)) => format!(" on {s}.{t}"),
            _ => String::new(),
        };
        match &self.index {
            Some(index) => format!("{} using `{index}`{relation}", self.node_type),
            None => format!("{}{relation}", self.node_type),
        }
    }
}

/// Every scan node in the plan, depth-first through the `Plans` tree.
fn scan_nodes(plan: &Value) -> Vec<ScanNode> {
    let mut nodes = Vec::new();
    // EXPLAIN (FORMAT JSON) returns a single-element array whose "Plan" is
    // the root; a defensive walk that also survives a bare plan object keeps
    // the fixture tests honest about what they feed in.
    let roots: Vec<&Value> = match plan {
        Value::Array(items) => items.iter().filter_map(|i| i.get("Plan")).collect(),
        Value::Object(_) => vec![plan.get("Plan").unwrap_or(plan)],
        _ => vec![],
    };
    for root in roots {
        walk_plan(root, &mut nodes);
    }
    nodes
}

fn walk_plan(node: &Value, nodes: &mut Vec<ScanNode>) {
    let node_type = node
        .get("Node Type")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if node_type.contains("Scan") {
        nodes.push(ScanNode {
            node_type,
            relation: node
                .get("Relation Name")
                .and_then(Value::as_str)
                .map(str::to_string),
            schema: node
                .get("Schema")
                .and_then(Value::as_str)
                .map(str::to_string),
            index: node
                .get("Index Name")
                .and_then(Value::as_str)
                .map(str::to_string),
            heap_fetches: node.get("Heap Fetches").and_then(Value::as_u64),
        });
    }
    if let Some(children) = node.get("Plans").and_then(Value::as_array) {
        for child in children {
            walk_plan(child, nodes);
        }
    }
}

fn qualified(schema: &str, table: &str) -> String {
    format!("{schema}.{table}")
}

/// Run one statement under `EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON)` on a
/// pooled application connection with the tenant GUC set — the exact shape
/// `TxManager` serves every scoped read through. Session-local `set_config`
/// is re-applied per call because a pool may hand back any connection.
///
/// Postgres 18 types the FORMAT JSON column as json; older servers send
/// text, which is parsed as a fallback.
async fn explain_json(pool: &PgPool, sql: &str, binds: &[Bind], scoped: bool) -> Value {
    let mut conn = pool
        .acquire()
        .await
        .expect("application connection for EXPLAIN");
    if scoped {
        sqlx::query("SELECT set_config('sanvi.tenant_id', $1, false)")
            .bind(TENANT.to_string())
            .execute(&mut *conn)
            .await
            .expect("set the tenant scope");
    }
    let statement = format!("EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON) {sql}");
    // Assembled from compile-time rule constants only.
    let mut query = sqlx::query(audited(&statement));
    for bind in binds {
        query = match bind {
            Bind::I64(v) => query.bind(*v),
            Bind::OptI64(v) => query.bind(*v),
            Bind::OptText(v) => query.bind(*v),
            Bind::Ts(v) => query.bind(*v),
            Bind::OptTs(v) => query.bind(*v),
        };
    }
    let row = query
        .fetch_one(&mut *conn)
        .await
        .expect("EXPLAIN (ANALYZE) runs and returns one row");
    match row.try_get::<Value, _>(0) {
        Ok(plan) => plan,
        Err(_) => {
            let text: String = row
                .try_get(0)
                .expect("the plan arrives as the first column");
            serde_json::from_str(&text).expect("FORMAT JSON yields parseable plan JSON")
        }
    }
}

// ---------------------------------------------------------------------------
// Fixtures: the gate's self-test. Every rule of the checker is proven on a
// synthetic plan so a broken walker or a softened assertion fails here, not
// in review — including the flip that actually happens when a pinned index
// is dropped (Index Scan on the primary key, *not* a Seq Scan).
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn rule() -> PlanRule {
        PlanRule {
            name: "fixture",
            sql: "SELECT 1",
            binds: Vec::new(),
            schema: "billing",
            table: "invoices",
            index: "invoices_tenant_idx",
        }
    }

    fn plan_with(nodes: Value) -> Value {
        json!([{ "Plan": { "Node Type": "Limit", "Plans": [nodes] } }])
    }

    fn index_node(node_type: &str, index: &str) -> Value {
        json!({
            "Node Type": node_type,
            "Relation Name": "invoices",
            "Schema": "billing",
            "Index Name": index
        })
    }

    #[test]
    fn the_pinned_index_passes_clean() {
        let plan = plan_with(index_node("Index Only Scan", "invoices_tenant_idx"));
        assert!(plan_violations(&plan, &rule()).is_empty());
    }

    #[test]
    fn a_dropped_index_fails_naming_the_flip() {
        // Measured on the 11.2d scratch seed: without `invoices_tenant_idx`
        // the planner walks the primary key and filters by tenant — the
        // regression this gate exists to catch, invisible to a "no Seq Scan"
        // check.
        let plan = plan_with(index_node("Index Scan Backward", "invoices_pkey"));
        let violations = plan_violations(&plan, &rule());
        assert_eq!(violations.len(), 1);
        let message = &violations[0];
        assert!(message.contains("fixture"), "{message}");
        assert!(message.contains("invoices_tenant_idx"), "{message}");
        assert!(
            message.contains("Index Scan Backward using `invoices_pkey`"),
            "failure must name the plan it flipped to: {message}"
        );
    }

    #[test]
    fn a_seq_scan_on_the_pinned_table_fails_naming_the_flip() {
        let plan = plan_with(json!({
            "Node Type": "Seq Scan", "Relation Name": "invoices", "Schema": "billing"
        }));
        let violations = plan_violations(&plan, &rule());
        assert_eq!(violations.len(), 1);
        assert!(
            violations[0].contains("Seq Scan on billing.invoices"),
            "{}",
            violations[0]
        );
    }

    #[test]
    fn a_seq_scan_on_another_table_is_not_this_rules_business() {
        // The members rule joins users; at fixture scale the planner may
        // legitimately seq-scan the joined side. Only the pinned table is
        // forbidden.
        let plan = plan_with(json!({
            "Node Type": "Limit", "Plans": [
                json!({"Node Type": "Seq Scan", "Relation Name": "users", "Schema": "identity"}),
                index_node("Index Only Scan", "invoices_tenant_idx")
            ]
        }));
        assert!(plan_violations(&plan, &rule()).is_empty());
    }

    #[test]
    fn a_plan_without_scan_nodes_is_a_violation_not_a_pass() {
        let plan = json!([{ "Plan": { "Node Type": "Result" } }]);
        let violations = plan_violations(&plan, &rule());
        assert_eq!(violations.len(), 1);
        assert!(violations[0].contains("no scan nodes"), "{}", violations[0]);
    }

    #[test]
    fn the_rule_set_is_well_formed() {
        let rules = rules();
        assert!(rules.len() >= 10, "the plan gate lost rules — restore them");
        let mut names = std::collections::BTreeSet::new();
        for r in &rules {
            assert!(names.insert(r.name), "duplicate rule: {}", r.name);
            assert!(r.sql.contains("FROM"), "{}: not a read", r.name);
            assert!(!r.index.is_empty() && !r.table.is_empty());
        }
    }
}
