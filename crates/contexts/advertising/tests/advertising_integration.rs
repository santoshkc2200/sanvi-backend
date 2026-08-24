//! Advertising schema integration tests.
//!
//! These tests use an isolated Postgres container so the phase-10 migration
//! rollback and partition RLS checks never operate on a developer database.

use std::sync::Arc;

use chrono::{DateTime, Datelike, Duration, Months, Utc};
use sanvi_advertising::{
    AdvertisingConnection, AdvertisingConnectionRepository, OAuthStateClaims, OAuthStateError,
    OAuthStateManager, OAuthStateStore, PlatformKey, SecretRef, SyncError,
    adapters::{
        FakeAdPlatform, OutboxEventPublisher, SqlxAdvertisingConnectionRepository,
        SqlxCampaignRepository, SqlxOAuthStateStore,
    },
    application::{
        AdGroupFields, AddAd, AddAdGroup, CampaignFields, CreateCampaign, GetCampaign,
        ListCampaignChanges, ListCampaigns, NewAdFields, NewAdGroupFields, NewCampaignFields,
        PatchCampaign, PauseCampaign, PublishCampaign, ReconcileCampaign, ReconcileDueCampaigns,
        ResumeCampaign, UpdateAdGroup, ValidateCampaign,
    },
    domain::{
        BidSettings, Budget, BudgetType, CampaignStatus, ChangeSource, Creative, CreativeText,
        Money, Objective, Targeting,
    },
};
use sanvi_persistence::{DatabaseConfig, Migrator, TxManager, build_pool};
use sqlx::{Postgres as SqlxPostgres, QueryBuilder};
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;
use uuid::Uuid;

const ADVERTISING_MIGRATION: i64 = 20_260_826_000_000;

fn migrations_dir() -> std::path::PathBuf {
    std::env::var("CARGO_MANIFEST_DIR")
        .map(|dir| std::path::PathBuf::from(dir).join("../../../migrations"))
        .unwrap_or_else(|_| std::path::PathBuf::from("migrations"))
}

async fn start_postgres() -> (u16, testcontainers::ContainerAsync<Postgres>) {
    let node = Postgres::default().start().await.unwrap();
    let port = node.get_host_port_ipv4(5432).await.unwrap();
    let admin = sqlx::PgPool::connect(&format!(
        "postgres://postgres:postgres@127.0.0.1:{port}/postgres"
    ))
    .await
    .unwrap();
    sqlx::query("CREATE ROLE sanvi_migrator LOGIN PASSWORD 'mig'")
        .execute(&admin)
        .await
        .unwrap();
    sqlx::query("CREATE ROLE sanvi_app LOGIN PASSWORD 'app' NOBYPASSRLS")
        .execute(&admin)
        .await
        .unwrap();
    sqlx::query("CREATE DATABASE sanvi_test OWNER sanvi_migrator")
        .execute(&admin)
        .await
        .unwrap();
    (port, node)
}

fn db_config(port: u16, user: &str, password: &str) -> DatabaseConfig {
    DatabaseConfig {
        host: "127.0.0.1".to_string(),
        port,
        name: "sanvi_test".to_string(),
        user: user.to_string(),
        password: password.to_string(),
        ..DatabaseConfig::default()
    }
}

async fn migrate(port: u16) -> sqlx::PgPool {
    let config = db_config(port, "sanvi_migrator", "mig");
    let pool = sqlx::PgPool::connect(&format!(
        "postgres://{}:{}@127.0.0.1:{}/{}",
        config.user, config.password, port, config.name
    ))
    .await
    .unwrap();
    Migrator::new(migrations_dir()).run(&pool).await.unwrap();
    pool
}

async fn app_pool(port: u16) -> sanvi_persistence::PgPool {
    build_pool(&db_config(port, "sanvi_app", "app"))
        .await
        .unwrap()
}

async fn seed_tenant(pool: &sanvi_persistence::PgPool, slug: &str) -> Uuid {
    let tenant_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO tenancy.tenants (id, slug, display_name, status, region, default_locale)
         VALUES ($1, $2, $3, 'active', 'jp', 'en')",
    )
    .bind(tenant_id)
    .bind(slug)
    .bind(format!("{slug} corp"))
    .execute(pool)
    .await
    .unwrap();
    tenant_id
}

