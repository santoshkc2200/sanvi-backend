//! FR-1108/FR-1109 (TASK-021, slice 11.2e): statement-count guards for the
//! read models.
//!
//! 11.2c's profiles caught the shape this gate exists to keep out: the public
//! pricing page issued one entitlements query and one prices query **per plan
//! row** — 1,803 statements per 601 requests, every one of them doing the
//! work a single batched query does. Nothing else in the workspace fails on
//! that shape: the plan gate (11.2d) pins access paths per statement, the
//! bounded-collections gate (11.2b) pins page sizes, and a loop of small,
//! fast, correctly-indexed queries sails through both.
//!
//! The oracle is `pg_stat_statements`. Every use case in this file runs
//! against its own freshly cloned test database between two snapshots of the
//! module's counters, and the *diff* is the use case's statement profile:
//! how many statements it issued in total, and how many times each one. The
//! assertions are per-fragment counts ("the pricing page touches
//! `billing.plan_prices` exactly once") plus a total ceiling. Reintroducing
//! the loop makes the per-row query fire once per seeded row — three seeded
//! plans turn "exactly 1" into "3" — so the count test is the falsifiable
//! form of "no N+1 here"; the deliberate-regression drill is in the slice
//! record.
//!
//! **Why the diff is safe under a parallel suite.** `pg_stat_statements`
//! aggregates by `(userid, dbid, queryid)` and the snapshot filters to the
//! *current database's* id, so statements issued by other tests — each on a
//! database of its own — never enter the window. No global reset is used, so
//! concurrent tests cannot erase each other's counters either. The counter's
//! own snapshot statement is excluded by text (it would otherwise sit in
//! every window as a phantom call).
//!
//! **The fixture.** Rows are seeded through the context's own repositories
//! before the measurement window opens, so the counts belong to the use case
//! alone. Seeds size every guarded list at three rows: that is what turns a
//! reintroduced per-row query from an off-by-one into a visible multiple.
//! The module must be preloaded on the shared test server
//! (`shared_preload_libraries=pg_stat_statements`, set in both `testsupport`
//! and `scripts/test-db.sh`); when it is not, the snapshot fails loudly
//! naming the fix rather than passing on an empty window.
//!
//! Every id in the 912_* range is owned by this suite.

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::Arc;

use sanvi_access::adapters::{SqlxEntitlementRepository, SqlxRoleRepository};
use sanvi_access::domain::RoleRepository;
use sanvi_billing::adapters::{
    SqlxPlanEntitlementRepository, SqlxPlanPriceRepository, SqlxPlanRepository,
};
use sanvi_billing::application::{ListPlans, ListPublicPlans};
use sanvi_billing::domain::{
    EntitlementGrant, Plan, PlanEntitlementRepository, PlanId, PlanPrice, PlanPriceRepository,
    PlanRepository, PlanTier, PlanVisibility, PriceId, PriceInterval,
};
use sanvi_domains::adapters::{SqlxCertificateRepository, SqlxCustomDomainRepository};
use sanvi_domains::application::ListCustomDomains;
use sanvi_domains::domain::{
    Certificate, CertificateId, CertificateRepository, CustomDomain, CustomDomainId,
    CustomDomainRepository, DomainKind, DomainRole, Hostname,
};
use sanvi_identity::adapters::repo::SqlxMembershipRepository;
use sanvi_identity::application::members::ListMembers;
use sanvi_payments::adapters::{
    SqlxCheckoutRepository, SqlxDisputeRepository, SqlxPaymentRepository,
    SqlxProviderConnectionRepository,
};
use sanvi_payments::application::{ListDisputes, ListDisputesCommand};
use sanvi_persistence::TxManager;
use sanvi_platform_admin::adapters::{
    SqlxImpersonationGrantRepository, SqlxTenantAdminViewRepository, SystemClock,
};
use sanvi_platform_admin::domain::{TenantAdminViewRepository, TenantViewSearch};
use sanvi_tenancy::adapters::SqlxTenantRepository;
use sanvi_testsupport::TestDb;
use sqlx::postgres::PgConnectOptions;
use sqlx::{Connection, PgConnection};

// ---------------------------------------------------------------------------
// The counting harness
// ---------------------------------------------------------------------------

/// One executed statement: the calls delta for this window and its
/// (normalized) text.
#[derive(Debug, Clone, PartialEq, Eq)]
struct StatementCount {
    calls: i64,
    text: String,
}

/// The statements one use case issued, as seen by `pg_stat_statements`.
#[derive(Debug, Clone, Default)]
struct StatementWindow {
    entries: Vec<StatementCount>,
}

impl StatementWindow {
    /// Total statements issued in the window.
    fn total(&self) -> i64 {
        self.entries.iter().map(|e| e.calls).sum()
    }

