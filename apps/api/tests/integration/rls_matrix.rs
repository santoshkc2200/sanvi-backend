//! Schema-wide RLS matrix (TASK-024 slice 11.5c), against a real Postgres
//! with the full migration set.
//!
//! The phase-01 matrix (`crates/contexts/identity/tests/integration/
//! identity_rls.rs`) proves tenant isolation for the identity tables and
//! stays as the per-context test it is. This suite generalizes it to every
//! context's tables by **enumerating the live catalog** instead of listing
//! tables by hand: any table that gains a `tenant_id` column — in phase 10
//! or later — is covered the moment its migration lands.
//!
//! Two properties are asserted per table, and the failure message always
//! names the `schema.table` that violated:
//!
//! 1. **Structural** — `ENABLE ROW LEVEL SECURITY` + `FORCE ROW LEVEL
//!    SECURITY` + exactly one canonical `tenant_isolation` policy whose
//!    `USING` and `WITH CHECK` are both the platform-escape-hatch-or-tenant
//!    predicate. Exact-match, not substring: a policy quietly recreated
//!    with a weaker predicate, a second permissive policy OR-ed in, or a
//!    policy scoped away from `{public}` all fail. `FORCE` matters because
//!    the migrator owns the schema; the app role being `NOBYPASSRLS` is
//!    asserted too, since a flipped bit there would make every policy
//!    decoration.
//! 2. **Partitions by name** — every child of a partitioned parent is
//!    asserted *individually*, because RLS does not automatically apply to
//!    a partition created after the policy: `ensure_monthly_partitions`
//!    creates children daily, and a child that slips out without its
//!    policy is the isolation gap most likely to be real. (PostgreSQL
//!    applies the *parent's* policy to queries through the parent, so the
//!    gap opens on direct-child access — which is exactly what the
//!    behavioural half below exercises.)
//!
//! The behavioural half re-runs the phase-01 proof (scoped reads cannot
//! see the other tenant even with an explicit `tenant_id` parameter; the
//! audited platform scope sees everything; writes for the other tenant are
//! rejected by `WITH CHECK`) against the two partitioned tables through
//! **both** the parent and the directly-named child.
//!
//! Two reasoned exemptions from the canonical shape, both frozen:
//!
//! * `domains.live_hostnames` carries a `tenant_id` but deliberately has
//!   no RLS: it holds only live hostnames — public DNS information — and
//!   host→tenant resolution must not depend on a scoped transaction
//!   (`migrations/20260822000000_domains_core.sql`). The exemption
//!   freezes its column set so a new column cannot silently widen what
//!   unscoped connections can read.
//! * `localization.translations` is RLS-forced without a `tenant_id`
//!   column: it is the shared string catalog, isolated on `scope` by nine
//!   named policies (`migrations/20260820000000_localization_i18n.sql`).
//!   The matrix pins that policy set by name and requires every predicate
//!   to stay bound to the `sanvi.` GUCs.
//!
//! Any table that fits neither the canonical shape nor an entry above
//! fails this suite until it is either migrated to the canonical shape or
//! given a reasoned exemption here. That is the gate.

use std::collections::BTreeMap;

use sanvi_persistence::TxManager;
use sqlx::Row;

// ---------------------------------------------------------------------------
// Fixed identities. The 910_235_xxx range is owned by this suite; nothing
// else may mint ids in it.
// ---------------------------------------------------------------------------

const TENANT_A: i64 = 910_235_001;
const TENANT_B: i64 = 910_235_002;

const CONN_A: i64 = 910_235_010;
const CONN_B: i64 = 910_235_011;
const CAMPAIGN_A: i64 = 910_235_020;
const CAMPAIGN_B: i64 = 910_235_021;

/// Conversion events seeded for tenant A (2) and tenant B (3): asymmetric
/// on purpose, so a query that mixed tenants would return a count matching
/// neither.
const EVENTS_A: &[i64] = &[910_235_101, 910_235_102];
const EVENTS_B: &[i64] = &[910_235_111, 910_235_112, 910_235_113];

/// The deparse PostgreSQL stores for the predicate `enable_tenant_rls` and
/// `ensure_monthly_partitions` create. Whitespace is normalized before
/// comparison; anything else changing means the policy text changed and
/// this constant must be re-reviewed, not silently widened.
const CANONICAL_PREDICATE: &str = "((current_setting('sanvi.platform_scope'::text, true) = 'on'::text) OR (tenant_id = (NULLIF(current_setting('sanvi.tenant_id'::text, true), ''::text))::bigint))";