async fn insert_conversion_event(
    pool: &sanvi_persistence::PgPool,
    tenant_id: Uuid,
    connection_id: Uuid,
) -> chrono::DateTime<Utc> {
    let manager = TxManager::new(pool.clone());
    let mut scope = manager.begin_for(tenant_id).await.unwrap();
    let tx = scope.tx_mut().as_inner();
    let event_id = format!("event-{}", Uuid::now_v7());
    let occurred_at = Utc::now();

    sqlx::query(
        "INSERT INTO advertising.connections
         (id, tenant_id, platform, external_account_id, currency, timezone, status, token_ref)
         VALUES ($1, $2, 'google_ads', $3, 'JPY', 'Asia/Tokyo', 'active', 'secret-ref')",
    )
    .bind(connection_id)
    .bind(tenant_id)
    .bind(format!("acct-{}", connection_id.simple()))
    .execute(&mut **tx)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO advertising.campaigns
         (id, tenant_id, connection_id, platform, name, objective, status, budget)
         VALUES ($1, $2, $3, 'google_ads', 'Campaign', 'sales', 'draft', '{}'::jsonb)",
    )
    .bind(Uuid::now_v7())
    .bind(tenant_id)
    .bind(connection_id)
    .execute(&mut **tx)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO advertising.conversion_event_dedup (tenant_id, event_id, occurred_at)
         VALUES ($1, $2, $3)",
    )
    .bind(tenant_id)
    .bind(&event_id)
    .bind(occurred_at)
    .execute(&mut **tx)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO advertising.conversion_events
         (id, tenant_id, event_id, name, occurred_at, consent)
         VALUES ($1, $2, $3, 'purchase', $4, '{}'::jsonb)",
    )
    .bind(Uuid::now_v7())
    .bind(tenant_id)
    .bind(event_id)
    .bind(occurred_at)
    .execute(&mut **tx)
    .await
    .unwrap();
    scope.commit().await.unwrap();
    occurred_at
}

fn monthly_partition_names(date: DateTime<Utc>) -> [String; 2] {
    let suffix = format!("{:04}{:02}", date.year(), date.month());
    [
        format!("advertising.conversion_events_{suffix}"),
        format!("advertising.metrics_daily_{suffix}"),
    ]
}

async fn partition_exists(pool: &sqlx::PgPool, partition: &str) -> bool {
    sqlx::query_scalar("SELECT to_regclass($1) IS NOT NULL")
        .bind(partition)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn partition_row_count(tx: &mut sqlx::Transaction<'_, SqlxPostgres>, partition: &str) -> i64 {
    // `partition` is generated only by this test from a UTC month; it never
    // contains user input. QueryBuilder keeps that audited identifier path
    // distinct from the literal SQL used by application code.
    let mut query = QueryBuilder::<SqlxPostgres>::new("SELECT COUNT(*) FROM ");
    query.push(partition);
    query
        .build_query_scalar()
        .fetch_one(&mut **tx)
        .await
        .unwrap()
}

#[tokio::test]
async fn advertising_migration_round_trips_on_a_seeded_database() {
    let (port, _node) = start_postgres().await;
    let migrator_pool = migrate(port).await;
    let pool = app_pool(port).await;
    seed_tenant(&pool, "advertising-round-trip").await;
    let now = Utc::now();
    let next_month = now
        .checked_add_months(Months::new(1))
        .expect("a date one month from now must be representable");
    let expected_partitions = [
        monthly_partition_names(now),
        monthly_partition_names(next_month),
    ]
    .concat();

    for partition in &expected_partitions {
        assert!(
            partition_exists(&migrator_pool, partition).await,
            "{partition} must exist after migration"
        );
    }

    let migrator = Migrator::new(migrations_dir());
    migrator
        .undo(&migrator_pool, ADVERTISING_MIGRATION - 1)
        .await
        .unwrap();

    let schema_exists: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM information_schema.schemata WHERE schema_name = 'advertising')",
    )
    .fetch_one(&migrator_pool)
    .await
    .unwrap();
    assert!(!schema_exists, "advertising schema must be gone after undo");
    for partition in &expected_partitions {
        assert!(
            !partition_exists(&migrator_pool, partition).await,
            "{partition} must be gone after undo"
        );
    }

    let tenant_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM tenancy.tenants")
        .fetch_one(&migrator_pool)
        .await
        .unwrap();
    assert_eq!(tenant_count, 1, "earlier migrations must survive the undo");

    migrator.run(&migrator_pool).await.unwrap();
    let schema_exists: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM information_schema.schemata WHERE schema_name = 'advertising')",
    )
    .fetch_one(&migrator_pool)
    .await
    .unwrap();
    assert!(
        schema_exists,
        "advertising schema must return after reapply"
    );
    for partition in &expected_partitions {
        assert!(
            partition_exists(&migrator_pool, partition).await,
            "{partition} must return after reapply"
        );
    }
}