    /// Calls summed over every statement whose text contains `fragment` —
    /// the count for "queries touching this table", however they are spelled.
    fn calls_matching(&self, fragment: &str) -> i64 {
        self.entries
            .iter()
            .filter(|e| e.text.contains(fragment))
            .map(|e| e.calls)
            .sum()
    }

    fn assert_exact(&self, fragment: &str, expected: i64) {
        let seen = self.calls_matching(fragment);
        assert_eq!(
            seen, expected,
            "statements matching {fragment:?} must fire exactly {expected}x per use \
             case, saw {seen}x; a multiple of the row count is the signature of a \
             per-row query in a loop (N+1). Window: {self:?}"
        );
    }

    /// A budget, not a shape pin: the use case may not grow statements
    /// without this gate being revisited. The caller accounts for fixed
    /// overhead (scope begin, set_config) in the number.
    fn assert_total_at_most(&self, cap: i64) {
        assert!(
            self.total() <= cap,
            "use case issued {} statements, cap is {cap}; a read path that grows \
             statements needs review — either it found an N+1 or the cap moved. \
             Window: {self:?}",
            self.total(),
        );
    }
}

/// `pg_stat_statements` counters for the database `conn` is attached to,
/// keyed by queryid with each entry's call count and text.
async fn statement_snapshot(
    conn: &mut PgConnection,
) -> Result<BTreeMap<i64, (i64, String)>, sqlx::Error> {
    let rows: Vec<(i64, i64, String)> = sqlx::query_as(
        "SELECT queryid, calls, query
         FROM pg_stat_statements
         WHERE dbid = (SELECT oid FROM pg_database WHERE datname = current_database())",
    )
    .fetch_all(conn)
    .await
    .map_err(|err| {
        // A silent empty window would be a *pass* with no oracle, so every
        // failure mode the gate can see must name its fix.
        let hint = match &err {
            sqlx::Error::Database(db) if db.message().contains("shared_preload_libraries") => {
                "the shared test server must preload pg_stat_statements; \
                     recreate it (scripts/test-db.sh down && scripts/test-db.sh up, \
                     or docker rm -f sanvi_test_pg) and rerun"
            }
            _ => "is pg_stat_statements preloaded on the shared test server?",
        };
        sqlx::Error::Configuration(format!("{err}: {hint}").into())
    })?;
    Ok(aggregate_statement_rows(rows))
}

/// pg_stat_statements has a separate row per role (and top-level flag).
/// A queryid alone is not unique: collecting directly into a map silently
/// dropped counters, making a real query occasionally appear to issue zero calls.
fn aggregate_statement_rows(rows: Vec<(i64, i64, String)>) -> BTreeMap<i64, (i64, String)> {
    let mut snapshot: BTreeMap<i64, (i64, String)> = BTreeMap::new();
    for (id, calls, text) in rows {
        snapshot
            .entry(id)
            .and_modify(|entry| entry.0 += calls)
            .or_insert((calls, text));
    }
    snapshot
}

/// Pure core of the gate, unit-tested below: two snapshots become the window
/// of statements the use case issued. The counter's own SELECT is dropped by
/// text — it would otherwise appear in every window as a phantom call.
fn diff_window(
    before: &BTreeMap<i64, (i64, String)>,
    after: &BTreeMap<i64, (i64, String)>,
) -> StatementWindow {
    let mut entries = Vec::new();
    for (id, (calls_after, text)) in after {
        let calls_before = before.get(id).map(|(calls, _)| *calls).unwrap_or(0);
        let delta = calls_after - calls_before;
        if delta > 0 && !text.contains("pg_stat_statements") {
            entries.push(StatementCount {
                calls: delta,
                text: text.clone(),
            });
        }
    }
    // Most-informative order for a failure dump: heaviest first.
    entries.sort_by(|a, b| b.calls.cmp(&a.calls).then(a.text.cmp(&b.text)));
    StatementWindow { entries }
}

/// Superuser connection to the test database — the least-privilege app role
/// sees only its own entries in `pg_stat_statements`, and the fixture seeds
/// through other roles.
async fn admin_conn(db: &TestDb) -> PgConnection {
    PgConnection::connect_with(
        &PgConnectOptions::new()
            .host("127.0.0.1")
            .port(db.port())
            .database(db.name())
            .username("postgres")
            .password("postgres")
            .application_name("sanvi-perf-statement-counts"),
    )
    .await
    .expect("admin connection for statement counting")
}

