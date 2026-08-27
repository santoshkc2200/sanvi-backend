//! Advertising schema integration tests.
//!
//! These tests use an isolated Postgres database so the phase-10 migration
//! rollback and partition RLS checks never operate on a developer database.

use std::sync::Arc;

use chrono::{DateTime, Datelike, Duration, Months, Utc};
use sanvi_advertising::{
    AdvertisingConnection, AdvertisingConnectionRepository, OAuthStateClaims, OAuthStateError,
    OAuthStateManager, OAuthStateStore, PlatformKey, SecretRef, SyncError,
    adapters::{
        FakeAdPlatform, OutboxEventPublisher, SqlxAdvertisingConnectionRepository,
        SqlxCampaignRepository, SqlxCreativeRepository, SqlxOAuthStateStore,
    },
    application::{
        AdFields, AdGroupFields, AddAd, AddAdGroup, CampaignFields, CreateCampaign,
        CreateConnection, CreateCreative, DeleteCreative, GetCampaign, GetCreative,
        GetCreativePreviews, ListCampaignChanges, ListCampaigns, ListCreatives, NewAdFields,
        NewAdGroupFields, NewCampaignFields, NewCreativeFields, PatchCampaign, PauseCampaign,
        PublishCampaign, ReconcileCampaign, ReconcileDueCampaigns, ResumeCampaign, UpdateAd,
        UpdateAdGroup, ValidateCampaign,
    },
    domain::{
        AssetMetadata, BidSettings, Budget, BudgetType, CampaignStatus, ChangeSource, Creative,
        CreativeRepository, CreativeText, Money, Objective, PlatformAccount, StoredCreative,
        Targeting, TargetingDimension, TenantEnvelopeCrypto,
    },
};
use sanvi_persistence::{Migrator, TxManager};
use sqlx::{Postgres as SqlxPostgres, QueryBuilder};
use uuid::Uuid;

const ADVERTISING_MIGRATION: i64 = 20_260_826_000_000;
const TOKEN_VAULT_MIGRATION: i64 = 20_260_826_050_000;

fn migrations_dir() -> std::path::PathBuf {
    std::env::var("CARGO_MANIFEST_DIR")
        .map(|dir| std::path::PathBuf::from(dir).join("../../../migrations"))
        .unwrap_or_else(|_| std::path::PathBuf::from("migrations"))
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
        "INSERT INTO advertising.conversion_event_dedup
           (tenant_id, event_id, dedupe_key, occurred_at)
         VALUES ($1, $2, $2, $3)",
    )
    .bind(tenant_id)
    .bind(&event_id)
    .bind(occurred_at)
    .execute(&mut **tx)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO advertising.conversion_events
         (id, tenant_id, event_id, dedupe_key, name, occurred_at, consent)
         VALUES ($1, $2, $3, $3, 'purchase', $4, '{}'::jsonb)",
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
    let db = sanvi_testsupport::test_db().await;
    let migrator_pool = db.migrator_pool().await;
    let pool = db.app_pool();
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
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
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
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
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
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
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
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
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

#[tokio::test]
async fn selecting_a_reconnected_account_reuses_the_original_row_and_token() {
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
    let tenant_id = seed_tenant(&pool, "advertising-reconnect-selection").await;
    let manager = TxManager::new(pool);
    let repository: Arc<dyn AdvertisingConnectionRepository> =
        Arc::new(SqlxAdvertisingConnectionRepository::new());
    let platform = PlatformKey::parse("google_ads").unwrap();
    let account = PlatformAccount {
        external_id: "123-456-7890".to_string(),
        display_name: "Reconnected account".to_string(),
    };

    let existing = AdvertisingConnection::new(
        tenant_id,
        platform.clone(),
        account.external_id.clone(),
        "JPY",
        "Asia/Tokyo",
        SecretRef::new("existing-token"),
        Some(Uuid::now_v7()),
        Utc::now(),
    )
    .unwrap();
    let existing_id = existing.id();
    let pending_id = Uuid::now_v7();
    let pending = AdvertisingConnection::new_pending(
        pending_id,
        tenant_id,
        platform,
        SecretRef::new(pending_id.to_string()),
        vec!["ads.read".to_string()],
        None,
        vec![account.clone()],
        Some(Uuid::now_v7()),
        Utc::now(),
    )
    .unwrap();

    let mut scope = manager.begin_for(tenant_id).await.unwrap();
    repository.upsert(&mut scope, &existing).await.unwrap();
    repository.upsert(&mut scope, &pending).await.unwrap();
    sqlx::query(
        "INSERT INTO advertising.connection_tokens (connection_id, tenant_id, ciphertext)
         VALUES ($1, $2, $3), ($4, $2, $5)",
    )
    .bind(existing_id)
    .bind(tenant_id)
    .bind(b"old-token".as_slice())
    .bind(pending_id)
    .bind(b"new-token".as_slice())
    .execute(&mut **scope.tx_mut().as_inner())
    .await
    .unwrap();
    scope.commit().await.unwrap();

    let create = CreateConnection::new(manager.clone(), repository.clone());
    let stored = create
        .execute(
            tenant_id,
            pending_id,
            &account.external_id,
            "JPY".to_string(),
            "Asia/Tokyo".to_string(),
        )
        .await
        .unwrap();

    assert_eq!(stored.id(), existing_id);
    let mut scope = manager.begin_for(tenant_id).await.unwrap();
    let connection_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM advertising.connections WHERE tenant_id = $1")
            .bind(tenant_id)
            .fetch_one(&mut **scope.tx_mut().as_inner())
            .await
            .unwrap();
    let token: Vec<u8> = sqlx::query_scalar(
        "SELECT ciphertext FROM advertising.connection_tokens WHERE tenant_id = $1 AND connection_id = $2",
    )
    .bind(tenant_id)
    .bind(existing_id)
    .fetch_one(&mut **scope.tx_mut().as_inner())
    .await
    .unwrap();
    let pending_token_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM advertising.connection_tokens WHERE tenant_id = $1 AND connection_id = $2",
    )
    .bind(tenant_id)
    .bind(pending_id)
    .fetch_one(&mut **scope.tx_mut().as_inner())
    .await
    .unwrap();
    scope.commit().await.unwrap();

    assert_eq!(connection_count, 1);
    assert_eq!(token, b"new-token");
    assert_eq!(pending_token_count, 0);
}

