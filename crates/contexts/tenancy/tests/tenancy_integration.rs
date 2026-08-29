//! Integration tests: real Postgres, the sanvi_app role (NOBYPASSRLS),
//! full migrations. Run with `just test` (docker must be available).

use std::sync::Arc;

use sanvi_kernel::RawIdGenerator;
use sanvi_persistence::TxManager;
use sanvi_tenancy::{
    adapters::{
        OutboxEventPublisher, SystemClock, SystemIdGenerator,
        repo::{SqlxSlugReservationPort, SqlxTenantRepository, SqlxTenantSettingsRepository},
    },
    application::{
        ActivateTenant, GetTenantSettings, ListTenants, ProvisionTenant, ProvisionTenantCommand,
        PurgeSweep, SettingsUpdate, UpdateTenantSettings,
    },
    domain::{
        Clock, EventPublisher, IdGenerator, SlugReservationPort, SuspensionReason, TenancyError,
        TenantId, TenantRepository, TenantSettingsRepository,
    },
};
use sanvi_testsupport::TestDb;

/// Fresh database with both migrations applied; returns app-role, owner-role
/// and admin pools, on a database private to the caller.
async fn prepare() -> (
    sanvi_persistence::PgPool,
    sanvi_persistence::PgPool,
    sanvi_persistence::PgPool,
    TestDb,
) {
    let db = sanvi_testsupport::test_db().await;
    let app = db.app_pool();
    let owner = db.migrator_pool().await;
    let admin = db.admin_pool().await;
    (app, owner, admin, db)
}

fn command(slug: &str) -> ProvisionTenantCommand {
    ProvisionTenantCommand {
        slug: slug.to_string(),
        display_name: format!("{slug} corp"),
        region: "us".to_string(),
        default_locale: "en".to_string(),
    }
}

fn raw_ids() -> Arc<dyn RawIdGenerator> {
    sanvi_testsupport::test_ids()
}

#[tokio::test]
async fn lifecycle_transitions_emit_events_to_the_outbox() {
    let (pool, _owner, _admin, _node) = prepare().await;
    let raw_ids = raw_ids();
    let tx_manager = TxManager::new(pool.clone(), raw_ids.clone());
    let tenants: Arc<dyn TenantRepository> = Arc::new(SqlxTenantRepository::new());
    let slugs: Arc<dyn SlugReservationPort> = Arc::new(SqlxSlugReservationPort::new());
    let events: Arc<dyn EventPublisher> = Arc::new(OutboxEventPublisher::new(raw_ids.clone()));
    let ids: Arc<dyn IdGenerator> = Arc::new(SystemIdGenerator::new(raw_ids));
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);

    let provision = ProvisionTenant::new(
        tx_manager.clone(),
        tenants.clone(),
        slugs,
        events.clone(),
        ids,
        clock.clone(),
    );
    let view = provision.execute(command("acme")).await.unwrap();
    let id: TenantId = view.id.parse().unwrap();

    let activate = ActivateTenant::new(
        tx_manager.clone(),
        tenants.clone(),
        events.clone(),
        clock.clone(),
    );
    let activated = activate.execute(id).await.unwrap();
    assert_eq!(activated.status, "active");

    // Outbox rows: slug_reserved + provisioned + activated.
    let outbox_rows: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM platform.outbox WHERE aggregate = 'tenancy.tenant'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(outbox_rows, 3);

    // Lifecycle state persisted.
    let row: (String,) = sqlx::query_as("SELECT status FROM tenancy.tenants WHERE id = $1")
        .bind(id.as_i64())
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(row.0, "active");
}