/// Run one use case between two counter snapshots; return its output and the
/// statements it issued.
async fn measured<T>(db: &TestDb, fut: impl Future<Output = T>) -> (T, StatementWindow) {
    let mut admin = admin_conn(db).await;
    let before = statement_snapshot(&mut admin)
        .await
        .expect("statement snapshot (before)");
    let output = fut.await;
    let after = statement_snapshot(&mut admin)
        .await
        .expect("statement snapshot (after)");
    let window = diff_window(&before, &after);
    assert!(
        window.total() > 0,
        "window is empty — the use case issued nothing, which means the counter is \
         not seeing this database's statements (vacuity guard)"
    );
    (output, window)
}

// ---------------------------------------------------------------------------
// Fixture tests for the harness itself (no database)
// ---------------------------------------------------------------------------

#[test]
fn role_counters_add_instead_of_overwriting_each_other() {
    let sql = "SELECT id FROM tenancy.tenants";
    let before = aggregate_statement_rows(vec![(1, 3, sql.to_string()), (1, 7, sql.to_string())]);
    let after = aggregate_statement_rows(vec![(1, 8, sql.to_string()), (1, 4, sql.to_string())]);
    let window = diff_window(&before, &after);
    window.assert_exact("FROM tenancy.tenants", 2);
    assert_eq!(window.total(), 2);
}

/// A statement the server has never seen has no `before` entry, and its whole
/// call count belongs to the window.
#[test]
fn fresh_statements_land_in_the_window_whole() {
    let before = BTreeMap::new();
    let after = BTreeMap::from([
        (1, (3, "SELECT * FROM billing.plans".to_string())),
        (
            2,
            (
                2,
                "SELECT calls FROM pg_stat_statements WHERE dbid = $1".to_string(),
            ),
        ),
    ]);
    let window = diff_window(&before, &after);
    assert_eq!(window.calls_matching("billing.plans"), 3);
    // The counter's own statement is excluded by text.
    assert_eq!(window.calls_matching("pg_stat_statements"), 0);
    assert_eq!(window.total(), 3);
}

/// A statement that existed before contributes only its delta — the window is
/// what *this* use case issued, not the server's history.
#[test]
fn only_the_delta_counts() {
    let before = BTreeMap::from([(7, (10, "SELECT 1 FROM billing.plan_prices".to_string()))]);
    let after = BTreeMap::from([(7, (12, "SELECT 1 FROM billing.plan_prices".to_string()))]);
    let window = diff_window(&before, &after);
    assert_eq!(window.calls_matching("billing.plan_prices"), 2);
}

/// The N+1 signature: one fragment fired once per row, summed into a
/// multiple that `assert_exact` rejects.
#[test]
#[should_panic(expected = "must fire exactly 1x per use case, saw 3x")]
fn a_per_row_query_trips_the_exact_assertion() {
    let window = StatementWindow {
        entries: vec![
            StatementCount {
                calls: 3,
                text: "SELECT * FROM billing.plan_prices WHERE plan_id = $1".to_string(),
            },
            StatementCount {
                calls: 1,
                text: "COMMIT".to_string(),
            },
        ],
    };
    window.assert_exact("billing.plan_prices", 1);
}

#[test]
#[should_panic(expected = "issued 5 statements, cap is 4")]
fn a_total_over_the_cap_trips_the_ceiling() {
    let window = StatementWindow {
        entries: vec![
            StatementCount {
                calls: 3,
                text: "SELECT 1".to_string(),
            },
            StatementCount {
                calls: 2,
                text: "SELECT 2".to_string(),
            },
        ],
    };
    window.assert_total_at_most(4);
}

// ---------------------------------------------------------------------------
// The guarded use cases
// ---------------------------------------------------------------------------

/// Three public plans — one with two prices and two grants, one with one
/// price, one with none — seeded through the billing repositories in one
/// scope. Three plans is the point: a reintroduced per-row query fires three
/// times, not twice.
struct PlanCatalogFixture {
    db: TestDb,
    pool: sanvi_persistence::PgPool,
}

async fn seed_plan_catalog() -> PlanCatalogFixture {
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
    let tx = TxManager::new(pool.clone(), sanvi_testsupport::test_ids());
    let mut scope = tx
        .begin_unscoped("seed perf statement-count fixture", "perf-gate")
        .await
        .expect("seed scope");

    for index in 0..3_i64 {
        let plan = Plan::new(
            PlanId::try_from(912_001_001_000 + index * 100).unwrap(),
            format!("perf-plan-{index}"),
            format!("Perf plan {index}"),
            PlanTier::Starter,
            format!("prod_perf_{index}"),
            PlanVisibility::Public,
            index as i32,
            chrono::Utc::now(),
        )
        .expect("fixture plan is valid");
        SqlxPlanRepository::new()
            .save(&mut scope, &plan)
            .await
            .expect("seed plan");

        for price_index in 0..=index {
            let price = PlanPrice::new(
                PriceId::try_from(912_002_002_000 + index * 100 + price_index).unwrap(),
                plan.id(),
                format!("price_perf_{index}_{price_index}"),
                "jpy".to_string(),
                PriceInterval::Month,
                1_000 * (price_index + 1),
                None,
                chrono::Utc::now(),
            )
            .expect("fixture price is valid");
            SqlxPlanPriceRepository::new()
                .save(&mut scope, &price)
                .await
                .expect("seed price");
        }

        SqlxPlanEntitlementRepository::new()
            .replace_for_plan(
                &mut scope,
                plan.id(),
                &[
                    EntitlementGrant {
                        feature_key: "feature.a".to_string(),
                        enabled: true,
                        limit: Some(10 + index),
                    },
                    EntitlementGrant {
                        feature_key: "feature.b".to_string(),
                        enabled: index == 0,
                        limit: None,
                    },
                ],
            )
            .await
            .expect("seed grants");
    }

    scope.commit().await.expect("seed commit");
    PlanCatalogFixture { db, pool }
}