#[tokio::test]
async fn existing_tenant_key_is_usable_after_process_restart() {
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
    let tenant_id = seed_tenant(&pool, "advertising-tenant-key-restart").await;
    let manager = TxManager::new(pool);

    let first_process = sanvi_advertising::adapters::SqlxTenantEnvelopeCrypto::new(
        [7u8; 32],
        "test-kek".to_string(),
    );
    let mut scope = manager.begin_for(tenant_id).await.unwrap();
    first_process
        .encrypt_for_tenant(&mut scope, tenant_id, b"first token")
        .await
        .unwrap();
    scope.commit().await.unwrap();

    let restarted_process = sanvi_advertising::adapters::SqlxTenantEnvelopeCrypto::new(
        [7u8; 32],
        "test-kek".to_string(),
    );
    let mut scope = manager.begin_for(tenant_id).await.unwrap();
    restarted_process
        .encrypt_for_tenant(&mut scope, tenant_id, b"second token")
        .await
        .unwrap();
    scope.commit().await.unwrap();
}

#[tokio::test]
// Rollback must preserve RLS-protected rows between callback and account selection.
async fn rolling_back_pending_connections_converts_them_to_disconnected() {
    let db = sanvi_testsupport::test_db().await;
    let migrator_pool = db.migrator_pool().await;
    let pool = db.app_pool();
    let tenant_id = seed_tenant(&pool, "advertising-pending-rollback").await;
    let connection_id = Uuid::now_v7();
    let manager = TxManager::new(pool.clone());
    let mut scope = manager.begin_for(tenant_id).await.unwrap();
    sqlx::query(
        "INSERT INTO advertising.connections
         (id, tenant_id, platform, external_account_id, currency, timezone, status, token_ref,
          pending_accounts)
         VALUES ($1, $2, 'google_ads', $3, 'XXX', 'UTC', 'pending', $4, '[]'::jsonb)",
    )
    .bind(connection_id)
    .bind(tenant_id)
    .bind(format!("pending:{connection_id}"))
    .bind(connection_id.to_string())
    .execute(&mut **scope.tx_mut().as_inner())
    .await
    .unwrap();
    scope.commit().await.unwrap();

    Migrator::new(migrations_dir())
        .undo(&migrator_pool, TOKEN_VAULT_MIGRATION)
        .await
        .unwrap();

    let manager = TxManager::new(pool);
    let mut scope = manager.begin_for(tenant_id).await.unwrap();
    let status: String = sqlx::query_scalar(
        "SELECT status FROM advertising.connections WHERE tenant_id = $1 AND id = $2",
    )
    .bind(tenant_id)
    .bind(connection_id)
    .fetch_one(&mut **scope.tx_mut().as_inner())
    .await
    .unwrap();
    scope.commit().await.unwrap();
    assert_eq!(status, "disconnected");
}