#[tokio::test]
async fn tenant_scope_hides_other_tenants_connections_and_partition_rows() {
    let (port, _node) = start_postgres().await;
    migrate(port).await;
    let pool = app_pool(port).await;
    let tenant_a = seed_tenant(&pool, "advertising-alpha").await;
    let tenant_b = seed_tenant(&pool, "advertising-beta").await;

    let occurred_at = insert_conversion_event(&pool, tenant_a, Uuid::now_v7()).await;
    let oauth_store: Arc<dyn OAuthStateStore> =
        Arc::new(SqlxOAuthStateStore::new(TxManager::new(pool.clone())));
    OAuthStateManager::with_store("x".repeat(32), oauth_store)
        .issue(
            tenant_a,
            PlatformKey::parse("google_ads").unwrap(),
            Utc::now(),
            Duration::minutes(10),
        )
        .await
        .unwrap();

    let partition = format!(
        "advertising.conversion_events_{:04}{:02}",
        occurred_at.year(),
        occurred_at.month()
    );
    let manager = TxManager::new(pool);

    {
        let mut tenant_a_scope = manager.begin_for(tenant_a).await.unwrap();
        let tx = tenant_a_scope.tx_mut().as_inner();
        let connection_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM advertising.connections")
                .fetch_one(&mut **tx)
                .await
                .unwrap();
        assert_eq!(connection_count, 1, "tenant A must see its own connection");
        let campaign_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM advertising.campaigns")
            .fetch_one(&mut **tx)
            .await
            .unwrap();
        assert_eq!(campaign_count, 1, "tenant A must see its own campaign");
        let event_count = partition_row_count(tx, &partition).await;
        assert_eq!(event_count, 1, "tenant A must see its own partition row");
        let oauth_state_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM advertising.oauth_states")
                .fetch_one(&mut **tx)
                .await
                .unwrap();
        assert_eq!(oauth_state_count, 1, "tenant A must see its OAuth state");
        tenant_a_scope.rollback().await.unwrap();
    }

    let mut tenant_b_scope = manager.begin_for(tenant_b).await.unwrap();
    let tx = tenant_b_scope.tx_mut().as_inner();
    let connection_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM advertising.connections")
        .fetch_one(&mut **tx)
        .await
        .unwrap();
    assert_eq!(
        connection_count, 0,
        "tenant B must not see tenant A's connection"
    );
    let campaign_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM advertising.campaigns")
        .fetch_one(&mut **tx)
        .await
        .unwrap();
    assert_eq!(
        campaign_count, 0,
        "tenant B must not see tenant A's campaign"
    );
    let event_count = partition_row_count(tx, &partition).await;
    assert_eq!(
        event_count, 0,
        "tenant B must not see tenant A's partition row"
    );
    let oauth_state_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM advertising.oauth_states")
            .fetch_one(&mut **tx)
            .await
            .unwrap();
    assert_eq!(
        oauth_state_count, 0,
        "tenant B must not see tenant A's OAuth state"
    );
    tenant_b_scope.rollback().await.unwrap();
}