/// FR-1108: the public pricing page. The guarded use case — the profiled N+1.
///
/// Expected profile: scope-begin audit INSERT + platform-scope set_config +
/// one plans SELECT + one batched prices SELECT + one batched grants SELECT +
/// COMMIT. The per-table asserts are the load-bearing ones: exactly one
/// statement per table, regardless of the three seeded plans.
#[tokio::test]
async fn list_public_plans_issues_one_statement_per_table() {
    let fixture = seed_plan_catalog().await;
    let use_case = ListPublicPlans::new(
        TxManager::new(fixture.pool.clone(), sanvi_testsupport::test_ids()),
        Arc::new(SqlxPlanRepository::new()),
        Arc::new(SqlxPlanPriceRepository::new()),
        Arc::new(SqlxPlanEntitlementRepository::new()),
    );

    let (result, window) = measured(&fixture.db, use_case.execute()).await;
    let views = result.expect("list public plans succeeds");

    // The migrated template seeds its own three plans; the fixture's are the
    // perf-plan-* keys. What matters for the gate is that every plan — seeded
    // or migrated — costs zero extra statements.
    let seeded: Vec<_> = views
        .iter()
        .filter(|view| view.key.starts_with("perf-plan-"))
        .collect();
    assert_eq!(seeded.len(), 3, "all three seeded public plans are listed");
    assert_eq!(seeded[0].prices.len(), 1, "plan 0 seeded with one price");
    assert_eq!(seeded[1].prices.len(), 2, "plan 1 seeded with two prices");
    assert_eq!(seeded[2].prices.len(), 3, "plan 2 seeded with three prices");
    assert_eq!(seeded[0].entitlements.len(), 2);
    assert_eq!(
        seeded[2].prices[2].unit_amount_minor, 3_000,
        "batch preserves amounts"
    );
    assert_eq!(
        seeded[2].prices[2].interval,
        sanvi_billing::domain::PriceInterval::Month
    );

    window.assert_exact("FROM billing.plans", 1);
    // These two are the N+1 the 11.2c profile caught: one per plan row before.
    window.assert_exact("FROM billing.plan_prices", 1);
    window.assert_exact("FROM billing.plan_entitlements", 1);
    // Fixed overhead: BEGIN, audit INSERT, set_config, COMMIT. Read-model
    // growth past this is the failure the ceiling exists to catch.
    window.assert_total_at_most(8);
}

/// The operator catalog — the same loop shape on the admin surface.
#[tokio::test]
async fn list_plans_issues_one_statement_per_table() {
    let fixture = seed_plan_catalog().await;
    let use_case = ListPlans::new(
        TxManager::new(fixture.pool.clone(), sanvi_testsupport::test_ids()),
        Arc::new(SqlxPlanRepository::new()),
        Arc::new(SqlxPlanPriceRepository::new()),
        Arc::new(SqlxPlanEntitlementRepository::new()),
    );

    let (result, window) = measured(&fixture.db, use_case.execute()).await;
    let views = result.expect("list plans succeeds");

    assert_eq!(
        views
            .iter()
            .filter(|view| view.key.starts_with("perf-plan-"))
            .count(),
        3
    );
    window.assert_exact("FROM billing.plans", 1);
    window.assert_exact("FROM billing.plan_prices", 1);
    window.assert_exact("FROM billing.plan_entitlements", 1);
    window.assert_total_at_most(8);
}

// ---------------------------------------------------------------------------
// domains — the custom-domain list decorated with certificate expiry
// ---------------------------------------------------------------------------

struct DomainsFixture {
    db: TestDb,
    pool: sanvi_persistence::PgPool,
    tenant: sanvi_kernel::TenantId,
}