/// SQLSTATE 42501 covers both `insufficient_privilege` and the row-level
/// security violation; the message distinguishes them.
const INSUFFICIENT_PRIVILEGE: &str = "42501";

fn normalize(expr: &str) -> String {
    expr.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The statements this suite builds at runtime interpolate only table
/// names read back from the Postgres catalog (or derived from a
/// `to_char(..., 'YYYYMM')` month suffix) and compile-time integer
/// constants — never anything a client sent. That is the audit
/// `AssertSqlSafe` demands before sqlx 0.9 accepts a dynamic string.
fn audited(sql: &str) -> sqlx::AssertSqlSafe<&str> {
    sqlx::AssertSqlSafe(sql)
}

fn is_rls_violation(error: &sqlx::Error) -> bool {
    match error {
        sqlx::Error::Database(db) => {
            db.code().as_deref() == Some(INSUFFICIENT_PRIVILEGE)
                && db.message().contains("row-level security")
        }
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Catalog model
// ---------------------------------------------------------------------------

struct CatalogTable {
    schema: String,
    name: String,
    /// `r` plain, `p` partitioned parent.
    kind: String,
    rls: bool,
    force: bool,
}

impl CatalogTable {
    fn qualified(&self) -> String {
        format!("{}.{}", self.schema, self.name)
    }
}

#[derive(Clone)]
struct CatalogPolicy {
    schema: String,
    table: String,
    name: String,
    permissive: bool,
    roles: Vec<String>,
    cmd: String,
    qual: Option<String>,
    with_check: Option<String>,
}

type CatalogRow = sqlx::postgres::PgRow;

fn catalog_table(row: CatalogRow) -> CatalogTable {
    // relkind is the one-character `char` type in pg_catalog, not text.
    let kind = (row.get::<i8, _>(2) as u8 as char).to_string();
    CatalogTable {
        schema: row.get(0),
        name: row.get(1),
        kind,
        rls: row.get(3),
        force: row.get(4),
    }
}

/// Every non-system schema table that carries a `tenant_id` column.
async fn tenant_tables(pool: &sqlx::PgPool) -> Vec<CatalogTable> {
    sqlx::query(
        r#"SELECT n.nspname, c.relname, c.relkind, c.relrowsecurity, c.relforcerowsecurity
           FROM pg_class c
           JOIN pg_namespace n ON n.oid = c.relnamespace
           WHERE c.relkind IN ('r', 'p')
             AND n.nspname NOT LIKE 'pg\_%'
             AND n.nspname NOT IN ('information_schema', 'public')
             AND EXISTS (
               SELECT 1 FROM pg_attribute a
               WHERE a.attrelid = c.oid AND a.attname = 'tenant_id'
                 AND a.attnum > 0 AND NOT a.attisdropped
             )
           ORDER BY 1, 2"#,
    )
    .fetch_all(pool)
    .await
    .expect("catalog query: tenant tables")
    .into_iter()
    .map(catalog_table)
    .collect()
}

/// Tables with RLS enabled but no `tenant_id` column — a shape that is
/// legitimate only where the matrix records one (`localization.translations`).
async fn rls_tables_without_tenant_id(pool: &sqlx::PgPool) -> Vec<CatalogTable> {
    sqlx::query(
        r#"SELECT n.nspname, c.relname, c.relkind, c.relrowsecurity, c.relforcerowsecurity
           FROM pg_class c
           JOIN pg_namespace n ON n.oid = c.relnamespace
           WHERE c.relkind IN ('r', 'p')
             AND n.nspname NOT LIKE 'pg\_%'
             AND n.nspname NOT IN ('information_schema', 'public')
             AND c.relrowsecurity
             AND NOT EXISTS (
               SELECT 1 FROM pg_attribute a
               WHERE a.attrelid = c.oid AND a.attname = 'tenant_id'
                 AND a.attnum > 0 AND NOT a.attisdropped
             )
           ORDER BY 1, 2"#,
    )
    .fetch_all(pool)
    .await
    .expect("catalog query: RLS tables without tenant_id")
    .into_iter()
    .map(catalog_table)
    .collect()
}

async fn policies(pool: &sqlx::PgPool) -> Vec<CatalogPolicy> {
    sqlx::query(
        r#"SELECT schemaname, tablename, policyname, permissive, roles, cmd, qual, with_check
           FROM pg_policies
           WHERE schemaname NOT LIKE 'pg\_%'
             AND schemaname NOT IN ('information_schema', 'public')
           ORDER BY 1, 2, 3"#,
    )
    .fetch_all(pool)
    .await
    .expect("catalog query: policies")
    .into_iter()
    .map(|row| CatalogPolicy {
        permissive: row.get::<String, _>(3).eq_ignore_ascii_case("permissive"),
        schema: row.get(0),
        table: row.get(1),
        name: row.get(2),
        roles: row.get(4),
        cmd: row.get(5),
        qual: row.get(6),
        with_check: row.get(7),
    })
    .collect()
}

/// Every `(parent schema, parent table, child table)` edge of every
/// partition tree — children by name, the unit this matrix asserts.
async fn partition_children(pool: &sqlx::PgPool) -> Vec<(String, String, String)> {
    sqlx::query(
        r#"SELECT pn.nspname, pc.relname, cc.relname
           FROM pg_inherits i
           JOIN pg_class pc ON pc.oid = i.inhparent
           JOIN pg_namespace pn ON pn.oid = pc.relnamespace
           JOIN pg_class cc ON cc.oid = i.inhrelid
           JOIN pg_namespace cn ON cn.oid = cc.relnamespace
           WHERE pn.nspname NOT LIKE 'pg\_%'
             AND pn.nspname NOT IN ('information_schema', 'public')
           ORDER BY 1, 2, 3"#,
    )
    .fetch_all(pool)
    .await
    .expect("catalog query: partition children")
    .into_iter()
    .map(|row| (row.get(0), row.get(1), row.get(2)))
    .collect()
}

/// The column names of one table, for freezing an exemption's shape.
async fn column_names(pool: &sqlx::PgPool, schema: &str, table: &str) -> Vec<String> {
    sqlx::query(
        r#"SELECT a.attname
           FROM pg_attribute a
           JOIN pg_class c ON c.oid = a.attrelid
           JOIN pg_namespace n ON n.oid = c.relnamespace
           WHERE n.nspname = $1 AND c.relname = $2
             AND a.attnum > 0 AND NOT a.attisdropped
           ORDER BY 1"#,
    )
    .bind(schema)
    .bind(table)
    .fetch_all(pool)
    .await
    .expect("catalog query: columns")
    .into_iter()
    .map(|row| row.get(0))
    .collect()
}

// ---------------------------------------------------------------------------
// Structural matrix
// ---------------------------------------------------------------------------

#[tokio::test]
async fn every_tenant_table_is_forced_and_canonically_isolated() {
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();

    // The whole matrix is decoration if the application role could simply
    // bypass it. NOBYPASSRLS is set at role creation by the test bootstrap
    // and in production by the provisioning runbook.
    let bypass: bool =
        sqlx::query_scalar("SELECT rolbypassrls FROM pg_roles WHERE rolname = 'sanvi_app'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(
        !bypass,
        "sanvi_app must stay NOBYPASSRLS or every policy below is decoration"
    );

    let tables = tenant_tables(&pool).await;
    let mut policies_by_table: BTreeMap<(String, String), Vec<CatalogPolicy>> = BTreeMap::new();
    for policy in policies(&pool).await {
        policies_by_table
            .entry((policy.schema.clone(), policy.table.clone()))
            .or_default()
            .push(policy);
    }

    assert!(
        tables.len() >= 50,
        "the tenant-table enumeration found only {} tables; the schema is known \
         to have 55+, so the catalog query broke",
        tables.len()
    );

    // Reasoned exemptions from the canonical shape, with their columns
    // frozen. `domains.live_hostnames`: public-DNS host→tenant resolution
    // hot path, deliberately readable from unscoped connections
    // (migrations/20260822000000_domains_core.sql). The column allowlist is
    // what keeps the exemption honest: a new column on a table any
    // tenant-scoped query can read must be a reviewed decision, not a
    // migration accident.
    let exempt: &[(&str, &str, &[&str])] = &[(
        "domains",
        "live_hostnames",
        &["normalized_hostname", "status", "tenant_id", "updated_at"],
    )];

    for table in &tables {
        let qualified = table.qualified();
        let exemption = exempt
            .iter()
            .find(|(schema, name, _)| *schema == table.schema && *name == table.name);

        if let Some((_, _, frozen_columns)) = exemption {
            assert!(
                !table.rls && !table.force,
                "{qualified}: exempted from RLS by matrix decision, so it must not \
                 carry ENABLE/FORCE ROW LEVEL SECURITY silently either"
            );
            let mut frozen = frozen_columns.to_vec();
            frozen.sort_unstable();
            let actual = column_names(&pool, &table.schema, &table.name).await;
            assert_eq!(
                actual, frozen,
                "{qualified}: the no-RLS exemption is frozen to these columns; a \
                 new column widens what unscoped connections can read and needs a \
                 new matrix decision"
            );
            continue;
        }

        assert!(
            table.rls,
            "{qualified}: carries tenant_id but has no ENABLE ROW LEVEL SECURITY — \
             add it via platform.enable_tenant_rls or record a reasoned exemption"
        );
        assert!(
            table.force,
            "{qualified}: RLS without FORCE leaves the table owner free of the policy"
        );

        let table_policies = policies_by_table
            .get(&(table.schema.clone(), table.name.clone()))
            .cloned()
            .unwrap_or_default();
        assert_eq!(
            table_policies.len(),
            1,
            "{qualified}: expected exactly the canonical tenant_isolation policy, \
             found {}: a missing one leaves the table empty or open, and an extra \
             one (permissive ORs in, restrictive ANDs in) changes the isolation \
             decision — either way it needs a matrix classification",
            table_policies.len()
        );
        let policy = &table_policies[0];
        assert_eq!(
            policy.name, "tenant_isolation",
            "{qualified}: policy is named {}",
            policy.name
        );
        assert!(
            policy.permissive,
            "{qualified}: tenant_isolation must stay permissive"
        );
        assert!(
            policy.roles.iter().any(|role| role == "public"),
            "{qualified}: tenant_isolation must apply to {{public}}; scoping it away \
             from the application role fails closed and hides the table"
        );
        assert_eq!(
            policy.cmd.to_uppercase(),
            "ALL",
            "{qualified}: tenant_isolation must cover ALL commands"
        );
        assert_eq!(
            policy.qual.as_deref().map(normalize).as_deref(),
            Some(CANONICAL_PREDICATE),
            "{qualified}: USING must be the canonical predicate"
        );
        assert_eq!(
            policy.with_check.as_deref().map(normalize).as_deref(),
            Some(CANONICAL_PREDICATE),
            "{qualified}: WITH CHECK must be the canonical predicate"
        );
    }

    // Tables that are RLS-forced without a tenant_id column: only the
    // shared string catalog has that shape today, and its policy set is
    // pinned by name so any addition or removal is a review, not a drift.
    let unclassified = rls_tables_without_tenant_id(&pool).await;
    assert_eq!(
        unclassified.len(),
        1,
        "tables with RLS but no tenant_id column must be classified in this \
         matrix; unclassified: {:?}",
        unclassified
            .iter()
            .map(|table| table.qualified())
            .collect::<Vec<_>>()
    );
    let translations = &unclassified[0];
    assert_eq!(
        translations.qualified(),
        "localization.translations",
        "unexpected unclassified RLS shape"
    );
    let pinned: &[&str] = &[
        "translations_platform_scope_delete",
        "translations_platform_scope_read",
        "translations_platform_scope_update",
        "translations_read_platform",
        "translations_write_platform",
        "translations_write_platform_update",
        "translations_write_tenant",
        "translations_write_tenant_delete",
        "translations_write_tenant_update",
    ];
    let translation_policies = policies_by_table
        .get(&(translations.schema.clone(), translations.name.clone()))
        .cloned()
        .unwrap_or_default();
    let mut names: Vec<&str> = translation_policies
        .iter()
        .map(|policy| policy.name.as_str())
        .collect();
    names.sort_unstable();
    let mut expected = pinned.to_vec();
    expected.sort_unstable();
    assert_eq!(
        names, expected,
        "localization.translations: the shared-catalog policy set is pinned; a \
         change here is a review, not a regeneration"
    );
    for policy in &translation_policies {
        assert!(
            policy.roles.iter().any(|role| role == "public"),
            "localization.translations/{}: must apply to {{public}}",
            policy.name
        );
        for (kind, predicate) in [
            ("USING", policy.qual.as_deref()),
            ("WITH CHECK", policy.with_check.as_deref()),
        ] {
            if let Some(expr) = predicate {
                assert!(
                    expr.contains("sanvi."),
                    "localization.translations/{}: {kind} predicate must stay bound \
                     to the sanvi.* GUCs, found: {expr}",
                    policy.name
                );
            }
        }
    }
    // The tenant-visible read path filters on the tenant GUC specifically.
    let read_platform = translation_policies
        .iter()
        .find(|policy| policy.name == "translations_read_platform")
        .unwrap();
    assert!(
        read_platform
            .qual
            .as_deref()
            .is_some_and(|qual| qual.contains("sanvi.tenant_id")),
        "localization.translations: tenant rows must stay filtered by sanvi.tenant_id"
    );

    // ---- Partitions, by name -------------------------------------------------
    //
    // Every child of every partitioned parent is asserted individually:
    // RLS does not automatically extend to a partition created after the
    // policy, so a child the maintenance job (or a migration) creates
    // without its own policy is invisible to a parent-only view of this
    // matrix. The table loop above already checked each child it could
    // enumerate; this block pins that every child *is* enumerated as a
    // tenant table and that no partitioned parent lost all of its children.
    let children = partition_children(&pool).await;
    let mut children_of: BTreeMap<(String, String), Vec<String>> = BTreeMap::new();
    for (parent_schema, parent_table, child) in &children {
        children_of
            .entry((parent_schema.clone(), parent_table.clone()))
            .or_default()
            .push(child.clone());
    }
    for table in tables.iter().filter(|table| table.kind == "p") {
        let qualified = table.qualified();
        let kids = children_of
            .get(&(table.schema.clone(), table.name.clone()))
            .cloned()
            .unwrap_or_default();
        assert!(
            !kids.is_empty(),
            "{qualified}: partitioned parent with no children — the by-name \
             assertions would pass vacuously"
        );
        for child in &kids {
            assert!(
                tables
                    .iter()
                    .any(|t| t.schema == table.schema && t.name == *child && t.kind == "r"),
                "{}.{}: child partition carries no tenant_id column or is missing \
                 from the catalog; every partition must be tenant-scoped by name",
                table.schema,
                child
            );
        }
    }

    // The two partitioned parents this schema has today. Not hardcoding the
    // children (they roll monthly); pinning the parents means removing
    // partitioning cannot silently shrink this matrix.
    for parent in ["advertising.conversion_events", "advertising.metrics_daily"] {
        assert!(
            tables
                .iter()
                .any(|table| table.qualified() == parent && table.kind == "p"),
            "{parent}: expected a partitioned parent; the partition-by-name half \
             of this matrix depends on it"
        );
    }
}

// ---------------------------------------------------------------------------
// Behavioural matrix: the phase-01 proof against the partitioned tables,
// through the parent and through the directly-named child.
// ---------------------------------------------------------------------------

/// Everything the behavioural half needs to address rows inside the
/// current month's child partitions by name.
struct MonthPartitions {
    conversion_child: String,
    metrics_child: String,
    /// The month start; adding small day offsets stays inside the current
    /// month, so every seeded row routes to the named children.
    month_start: chrono::NaiveDate,
}

async fn current_month_children(pool: &sqlx::PgPool) -> MonthPartitions {
    // The worker's own daily call, made here through the application role
    // (the only role granted EXECUTE): it guarantees the current month's
    // children exist even when the shared test template was built near a
    // month boundary. That staleness is exactly why this must be a call
    // and not an assumption.
    sqlx::query("SELECT advertising.ensure_monthly_partitions(now()::date)")
        .execute(pool)
        .await
        .expect("sanvi_app can run the partition maintenance job");
    sqlx::query("SELECT advertising.ensure_monthly_partitions((now() + interval '1 month')::date)")
        .execute(pool)
        .await
        .expect("sanvi_app can run next month's partition maintenance too");

    let suffix: String = sqlx::query_scalar("SELECT to_char(date_trunc('month', now()), 'YYYYMM')")
        .fetch_one(pool)
        .await
        .unwrap();
    let month_start: chrono::NaiveDate =
        sqlx::query_scalar("SELECT date_trunc('month', now())::date")
            .fetch_one(pool)
            .await
            .unwrap();

    let parts = MonthPartitions {
        conversion_child: format!("advertising.conversion_events_{suffix}"),
        metrics_child: format!("advertising.metrics_daily_{suffix}"),
        month_start,
    };
    for named in [&parts.conversion_child, &parts.metrics_child] {
        let bare = named.rsplit('.').next().unwrap();
        let exists: bool = sqlx::query_scalar(
            r#"SELECT EXISTS (
                 SELECT 1 FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
                 WHERE n.nspname = 'advertising' AND c.relname = $1 AND c.relkind = 'r'
               )"#,
        )
        .bind(bare)
        .fetch_one(pool)
        .await
        .unwrap();
        assert!(
            exists,
            "{named}: current-month partition missing after ensure_monthly_partitions"
        );
    }
    parts
}

#[tokio::test]
async fn partitions_isolate_per_child_through_parent_and_direct_access() {
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
    let manager = TxManager::new(pool.clone(), sanvi_testsupport::test_ids());

    let parts = current_month_children(&pool).await;
    let day = |offset: i64| parts.month_start + chrono::Duration::days(offset);
    let at = |offset: i64, hour: u32| day(offset).and_hms_opt(hour, 0, 0).unwrap().and_utc();

    // Two tenants, as every RLS suite seeds them.
    for (id, slug) in [(TENANT_A, "rls-matrix-a"), (TENANT_B, "rls-matrix-b")] {
        sqlx::query(
            "INSERT INTO tenancy.tenants (id, slug, display_name, status, region)
             VALUES ($1, $2, $3, 'active', 'us')",
        )
        .bind(id)
        .bind(slug)
        .bind(format!("{slug} corp"))
        .execute(&pool)
        .await
        .unwrap();
    }

    // Seed both tenants' rows through the audited platform scope — the
    // single escape hatch — proving it writes every tenant on the record.
    {
        let mut scope = manager.begin_unscoped("seed", "rls-matrix").await.unwrap();
        let tx = scope.tx_mut().as_inner();
        for (tenant, conn, campaign) in [
            (TENANT_A, CONN_A, CAMPAIGN_A),
            (TENANT_B, CONN_B, CAMPAIGN_B),
        ] {
            sqlx::query(
                "INSERT INTO advertising.connections
                 (id, tenant_id, platform, external_account_id, currency, timezone,
                  status, token_ref)
                 VALUES ($1, $2, 'google_ads', $3, 'JPY', 'Asia/Tokyo', 'active', 'ref')",
            )
            .bind(conn)
            .bind(tenant)
            .bind(format!("ext-{conn}"))
            .execute(&mut **tx)
            .await
            .unwrap();
            sqlx::query(
                "INSERT INTO advertising.campaigns
                 (id, tenant_id, connection_id, platform, name, objective, status, budget)
                 VALUES ($1, $2, $3, 'google_ads', $4, 'sales', 'draft', '{}')",
            )
            .bind(campaign)
            .bind(tenant)
            .bind(conn)
            .bind(format!("campaign-{campaign}"))
            .execute(&mut **tx)
            .await
            .unwrap();
        }
        for (tenant, events) in [(TENANT_A, EVENTS_A), (TENANT_B, EVENTS_B)] {
            for event in events {
                // The immutable dedup identity is (tenant_id, dedupe_key);
                // event_id is the vendor's mutable id kept for matching.
                let key = format!("ded-{event}");
                sqlx::query(
                    "INSERT INTO advertising.conversion_event_dedup
                     (tenant_id, event_id, dedupe_key, occurred_at) VALUES ($1, $2, $3, $4)",
                )
                .bind(tenant)
                .bind(format!("evt-{event}"))
                .bind(&key)
                .bind(at(0, 9))
                .execute(&mut **tx)
                .await
                .unwrap();
                sqlx::query(
                    "INSERT INTO advertising.conversion_events
                     (id, tenant_id, event_id, dedupe_key, name, occurred_at, value_minor,
                      currency, consent)
                     VALUES ($1, $2, $3, $4, $5, $6, 100, 'JPY', '{}')",
                )
                .bind(event)
                .bind(tenant)
                .bind(format!("evt-{event}"))
                .bind(&key)
                .bind(format!("event-{event}"))
                .bind(at(0, 10))
                .execute(&mut **tx)
                .await
                .unwrap();
            }
            // metrics_daily rows on distinct in-month days; the counts stay
            // asymmetric (A: 2, B: 3) so a mixed read matches neither.
            let offsets: &[i64] = if tenant == TENANT_A {
                &[0, 1]
            } else {
                &[0, 1, 2]
            };
            for offset in offsets {
                sqlx::query(
                    "INSERT INTO advertising.metrics_daily
                     (tenant_id, platform, connection_id, campaign_id, date, currency)
                     VALUES ($1, 'google_ads', $2, $3, $4, 'JPY')",
                )
                .bind(tenant)
                .bind(if tenant == TENANT_A { CONN_A } else { CONN_B })
                .bind(if tenant == TENANT_A {
                    CAMPAIGN_A
                } else {
                    CAMPAIGN_B
                })
                .bind(day(*offset))
                .execute(&mut **tx)
                .await
                .unwrap();
            }
        }
        scope.commit().await.unwrap();
    }

    // ---- Tenant A's scope ----------------------------------------------------
    {
        let mut scope = manager.begin_for(TENANT_A).await.unwrap();
        let tx = scope.tx_mut().as_inner();

        // Reads through the parent: only A's rows, never B's.
        let conversions: i64 =
            sqlx::query_scalar("SELECT count(*) FROM advertising.conversion_events")
                .fetch_one(&mut **tx)
                .await
                .unwrap();
        assert_eq!(
            conversions,
            EVENTS_A.len() as i64,
            "conversion_events through the parent must show only tenant A's rows"
        );
        let metrics: i64 = sqlx::query_scalar("SELECT count(*) FROM advertising.metrics_daily")
            .fetch_one(&mut **tx)
            .await
            .unwrap();
        assert_eq!(
            metrics, 2,
            "metrics_daily through the parent must show only tenant A's rows"
        );

        // An explicit tenant_id parameter cannot leak (the phase-01 matrix's
        // raw-SQL proof), against both access paths.
        for what in [
            "SELECT count(*) FROM advertising.conversion_events WHERE tenant_id = $1".to_string(),
            format!(
                "SELECT count(*) FROM {} WHERE tenant_id = $1",
                parts.conversion_child
            ),
            "SELECT count(*) FROM advertising.metrics_daily WHERE tenant_id = $1".to_string(),
            format!(
                "SELECT count(*) FROM {} WHERE tenant_id = $1",
                parts.metrics_child
            ),
        ] {
            let seen: i64 = sqlx::query_scalar(audited(&what))
                .bind(TENANT_B)
                .fetch_one(&mut **tx)
                .await
                .unwrap_or(-1);
            assert_eq!(
                seen, 0,
                "{what}: scoped to tenant A, tenant B must be invisible"
            );
        }

        // Direct reads of the named child partitions: the exact access path a
        // missing child policy would expose.
        let conversions_child: i64 = sqlx::query_scalar(audited(&format!(
            "SELECT count(*) FROM {}",
            parts.conversion_child
        )))
        .fetch_one(&mut **tx)
        .await
        .unwrap();
        assert_eq!(
            conversions_child,
            EVENTS_A.len() as i64,
            "{} read directly must show only tenant A's rows",
            parts.conversion_child
        );
        let metrics_child: i64 = sqlx::query_scalar(audited(&format!(
            "SELECT count(*) FROM {}",
            parts.metrics_child
        )))
        .fetch_one(&mut **tx)
        .await
        .unwrap();
        assert_eq!(
            metrics_child, 2,
            "{} read directly must show only tenant A's rows",
            parts.metrics_child
        );

        // conversion_events has no DELETE grant (retention drops partitions),
        // so its write-path USING proof is an UPDATE that must touch nothing.
        let touched = sqlx::query(audited(&format!(
            "UPDATE {} SET value_minor = 1 WHERE tenant_id = {}",
            parts.conversion_child, TENANT_B
        )))
        .execute(&mut **tx)
        .await
        .unwrap()
        .rows_affected();
        assert_eq!(
            touched, 0,
            "{}: tenant A's scope must not update tenant B's rows",
            parts.conversion_child
        );

        // metrics_daily carries DELETE (the ingestion window replace): a
        // direct-child delete aimed at B must remove nothing…
        let deleted = sqlx::query(audited(&format!(
            "DELETE FROM {} WHERE tenant_id = {}",
            parts.metrics_child, TENANT_B
        )))
        .execute(&mut **tx)
        .await
        .unwrap()
        .rows_affected();
        assert_eq!(
            deleted, 0,
            "{}: tenant A's scope must not delete tenant B's rows",
            parts.metrics_child
        );

        // …and an unqualified delete through the parent must stop at A's own
        // rows, never cross into the child's other tenants.
        let own = sqlx::query("DELETE FROM advertising.metrics_daily")
            .execute(&mut **tx)
            .await
            .unwrap()
            .rows_affected();
        assert_eq!(
            own, 2,
            "the parent-path delete removes exactly tenant A's rows"
        );

        scope.commit().await.unwrap();
    }

    // Writes for tenant B are rejected by WITH CHECK, through the parent
    // (which routes into the current child) and against the child by name.
    // Every FK target exists in B's seeded rows (the dedupe keys reuse a
    // live one), so the only gate that can fail is RLS. Each attempt gets
    // its own transaction: a rejected statement aborts the transaction it
    // ran in, exactly as it would abort a request in production.
    for target in [
        format!(
            "INSERT INTO advertising.conversion_events
             (id, tenant_id, event_id, dedupe_key, name, occurred_at, consent)
             VALUES ({}, {}, 'evt-{}', 'ded-{}', 'x', now(), '{{}}')",
            EVENTS_B[0] + 1,
            TENANT_B,
            EVENTS_B[0] + 1,
            EVENTS_B[0]
        ),
        format!(
            "INSERT INTO {} (id, tenant_id, event_id, dedupe_key, name, occurred_at, consent)
             VALUES ({}, {}, 'evt-{}', 'ded-{}', 'x', now(), '{{}}')",
            parts.conversion_child,
            EVENTS_B[0] + 2,
            TENANT_B,
            EVENTS_B[0] + 2,
            EVENTS_B[0]
        ),
        format!(
            "INSERT INTO advertising.metrics_daily
             (tenant_id, platform, connection_id, campaign_id, date, currency)
             VALUES ({}, 'google_ads', {}, {}, date_trunc('month', now())::date + 3, 'JPY')",
            TENANT_B, CONN_B, CAMPAIGN_B
        ),
        format!(
            "INSERT INTO {} (tenant_id, platform, connection_id, campaign_id, date, currency)
             VALUES ({}, 'google_ads', {}, {}, date_trunc('month', now())::date + 4, 'JPY')",
            parts.metrics_child, TENANT_B, CONN_B, CAMPAIGN_B
        ),
    ] {
        let mut scope = manager.begin_for(TENANT_A).await.unwrap();
        let error = sqlx::query(audited(&target))
            .execute(&mut **scope.tx_mut().as_inner())
            .await
            .expect_err(&format!("cross-tenant write must be rejected: {target}"));
        assert!(
            is_rls_violation(&error),
            "{target}: expected a row-level security violation, got: {error}"
        );
        scope.rollback().await.unwrap();
    }

    // ---- The audited platform scope sees everything --------------------------
    {
        let mut scope = manager
            .begin_unscoped("verify", "rls-matrix")
            .await
            .unwrap();
        let tx = scope.tx_mut().as_inner();
        // conversion_events: nothing was ever deleted there (no DELETE grant
        // on that table by design — retention drops partitions instead), so
        // both tenants' rows must be exactly as seeded, on both access paths.
        // metrics_daily: tenant A's rows were deleted under A's own scope;
        // only tenant B's three survive.
        let both = (EVENTS_A.len() + EVENTS_B.len()) as i64;
        for (what, expected) in [
            (
                "SELECT count(*) FROM advertising.conversion_events".to_string(),
                both,
            ),
            (
                format!("SELECT count(*) FROM {}", parts.conversion_child),
                both,
            ),
            (
                "SELECT count(*) FROM advertising.metrics_daily".to_string(),
                3,
            ),
            (format!("SELECT count(*) FROM {}", parts.metrics_child), 3),
        ] {
            let seen: i64 = sqlx::query_scalar(audited(&what))
                .fetch_one(&mut **tx)
                .await
                .unwrap();
            assert_eq!(
                seen, expected,
                "{what}: after tenant A's scoped deletes, both access paths must \
                 agree with the seeded world ({expected} rows)"
            );
        }
        // The rejected cross-tenant writes really were rejected: no sixth
        // conversion event, no fourth B metrics row.
        let b_conversions: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM advertising.conversion_events WHERE tenant_id = $1",
        )
        .bind(TENANT_B)
        .fetch_one(&mut **tx)
        .await
        .unwrap();
        assert_eq!(b_conversions, EVENTS_B.len() as i64);
        let a_conversions: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM advertising.conversion_events WHERE tenant_id = $1",
        )
        .bind(TENANT_A)
        .fetch_one(&mut **tx)
        .await
        .unwrap();
        assert_eq!(a_conversions, EVENTS_A.len() as i64);
        scope.commit().await.unwrap();

        // audit_log is INSERT-only for the application role (append-only by
        // design), so the count is read back through the superuser pool —
        // after the commit, or this scope's own row would still be hidden
        // inside its transaction.
        let admin = db.admin_pool().await;
        let audits: i64 = sqlx::query_scalar("SELECT count(*) FROM platform.audit_log")
            .fetch_one(&admin)
            .await
            .unwrap();
        assert!(
            audits >= 2,
            "every platform scope must have written its audit row"
        );
    }
}