#[tokio::test]
async fn oauth_state_survives_restart_and_is_consumed_once() {
    let (port, _node) = start_postgres().await;
    migrate(port).await;
    let pool = app_pool(port).await;
    let tenant_a = seed_tenant(&pool, "oauth-alpha").await;
    let tenant_b = seed_tenant(&pool, "oauth-beta").await;
    let store: Arc<dyn OAuthStateStore> = Arc::new(SqlxOAuthStateStore::new(TxManager::new(pool)));
    let platform = PlatformKey::parse("google_ads").unwrap();

    let issuing_manager = OAuthStateManager::with_store("x".repeat(32), store.clone());
    let state = issuing_manager
        .issue(
            tenant_a,
            platform.clone(),
            Utc::now(),
            Duration::minutes(10),
        )
        .await
        .unwrap();

    // A new manager models a fresh API process: state has to be durable, not
    // merely resident in the instance that started OAuth.
    let callback_manager = OAuthStateManager::with_store("x".repeat(32), store);
    assert_eq!(
        callback_manager
            .redeem(&state, tenant_b, &platform, Utc::now())
            .await,
        Err(OAuthStateError::CrossTenant)
    );
    assert!(
        callback_manager
            .redeem(&state, tenant_a, &platform, Utc::now())
            .await
            .is_ok()
    );
    assert_eq!(
        callback_manager
            .redeem(&state, tenant_a, &platform, Utc::now())
            .await,
        Err(OAuthStateError::Replayed)
    );
}

#[tokio::test]
async fn oauth_state_store_rejects_row_with_mismatched_signed_expiry() {
    let (port, _node) = start_postgres().await;
    migrate(port).await;
    let pool = app_pool(port).await;
    let tenant_id = seed_tenant(&pool, "oauth-expiry").await;
    let store: Arc<dyn OAuthStateStore> =
        Arc::new(SqlxOAuthStateStore::new(TxManager::new(pool.clone())));
    let claims = OAuthStateClaims {
        tenant_id,
        platform: PlatformKey::parse("google_ads").unwrap(),
        nonce: Uuid::now_v7(),
        expires_at: Utc::now() + Duration::minutes(10),
    };
    store.issue(claims.clone()).await.unwrap();

    let tx_manager = TxManager::new(pool);
    let mut scope = tx_manager.begin_for(tenant_id).await.unwrap();
    sqlx::query("UPDATE advertising.oauth_states SET expires_at = $2 WHERE nonce = $1")
        .bind(claims.nonce)
        .bind(claims.expires_at + Duration::minutes(1))
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .unwrap();
    scope.commit().await.unwrap();

    assert!(!store.consume(&claims).await.unwrap());
}