async fn seed_domain_catalog() -> DomainsFixture {
    use sanvi_domains::domain::CertificateStatus;

    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
    let admin = db.admin_pool().await;
    let tenant = sanvi_kernel::TenantId::try_from(912_030_000_001).unwrap();
    sqlx::query(
        "INSERT INTO tenancy.tenants (id, slug, display_name, status, region, default_locale)
         VALUES ($1, 'perf-domains', 'Perf domains', 'active', 'jp', 'en')",
    )
    .bind(tenant.as_i64())
    .execute(&admin)
    .await
    .expect("seed tenant");
    let tx = TxManager::new(pool.clone(), sanvi_testsupport::test_ids());
    let mut scope = tx.begin_for(tenant.as_i64()).await.expect("seed scope");
    let domains = SqlxCustomDomainRepository::new();
    let certs = SqlxCertificateRepository::new();

    for index in 0..3_i64 {
        let hostname =
            Hostname::parse(&format!("perf-dom-{index}.example.test")).expect("fixture hostname");
        let domain = CustomDomain::claim(
            CustomDomainId::try_from(912_031_000_100 + index * 10).unwrap(),
            tenant,
            hostname.clone(),
            DomainKind::Connected,
            if index == 0 {
                DomainRole::Primary
            } else {
                DomainRole::Alias
            },
            format!("perf-challenge-{index}"),
            chrono::Utc::now(),
            24,
            60,
        );
        domains
            .insert(&mut scope, &domain)
            .await
            .expect("seed domain");
        let cert = Certificate::issued(
            CertificateId::try_from(912_032_000_200 + index * 10).unwrap(),
            domain.id,
            tenant,
            hostname,
            "perf-issuer".to_string(),
            format!("perf-serial-{index}"),
            chrono::Utc::now() - chrono::Duration::days(30),
            chrono::Utc::now() + chrono::Duration::days(60),
        );
        assert_eq!(cert.status, CertificateStatus::Active);
        certs.insert(&mut scope, &cert).await.expect("seed cert");
    }

    scope.commit().await.expect("seed commit");
    DomainsFixture { db, pool, tenant }
}

/// The list view decorates every row with its latest active certificate —
/// per-domain lookups there were up to 200 queries per page.
#[tokio::test]
async fn list_custom_domains_issues_one_statement_per_table() {
    let fixture = seed_domain_catalog().await;
    let tx = TxManager::new(fixture.pool.clone(), sanvi_testsupport::test_ids());
    let use_case = ListCustomDomains::new(
        Arc::new(SqlxCustomDomainRepository::new()),
        Arc::new(SqlxCertificateRepository::new()),
    );
    let tenant = fixture.tenant;

    let (result, window) = measured(&fixture.db, async move {
        let mut scope = tx.begin_for(tenant.as_i64()).await.expect("tenant scope");
        use_case.execute(&mut scope, tenant, None, None).await
    })
    .await;

    let views = result.expect("list custom domains succeeds");
    assert_eq!(views.len(), 3);
    assert!(
        views.iter().all(|view| view.cert_expires_at.is_some()),
        "every domain carries its certificate expiry through the batch"
    );
    window.assert_exact("FROM domains.custom_domains", 1);
    // The N+1: one certificates query per domain row before.
    window.assert_exact("FROM domains.certificates", 1);
    window.assert_total_at_most(8);
}

// ---------------------------------------------------------------------------
// identity — the member directory's role hydration
// ---------------------------------------------------------------------------

struct IdentityFixture {
    db: TestDb,
    pool: sanvi_persistence::PgPool,
    tenant: sanvi_kernel::TenantId,
}

async fn seed_member_directory() -> IdentityFixture {
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
    let admin = db.admin_pool().await;
    let tenant = sanvi_kernel::TenantId::try_from(912_040_000_001).unwrap();

    sqlx::query(
        "INSERT INTO tenancy.tenants (id, slug, display_name, status, region, default_locale)
         VALUES ($1, 'perf-members', 'Perf members', 'active', 'jp', 'en')",
    )
    .bind(tenant.as_i64())
    .execute(&admin)
    .await
    .expect("seed tenant");
    // membership_roles.role_id references access.roles — seed the referenced
    // system-template roles first.
    for role_index in 0..2_i64 {
        sqlx::query(
            "INSERT INTO access.roles (id, scope, tenant_id, key, name, is_system)
             VALUES ($1, 'tenant', NULL, $2, $3, true)
             ON CONFLICT (id) DO NOTHING",
        )
        .bind(912_042_000_000 + role_index)
        .bind(format!("perf-member-role-{role_index}"))
        .bind(format!("Perf member role {role_index}"))
        .execute(&admin)
        .await
        .expect("seed referenced role");
    }

    for index in 0..3_i64 {
        let user_id = 912_041_000_100 + index * 10;
        sqlx::query(
            "INSERT INTO identity.users (id, kratos_identity_id, primary_email, email_verified, status)
             VALUES ($1, $2, $3, true, 'active')",
        )
        .bind(user_id)
        .bind(uuid::Uuid::now_v7())
        .bind(format!("perf-member-{index}@example.test"))
        .execute(&admin)
        .await
        .expect("seed user");
        sqlx::query(
            "INSERT INTO identity.memberships (tenant_id, user_id, status, invited_by, created_at, version)
             VALUES ($1, $2, 'active', NULL, now(), 1)",
        )
        .bind(tenant.as_i64())
        .bind(user_id)
        .execute(&admin)
        .await
        .expect("seed membership");
        // Role ids are unconstrained by foreign keys; two per member.
        for role_index in 0..2_i64 {
            sqlx::query(
                "INSERT INTO identity.membership_roles (tenant_id, user_id, role_id)
                 VALUES ($1, $2, $3)",
            )
            .bind(tenant.as_i64())
            .bind(user_id)
            .bind(912_042_000_000 + role_index)
            .execute(&admin)
            .await
            .expect("seed membership role");
        }
    }

    IdentityFixture { db, pool, tenant }
}