#[tokio::test]
async fn concurrent_provisions_of_the_same_slug_one_wins_one_conflicts() {
    let (pool, _owner, _admin, _node) = prepare().await;
    let raw_ids = raw_ids();
    let tx_manager = TxManager::new(pool.clone(), raw_ids.clone());
    let tenants: Arc<dyn TenantRepository> = Arc::new(SqlxTenantRepository::new());
    let slugs: Arc<dyn SlugReservationPort> = Arc::new(SqlxSlugReservationPort::new());
    let events: Arc<dyn EventPublisher> = Arc::new(OutboxEventPublisher::new(raw_ids.clone()));
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);

    let provision = Arc::new(ProvisionTenant::new(
        tx_manager.clone(),
        tenants.clone(),
        slugs.clone(),
        events.clone(),
        Arc::new(SystemIdGenerator::new(raw_ids.clone())),
        clock.clone(),
    ));
    let provision_b = Arc::new(ProvisionTenant::new(
        tx_manager.clone(),
        tenants.clone(),
        slugs,
        events.clone(),
        Arc::new(SystemIdGenerator::new(raw_ids)),
        clock.clone(),
    ));

    let (a, b) = tokio::join!(
        provision.execute(command("race")),
        provision_b.execute(command("race"))
    );

    let outcomes = [a, b];
    let winners = outcomes.iter().filter(|outcome| outcome.is_ok()).count();
    let conflicts = outcomes
        .iter()
        .filter(|outcome| matches!(outcome, Err(TenancyError::SlugTaken { .. })))
        .count();
    assert_eq!(winners, 1, "exactly one provision must win");
    assert_eq!(conflicts, 1, "the loser must get a clean slug conflict");

    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM tenancy.tenants WHERE slug = 'race'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 1);
}

#[tokio::test]
async fn tenant_scoped_settings_are_isolated_by_rls() {
    let (pool, owner, _admin, _node) = prepare().await;
    let raw_ids = raw_ids();
    let tx_manager = TxManager::new(pool.clone(), raw_ids.clone());
    let settings: Arc<dyn TenantSettingsRepository> = Arc::new(SqlxTenantSettingsRepository::new());
    let events: Arc<dyn EventPublisher> = Arc::new(OutboxEventPublisher::new(raw_ids));

    let tenant_a = TenantId::try_from(201_i64).unwrap();
    let tenant_b = TenantId::try_from(202_i64).unwrap();
    // Tenants must exist for the FK; insert via the platform-scoped path.
    for (id, slug) in [(tenant_a, "isoa"), (tenant_b, "isob")] {
        let (tenant, _) = sanvi_tenancy::Tenant::provision(
            id,
            sanvi_tenancy::TenantSlug::parse(slug).unwrap(),
            sanvi_tenancy::TenantName::parse(slug).unwrap(),
            sanvi_tenancy::DataRegion::Us,
            sanvi_tenancy::Locale::default(),
            chrono::Utc::now(),
        );
        let mut scope = tx_manager
            .begin_unscoped("test_setup", "test")
            .await
            .unwrap();
        SqlxTenantRepository::new()
            .save(&mut scope, &tenant)
            .await
            .unwrap();
        scope.commit().await.unwrap();
    }

    let update = UpdateTenantSettings::new(tx_manager.clone(), settings.clone(), events.clone());
    let mut map = std::collections::BTreeMap::new();
    map.insert("theme.primary".to_string(), serde_json::json!("#000000"));
    update
        .execute(tenant_a, SettingsUpdate { settings: map })
        .await
        .unwrap();

    // Tenant A sees its settings through the scoped path.
    let get = GetTenantSettings::new(tx_manager.clone(), settings.clone());
    let output = get.execute(tenant_a).await.unwrap();
    assert_eq!(
        output.settings["theme.primary"],
        serde_json::json!("#000000")
    );

    // Tenant B's scoped read sees nothing (RLS filters tenant A's rows).
    let output_b = get.execute(tenant_b).await.unwrap();
    assert!(output_b.settings.is_empty());

    // Raw SQL without a tenant scope sees nothing either (FORCE RLS applies
    // to the owner too).
    let admin_count: i64 = sqlx::query_scalar("SELECT count(*) FROM tenancy.tenant_settings")
        .fetch_one(&owner)
        .await
        .unwrap();
    assert_eq!(
        admin_count, 0,
        "FORCE RLS must hide rows even from the owner"
    );

    // Cross-tenant update through the scoped path cannot touch B's rows even
    // if the caller knows the tenant id exists — the setting simply does not
    // exist under A's scope.
    let mut map = std::collections::BTreeMap::new();
    map.insert("theme.primary".to_string(), serde_json::json!("#ffffff"));
    update
        .execute(tenant_b, SettingsUpdate { settings: map })
        .await
        .unwrap();
    let a_rows: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM tenancy.tenant_settings WHERE tenant_id = $1 AND value = '\"#ffffff\"'::jsonb",
    )
    .bind(tenant_a.as_i64())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(a_rows, 0);
}