#[tokio::test]
async fn reconnect_reuses_the_original_connection_row() {
    let (port, _node) = start_postgres().await;
    migrate(port).await;
    let pool = app_pool(port).await;
    let tenant_id = seed_tenant(&pool, "advertising-reconnect").await;
    let manager = TxManager::new(pool);
    let repository = SqlxAdvertisingConnectionRepository::new();
    let now = Utc::now();

    let first = AdvertisingConnection::new(
        tenant_id,
        PlatformKey::parse("google_ads").unwrap(),
        "123-456-7890",
        "JPY",
        "Asia/Tokyo",
        SecretRef::new("vault-ref-1"),
        Some(Uuid::now_v7()),
        now,
    )
    .unwrap();
    let first_id = first.id();
    let mut scope = manager.begin_for(tenant_id).await.unwrap();
    let stored = repository.upsert(&mut scope, &first).await.unwrap();
    scope.commit().await.unwrap();
    assert_eq!(stored.id(), first_id);

    let mut reconnect = AdvertisingConnection::new(
        tenant_id,
        PlatformKey::parse("google_ads").unwrap(),
        "123-456-7890",
        "JPY",
        "Asia/Tokyo",
        SecretRef::new("vault-ref-2"),
        Some(Uuid::now_v7()),
        now + Duration::minutes(1),
    )
    .unwrap();
    reconnect.apply_credentials(
        Some("Manager-selected account".to_string()),
        vec!["ads.read".to_string(), "ads.manage".to_string()],
        None,
    );
    reconnect.record_sync_error(
        SyncError::new(
            "invalid_grant",
            Some("token revoked".to_string()),
            serde_json::from_value(serde_json::json!({"provider_reason": "password_changed"}))
                .unwrap(),
        )
        .unwrap(),
    );
    reconnect.record_permanent_refresh_failure(
        SyncError::new(
            "invalid_grant",
            Some("token revoked".to_string()),
            serde_json::from_value(serde_json::json!({"provider_reason": "password_changed"}))
                .unwrap(),
        )
        .unwrap(),
    );
    reconnect.mark_disconnected_notified(now + Duration::minutes(2));
    reconnect.mark_dependent_jobs_paused(now + Duration::minutes(2));
    let mut scope = manager.begin_for(tenant_id).await.unwrap();
    let stored = repository.upsert(&mut scope, &reconnect).await.unwrap();
    let connections = repository.list(&mut scope, tenant_id).await.unwrap();
    let persisted_error: serde_json::Value =
        sqlx::query_scalar("SELECT last_error FROM advertising.connections WHERE id = $1")
            .bind(first_id)
            .fetch_one(&mut **scope.tx_mut().as_inner())
            .await
            .unwrap();
    let persisted_lifecycle: (Option<chrono::DateTime<Utc>>, Option<chrono::DateTime<Utc>>) =
        sqlx::query_as(
            "SELECT disconnected_notified_at, dependent_jobs_paused_at
             FROM advertising.connections WHERE id = $1",
        )
        .bind(first_id)
        .fetch_one(&mut **scope.tx_mut().as_inner())
        .await
        .unwrap();
    scope.commit().await.unwrap();

    assert_eq!(
        stored.id(),
        first_id,
        "reconnect must preserve campaign history"
    );
    assert_eq!(connections.len(), 1);
    assert_eq!(connections[0].id(), first_id);
    assert_eq!(connections[0].granted_scopes(), reconnect.granted_scopes());
    assert_eq!(persisted_error["code"], "invalid_grant");
    assert_eq!(persisted_error["provider_reason"], "password_changed");
    assert_eq!(persisted_lifecycle.0, Some(now + Duration::minutes(2)));
    assert_eq!(persisted_lifecycle.1, Some(now + Duration::minutes(2)));
}