/// `ListMembers` hydrates each member's roles — per-member queries there were
/// up to 200 per page before the batch.
#[tokio::test]
async fn list_members_issues_one_statement_per_table() {
    let fixture = seed_member_directory().await;
    let use_case = ListMembers::new(
        TxManager::new(fixture.pool.clone(), sanvi_testsupport::test_ids()),
        Arc::new(SqlxMembershipRepository::new()),
    );

    let (result, window) =
        measured(&fixture.db, use_case.execute(fixture.tenant, None, None)).await;
    let views = result.expect("list members succeeds");
    assert_eq!(views.len(), 3);
    assert!(
        views.iter().all(|view| view.role_ids.len() == 2),
        "batch hydration preserves both roles per member"
    );
    window.assert_exact("FROM identity.memberships", 1);
    // The N+1: one membership_roles query per member before.
    window.assert_exact("FROM identity.membership_roles", 1);
    window.assert_total_at_most(8);
}

// ---------------------------------------------------------------------------
// payments — the dispute list's deep-link resolution chain
// ---------------------------------------------------------------------------

struct PaymentsFixture {
    db: TestDb,
    pool: sanvi_persistence::PgPool,
    tenant: sanvi_kernel::TenantId,
}

async fn seed_dispute_page() -> PaymentsFixture {
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
    let admin = db.admin_pool().await;
    let tenant = sanvi_kernel::TenantId::try_from(912_050_000_001).unwrap();

    sqlx::query(
        "INSERT INTO tenancy.tenants (id, slug, display_name, status, region, default_locale)
         VALUES ($1, 'perf-disputes', 'Perf disputes', 'active', 'jp', 'en')",
    )
    .bind(tenant.as_i64())
    .execute(&admin)
    .await
    .expect("seed tenant");

    // One live connection the whole chain resolves through.
    sqlx::query(
        "INSERT INTO payments.provider_connections
           (id, tenant_id, provider, external_account_id, status, capabilities, requirements)
         VALUES ($1, $2, 'stripe_connect', 'acct_perf', 'active', '{}', '{}')",
    )
    .bind(912_051_000_001_i64)
    .bind(tenant.as_i64())
    .execute(&admin)
    .await
    .expect("seed connection");

    for index in 0..3_i64 {
        let checkout_id = 912_052_000_100 + index * 10;
        let payment_id = 912_053_000_200 + index * 10;
        sqlx::query(
            "INSERT INTO payments.checkouts
               (id, tenant_id, connection_id, reference, amount_minor, currency, status)
             VALUES ($1, $2, $3, $4, 1000, 'JPY', 'complete')",
        )
        .bind(checkout_id)
        .bind(tenant.as_i64())
        .bind(912_051_000_001_i64)
        .bind(format!("perf-order-{index}"))
        .execute(&admin)
        .await
        .expect("seed checkout");
        sqlx::query(
            "INSERT INTO payments.payments
               (id, tenant_id, checkout_id, external_payment_id, status, amount_minor, currency)
             VALUES ($1, $2, $3, $4, 'succeeded', 1000, 'JPY')",
        )
        .bind(payment_id)
        .bind(tenant.as_i64())
        .bind(checkout_id)
        .bind(format!("pi_perf_{index}"))
        .execute(&admin)
        .await
        .expect("seed payment");
        // One dispute per payment (payment 0 additionally gets an older row,
        // so the page carries two disputes on one payment).
        sqlx::query(
            "INSERT INTO payments.disputes
               (id, tenant_id, payment_id, external_dispute_id, status, amount_minor, currency,
                updated_at)
             VALUES ($1, $2, $3, $4, 'needs_response', 1000, 'JPY', now())",
        )
        .bind(912_054_000_300 + index * 10)
        .bind(tenant.as_i64())
        .bind(payment_id)
        .bind(format!("dp_perf_{index}"))
        .execute(&admin)
        .await
        .expect("seed dispute");
    }
    sqlx::query(
        "INSERT INTO payments.disputes
           (id, tenant_id, payment_id, external_dispute_id, status, amount_minor, currency,
            updated_at)
         VALUES ($1, $2, $3, $4, 'under_review', 1000, 'JPY', now() - interval '1 day')",
    )
    .bind(912_054_000_301_i64)
    .bind(tenant.as_i64())
    .bind(912_053_000_200_i64)
    .bind("dp_perf_old")
    .execute(&admin)
    .await
    .expect("seed older dispute");

    PaymentsFixture { db, pool, tenant }
}