/// The RLS matrix test, generated from the catalog: every table with a
/// `tenant_id` column must have RLS enabled, RLS forced, the standard
/// `tenant_isolation` policy, and must actually hide other tenants' rows
/// (select + update + delete). A new tenant-scoped table without a policy
/// fails this test.
#[tokio::test]
async fn every_tenant_scoped_table_enforces_rls_isolation() {
    let (pool, _owner, _admin, _node) = prepare().await;

    // One documented exemption: `domains.live_hostnames` (phase 08) is the
    // host→tenant projection the tenancy resolver and the TLS authorize
    // gate read on the hot path. Unscoped pool connections cannot see RLS
    // rows, so the projection deliberately has no policy; it contains only
    // live-claim hostnames (public DNS information) and is written only
    // inside transactions that also update `domains.custom_domains` — the
    // RLS-enforced source of truth (see docs/runbooks/domains.md §1 for the
    // rebuild statement).
    let tables: Vec<(String, String)> = sqlx::query_as(
        "SELECT table_schema, table_name
         FROM information_schema.columns
         WHERE column_name = 'tenant_id'
           AND table_schema NOT IN ('pg_catalog', 'information_schema')
           AND NOT (table_schema = 'domains' AND table_name = 'live_hostnames')
         ORDER BY table_schema, table_name",
    )
    .fetch_all(&pool)
    .await
    .unwrap();

    assert!(
        !tables.is_empty(),
        "expected at least one tenant-scoped table"
    );

    for (schema, table) in &tables {
        let (rls_enabled, rls_forced): (bool, bool) = sqlx::query_as(
            "SELECT relrowsecurity, relforcerowsecurity
             FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
             WHERE n.nspname = $1 AND c.relname = $2",
        )
        .bind(schema)
        .bind(table)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(rls_enabled, "{schema}.{table} must have RLS enabled");
        assert!(rls_forced, "{schema}.{table} must have RLS forced");

        let policies: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pg_policies
             WHERE schemaname = $1 AND tablename = $2 AND policyname = 'tenant_isolation'",
        )
        .bind(schema)
        .bind(table)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            policies, 1,
            "{schema}.{table} must use the tenant_isolation policy"
        );
    }

    // Behaviour: tenant A must not see/update/delete tenant B's rows.
    // (tenant_settings is the phase-01 tenant-scoped table; the loop above
    // covers every future table by construction.)
    let (a, b) = (
        TenantId::try_from(301_i64).unwrap(),
        TenantId::try_from(302_i64).unwrap(),
    );
    let manager = TxManager::new(pool.clone(), raw_ids());
    for (id, slug) in [(a, "matrixa"), (b, "matrixb")] {
        let (tenant, _) = sanvi_tenancy::Tenant::provision(
            id,
            sanvi_tenancy::TenantSlug::parse(slug).unwrap(),
            sanvi_tenancy::TenantName::parse(slug).unwrap(),
            sanvi_tenancy::DataRegion::Us,
            sanvi_tenancy::Locale::default(),
            chrono::Utc::now(),
        );
        let mut scope = manager.begin_unscoped("test_setup", "test").await.unwrap();
        SqlxTenantRepository::new()
            .save(&mut scope, &tenant)
            .await
            .unwrap();
        scope.commit().await.unwrap();
    }
    // Insert rows as each tenant (WITH CHECK forces the matching scope).
    {
        let mut tx = pool.begin().await.unwrap();
        sqlx::query("SELECT set_config('sanvi.tenant_id', $1, true)")
            .bind(a.as_i64().to_string())
            .execute(&mut *tx)
            .await
            .unwrap();
        sqlx::query("INSERT INTO tenancy.tenant_settings (tenant_id, key, value) VALUES ($1, 'k', '1'::jsonb)")
            .bind(a.as_i64())
            .execute(&mut *tx)
            .await
            .unwrap();
        tx.commit().await.unwrap();
    }
    {
        let mut tx = pool.begin().await.unwrap();
        sqlx::query("SELECT set_config('sanvi.tenant_id', $1, true)")
            .bind(b.as_i64().to_string())
            .execute(&mut *tx)
            .await
            .unwrap();
        sqlx::query("INSERT INTO tenancy.tenant_settings (tenant_id, key, value) VALUES ($1, 'k', '2'::jsonb)")
            .bind(b.as_i64())
            .execute(&mut *tx)
            .await
            .unwrap();
        tx.commit().await.unwrap();
    }

    // As tenant A: see only A's row.
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("SELECT set_config('sanvi.tenant_id', $1, true)")
        .bind(a.as_i64().to_string())
        .execute(&mut *tx)
        .await
        .unwrap();
    let visible: Vec<(String,)> =
        sqlx::query_as("SELECT key FROM tenancy.tenant_settings ORDER BY key")
            .fetch_all(&mut *tx)
            .await
            .unwrap();
    assert_eq!(visible.len(), 1);

    // Update B's row as A: nothing happens (row invisible).
    let updated = sqlx::query(
        "UPDATE tenancy.tenant_settings SET value = '\"x\"'::jsonb WHERE tenant_id = $1",
    )
    .bind(b.as_i64())
    .execute(&mut *tx)
    .await
    .unwrap()
    .rows_affected();
    assert_eq!(updated, 0);

    // Delete B's row as A: nothing happens.
    let deleted = sqlx::query("DELETE FROM tenancy.tenant_settings WHERE tenant_id = $1")
        .bind(b.as_i64())
        .execute(&mut *tx)
        .await
        .unwrap()
        .rows_affected();
    assert_eq!(deleted, 0);

    // Insert as A with B's tenant_id: rejected by the WITH CHECK policy.
    let insert = sqlx::query(
        "INSERT INTO tenancy.tenant_settings (tenant_id, key, value) VALUES ($1, 'k2', '1'::jsonb)",
    )
    .bind(b.as_i64())
    .execute(&mut *tx)
    .await;
    assert!(
        insert.is_err(),
        "WITH CHECK must reject cross-tenant inserts"
    );
    tx.rollback().await.unwrap();
}