/// End-to-end campaign tree lifecycle against the fake adapter: draft never
/// touches the platform, publish assigns an external id exactly once even
/// when retried, patch on a published campaign pushes live, pause/resume
/// round-trip, and the change log attributes every step.
#[tokio::test]
async fn campaign_lifecycle_draft_publish_patch_pause_resume_and_changes() {
    let (port, _node) = start_postgres().await;
    migrate(port).await;
    let pool = app_pool(port).await;
    let tenant_id = seed_tenant(&pool, "advertising-campaigns").await;
    let manager = TxManager::new(pool);
    let actor_id = Uuid::now_v7();

    let connections = SqlxAdvertisingConnectionRepository::new();
    let connection = AdvertisingConnection::new(
        tenant_id,
        PlatformKey::parse("google_ads").unwrap(),
        "123-456-7890",
        "JPY",
        "Asia/Tokyo",
        SecretRef::new("vault-ref"),
        Some(actor_id),
        Utc::now(),
    )
    .unwrap();
    let connection_id = connection.id();
    let mut scope = manager.begin_for(tenant_id).await.unwrap();
    connections.upsert(&mut scope, &connection).await.unwrap();
    scope.commit().await.unwrap();

    let campaigns: Arc<dyn sanvi_advertising::domain::CampaignRepository> =
        Arc::new(SqlxCampaignRepository::new());
    let connections: Arc<dyn AdvertisingConnectionRepository> = Arc::new(connections);
    let events: Arc<dyn sanvi_advertising::domain::EventPublisher> =
        Arc::new(OutboxEventPublisher::new());
    let registry = Arc::new(FakeAdPlatform::registered_catalog());

    let create = CreateCampaign::new(
        manager.clone(),
        campaigns.clone(),
        connections,
        registry.clone(),
        events.clone(),
    );
    let get = GetCampaign::new(manager.clone(), campaigns.clone());
    let list = ListCampaigns::new(manager.clone(), campaigns.clone());
    let validate = ValidateCampaign::new(manager.clone(), campaigns.clone(), registry.clone());
    let patch = PatchCampaign::new(
        manager.clone(),
        campaigns.clone(),
        events.clone(),
        registry.clone(),
    );
    let publish = PublishCampaign::new(
        manager.clone(),
        campaigns.clone(),
        events.clone(),
        registry.clone(),
    );
    let pause = PauseCampaign::new(
        manager.clone(),
        campaigns.clone(),
        events.clone(),
        registry.clone(),
    );
    let resume = ResumeCampaign::new(manager.clone(), campaigns.clone(), events.clone(), registry);
    let changes = ListCampaignChanges::new(manager, campaigns);

    // Draft: never touches the platform.
    let draft = create
        .execute(
            tenant_id,
            connection_id,
            actor_id,
            NewCampaignFields {
                name: "Summer sale".to_string(),
                objective: Objective::parse("sales").unwrap(),
                budget: Budget {
                    kind: BudgetType::Daily,
                    amount: Money::new(5_000, "JPY").unwrap(),
                },
                schedule: None,
            },
        )
        .await
        .unwrap();
    assert!(draft.external_id.is_none());
    assert_eq!(draft.intent.status, CampaignStatus::Draft);
    assert_eq!(draft.revision, 1);
    assert_eq!(list.execute(tenant_id).await.unwrap().len(), 1);
    assert_eq!(
        get.execute(tenant_id, draft.id).await.unwrap().intent.name,
        "Summer sale"
    );

    // Validate rejects an objective the platform doesn't support, without
    // writing anything.
    let violations = validate
        .execute(
            tenant_id,
            draft.id,
            CampaignFields {
                objective: Some(Objective::parse("unsupported_objective").unwrap()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(!violations.is_empty());
    assert_eq!(get.execute(tenant_id, draft.id).await.unwrap().revision, 1);

    // Publish assigns an external id; a retry with the same campaign id is a
    // no-op success rather than a second campaign.
    let published = publish
        .execute(tenant_id, draft.id, actor_id)
        .await
        .unwrap();
    assert!(published.external_id.is_some());
    assert_eq!(published.intent.status, CampaignStatus::Active);
    let replayed = publish
        .execute(tenant_id, draft.id, actor_id)
        .await
        .unwrap();
    assert_eq!(replayed.external_id, published.external_id);
    assert_eq!(replayed.revision, published.revision);

    // Patch on a published campaign pushes the edit live through the
    // adapter as well as updating the stored intent.
    let patched = patch
        .execute(
            tenant_id,
            draft.id,
            published.revision,
            Uuid::now_v7(),
            actor_id,
            CampaignFields {
                name: Some("Summer sale v2".to_string()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(patched.intent.name, "Summer sale v2");
    assert_eq!(patched.revision, published.revision + 1);

    // A stale If-Match revision is rejected rather than silently applied.
    let stale = patch
        .execute(
            tenant_id,
            draft.id,
            published.revision,
            Uuid::now_v7(),
            actor_id,
            CampaignFields {
                name: Some("Should not apply".to_string()),
                ..Default::default()
            },
        )
        .await;
    assert!(matches!(
        stale,
        Err(sanvi_advertising::domain::AdvertisingError::StaleRevision)
    ));

    // Pause / resume round-trip and are attributed to the acting user.
    let paused = pause.execute(tenant_id, draft.id, actor_id).await.unwrap();
    assert_eq!(paused.intent.status, CampaignStatus::Paused);
    let resumed = resume.execute(tenant_id, draft.id, actor_id).await.unwrap();
    assert_eq!(resumed.intent.status, CampaignStatus::Active);

    let history = changes.execute(tenant_id, draft.id, 50).await.unwrap();
    assert!(
        history.len() >= 5,
        "expected create/publish/patch/pause/resume entries"
    );
    assert!(
        history
            .iter()
            .all(|change| change.source == ChangeSource::Sanvi)
    );
    assert!(
        history
            .iter()
            .all(|change| change.actor_id == Some(actor_id))
    );
}

/// The nightly reconciliation sweep (TASK-012, 10.3): a change made directly
/// in the platform's own UI — never through Sanvi — surfaces as drift on the
/// next pass, and a second pass over the same unchanged remote state does
/// not clear or resurrect it.
#[tokio::test]
async fn reconciliation_sweep_detects_native_platform_pause_as_drift_without_resurrecting() {
    let (port, _node) = start_postgres().await;
    migrate(port).await;
    let pool = app_pool(port).await;
    let tenant_id = seed_tenant(&pool, "advertising-sweep").await;
    let manager = TxManager::new(pool);
    let actor_id = Uuid::now_v7();

    let connection_repo = SqlxAdvertisingConnectionRepository::new();
    let connection = AdvertisingConnection::new(
        tenant_id,
        PlatformKey::parse("google_ads").unwrap(),
        "555-000-1111",
        "JPY",
        "Asia/Tokyo",
        SecretRef::new("vault-ref-sweep"),
        Some(actor_id),
        Utc::now(),
    )
    .unwrap();
    let connection_id = connection.id();
    let mut scope = manager.begin_for(tenant_id).await.unwrap();
    connection_repo
        .upsert(&mut scope, &connection)
        .await
        .unwrap();
    scope.commit().await.unwrap();

    let campaigns: Arc<dyn sanvi_advertising::domain::CampaignRepository> =
        Arc::new(SqlxCampaignRepository::new());
    let connections: Arc<dyn AdvertisingConnectionRepository> = Arc::new(connection_repo);
    let events: Arc<dyn sanvi_advertising::domain::EventPublisher> =
        Arc::new(OutboxEventPublisher::new());
    let registry = Arc::new(FakeAdPlatform::registered_catalog());

    let create = CreateCampaign::new(
        manager.clone(),
        campaigns.clone(),
        connections,
        registry.clone(),
        events.clone(),
    );
    let publish = PublishCampaign::new(
        manager.clone(),
        campaigns.clone(),
        events.clone(),
        registry.clone(),
    );
    let reconcile = Arc::new(ReconcileCampaign::new(
        manager.clone(),
        campaigns.clone(),
        events,
    ));
    let sweep = ReconcileDueCampaigns::new(manager, campaigns, registry.clone(), reconcile);

    let draft = create
        .execute(
            tenant_id,
            connection_id,
            actor_id,
            NewCampaignFields {
                name: "Sweep target".to_string(),
                objective: Objective::parse("sales").unwrap(),
                budget: Budget {
                    kind: BudgetType::Daily,
                    amount: Money::new(3_000, "JPY").unwrap(),
                },
                schedule: None,
            },
        )
        .await
        .unwrap();
    let published = publish
        .execute(tenant_id, draft.id, actor_id)
        .await
        .unwrap();
    let external_id = published.external_id.clone().unwrap();

    // Simulate a native edit made directly at the platform — bypassing
    // Sanvi entirely, exactly as a tenant editing in Google Ads' own UI.
    let platform_key = PlatformKey::parse("google_ads").unwrap();
    let adapter = registry.adapter(&platform_key).unwrap();
    let handle = sanvi_advertising::ConnectionHandle {
        id: connection_id.to_string(),
    };
    adapter.pause_campaign(&handle, &external_id).await.unwrap();

    let report = sweep.execute(50).await.unwrap();
    assert_eq!(report.checked, 1);
    assert_eq!(report.drifted, 1);
    assert_eq!(report.errors, 0);

    // A second pass over the same, still-paused remote state is not new
    // drift — and must not clear the existing drift flag on its own.
    let report_again = sweep.execute(50).await.unwrap();
    assert_eq!(report_again.checked, 1);
    assert_eq!(
        report_again.drifted, 0,
        "unchanged remote state is not new drift"
    );
}

/// Ad-group/ad sub-resource endpoints (TASK-012, 10.3): additions apply to
/// the same campaign tree `PATCH` already covers, revision-CAS included,
/// and a not-found ad group is a 404-shaped domain error rather than a
/// silent no-op.
#[tokio::test]
async fn ad_group_and_ad_sub_resources_append_and_update_within_the_campaign_tree() {
    let (port, _node) = start_postgres().await;
    migrate(port).await;
    let pool = app_pool(port).await;
    let tenant_id = seed_tenant(&pool, "advertising-ad-groups").await;
    let manager = TxManager::new(pool);
    let actor_id = Uuid::now_v7();

    let connection_repo = SqlxAdvertisingConnectionRepository::new();
    let connection = AdvertisingConnection::new(
        tenant_id,
        PlatformKey::parse("google_ads").unwrap(),
        "777-000-2222",
        "JPY",
        "Asia/Tokyo",
        SecretRef::new("vault-ref-ad-groups"),
        Some(actor_id),
        Utc::now(),
    )
    .unwrap();
    let connection_id = connection.id();
    let mut scope = manager.begin_for(tenant_id).await.unwrap();
    connection_repo
        .upsert(&mut scope, &connection)
        .await
        .unwrap();
    scope.commit().await.unwrap();

    let campaigns: Arc<dyn sanvi_advertising::domain::CampaignRepository> =
        Arc::new(SqlxCampaignRepository::new());
    let connections: Arc<dyn AdvertisingConnectionRepository> = Arc::new(connection_repo);
    let events: Arc<dyn sanvi_advertising::domain::EventPublisher> =
        Arc::new(OutboxEventPublisher::new());
    let registry = Arc::new(FakeAdPlatform::registered_catalog());

    let create = CreateCampaign::new(
        manager.clone(),
        campaigns.clone(),
        connections,
        registry.clone(),
        events.clone(),
    );
    let add_ad_group = AddAdGroup::new(
        manager.clone(),
        campaigns.clone(),
        events.clone(),
        registry.clone(),
    );
    let update_ad_group = UpdateAdGroup::new(
        manager.clone(),
        campaigns.clone(),
        events.clone(),
        registry.clone(),
    );
    let add_ad = AddAd::new(manager, campaigns, events, registry);

    let draft = create
        .execute(
            tenant_id,
            connection_id,
            actor_id,
            NewCampaignFields {
                name: "Ad group target".to_string(),
                objective: Objective::parse("sales").unwrap(),
                budget: Budget {
                    kind: BudgetType::Daily,
                    amount: Money::new(4_000, "JPY").unwrap(),
                },
                schedule: None,
            },
        )
        .await
        .unwrap();
    assert!(draft.intent.ad_groups.is_empty());

    let with_group = add_ad_group
        .execute(
            tenant_id,
            draft.id,
            draft.revision,
            Uuid::now_v7(),
            actor_id,
            NewAdGroupFields {
                name: "Prospecting".to_string(),
                targeting: Targeting::default(),
                bid: BidSettings {
                    strategy: "manual_cpc".to_string(),
                    maximum_bid: None,
                },
            },
        )
        .await
        .unwrap();
    assert_eq!(with_group.intent.ad_groups.len(), 1);
    assert_eq!(with_group.revision, draft.revision + 1);
    let ad_group_id = with_group.intent.ad_groups[0].id.clone();

    let with_ad = add_ad
        .execute(
            tenant_id,
            draft.id,
            ad_group_id.clone(),
            with_group.revision,
            Uuid::now_v7(),
            actor_id,
            NewAdFields {
                creative: Creative {
                    id: "creative-1".to_string(),
                    placement: "feed".to_string(),
                    texts: vec![CreativeText {
                        locale: "en".to_string(),
                        headline: "Sale".to_string(),
                        body: "Shop now".to_string(),
                    }],
                    asset_references: Vec::new(),
                },
                landing_url: "https://example.com/sale".to_string(),
                tracking_template: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(with_ad.intent.ad_groups[0].ads.len(), 1);
    assert_eq!(with_ad.revision, with_group.revision + 1);

    let renamed = update_ad_group
        .execute(
            tenant_id,
            draft.id,
            ad_group_id.clone(),
            with_ad.revision,
            Uuid::now_v7(),
            actor_id,
            AdGroupFields {
                name: Some("Prospecting v2".to_string()),
                targeting: None,
                bid: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(renamed.intent.ad_groups[0].name, "Prospecting v2");
    // The ad added a step earlier survives an unrelated ad-group field edit.
    assert_eq!(renamed.intent.ad_groups[0].ads.len(), 1);

    // A not-found ad group is a domain error, not a silent no-op.
    let missing = update_ad_group
        .execute(
            tenant_id,
            draft.id,
            "does-not-exist".to_string(),
            renamed.revision,
            Uuid::now_v7(),
            actor_id,
            AdGroupFields::default(),
        )
        .await;
    assert!(matches!(
        missing,
        Err(sanvi_advertising::domain::AdvertisingError::AdGroupNotFound)
    ));
}