/// The deep link resolves payment → checkout → connection per dispute; the
/// chain used to cost up to three queries per row.
#[tokio::test]
async fn list_disputes_resolves_accounts_in_constant_statements() {
    let fixture = seed_dispute_page().await;
    let use_case = ListDisputes::new(
        TxManager::new(fixture.pool.clone(), sanvi_testsupport::test_ids()),
        Arc::new(SqlxDisputeRepository::new()),
        Arc::new(SqlxPaymentRepository::new()),
        Arc::new(SqlxCheckoutRepository::new()),
        Arc::new(SqlxProviderConnectionRepository::new()),
    );
    let tenant = fixture.tenant;

    let (result, window) = measured(
        &fixture.db,
        use_case.execute(ListDisputesCommand {
            tenant_id: tenant,
            status: None,
            currency: None,
            date_from: None,
            date_to: None,
            cursor: None,
            limit: None,
        }),
    )
    .await;

    let view = result.expect("list disputes succeeds");
    assert_eq!(view.items.len(), 4, "four disputes on the page");
    window.assert_exact("FROM payments.disputes", 1);
    // The resolution chain: three batched queries — one per hop, not per row.
    window.assert_exact("FROM payments.payments", 1);
    window.assert_exact("FROM payments.checkouts", 1);
    // Exactly one: the connections batch. A second hit would be the
    // live-connection fallback, which only fires when the chain dangles.
    window.assert_exact("FROM payments.provider_connections", 1);
    window.assert_total_at_most(12);
}

// ---------------------------------------------------------------------------
// platform-admin — the tenant directory's per-tenant counts
// ---------------------------------------------------------------------------

struct TenantDirectoryFixture {
    db: TestDb,
    pool: sanvi_persistence::PgPool,
}

async fn seed_tenant_directory() -> TenantDirectoryFixture {
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
    let admin = db.admin_pool().await;

    sqlx::query(
        "INSERT INTO access.features (key, name, kind) VALUES ('perf.feature', 'Perf feature', 'boolean')
         ON CONFLICT (key) DO NOTHING",
    )
    .execute(&admin)
    .await
    .expect("seed feature");

    for index in 0..3_i64 {
        let tenant_id = 912_060_000_100 + index * 10;
        sqlx::query(
            "INSERT INTO tenancy.tenants (id, slug, display_name, status, region, default_locale)
             VALUES ($1, $2, $3, 'active', 'jp', 'en')",
        )
        .bind(tenant_id)
        .bind(format!("perf-view-{index}"))
        .bind(format!("Perf view {index}"))
        .execute(&admin)
        .await
        .expect("seed tenant");
        sqlx::query(
            "INSERT INTO identity.users (id, kratos_identity_id, primary_email, email_verified, status)
             VALUES ($1, $2, $3, true, 'active')
             ON CONFLICT (id) DO NOTHING",
        )
        .bind(912_061_000_000 + index)
        .bind(uuid::Uuid::now_v7())
        .bind(format!("perf-view-member-{index}@example.test"))
        .execute(&admin)
        .await
        .expect("seed user");
        sqlx::query(
            "INSERT INTO identity.memberships (tenant_id, user_id, status, invited_by, created_at, version)
             VALUES ($1, $2, 'active', NULL, now(), 1)",
        )
        .bind(tenant_id)
        .bind(912_061_000_000 + index)
        .execute(&admin)
        .await
        .expect("seed membership");
        sqlx::query(
            "INSERT INTO access.entitlements (tenant_id, feature_key, enabled, source)
             VALUES ($1, 'perf.feature', true, 'override')
             ON CONFLICT (tenant_id, feature_key, source) DO NOTHING",
        )
        .bind(tenant_id)
        .execute(&admin)
        .await
        .expect("seed entitlement");
        sqlx::query(
            "INSERT INTO platform_admin.impersonation_grants
               (id, actor_id, tenant_id, target_user_id, reason, mode, expires_at)
             VALUES ($1, $2, $3, $4, 'perf fixture', 'read_only', now() + interval '1 day')",
        )
        .bind(912_062_000_000 + index)
        .bind(912_063_000_000_i64)
        .bind(tenant_id)
        .bind(912_061_000_000 + index)
        .execute(&admin)
        .await
        .expect("seed impersonation grant");
    }

    TenantDirectoryFixture { db, pool }
}