#[tokio::test]
async fn purge_sweep_requests_purge_only_once_for_due_archived_tenants() {
    let (pool, _owner, _admin, _node) = prepare().await;
    let raw_ids = raw_ids();
    let tx_manager = TxManager::new(pool.clone(), raw_ids.clone());
    let tenants: Arc<dyn TenantRepository> = Arc::new(SqlxTenantRepository::new());
    let events: Arc<dyn EventPublisher> = Arc::new(OutboxEventPublisher::new(raw_ids));
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);

    // Create + activate + archive a tenant directly through the aggregate.
    let id = TenantId::try_from(401_i64).unwrap();
    let (mut tenant, _) = sanvi_tenancy::Tenant::provision(
        id,
        sanvi_tenancy::TenantSlug::parse("sweepme").unwrap(),
        sanvi_tenancy::TenantName::parse("Sweep Me").unwrap(),
        sanvi_tenancy::DataRegion::Us,
        sanvi_tenancy::Locale::default(),
        chrono::Utc::now(),
    );
    tenant.activate(chrono::Utc::now()).unwrap();
    tenant.archive(chrono::Utc::now()).unwrap();
    {
        let mut scope = tx_manager
            .begin_unscoped("test_setup", "test")
            .await
            .unwrap();
        tenants.save(&mut scope, &tenant).await.unwrap();
        scope.commit().await.unwrap();
    }
    // Backdate the archive so the retention window has passed.
    sqlx::query(
        "UPDATE tenancy.tenants SET archived_at = now() - interval '31 days' WHERE id = $1",
    )
    .bind(id.as_i64())
    .execute(&pool)
    .await
    .unwrap();

    let sweep = PurgeSweep::new(tx_manager.clone(), tenants.clone(), events, clock, 30);
    let first = sweep.execute(100).await.unwrap();
    assert_eq!(first, 1);

    let purge_events: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM platform.outbox WHERE event_type = 'tenancy.tenant_purge_requested'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(purge_events, 1);

    // Idempotent: a second sweep finds nothing new to request.
    let second = sweep.execute(100).await.unwrap();
    assert_eq!(second, 0);
    let purge_events: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM platform.outbox WHERE event_type = 'tenancy.tenant_purge_requested'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(purge_events, 1);
}