/// End-to-end campaign tree lifecycle against the fake adapter: draft never
/// touches the platform, publish assigns an external id exactly once even
/// when retried, patch on a published campaign pushes live, pause/resume
/// round-trip, and the change log attributes every step.
#[tokio::test]
async fn campaign_lifecycle_draft_publish_patch_pause_resume_and_changes() {
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
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
    let creatives: Arc<dyn CreativeRepository> = Arc::new(SqlxCreativeRepository::new());
    let validate = ValidateCampaign::new(
        manager.clone(),
        campaigns.clone(),
        creatives.clone(),
        registry.clone(),
    );
    let patch = PatchCampaign::new(
        manager.clone(),
        campaigns.clone(),
        creatives.clone(),
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
            Uuid::now_v7(),
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
    let publish_mutation_id = Uuid::now_v7();
    let published = publish
        .execute(tenant_id, draft.id, publish_mutation_id, actor_id)
        .await
        .unwrap();
    assert!(published.external_id.is_some());
    assert_eq!(published.intent.status, CampaignStatus::Active);
    let replayed = publish
        .execute(tenant_id, draft.id, publish_mutation_id, actor_id)
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
    let paused = pause
        .execute(tenant_id, draft.id, Uuid::now_v7(), actor_id)
        .await
        .unwrap();
    assert_eq!(paused.intent.status, CampaignStatus::Paused);
    let resumed = resume
        .execute(tenant_id, draft.id, Uuid::now_v7(), actor_id)
        .await
        .unwrap();
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
    assert!(
        history.iter().all(|change| change.mutation_id.is_some()),
        "every client mutation must be traceable to its idempotency key"
    );
}

/// The nightly reconciliation sweep (TASK-012, 10.3): a change made directly
/// in the platform's own UI — never through Sanvi — surfaces as drift on the
/// next pass, and a second pass over the same unchanged remote state does
/// not clear or resurrect it.
#[tokio::test]
async fn reconciliation_sweep_detects_native_platform_pause_as_drift_without_resurrecting() {
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
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
            Uuid::now_v7(),
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
        .execute(tenant_id, draft.id, Uuid::now_v7(), actor_id)
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
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
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
    let creatives: Arc<dyn CreativeRepository> = Arc::new(SqlxCreativeRepository::new());
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
    let add_ad = AddAd::new(
        manager.clone(),
        campaigns,
        creatives.clone(),
        events,
        registry,
    );

    let draft = create
        .execute(
            tenant_id,
            connection_id,
            Uuid::now_v7(),
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

    let creative_id = Uuid::now_v7();
    let stored_creative = StoredCreative {
        id: creative_id,
        tenant_id,
        connection_id,
        platform: PlatformKey::parse("google_ads").unwrap(),
        external_id: None,
        creative: Creative {
            id: creative_id.to_string(),
            placement: "feed".to_string(),
            texts: vec![CreativeText {
                locale: "en".to_string(),
                headline: "Sale".to_string(),
                body: "Shop now".to_string(),
            }],
            asset_references: Vec::new(),
        },
        assets: Vec::new(),
        created_at: Utc::now(),
        updated_at: Utc::now(),
    };
    let mut scope = manager.begin_for(tenant_id).await.unwrap();
    creatives
        .insert(&mut scope, &stored_creative)
        .await
        .unwrap();
    scope.commit().await.unwrap();

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
                    id: creative_id.to_string(),
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

#[tokio::test]
async fn reconciliation_sweep_detects_natively_paused_meta_ad_set_as_drift() {
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
    let tenant_id = seed_tenant(&pool, "meta-drift-sweep").await;
    let manager = TxManager::new(pool);
    let actor_id = Uuid::now_v7();

    let connection_repo = SqlxAdvertisingConnectionRepository::new();
    let meta_key = PlatformKey::parse("meta").unwrap();
    let connection = AdvertisingConnection::new(
        tenant_id,
        meta_key.clone(),
        "act_9988776655",
        "JPY",
        "Asia/Tokyo",
        SecretRef::new("vault-ref-meta-sweep"),
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
    let add_ad_group = AddAdGroup::new(
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
    let sweep = ReconcileDueCampaigns::new(
        manager.clone(),
        campaigns.clone(),
        registry.clone(),
        reconcile,
    );

    // Create a Meta campaign
    let draft = create
        .execute(
            tenant_id,
            connection_id,
            Uuid::now_v7(),
            actor_id,
            NewCampaignFields {
                name: "Meta Retargeting".to_string(),
                objective: Objective::parse("sales").unwrap(),
                budget: Budget {
                    kind: BudgetType::Daily,
                    amount: Money::new(5000, "JPY").unwrap(),
                },
                schedule: None,
            },
        )
        .await
        .unwrap();

    // Add an Ad Set (ad group)
    let _with_group = add_ad_group
        .execute(
            tenant_id,
            draft.id,
            draft.revision,
            Uuid::now_v7(),
            actor_id,
            NewAdGroupFields {
                name: "Audience 18-35".to_string(),
                targeting: Targeting {
                    dimensions: std::collections::BTreeMap::from([(
                        TargetingDimension::parse("age").unwrap(),
                        vec!["18-35".to_string()],
                    )]),
                    extension_fields: std::collections::BTreeMap::new(),
                },
                bid: BidSettings {
                    strategy: "cpc".to_string(),
                    maximum_bid: None,
                },
            },
        )
        .await
        .unwrap();

    let published = publish
        .execute(tenant_id, draft.id, Uuid::now_v7(), actor_id)
        .await
        .unwrap();
    assert_eq!(published.intent.status, CampaignStatus::Active);

    // Simulate drift on Meta: the ad set was natively paused or removed in Meta Ads Manager
    let adapter = registry.adapter(&meta_key).unwrap();
    let conn_handle = sanvi_advertising::domain::ConnectionHandle {
        id: format!("act_{connection_id}"),
    };
    let mut remote = adapter
        .get_campaign(&conn_handle, published.external_id.as_deref().unwrap())
        .await
        .unwrap()
        .expect("published campaign exists in fake adapter");

    // Ad set removed on remote Meta platform
    remote.ad_groups.clear();
    adapter
        .update_campaign(&conn_handle, remote, Uuid::now_v7())
        .await
        .unwrap();

    // Run reconciliation sweep
    let report = sweep.execute(50).await.unwrap();
    assert_eq!(report.checked, 1);
    assert_eq!(report.drifted, 1);

    // Verify stored state is drifted on ad_groups
    let mut scope = manager.begin_for(tenant_id).await.unwrap();
    let stored = campaigns
        .find(&mut scope, tenant_id, draft.id)
        .await
        .unwrap()
        .unwrap();
    assert!(stored.intent.drift.drifted);
    assert!(
        stored
            .intent
            .drift
            .changed_fields
            .contains(&"ad_groups".to_string())
    );
}

#[tokio::test]
async fn creative_crud_and_matrix_validation_lifecycle() {
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
    let tenant_id = seed_tenant(&pool, "creative-lifecycle").await;
    let manager = TxManager::new(pool);
    let actor_id = Uuid::now_v7();

    let connection_repo = Arc::new(SqlxAdvertisingConnectionRepository::new());
    let creative_repo = Arc::new(SqlxCreativeRepository::new());
    let registry = Arc::new(FakeAdPlatform::registered_catalog());

    let meta_key = PlatformKey::parse("meta").unwrap();
    let connection = AdvertisingConnection::new(
        tenant_id,
        meta_key,
        "act_creative_test",
        "JPY",
        "Asia/Tokyo",
        SecretRef::new("vault-ref-creative"),
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

    let create_creative = CreateCreative::new(
        manager.clone(),
        creative_repo.clone(),
        connection_repo.clone(),
        registry.clone(),
    );
    assert_eq!(
        create_creative
            .platform_for_connection(tenant_id, connection_id)
            .await
            .unwrap(),
        PlatformKey::parse("meta").unwrap()
    );
    let get_creative = GetCreative::new(manager.clone(), creative_repo.clone());
    let list_creatives = ListCreatives::new(manager.clone(), creative_repo.clone());
    let delete_creative = DeleteCreative::new(manager.clone(), creative_repo.clone());
    let get_previews =
        GetCreativePreviews::new(manager.clone(), creative_repo.clone(), registry.clone());

    // 1. Validation error: unsupported placement
    let err = create_creative
        .execute(
            tenant_id,
            NewCreativeFields {
                connection_id,
                placement: "unsupported_placement".to_string(),
                texts: vec![CreativeText {
                    locale: "en".to_string(),
                    headline: "Test".to_string(),
                    body: "Body".to_string(),
                }],
                asset_references: Vec::new(),
                assets: vec![AssetMetadata {
                    width_px: 1080,
                    height_px: 1080,
                    aspect_ratio: "1:1".to_string(),
                    file_size_bytes: 1_000_000,
                    duration_seconds: None,
                    is_video: false,
                }],
            },
        )
        .await;
    assert!(matches!(
        err,
        Err(sanvi_advertising::domain::AdvertisingError::Validation(_))
    ));

    // 2. Validation error: text length exceeding Meta limit
    let err_text = create_creative
        .execute(
            tenant_id,
            NewCreativeFields {
                connection_id,
                placement: "feed".to_string(),
                texts: vec![CreativeText {
                    locale: "en".to_string(),
                    headline: "A".repeat(50), // limit is 40
                    body: "Valid body".to_string(),
                }],
                asset_references: Vec::new(),
                assets: vec![AssetMetadata {
                    width_px: 1080,
                    height_px: 1080,
                    aspect_ratio: "1:1".to_string(),
                    file_size_bytes: 1_000_000,
                    duration_seconds: None,
                    is_video: false,
                }],
            },
        )
        .await;
    assert!(matches!(
        err_text,
        Err(sanvi_advertising::domain::AdvertisingError::Validation(_))
    ));

    // 3. Validation error: invalid asset (below min dimension of 600px on Meta feed)
    let err_asset = create_creative
        .execute(
            tenant_id,
            NewCreativeFields {
                connection_id,
                placement: "feed".to_string(),
                texts: vec![CreativeText {
                    locale: "en".to_string(),
                    headline: "Valid Headline".to_string(),
                    body: "Valid body".to_string(),
                }],
                asset_references: Vec::new(),
                assets: vec![AssetMetadata {
                    width_px: 400, // min is 600
                    height_px: 400,
                    aspect_ratio: "1:1".to_string(),
                    file_size_bytes: 1_000_000,
                    duration_seconds: None,
                    is_video: false,
                }],
            },
        )
        .await;
    assert!(matches!(
        err_asset,
        Err(sanvi_advertising::domain::AdvertisingError::Validation(_))
    ));

    // 4. Success: valid creative creation
    let valid_stored = create_creative
        .execute(
            tenant_id,
            NewCreativeFields {
                connection_id,
                placement: "feed".to_string(),
                texts: vec![CreativeText {
                    locale: "en".to_string(),
                    headline: "Summer Launch".to_string(),
                    body: "Explore our collection today.".to_string(),
                }],
                asset_references: vec!["media/feed_hero.jpg".to_string()],
                assets: vec![AssetMetadata {
                    width_px: 1080,
                    height_px: 1080,
                    aspect_ratio: "1:1".to_string(),
                    file_size_bytes: 2_000_000,
                    duration_seconds: None,
                    is_video: false,
                }],
            },
        )
        .await
        .unwrap();

    assert_eq!(valid_stored.creative.placement, "feed");

    // 5. Get Creative
    let retrieved = get_creative
        .execute(tenant_id, valid_stored.id)
        .await
        .unwrap()
        .expect("creative exists");
    assert_eq!(retrieved.id, valid_stored.id);
    assert_eq!(retrieved.creative.texts[0].headline, "Summer Launch");

    // 6. List Creatives
    let all = list_creatives.execute(tenant_id).await.unwrap();
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].id, valid_stored.id);

    // 7. Get Previews
    let previews = get_previews
        .execute(tenant_id, valid_stored.id, None)
        .await
        .unwrap();
    assert_eq!(previews.len(), 1);
    assert_eq!(previews[0].placement, "feed");
    assert_eq!(previews[0].headline.as_deref(), Some("Summer Launch"));
    assert_eq!(previews[0].spec.min_width_px, Some(600));

    // 8. Delete Creative
    delete_creative
        .execute(tenant_id, valid_stored.id)
        .await
        .unwrap();

    let missing = get_creative
        .execute(tenant_id, valid_stored.id)
        .await
        .unwrap();
    assert!(missing.is_none());
}

#[tokio::test]
async fn add_ad_rejects_missing_creative_id() {
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
    let tenant_id = seed_tenant(&pool, "ad-missing-creative").await;
    let manager = TxManager::new(pool);
    let actor_id = Uuid::now_v7();

    let connection_repo = SqlxAdvertisingConnectionRepository::new();
    let connection = AdvertisingConnection::new(
        tenant_id,
        PlatformKey::parse("google_ads").unwrap(),
        "777-000-3333",
        "JPY",
        "Asia/Tokyo",
        SecretRef::new("vault-ref-missing-creative"),
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
    let creatives: Arc<dyn CreativeRepository> = Arc::new(SqlxCreativeRepository::new());
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
    let add_ad = AddAd::new(manager.clone(), campaigns, creatives, events, registry);

    let draft = create
        .execute(
            tenant_id,
            connection_id,
            Uuid::now_v7(),
            actor_id,
            NewCampaignFields {
                name: "Missing creative test".to_string(),
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

    let with_group = add_ad_group
        .execute(
            tenant_id,
            draft.id,
            draft.revision,
            Uuid::now_v7(),
            actor_id,
            NewAdGroupFields {
                name: "Group 1".to_string(),
                targeting: Targeting::default(),
                bid: BidSettings {
                    strategy: "manual_cpc".to_string(),
                    maximum_bid: None,
                },
            },
        )
        .await
        .unwrap();
    let ad_group_id = with_group.intent.ad_groups[0].id.clone();

    // 1. Non-existent UUID creative id -> CreativeNotFound
    let unpersisted_creative_id = Uuid::now_v7();
    let err = add_ad
        .execute(
            tenant_id,
            draft.id,
            ad_group_id.clone(),
            with_group.revision,
            Uuid::now_v7(),
            actor_id,
            NewAdFields {
                creative: Creative {
                    id: unpersisted_creative_id.to_string(),
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
        .await;
    assert!(matches!(
        err,
        Err(sanvi_advertising::domain::AdvertisingError::CreativeNotFound)
    ));

    // 2. Non-UUID string -> InvalidInput
    let err_invalid = add_ad
        .execute(
            tenant_id,
            draft.id,
            ad_group_id,
            with_group.revision,
            Uuid::now_v7(),
            actor_id,
            NewAdFields {
                creative: Creative {
                    id: "not-a-uuid".to_string(),
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
        .await;
    assert!(matches!(
        err_invalid,
        Err(sanvi_advertising::domain::AdvertisingError::InvalidInput(_))
    ));
}

#[tokio::test]
async fn add_ad_rejects_creative_placement_mismatch_or_unsupported() {
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
    let tenant_id = seed_tenant(&pool, "ad-creative-fit").await;
    let manager = TxManager::new(pool);
    let actor_id = Uuid::now_v7();

    let connection_repo = SqlxAdvertisingConnectionRepository::new();
    let connection = AdvertisingConnection::new(
        tenant_id,
        PlatformKey::parse("google_ads").unwrap(),
        "777-000-4444",
        "JPY",
        "Asia/Tokyo",
        SecretRef::new("vault-ref-creative-fit"),
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
    let creatives: Arc<dyn CreativeRepository> = Arc::new(SqlxCreativeRepository::new());
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
    let add_ad = AddAd::new(
        manager.clone(),
        campaigns,
        creatives.clone(),
        events,
        registry,
    );

    let draft = create
        .execute(
            tenant_id,
            connection_id,
            Uuid::now_v7(),
            actor_id,
            NewCampaignFields {
                name: "Placement fit test".to_string(),
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

    let with_group = add_ad_group
        .execute(
            tenant_id,
            draft.id,
            draft.revision,
            Uuid::now_v7(),
            actor_id,
            NewAdGroupFields {
                name: "Group 1".to_string(),
                targeting: Targeting::default(),
                bid: BidSettings {
                    strategy: "manual_cpc".to_string(),
                    maximum_bid: None,
                },
            },
        )
        .await
        .unwrap();
    let ad_group_id = with_group.intent.ad_groups[0].id.clone();

    // 1. Creative has placement "reels", which Google Ads does not support (only feed and stories).
    //    The creative sits on the campaign's own platform so this exercises the placement
    //    check rather than the platform-mismatch check, which runs first.
    let reels_creative_id = Uuid::now_v7();
    let reels_creative = StoredCreative {
        id: reels_creative_id,
        tenant_id,
        connection_id,
        platform: PlatformKey::parse("google_ads").unwrap(),
        external_id: None,
        creative: Creative {
            id: reels_creative_id.to_string(),
            placement: "reels".to_string(),
            texts: vec![CreativeText {
                locale: "en".to_string(),
                headline: "Reels headline".to_string(),
                body: "Reels body".to_string(),
            }],
            asset_references: Vec::new(),
        },
        assets: Vec::new(),
        created_at: Utc::now(),
        updated_at: Utc::now(),
    };
    let mut scope = manager.begin_for(tenant_id).await.unwrap();
    creatives.insert(&mut scope, &reels_creative).await.unwrap();
    scope.commit().await.unwrap();

    let err_unsupported = add_ad
        .execute(
            tenant_id,
            draft.id,
            ad_group_id.clone(),
            with_group.revision,
            Uuid::now_v7(),
            actor_id,
            NewAdFields {
                creative: Creative {
                    id: reels_creative_id.to_string(),
                    placement: "reels".to_string(),
                    texts: vec![CreativeText {
                        locale: "en".to_string(),
                        headline: "Reels headline".to_string(),
                        body: "Reels body".to_string(),
                    }],
                    asset_references: Vec::new(),
                },
                landing_url: "https://example.com/reels".to_string(),
                tracking_template: None,
            },
        )
        .await;
    match err_unsupported {
        Err(sanvi_advertising::domain::AdvertisingError::Validation(violations)) => {
            assert!(
                violations
                    .iter()
                    .any(|v| v.field_path == "ad_groups[0].ads[0].creative.placement"
                        && v.code == "capability_unsupported")
            );
        }
        other => panic!("expected Validation error, got {other:?}"),
    }

    // 2. Creative has stored placement "feed", but payload spoofs "stories" (placement mismatch).
    let feed_creative_id = Uuid::now_v7();
    let feed_creative = StoredCreative {
        id: feed_creative_id,
        tenant_id,
        connection_id,
        platform: PlatformKey::parse("google_ads").unwrap(),
        external_id: None,
        creative: Creative {
            id: feed_creative_id.to_string(),
            placement: "feed".to_string(),
            texts: vec![CreativeText {
                locale: "en".to_string(),
                headline: "Feed headline".to_string(),
                body: "Feed body".to_string(),
            }],
            asset_references: Vec::new(),
        },
        assets: Vec::new(),
        created_at: Utc::now(),
        updated_at: Utc::now(),
    };
    let mut scope = manager.begin_for(tenant_id).await.unwrap();
    creatives.insert(&mut scope, &feed_creative).await.unwrap();
    scope.commit().await.unwrap();

    let err_mismatch = add_ad
        .execute(
            tenant_id,
            draft.id,
            ad_group_id,
            with_group.revision,
            Uuid::now_v7(),
            actor_id,
            NewAdFields {
                creative: Creative {
                    id: feed_creative_id.to_string(),
                    placement: "stories".to_string(),
                    texts: vec![CreativeText {
                        locale: "en".to_string(),
                        headline: "Feed headline".to_string(),
                        body: "Feed body".to_string(),
                    }],
                    asset_references: Vec::new(),
                },
                landing_url: "https://example.com/feed".to_string(),
                tracking_template: None,
            },
        )
        .await;
    match err_mismatch {
        Err(sanvi_advertising::domain::AdvertisingError::Validation(violations)) => {
            assert!(
                violations
                    .iter()
                    .any(|v| v.field_path == "ad_groups[0].ads[0].creative.placement"
                        && v.code == "invalid")
            );
        }
        other => panic!("expected Validation error, got {other:?}"),
    }
}

#[tokio::test]
async fn add_ad_rejects_creative_platform_mismatch_even_when_placement_name_overlaps() {
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
    let tenant_id = seed_tenant(&pool, "ad-platform-overlap").await;
    let manager = TxManager::new(pool);
    let actor_id = Uuid::now_v7();

    let connection_repo = SqlxAdvertisingConnectionRepository::new();
    let connection = AdvertisingConnection::new(
        tenant_id,
        PlatformKey::parse("google_ads").unwrap(),
        "777-000-5555",
        "JPY",
        "Asia/Tokyo",
        SecretRef::new("vault-ref-platform-overlap"),
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
    let creatives: Arc<dyn CreativeRepository> = Arc::new(SqlxCreativeRepository::new());
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
    let add_ad = AddAd::new(
        manager.clone(),
        campaigns,
        creatives.clone(),
        events,
        registry,
    );

    let draft = create
        .execute(
            tenant_id,
            connection_id,
            Uuid::now_v7(),
            actor_id,
            NewCampaignFields {
                name: "Platform mismatch test".to_string(),
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

    let with_group = add_ad_group
        .execute(
            tenant_id,
            draft.id,
            draft.revision,
            Uuid::now_v7(),
            actor_id,
            NewAdGroupFields {
                name: "Group 1".to_string(),
                targeting: Targeting::default(),
                bid: BidSettings {
                    strategy: "manual_cpc".to_string(),
                    maximum_bid: None,
                },
            },
        )
        .await
        .unwrap();
    let ad_group_id = with_group.intent.ad_groups[0].id.clone();

    // Construct a Meta creative with placement "feed" (valid placement in both meta and google_ads).
    let meta_creative_id = Uuid::now_v7();
    let meta_creative = StoredCreative {
        id: meta_creative_id,
        tenant_id,
        connection_id,
        platform: PlatformKey::parse("meta").unwrap(),
        external_id: None,
        creative: Creative {
            id: meta_creative_id.to_string(),
            placement: "feed".to_string(),
            texts: vec![CreativeText {
                locale: "en".to_string(),
                headline: "Meta feed headline".to_string(),
                body: "Meta feed body".to_string(),
            }],
            asset_references: Vec::new(),
        },
        assets: Vec::new(),
        created_at: Utc::now(),
        updated_at: Utc::now(),
    };
    let mut scope = manager.begin_for(tenant_id).await.unwrap();
    creatives.insert(&mut scope, &meta_creative).await.unwrap();
    scope.commit().await.unwrap();

    // Attach Meta creative to Google Ads campaign -> must be rejected naming both platforms.
    let err = add_ad
        .execute(
            tenant_id,
            draft.id,
            ad_group_id,
            with_group.revision,
            Uuid::now_v7(),
            actor_id,
            NewAdFields {
                creative: Creative {
                    id: meta_creative_id.to_string(),
                    placement: "feed".to_string(),
                    texts: vec![CreativeText {
                        locale: "en".to_string(),
                        headline: "Meta feed headline".to_string(),
                        body: "Meta feed body".to_string(),
                    }],
                    asset_references: Vec::new(),
                },
                landing_url: "https://example.com/meta-feed".to_string(),
                tracking_template: None,
            },
        )
        .await;

    match err {
        Err(sanvi_advertising::domain::AdvertisingError::Validation(violations)) => {
            assert_eq!(violations.len(), 1);
            // The mismatch is about which creative was referenced, not its placement —
            // the placement itself is valid on both platforms here.
            assert_eq!(violations[0].field_path, "ad_groups[0].ads[0].creative.id");
            assert_eq!(violations[0].code, "capability_unsupported");
            assert!(
                violations[0].message.contains("meta")
                    && violations[0].message.contains("google_ads"),
                "Violation message must name both platforms: {}",
                violations[0].message
            );
        }
        other => panic!("expected Validation error with platform mismatch, got {other:?}"),
    }
}

#[tokio::test]
async fn add_ad_and_update_ad_persist_stored_creative_content_authoritatively() {
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
    let tenant_id = seed_tenant(&pool, "ad-authoritative-copy").await;
    let manager = TxManager::new(pool);
    let actor_id = Uuid::now_v7();

    let connection_repo = SqlxAdvertisingConnectionRepository::new();
    let connection = AdvertisingConnection::new(
        tenant_id,
        PlatformKey::parse("google_ads").unwrap(),
        "777-000-6666",
        "JPY",
        "Asia/Tokyo",
        SecretRef::new("vault-ref-auth-copy"),
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
    let creatives: Arc<dyn CreativeRepository> = Arc::new(SqlxCreativeRepository::new());
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
    let add_ad = AddAd::new(
        manager.clone(),
        campaigns.clone(),
        creatives.clone(),
        events.clone(),
        registry.clone(),
    );
    let update_ad = UpdateAd::new(
        manager.clone(),
        campaigns,
        creatives.clone(),
        events,
        registry,
    );

    let draft = create
        .execute(
            tenant_id,
            connection_id,
            Uuid::now_v7(),
            actor_id,
            NewCampaignFields {
                name: "Authoritative copy test".to_string(),
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

    let with_group = add_ad_group
        .execute(
            tenant_id,
            draft.id,
            draft.revision,
            Uuid::now_v7(),
            actor_id,
            NewAdGroupFields {
                name: "Group 1".to_string(),
                targeting: Targeting::default(),
                bid: BidSettings {
                    strategy: "manual_cpc".to_string(),
                    maximum_bid: None,
                },
            },
        )
        .await
        .unwrap();
    let ad_group_id = with_group.intent.ad_groups[0].id.clone();

    // 1. Insert stored creative 1.
    let creative1_id = Uuid::now_v7();
    let stored1 = StoredCreative {
        id: creative1_id,
        tenant_id,
        connection_id,
        platform: PlatformKey::parse("google_ads").unwrap(),
        external_id: None,
        creative: Creative {
            id: creative1_id.to_string(),
            placement: "feed".to_string(),
            texts: vec![CreativeText {
                locale: "en".to_string(),
                headline: "Authoritative Headline 1".to_string(),
                body: "Authoritative Body 1".to_string(),
            }],
            asset_references: vec!["ref-auth-1".to_string()],
        },
        assets: Vec::new(),
        created_at: Utc::now(),
        updated_at: Utc::now(),
    };
    let mut scope = manager.begin_for(tenant_id).await.unwrap();
    creatives.insert(&mut scope, &stored1).await.unwrap();
    scope.commit().await.unwrap();

    // Add ad with spoofed payload copy -> stored copy must be persisted.
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
                    id: creative1_id.to_string(),
                    placement: "feed".to_string(),
                    texts: vec![CreativeText {
                        locale: "en".to_string(),
                        headline: "Spoofed Client Headline".to_string(),
                        body: "Spoofed Client Body".to_string(),
                    }],
                    asset_references: vec!["spoofed-ref".to_string()],
                },
                landing_url: "https://example.com".to_string(),
                tracking_template: None,
            },
        )
        .await
        .unwrap();

    let persisted_ad = &with_ad.intent.ad_groups[0].ads[0];
    assert_eq!(
        persisted_ad.creative.texts[0].headline,
        "Authoritative Headline 1"
    );
    assert_eq!(persisted_ad.creative.texts[0].body, "Authoritative Body 1");
    assert_eq!(
        persisted_ad.creative.asset_references,
        vec!["ref-auth-1".to_string()]
    );
    let ad_id = persisted_ad.id.clone();

    // 2. Insert stored creative 2.
    let creative2_id = Uuid::now_v7();
    let stored2 = StoredCreative {
        id: creative2_id,
        tenant_id,
        connection_id,
        platform: PlatformKey::parse("google_ads").unwrap(),
        external_id: None,
        creative: Creative {
            id: creative2_id.to_string(),
            placement: "feed".to_string(),
            texts: vec![CreativeText {
                locale: "en".to_string(),
                headline: "Authoritative Headline 2".to_string(),
                body: "Authoritative Body 2".to_string(),
            }],
            asset_references: vec!["ref-auth-2".to_string()],
        },
        assets: Vec::new(),
        created_at: Utc::now(),
        updated_at: Utc::now(),
    };
    let mut scope = manager.begin_for(tenant_id).await.unwrap();
    creatives.insert(&mut scope, &stored2).await.unwrap();
    scope.commit().await.unwrap();

    // Update ad with spoofed payload copy -> stored copy 2 must be persisted.
    let updated = update_ad
        .execute(
            tenant_id,
            draft.id,
            ad_group_id,
            ad_id,
            with_ad.revision,
            Uuid::now_v7(),
            actor_id,
            AdFields {
                creative: Some(Creative {
                    id: creative2_id.to_string(),
                    placement: "feed".to_string(),
                    texts: vec![CreativeText {
                        locale: "en".to_string(),
                        headline: "Another Spoofed Headline".to_string(),
                        body: "Another Spoofed Body".to_string(),
                    }],
                    asset_references: Vec::new(),
                }),
                landing_url: None,
                tracking_template: None,
            },
        )
        .await
        .unwrap();

    let updated_ad = &updated.intent.ad_groups[0].ads[0];
    assert_eq!(
        updated_ad.creative.texts[0].headline,
        "Authoritative Headline 2"
    );
    assert_eq!(updated_ad.creative.texts[0].body, "Authoritative Body 2");
    assert_eq!(
        updated_ad.creative.asset_references,
        vec!["ref-auth-2".to_string()]
    );
}

#[tokio::test]
async fn patch_campaign_validates_and_hydrates_creatives_in_ad_groups() {
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
    let tenant_id = seed_tenant(&pool, "patch-campaign-creatives").await;
    let manager = TxManager::new(pool);
    let actor_id = Uuid::now_v7();

    let connection_repo = SqlxAdvertisingConnectionRepository::new();
    let connection = AdvertisingConnection::new(
        tenant_id,
        PlatformKey::parse("google_ads").unwrap(),
        "777-000-7777",
        "JPY",
        "Asia/Tokyo",
        SecretRef::new("vault-ref-patch-creatives"),
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
    let creatives: Arc<dyn CreativeRepository> = Arc::new(SqlxCreativeRepository::new());
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
    let patch = PatchCampaign::new(
        manager.clone(),
        campaigns.clone(),
        creatives.clone(),
        events,
        registry.clone(),
    );
    let validate = ValidateCampaign::new(manager.clone(), campaigns, creatives.clone(), registry);

    let draft = create
        .execute(
            tenant_id,
            connection_id,
            Uuid::now_v7(),
            actor_id,
            NewCampaignFields {
                name: "Patch creative validation test".to_string(),
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

    // 1. Non-UUID creative id in PATCH ad_groups -> InvalidInput.
    let err_invalid_uuid = patch
        .execute(
            tenant_id,
            draft.id,
            draft.revision,
            Uuid::now_v7(),
            actor_id,
            CampaignFields {
                ad_groups: Some(vec![sanvi_advertising::domain::AdGroup {
                    id: Uuid::now_v7().to_string(),
                    name: "Group 1".to_string(),
                    targeting: Targeting::default(),
                    bid: BidSettings {
                        strategy: "manual_cpc".to_string(),
                        maximum_bid: None,
                    },
                    ads: vec![sanvi_advertising::domain::Ad {
                        id: Uuid::now_v7().to_string(),
                        creative: Creative {
                            id: "not-a-uuid".to_string(),
                            placement: "feed".to_string(),
                            texts: vec![],
                            asset_references: vec![],
                        },
                        landing_url: "https://example.com".to_string(),
                        tracking_template: None,
                    }],
                }]),
                ..Default::default()
            },
        )
        .await;
    assert!(matches!(
        err_invalid_uuid,
        Err(sanvi_advertising::domain::AdvertisingError::InvalidInput(_))
    ));

    // 2. Unpersisted UUID creative id -> CreativeNotFound.
    let missing_uuid = Uuid::now_v7();
    let err_missing = patch
        .execute(
            tenant_id,
            draft.id,
            draft.revision,
            Uuid::now_v7(),
            actor_id,
            CampaignFields {
                ad_groups: Some(vec![sanvi_advertising::domain::AdGroup {
                    id: Uuid::now_v7().to_string(),
                    name: "Group 1".to_string(),
                    targeting: Targeting::default(),
                    bid: BidSettings {
                        strategy: "manual_cpc".to_string(),
                        maximum_bid: None,
                    },
                    ads: vec![sanvi_advertising::domain::Ad {
                        id: Uuid::now_v7().to_string(),
                        creative: Creative {
                            id: missing_uuid.to_string(),
                            placement: "feed".to_string(),
                            texts: vec![],
                            asset_references: vec![],
                        },
                        landing_url: "https://example.com".to_string(),
                        tracking_template: None,
                    }],
                }]),
                ..Default::default()
            },
        )
        .await;
    assert!(matches!(
        err_missing,
        Err(sanvi_advertising::domain::AdvertisingError::CreativeNotFound)
    ));

    // 3. Platform mismatch in PATCH ad_groups -> Validation error with indexed path.
    let meta_creative_id = Uuid::now_v7();
    let meta_creative = StoredCreative {
        id: meta_creative_id,
        tenant_id,
        connection_id,
        platform: PlatformKey::parse("meta").unwrap(),
        external_id: None,
        creative: Creative {
            id: meta_creative_id.to_string(),
            placement: "feed".to_string(),
            texts: vec![CreativeText {
                locale: "en".to_string(),
                headline: "Meta headline".to_string(),
                body: "Meta body".to_string(),
            }],
            asset_references: Vec::new(),
        },
        assets: Vec::new(),
        created_at: Utc::now(),
        updated_at: Utc::now(),
    };
    let mut scope = manager.begin_for(tenant_id).await.unwrap();
    creatives.insert(&mut scope, &meta_creative).await.unwrap();
    scope.commit().await.unwrap();

    let err_platform_mismatch = patch
        .execute(
            tenant_id,
            draft.id,
            draft.revision,
            Uuid::now_v7(),
            actor_id,
            CampaignFields {
                ad_groups: Some(vec![sanvi_advertising::domain::AdGroup {
                    id: Uuid::now_v7().to_string(),
                    name: "Group 1".to_string(),
                    targeting: Targeting::default(),
                    bid: BidSettings {
                        strategy: "manual_cpc".to_string(),
                        maximum_bid: None,
                    },
                    ads: vec![sanvi_advertising::domain::Ad {
                        id: Uuid::now_v7().to_string(),
                        creative: Creative {
                            id: meta_creative_id.to_string(),
                            placement: "feed".to_string(),
                            texts: vec![],
                            asset_references: vec![],
                        },
                        landing_url: "https://example.com".to_string(),
                        tracking_template: None,
                    }],
                }]),
                ..Default::default()
            },
        )
        .await;
    match err_platform_mismatch {
        Err(sanvi_advertising::domain::AdvertisingError::Validation(violations)) => {
            assert_eq!(violations.len(), 1);
            assert_eq!(violations[0].field_path, "ad_groups[0].ads[0].creative.id");
            assert!(
                violations[0].message.contains("meta")
                    && violations[0].message.contains("google_ads")
            );
        }
        other => panic!("expected Validation error for platform mismatch, got {other:?}"),
    }

    // 4. Valid creative attached via PATCH is validated and authoritatively hydrated.
    let google_creative_id = Uuid::now_v7();
    let google_creative = StoredCreative {
        id: google_creative_id,
        tenant_id,
        connection_id,
        platform: PlatformKey::parse("google_ads").unwrap(),
        external_id: None,
        creative: Creative {
            id: google_creative_id.to_string(),
            placement: "feed".to_string(),
            texts: vec![CreativeText {
                locale: "en".to_string(),
                headline: "Authoritative Google Headline".to_string(),
                body: "Authoritative Google Body".to_string(),
            }],
            asset_references: vec!["ref-google".to_string()],
        },
        assets: Vec::new(),
        created_at: Utc::now(),
        updated_at: Utc::now(),
    };
    let mut scope = manager.begin_for(tenant_id).await.unwrap();
    creatives
        .insert(&mut scope, &google_creative)
        .await
        .unwrap();
    scope.commit().await.unwrap();

    let patched = patch
        .execute(
            tenant_id,
            draft.id,
            draft.revision,
            Uuid::now_v7(),
            actor_id,
            CampaignFields {
                ad_groups: Some(vec![sanvi_advertising::domain::AdGroup {
                    id: Uuid::now_v7().to_string(),
                    name: "Group 1".to_string(),
                    targeting: Targeting::default(),
                    bid: BidSettings {
                        strategy: "manual_cpc".to_string(),
                        maximum_bid: None,
                    },
                    ads: vec![sanvi_advertising::domain::Ad {
                        id: Uuid::now_v7().to_string(),
                        creative: Creative {
                            id: google_creative_id.to_string(),
                            placement: "feed".to_string(),
                            texts: vec![CreativeText {
                                locale: "en".to_string(),
                                headline: "Spoofed Client Headline".to_string(),
                                body: "Spoofed Client Body".to_string(),
                            }],
                            asset_references: Vec::new(),
                        },
                        landing_url: "https://example.com".to_string(),
                        tracking_template: None,
                    }],
                }]),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    let ad = &patched.intent.ad_groups[0].ads[0];
    assert_eq!(
        ad.creative.texts[0].headline,
        "Authoritative Google Headline"
    );
    assert_eq!(ad.creative.texts[0].body, "Authoritative Google Body");
    assert_eq!(ad.creative.asset_references, vec!["ref-google".to_string()]);

    // 5. The dry run makes the same creative trip, so it cannot call a tree
    //    valid that the patch above would reject. An unknown creative and a
    //    cross-platform one are both reported as violations rather than as the
    //    404/400 the write path raises.
    let unknown_creative_id = Uuid::now_v7();
    let dry_run_violations = validate
        .execute(
            tenant_id,
            draft.id,
            CampaignFields {
                ad_groups: Some(vec![sanvi_advertising::domain::AdGroup {
                    id: Uuid::now_v7().to_string(),
                    name: "Group 1".to_string(),
                    targeting: Targeting::default(),
                    bid: BidSettings {
                        strategy: "manual_cpc".to_string(),
                        maximum_bid: None,
                    },
                    ads: vec![
                        sanvi_advertising::domain::Ad {
                            id: Uuid::now_v7().to_string(),
                            creative: Creative {
                                id: unknown_creative_id.to_string(),
                                placement: "feed".to_string(),
                                texts: vec![],
                                asset_references: vec![],
                            },
                            landing_url: "https://example.com".to_string(),
                            tracking_template: None,
                        },
                        sanvi_advertising::domain::Ad {
                            id: Uuid::now_v7().to_string(),
                            creative: Creative {
                                id: meta_creative_id.to_string(),
                                placement: "feed".to_string(),
                                texts: vec![],
                                asset_references: vec![],
                            },
                            landing_url: "https://example.com".to_string(),
                            tracking_template: None,
                        },
                    ],
                }]),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(
        dry_run_violations
            .iter()
            .any(|v| v.field_path == "ad_groups[0].ads[0].creative.id"
                && v.message.contains("does not exist")),
        "expected a violation for the unknown creative, got {dry_run_violations:?}"
    );
    assert!(
        dry_run_violations
            .iter()
            .any(|v| v.field_path == "ad_groups[0].ads[1].creative.id"
                && v.message.contains("meta")
                && v.message.contains("google_ads")),
        "expected a violation for the cross-platform creative, got {dry_run_violations:?}"
    );

    // A dry run writes nothing.
    let get = GetCampaign::new(
        manager.clone(),
        Arc::new(SqlxCampaignRepository::new())
            as Arc<dyn sanvi_advertising::domain::CampaignRepository>,
    );
    assert_eq!(
        get.execute(tenant_id, draft.id).await.unwrap().revision,
        patched.revision
    );
}