/// The tenant list decorates every row with member/override/grant counts —
/// three queries per tenant before the batch.
#[tokio::test]
async fn tenant_view_search_counts_in_constant_statements() {
    let fixture = seed_tenant_directory().await;
    let repo = SqlxTenantAdminViewRepository::new(
        Arc::new(SqlxTenantRepository::new()),
        Arc::new(SqlxMembershipRepository::new()),
        Arc::new(SqlxEntitlementRepository::new()),
        Arc::new(SqlxImpersonationGrantRepository::new()),
        Arc::new(SystemClock),
    );
    let tx = TxManager::new(fixture.pool.clone(), sanvi_testsupport::test_ids());

    let (result, window) = measured(&fixture.db, async move {
        let mut scope = tx
            .begin_unscoped("perf tenant view search", "perf-gate")
            .await
            .expect("platform scope");
        repo.search(
            &mut scope,
            &TenantViewSearch {
                query: None,
                status: None,
                after: None,
                limit: None,
            },
        )
        .await
    })
    .await;

    let (views, _cursor) = result.expect("tenant view search succeeds");
    assert_eq!(
        views
            .iter()
            .filter(|view| view.slug.starts_with("perf-view-"))
            .count(),
        3
    );
    let seeded = views
        .iter()
        .find(|view| view.slug == "perf-view-1")
        .expect("seeded tenant listed");
    assert_eq!(seeded.member_count, 1);
    assert_eq!(seeded.override_count, 1);
    assert_eq!(seeded.active_impersonations, 1);
    window.assert_exact("FROM tenancy.tenants", 1);
    // The three counts: one grouped query each, not one per tenant.
    window.assert_exact("FROM identity.memberships", 1);
    window.assert_exact("FROM access.entitlements", 1);
    window.assert_exact("FROM platform_admin.impersonation_grants", 1);
    window.assert_total_at_most(12);
}

// ---------------------------------------------------------------------------
// access — role hydration on the catalog read path
// ---------------------------------------------------------------------------

struct RoleCatalogFixture {
    db: TestDb,
    pool: sanvi_persistence::PgPool,
}

async fn seed_role_catalog() -> RoleCatalogFixture {
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
    let admin = db.admin_pool().await;

    for index in 0..3_i64 {
        sqlx::query(
            "INSERT INTO access.roles (id, scope, tenant_id, key, name, is_system)
             VALUES ($1, 'tenant', NULL, $2, $3, true)",
        )
        .bind(912_070_000_100 + index * 10)
        .bind(format!("perf-role-{index}"))
        .bind(format!("Perf role {index}"))
        .execute(&admin)
        .await
        .expect("seed role");
        for perm_index in 0..2_i64 {
            sqlx::query(
                "INSERT INTO access.role_permissions (role_id, permission) VALUES ($1, $2)",
            )
            .bind(912_070_000_100 + index * 10)
            .bind(format!("perf.permission.{index}.{perm_index}"))
            .execute(&admin)
            .await
            .expect("seed role permission");
        }
    }

    RoleCatalogFixture { db, pool }
}

/// `assignable_to` feeds the member-role picker and, through the role
/// catalog, `/me` — per-role permission queries compounded everywhere.
#[tokio::test]
async fn assignable_roles_hydrate_permissions_in_one_query() {
    let fixture = seed_role_catalog().await;
    let repo = SqlxRoleRepository::new();
    let tx = TxManager::new(fixture.pool.clone(), sanvi_testsupport::test_ids());
    let tenant = sanvi_kernel::TenantId::try_from(912_070_000_001).unwrap();

    let (result, window) = measured(&fixture.db, async move {
        let mut scope = tx
            .begin_unscoped("perf role hydration", "perf-gate")
            .await
            .expect("platform scope");
        repo.assignable_to(&mut scope, tenant).await
    })
    .await;

    let roles = result.expect("assignable roles succeed");
    let seeded: Vec<_> = roles
        .iter()
        .filter(|role| role.key().starts_with("perf-role-"))
        .collect();
    assert_eq!(seeded.len(), 3, "the three seeded system templates resolve");
    assert!(
        seeded.iter().all(|role| role.permissions().len() == 2),
        "batch hydration preserves both permissions per role"
    );
    window.assert_exact("FROM access.roles", 1);
    // The N+1: one permission query per role before.
    window.assert_exact("FROM access.role_permissions", 1);
    window.assert_total_at_most(8);
}