#[tokio::test]
async fn list_tenants_paginates_by_cursor_and_filters_by_status() {
    let (pool, _owner, _admin, _node) = prepare().await;
    let raw_ids = raw_ids();
    let tx_manager = TxManager::new(pool.clone(), raw_ids.clone());
    let tenants: Arc<dyn TenantRepository> = Arc::new(SqlxTenantRepository::new());
    let slugs: Arc<dyn SlugReservationPort> = Arc::new(SqlxSlugReservationPort::new());
    let events: Arc<dyn EventPublisher> = Arc::new(OutboxEventPublisher::new(raw_ids.clone()));
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let provision = ProvisionTenant::new(
        tx_manager.clone(),
        tenants.clone(),
        slugs,
        events,
        Arc::new(SystemIdGenerator::new(raw_ids)),
        clock,
    );

    for slug in ["lista", "listb", "listc"] {
        provision.execute(command(slug)).await.unwrap();
    }

    let list = ListTenants::new(tx_manager, tenants);
    let page = list.execute(None, 2, None).await.unwrap();
    assert_eq!(page.items.len(), 2);
    let cursor = page.next_cursor.expect("more pages expected");
    let page2 = list
        .execute(Some(cursor.parse().unwrap()), 2, None)
        .await
        .unwrap();
    assert_eq!(page2.items.len(), 1);
    assert!(page2.next_cursor.is_none());

    let status_page = list
        .execute(None, 10, Some(sanvi_tenancy::TenantStatus::Provisioning))
        .await
        .unwrap();
    assert_eq!(status_page.items.len(), 3);
}

#[tokio::test]
async fn suspension_reason_roundtrips_through_storage() {
    let (pool, _owner, _admin, _node) = prepare().await;
    let raw_ids = raw_ids();
    let tx_manager = TxManager::new(pool.clone(), raw_ids.clone());
    let tenants: Arc<dyn TenantRepository> = Arc::new(SqlxTenantRepository::new());
    let slugs: Arc<dyn SlugReservationPort> = Arc::new(SqlxSlugReservationPort::new());
    let events: Arc<dyn EventPublisher> = Arc::new(OutboxEventPublisher::new(raw_ids.clone()));
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);

    let provision = ProvisionTenant::new(
        tx_manager.clone(),
        tenants.clone(),
        slugs,
        events.clone(),
        Arc::new(SystemIdGenerator::new(raw_ids)),
        clock.clone(),
    );
    let view = provision.execute(command("suspendme")).await.unwrap();
    let id: TenantId = view.id.parse().unwrap();

    let activate = ActivateTenant::new(
        tx_manager.clone(),
        tenants.clone(),
        events.clone(),
        clock.clone(),
    );
    activate.execute(id).await.unwrap();

    let suspend = sanvi_tenancy::application::SuspendTenant::new(
        tx_manager.clone(),
        tenants.clone(),
        events,
        clock,
    );
    let view = suspend
        .execute(id, SuspensionReason::Billing)
        .await
        .unwrap();
    assert_eq!(view.status, "suspended");
    assert_eq!(view.suspended_reason.as_deref(), Some("billing"));

    // Round-trip through the repository.
    let mut scope = tx_manager
        .begin_unscoped("test_read", "test")
        .await
        .unwrap();
    let tenant = tenants.find_by_id(&mut scope, id).await.unwrap().unwrap();
    assert_eq!(tenant.suspended_reason(), Some(SuspensionReason::Billing));
    scope.commit().await.unwrap();
}
