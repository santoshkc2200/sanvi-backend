//! Advertising schema integration tests.
//!
//! These tests use an isolated Postgres database so the phase-10 migration
//! rollback and partition RLS checks never operate on a developer database.

use std::sync::Arc;

use chrono::{DateTime, Datelike, Duration, Months, NaiveDate, SubsecRound, Utc};
use sanvi_advertising::domain::{ConnectionId, CreativeId};
use sanvi_advertising::{
    AdvertisingConnection, AdvertisingConnectionRepository, OAuthStateClaims, OAuthStateError,
    OAuthStateManager, OAuthStateStore, PlatformKey, SecretRef, SyncError,
    adapters::{
        FakeAdPlatform, OutboxEventPublisher, SqlxAdvertisingConnectionRepository,
        SqlxCampaignRepository, SqlxCreativeRepository, SqlxMetricsRepository, SqlxOAuthStateStore,
        SystemIdGenerator,
    },
    application::{
        AdFields, AdGroupFields, AddAd, AddAdGroup, CampaignFields, CreateCampaign,
        CreateConnection, CreateCreative, DeleteCreative, GetCampaign, GetCreative,
        GetCreativePreviews, GetPlatformAdsHealth, IngestConnectionMetrics, JoinCampaignRevenue,
        ListCampaignChanges, ListCampaigns, ListCreatives, NewAdFields, NewAdGroupFields,
        NewCampaignFields, NewCreativeFields, PatchCampaign, PauseCampaign,
        PollCampaignReviewStatus, PublishCampaign, PublishError, QueryMetrics, ReconcileCampaign,
        ReconcileDueCampaigns, ResumeCampaign, RunTokenRefresh, UpdateAd, UpdateAdGroup,
        ValidateCampaign,
    },
    domain::{
        AdReviewState, AssetMetadata, BidSettings, Budget, BudgetType, CampaignStatus,
        ChangeSource, Creative, CreativeRepository, CreativeText, MetricsDailyRow, MetricsGroupBy,
        MetricsRangeQuery, MetricsRepository, Money, Objective, PlatformAccount, StoredCreative,
        Targeting, TargetingDimension, TenantEnvelopeCrypto,
    },
};
use sanvi_kernel::{TenantId, UserId};
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

fn test_actor() -> UserId {
    UserId::try_from(9_999_999_i64).unwrap()
}

async fn seed_tenant(pool: &sanvi_persistence::PgPool, slug: &str) -> TenantId {
    let tenant_id =
        TenantId::from_snowflake(sanvi_testsupport::test_ids().next_id().expect("test id"));
    sqlx::query(
        "INSERT INTO tenancy.tenants (id, slug, display_name, status, region, default_locale)
         VALUES ($1, $2, $3, 'active', 'jp', 'en')",
    )
    .bind(tenant_id.as_i64())
    .bind(slug)
    .bind(format!("{slug} corp"))
    .execute(pool)
    .await
    .unwrap();
    tenant_id
}

async fn insert_conversion_event(
    pool: &sanvi_persistence::PgPool,
    tenant_id: TenantId,
    connection_id: ConnectionId,
) -> chrono::DateTime<Utc> {
    let manager = TxManager::new(pool.clone(), sanvi_testsupport::test_ids());
    let mut scope = manager.begin_for(tenant_id.as_i64()).await.unwrap();
    let tx = scope.tx_mut().as_inner();
    let event_id = format!("event-{}", Uuid::now_v7());
    let occurred_at = Utc::now();

    sqlx::query(
        "INSERT INTO advertising.connections
         (id, tenant_id, platform, external_account_id, currency, timezone, status, token_ref)
         VALUES ($1, $2, 'google_ads', $3, 'JPY', 'Asia/Tokyo', 'active', 'secret-ref')",
    )
    .bind(connection_id.as_i64())
    .bind(tenant_id.as_i64())
    .bind(format!("acct-{}", connection_id))
    .execute(&mut **tx)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO advertising.campaigns
         (id, tenant_id, connection_id, platform, name, objective, status, budget)
         VALUES ($1, $2, $3, 'google_ads', 'Campaign', 'sales', 'draft', '{}'::jsonb)",
    )
    .bind(sanvi_testsupport::test_ids().next_id().unwrap().as_i64())
    .bind(tenant_id.as_i64())
    .bind(connection_id.as_i64())
    .execute(&mut **tx)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO advertising.conversion_event_dedup
           (tenant_id, event_id, dedupe_key, occurred_at)
         VALUES ($1, $2, $2, $3)",
    )
    .bind(tenant_id.as_i64())
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
    .bind(sanvi_testsupport::test_ids().next_id().unwrap().as_i64())
    .bind(tenant_id.as_i64())
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

    let occurred_at = insert_conversion_event(
        &pool,
        tenant_a,
        ConnectionId::from_snowflake(sanvi_testsupport::test_ids().next_id().unwrap()),
    )
    .await;
    let oauth_store: Arc<dyn OAuthStateStore> = Arc::new(SqlxOAuthStateStore::new(TxManager::new(
        pool.clone(),
        sanvi_testsupport::test_ids(),
    )));
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
    let manager = TxManager::new(pool, sanvi_testsupport::test_ids());

    {
        let mut tenant_a_scope = manager.begin_for(tenant_a.as_i64()).await.unwrap();
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

    let mut tenant_b_scope = manager.begin_for(tenant_b.as_i64()).await.unwrap();
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
    let store: Arc<dyn OAuthStateStore> = Arc::new(SqlxOAuthStateStore::new(TxManager::new(
        pool,
        sanvi_testsupport::test_ids(),
    )));
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
    let store: Arc<dyn OAuthStateStore> = Arc::new(SqlxOAuthStateStore::new(TxManager::new(
        pool.clone(),
        sanvi_testsupport::test_ids(),
    )));
    let claims = OAuthStateClaims {
        tenant_id,
        platform: PlatformKey::parse("google_ads").unwrap(),
        nonce: Uuid::now_v7(),
        expires_at: Utc::now() + Duration::minutes(10),
    };
    store.issue(claims.clone()).await.unwrap();

    let tx_manager = TxManager::new(pool, sanvi_testsupport::test_ids());
    let mut scope = tx_manager.begin_for(tenant_id.as_i64()).await.unwrap();
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
    let manager = TxManager::new(pool, sanvi_testsupport::test_ids());
    let repository = SqlxAdvertisingConnectionRepository::new();
    // Postgres keeps microseconds; a finer host clock (Windows 100 ns ticks)
    // stores truncated timestamps that never equal the in-memory ones.
    let now = Utc::now().trunc_subsecs(6);

    let first = AdvertisingConnection::new(
        ConnectionId::from_snowflake(sanvi_testsupport::test_ids().next_id().expect("test id")),
        tenant_id,
        PlatformKey::parse("google_ads").unwrap(),
        "123-456-7890",
        "JPY",
        "Asia/Tokyo",
        SecretRef::new("vault-ref-1"),
        Some(test_actor()),
        now,
    )
    .unwrap();
    let first_id = first.id();
    let mut scope = manager.begin_for(tenant_id.as_i64()).await.unwrap();
    let stored = repository.upsert(&mut scope, &first).await.unwrap();
    scope.commit().await.unwrap();
    assert_eq!(stored.id(), first_id);

    let mut reconnect = AdvertisingConnection::new(
        ConnectionId::from_snowflake(sanvi_testsupport::test_ids().next_id().expect("test id")),
        tenant_id,
        PlatformKey::parse("google_ads").unwrap(),
        "123-456-7890",
        "JPY",
        "Asia/Tokyo",
        SecretRef::new("vault-ref-2"),
        Some(test_actor()),
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
    let mut scope = manager.begin_for(tenant_id.as_i64()).await.unwrap();
    let stored = repository.upsert(&mut scope, &reconnect).await.unwrap();
    let connections = repository.list(&mut scope, tenant_id).await.unwrap();
    let persisted_error: serde_json::Value =
        sqlx::query_scalar("SELECT last_error FROM advertising.connections WHERE id = $1")
            .bind(first_id.as_i64())
            .fetch_one(&mut **scope.tx_mut().as_inner())
            .await
            .unwrap();
    let persisted_lifecycle: (Option<chrono::DateTime<Utc>>, Option<chrono::DateTime<Utc>>) =
        sqlx::query_as(
            "SELECT disconnected_notified_at, dependent_jobs_paused_at
             FROM advertising.connections WHERE id = $1",
        )
        .bind(first_id.as_i64())
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
    let manager = TxManager::new(pool, sanvi_testsupport::test_ids());
    let repository: Arc<dyn AdvertisingConnectionRepository> =
        Arc::new(SqlxAdvertisingConnectionRepository::new());
    let platform = PlatformKey::parse("google_ads").unwrap();
    let account = PlatformAccount {
        external_id: "123-456-7890".to_string(),
        display_name: "Reconnected account".to_string(),
    };

    let existing = AdvertisingConnection::new(
        ConnectionId::from_snowflake(sanvi_testsupport::test_ids().next_id().expect("test id")),
        tenant_id,
        platform.clone(),
        account.external_id.clone(),
        "JPY",
        "Asia/Tokyo",
        SecretRef::new("existing-token"),
        Some(test_actor()),
        Utc::now(),
    )
    .unwrap();
    let existing_id = existing.id();
    let pending_id = ConnectionId::from_snowflake(sanvi_testsupport::test_ids().next_id().unwrap());
    let pending = AdvertisingConnection::new_pending(
        pending_id,
        tenant_id,
        platform,
        SecretRef::new(pending_id.to_string()),
        vec!["ads.read".to_string()],
        None,
        vec![account.clone()],
        Some(test_actor()),
        Utc::now(),
    )
    .unwrap();

    let mut scope = manager.begin_for(tenant_id.as_i64()).await.unwrap();
    repository.upsert(&mut scope, &existing).await.unwrap();
    repository.upsert(&mut scope, &pending).await.unwrap();
    sqlx::query(
        "INSERT INTO advertising.connection_tokens (connection_id, tenant_id, ciphertext)
         VALUES ($1, $2, $3), ($4, $2, $5)",
    )
    .bind(existing_id.as_i64())
    .bind(tenant_id.as_i64())
    .bind(b"old-token".as_slice())
    .bind(pending_id.as_i64())
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
    let mut scope = manager.begin_for(tenant_id.as_i64()).await.unwrap();
    let connection_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM advertising.connections WHERE tenant_id = $1")
            .bind(tenant_id.as_i64())
            .fetch_one(&mut **scope.tx_mut().as_inner())
            .await
            .unwrap();
    let token: Vec<u8> = sqlx::query_scalar(
        "SELECT ciphertext FROM advertising.connection_tokens WHERE tenant_id = $1 AND connection_id = $2",
    )
    .bind(tenant_id.as_i64())
    .bind(existing_id.as_i64())
    .fetch_one(&mut **scope.tx_mut().as_inner())
    .await
    .unwrap();
    let pending_token_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM advertising.connection_tokens WHERE tenant_id = $1 AND connection_id = $2",
    )
    .bind(tenant_id.as_i64())
    .bind(pending_id.as_i64())
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
    let manager = TxManager::new(pool, sanvi_testsupport::test_ids());

    let first_process = sanvi_advertising::adapters::SqlxTenantEnvelopeCrypto::new(
        [7u8; 32],
        "test-kek".to_string(),
    );
    let mut scope = manager.begin_for(tenant_id.as_i64()).await.unwrap();
    first_process
        .encrypt_for_tenant(&mut scope, tenant_id, b"first token")
        .await
        .unwrap();
    scope.commit().await.unwrap();

    let restarted_process = sanvi_advertising::adapters::SqlxTenantEnvelopeCrypto::new(
        [7u8; 32],
        "test-kek".to_string(),
    );
    let mut scope = manager.begin_for(tenant_id.as_i64()).await.unwrap();
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
    let connection_id =
        ConnectionId::from_snowflake(sanvi_testsupport::test_ids().next_id().unwrap());
    let manager = TxManager::new(pool.clone(), sanvi_testsupport::test_ids());
    let mut scope = manager.begin_for(tenant_id.as_i64()).await.unwrap();
    sqlx::query(
        "INSERT INTO advertising.connections
         (id, tenant_id, platform, external_account_id, currency, timezone, status, token_ref,
          pending_accounts)
         VALUES ($1, $2, 'google_ads', $3, 'XXX', 'UTC', 'pending', $4, '[]'::jsonb)",
    )
    .bind(connection_id.as_i64())
    .bind(tenant_id.as_i64())
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

    let manager = TxManager::new(pool, sanvi_testsupport::test_ids());
    let mut scope = manager.begin_for(tenant_id.as_i64()).await.unwrap();
    let status: String = sqlx::query_scalar(
        "SELECT status FROM advertising.connections WHERE tenant_id = $1 AND id = $2",
    )
    .bind(tenant_id.as_i64())
    .bind(connection_id.as_i64())
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
    let manager = TxManager::new(pool, sanvi_testsupport::test_ids());
    let actor_id = test_actor();

    let connections = SqlxAdvertisingConnectionRepository::new();
    let connection = AdvertisingConnection::new(
        ConnectionId::from_snowflake(sanvi_testsupport::test_ids().next_id().expect("test id")),
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
    let mut scope = manager.begin_for(tenant_id.as_i64()).await.unwrap();
    connections.upsert(&mut scope, &connection).await.unwrap();
    scope.commit().await.unwrap();

    let campaigns: Arc<dyn sanvi_advertising::domain::CampaignRepository> =
        Arc::new(SqlxCampaignRepository::new());
    let connections: Arc<dyn AdvertisingConnectionRepository> = Arc::new(connections);
    let events: Arc<dyn sanvi_advertising::domain::EventPublisher> =
        Arc::new(OutboxEventPublisher::new(sanvi_testsupport::test_ids()));
    let registry = Arc::new(FakeAdPlatform::registered_catalog());

    let create = CreateCampaign::new(
        manager.clone(),
        campaigns.clone(),
        connections,
        registry.clone(),
        events.clone(),
        Arc::new(SystemIdGenerator::new(sanvi_testsupport::test_ids())),
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
        Arc::new(SqlxAdvertisingConnectionRepository::new()),
        creatives.clone(),
        events.clone(),
        registry.clone(),
        Arc::new(SystemIdGenerator::new(sanvi_testsupport::test_ids())),
    );
    let publish = PublishCampaign::new(
        manager.clone(),
        campaigns.clone(),
        Arc::new(SqlxAdvertisingConnectionRepository::new()),
        creatives.clone(),
        events.clone(),
        registry.clone(),
        Arc::new(SystemIdGenerator::new(sanvi_testsupport::test_ids())),
    );
    let pause = PauseCampaign::new(
        manager.clone(),
        campaigns.clone(),
        Arc::new(SqlxAdvertisingConnectionRepository::new()),
        events.clone(),
        registry.clone(),
        Arc::new(SystemIdGenerator::new(sanvi_testsupport::test_ids())),
    );
    let resume = ResumeCampaign::new(
        manager.clone(),
        campaigns.clone(),
        Arc::new(SqlxAdvertisingConnectionRepository::new()),
        events.clone(),
        registry,
        Arc::new(SystemIdGenerator::new(sanvi_testsupport::test_ids())),
    );
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
    assert_eq!(list.execute(tenant_id, None, None).await.unwrap().len(), 1);
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

    let history = changes
        .execute(tenant_id, draft.id, Some(50), None)
        .await
        .unwrap();
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
    let manager = TxManager::new(pool, sanvi_testsupport::test_ids());
    let actor_id = test_actor();

    let connection_repo = SqlxAdvertisingConnectionRepository::new();
    let connection = AdvertisingConnection::new(
        ConnectionId::from_snowflake(sanvi_testsupport::test_ids().next_id().expect("test id")),
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
    let mut scope = manager.begin_for(tenant_id.as_i64()).await.unwrap();
    connection_repo
        .upsert(&mut scope, &connection)
        .await
        .unwrap();
    scope.commit().await.unwrap();

    let campaigns: Arc<dyn sanvi_advertising::domain::CampaignRepository> =
        Arc::new(SqlxCampaignRepository::new());
    let connections: Arc<dyn AdvertisingConnectionRepository> = Arc::new(connection_repo);
    let events: Arc<dyn sanvi_advertising::domain::EventPublisher> =
        Arc::new(OutboxEventPublisher::new(sanvi_testsupport::test_ids()));
    let registry = Arc::new(FakeAdPlatform::registered_catalog());

    let create = CreateCampaign::new(
        manager.clone(),
        campaigns.clone(),
        connections,
        registry.clone(),
        events.clone(),
        Arc::new(SystemIdGenerator::new(sanvi_testsupport::test_ids())),
    );
    let publish = PublishCampaign::new(
        manager.clone(),
        campaigns.clone(),
        Arc::new(SqlxAdvertisingConnectionRepository::new()),
        Arc::new(SqlxCreativeRepository::new()),
        events.clone(),
        registry.clone(),
        Arc::new(SystemIdGenerator::new(sanvi_testsupport::test_ids())),
    );
    let reconcile = Arc::new(ReconcileCampaign::new(
        manager.clone(),
        campaigns.clone(),
        events,
        Arc::new(SystemIdGenerator::new(sanvi_testsupport::test_ids())),
    ));
    let sweep = ReconcileDueCampaigns::new(
        manager,
        campaigns,
        Arc::new(SqlxAdvertisingConnectionRepository::new()),
        registry.clone(),
        reconcile,
    );

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
        tenant_id,
        external_account_id: connection_id.to_string(),
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
    let manager = TxManager::new(pool, sanvi_testsupport::test_ids());
    let actor_id = test_actor();

    let connection_repo = SqlxAdvertisingConnectionRepository::new();
    let connection = AdvertisingConnection::new(
        ConnectionId::from_snowflake(sanvi_testsupport::test_ids().next_id().expect("test id")),
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
    let mut scope = manager.begin_for(tenant_id.as_i64()).await.unwrap();
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
        Arc::new(OutboxEventPublisher::new(sanvi_testsupport::test_ids()));
    let registry = Arc::new(FakeAdPlatform::registered_catalog());

    let create = CreateCampaign::new(
        manager.clone(),
        campaigns.clone(),
        connections,
        registry.clone(),
        events.clone(),
        Arc::new(SystemIdGenerator::new(sanvi_testsupport::test_ids())),
    );
    let add_ad_group = AddAdGroup::new(
        manager.clone(),
        campaigns.clone(),
        Arc::new(SqlxAdvertisingConnectionRepository::new()),
        events.clone(),
        registry.clone(),
        Arc::new(SystemIdGenerator::new(sanvi_testsupport::test_ids())),
    );
    let update_ad_group = UpdateAdGroup::new(
        manager.clone(),
        campaigns.clone(),
        Arc::new(SqlxAdvertisingConnectionRepository::new()),
        events.clone(),
        registry.clone(),
        Arc::new(SystemIdGenerator::new(sanvi_testsupport::test_ids())),
    );
    let add_ad = AddAd::new(
        manager.clone(),
        campaigns,
        Arc::new(SqlxAdvertisingConnectionRepository::new()),
        creatives.clone(),
        events,
        registry,
        Arc::new(SystemIdGenerator::new(sanvi_testsupport::test_ids())),
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

    let creative_id = CreativeId::from_snowflake(sanvi_testsupport::test_ids().next_id().unwrap());
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
    let mut scope = manager.begin_for(tenant_id.as_i64()).await.unwrap();
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
    let manager = TxManager::new(pool, sanvi_testsupport::test_ids());
    let actor_id = test_actor();

    let connection_repo = SqlxAdvertisingConnectionRepository::new();
    let meta_key = PlatformKey::parse("meta").unwrap();
    let connection = AdvertisingConnection::new(
        ConnectionId::from_snowflake(sanvi_testsupport::test_ids().next_id().expect("test id")),
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
    let mut scope = manager.begin_for(tenant_id.as_i64()).await.unwrap();
    connection_repo
        .upsert(&mut scope, &connection)
        .await
        .unwrap();
    scope.commit().await.unwrap();

    let campaigns: Arc<dyn sanvi_advertising::domain::CampaignRepository> =
        Arc::new(SqlxCampaignRepository::new());
    let connections: Arc<dyn AdvertisingConnectionRepository> = Arc::new(connection_repo);
    let events: Arc<dyn sanvi_advertising::domain::EventPublisher> =
        Arc::new(OutboxEventPublisher::new(sanvi_testsupport::test_ids()));
    let registry = Arc::new(FakeAdPlatform::registered_catalog());

    let create = CreateCampaign::new(
        manager.clone(),
        campaigns.clone(),
        connections,
        registry.clone(),
        events.clone(),
        Arc::new(SystemIdGenerator::new(sanvi_testsupport::test_ids())),
    );
    let publish = PublishCampaign::new(
        manager.clone(),
        campaigns.clone(),
        Arc::new(SqlxAdvertisingConnectionRepository::new()),
        Arc::new(SqlxCreativeRepository::new()),
        events.clone(),
        registry.clone(),
        Arc::new(SystemIdGenerator::new(sanvi_testsupport::test_ids())),
    );
    let add_ad_group = AddAdGroup::new(
        manager.clone(),
        campaigns.clone(),
        Arc::new(SqlxAdvertisingConnectionRepository::new()),
        events.clone(),
        registry.clone(),
        Arc::new(SystemIdGenerator::new(sanvi_testsupport::test_ids())),
    );
    let reconcile = Arc::new(ReconcileCampaign::new(
        manager.clone(),
        campaigns.clone(),
        events,
        Arc::new(SystemIdGenerator::new(sanvi_testsupport::test_ids())),
    ));
    let sweep = ReconcileDueCampaigns::new(
        manager.clone(),
        campaigns.clone(),
        Arc::new(SqlxAdvertisingConnectionRepository::new()),
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
        tenant_id,
        external_account_id: format!("act_{connection_id}"),
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
    let mut scope = manager.begin_for(tenant_id.as_i64()).await.unwrap();
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
    let manager = TxManager::new(pool, sanvi_testsupport::test_ids());
    let actor_id = test_actor();

    let connection_repo = Arc::new(SqlxAdvertisingConnectionRepository::new());
    let creative_repo = Arc::new(SqlxCreativeRepository::new());
    let registry = Arc::new(FakeAdPlatform::registered_catalog());

    let meta_key = PlatformKey::parse("meta").unwrap();
    let connection = AdvertisingConnection::new(
        ConnectionId::from_snowflake(sanvi_testsupport::test_ids().next_id().expect("test id")),
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
    let mut scope = manager.begin_for(tenant_id.as_i64()).await.unwrap();
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
        Arc::new(SystemIdGenerator::new(sanvi_testsupport::test_ids())),
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
    let all = list_creatives.execute(tenant_id, None, None).await.unwrap();
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
    let manager = TxManager::new(pool, sanvi_testsupport::test_ids());
    let actor_id = test_actor();

    let connection_repo = SqlxAdvertisingConnectionRepository::new();
    let connection = AdvertisingConnection::new(
        ConnectionId::from_snowflake(sanvi_testsupport::test_ids().next_id().expect("test id")),
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
    let mut scope = manager.begin_for(tenant_id.as_i64()).await.unwrap();
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
        Arc::new(OutboxEventPublisher::new(sanvi_testsupport::test_ids()));
    let registry = Arc::new(FakeAdPlatform::registered_catalog());

    let create = CreateCampaign::new(
        manager.clone(),
        campaigns.clone(),
        connections,
        registry.clone(),
        events.clone(),
        Arc::new(SystemIdGenerator::new(sanvi_testsupport::test_ids())),
    );
    let add_ad_group = AddAdGroup::new(
        manager.clone(),
        campaigns.clone(),
        Arc::new(SqlxAdvertisingConnectionRepository::new()),
        events.clone(),
        registry.clone(),
        Arc::new(SystemIdGenerator::new(sanvi_testsupport::test_ids())),
    );
    let add_ad = AddAd::new(
        manager.clone(),
        campaigns,
        Arc::new(SqlxAdvertisingConnectionRepository::new()),
        creatives,
        events,
        registry,
        Arc::new(SystemIdGenerator::new(sanvi_testsupport::test_ids())),
    );

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
    let unpersisted_creative_id =
        CreativeId::from_snowflake(sanvi_testsupport::test_ids().next_id().unwrap());
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

    // 2. Non-UUID string -> Validation naming the field (TASK-027): the
    //    same hydration path the PATCH tree uses addresses the rejection.
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
    match err_invalid {
        Err(sanvi_advertising::domain::AdvertisingError::Validation(violations)) => {
            assert_eq!(violations.len(), 1);
            assert_eq!(violations[0].field_path, "ad_groups[0].ads[0].creative.id");
            assert_eq!(violations[0].code, "invalid");
        }
        other => panic!("expected field-addressed Validation error, got {other:?}"),
    }
}

#[tokio::test]
async fn add_ad_rejects_creative_placement_mismatch_or_unsupported() {
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
    let tenant_id = seed_tenant(&pool, "ad-creative-fit").await;
    let manager = TxManager::new(pool, sanvi_testsupport::test_ids());
    let actor_id = test_actor();

    let connection_repo = SqlxAdvertisingConnectionRepository::new();
    let connection = AdvertisingConnection::new(
        ConnectionId::from_snowflake(sanvi_testsupport::test_ids().next_id().expect("test id")),
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
    let mut scope = manager.begin_for(tenant_id.as_i64()).await.unwrap();
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
        Arc::new(OutboxEventPublisher::new(sanvi_testsupport::test_ids()));
    let registry = Arc::new(FakeAdPlatform::registered_catalog());

    let create = CreateCampaign::new(
        manager.clone(),
        campaigns.clone(),
        connections,
        registry.clone(),
        events.clone(),
        Arc::new(SystemIdGenerator::new(sanvi_testsupport::test_ids())),
    );
    let add_ad_group = AddAdGroup::new(
        manager.clone(),
        campaigns.clone(),
        Arc::new(SqlxAdvertisingConnectionRepository::new()),
        events.clone(),
        registry.clone(),
        Arc::new(SystemIdGenerator::new(sanvi_testsupport::test_ids())),
    );
    let add_ad = AddAd::new(
        manager.clone(),
        campaigns,
        Arc::new(SqlxAdvertisingConnectionRepository::new()),
        creatives.clone(),
        events,
        registry,
        Arc::new(SystemIdGenerator::new(sanvi_testsupport::test_ids())),
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
    let reels_creative_id =
        CreativeId::from_snowflake(sanvi_testsupport::test_ids().next_id().unwrap());
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
    let mut scope = manager.begin_for(tenant_id.as_i64()).await.unwrap();
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
    let feed_creative_id =
        CreativeId::from_snowflake(sanvi_testsupport::test_ids().next_id().unwrap());
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
    let mut scope = manager.begin_for(tenant_id.as_i64()).await.unwrap();
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
    let manager = TxManager::new(pool, sanvi_testsupport::test_ids());
    let actor_id = test_actor();

    let connection_repo = SqlxAdvertisingConnectionRepository::new();
    let connection = AdvertisingConnection::new(
        ConnectionId::from_snowflake(sanvi_testsupport::test_ids().next_id().expect("test id")),
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
    let mut scope = manager.begin_for(tenant_id.as_i64()).await.unwrap();
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
        Arc::new(OutboxEventPublisher::new(sanvi_testsupport::test_ids()));
    let registry = Arc::new(FakeAdPlatform::registered_catalog());

    let create = CreateCampaign::new(
        manager.clone(),
        campaigns.clone(),
        connections,
        registry.clone(),
        events.clone(),
        Arc::new(SystemIdGenerator::new(sanvi_testsupport::test_ids())),
    );
    let add_ad_group = AddAdGroup::new(
        manager.clone(),
        campaigns.clone(),
        Arc::new(SqlxAdvertisingConnectionRepository::new()),
        events.clone(),
        registry.clone(),
        Arc::new(SystemIdGenerator::new(sanvi_testsupport::test_ids())),
    );
    let add_ad = AddAd::new(
        manager.clone(),
        campaigns,
        Arc::new(SqlxAdvertisingConnectionRepository::new()),
        creatives.clone(),
        events,
        registry,
        Arc::new(SystemIdGenerator::new(sanvi_testsupport::test_ids())),
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
    let meta_creative_id =
        CreativeId::from_snowflake(sanvi_testsupport::test_ids().next_id().unwrap());
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
    let mut scope = manager.begin_for(tenant_id.as_i64()).await.unwrap();
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
    let manager = TxManager::new(pool, sanvi_testsupport::test_ids());
    let actor_id = test_actor();

    let connection_repo = SqlxAdvertisingConnectionRepository::new();
    let connection = AdvertisingConnection::new(
        ConnectionId::from_snowflake(sanvi_testsupport::test_ids().next_id().expect("test id")),
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
    let mut scope = manager.begin_for(tenant_id.as_i64()).await.unwrap();
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
        Arc::new(OutboxEventPublisher::new(sanvi_testsupport::test_ids()));
    let registry = Arc::new(FakeAdPlatform::registered_catalog());

    let create = CreateCampaign::new(
        manager.clone(),
        campaigns.clone(),
        connections,
        registry.clone(),
        events.clone(),
        Arc::new(SystemIdGenerator::new(sanvi_testsupport::test_ids())),
    );
    let add_ad_group = AddAdGroup::new(
        manager.clone(),
        campaigns.clone(),
        Arc::new(SqlxAdvertisingConnectionRepository::new()),
        events.clone(),
        registry.clone(),
        Arc::new(SystemIdGenerator::new(sanvi_testsupport::test_ids())),
    );
    let add_ad = AddAd::new(
        manager.clone(),
        campaigns.clone(),
        Arc::new(SqlxAdvertisingConnectionRepository::new()),
        creatives.clone(),
        events.clone(),
        registry.clone(),
        Arc::new(SystemIdGenerator::new(sanvi_testsupport::test_ids())),
    );
    let update_ad = UpdateAd::new(
        manager.clone(),
        campaigns,
        Arc::new(SqlxAdvertisingConnectionRepository::new()),
        creatives.clone(),
        events,
        registry,
        Arc::new(SystemIdGenerator::new(sanvi_testsupport::test_ids())),
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
    let creative1_id = CreativeId::from_snowflake(sanvi_testsupport::test_ids().next_id().unwrap());
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
    let mut scope = manager.begin_for(tenant_id.as_i64()).await.unwrap();
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
    let creative2_id = CreativeId::from_snowflake(sanvi_testsupport::test_ids().next_id().unwrap());
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
    let mut scope = manager.begin_for(tenant_id.as_i64()).await.unwrap();
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
    let manager = TxManager::new(pool, sanvi_testsupport::test_ids());
    let actor_id = test_actor();

    let connection_repo = SqlxAdvertisingConnectionRepository::new();
    let connection = AdvertisingConnection::new(
        ConnectionId::from_snowflake(sanvi_testsupport::test_ids().next_id().expect("test id")),
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
    let mut scope = manager.begin_for(tenant_id.as_i64()).await.unwrap();
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
        Arc::new(OutboxEventPublisher::new(sanvi_testsupport::test_ids()));
    let registry = Arc::new(FakeAdPlatform::registered_catalog());

    let create = CreateCampaign::new(
        manager.clone(),
        campaigns.clone(),
        connections,
        registry.clone(),
        events.clone(),
        Arc::new(SystemIdGenerator::new(sanvi_testsupport::test_ids())),
    );
    let patch = PatchCampaign::new(
        manager.clone(),
        campaigns.clone(),
        Arc::new(SqlxAdvertisingConnectionRepository::new()),
        creatives.clone(),
        events,
        registry.clone(),
        Arc::new(SystemIdGenerator::new(sanvi_testsupport::test_ids())),
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

    // 1. Non-UUID creative id in PATCH ad_groups -> Validation naming the
    //    exact step-deep field (TASK-027, FR-1127): a parse failure is a
    //    field rejection, not a flat envelope error.
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
                        external_id: None,
                        creative: Creative {
                            id: "not-a-uuid".to_string(),
                            placement: "feed".to_string(),
                            texts: vec![],
                            asset_references: vec![],
                        },
                        landing_url: "https://example.com".to_string(),
                        tracking_template: None,
                        review: Default::default(),
                    }],
                }]),
                ..Default::default()
            },
        )
        .await;
    match err_invalid_uuid {
        Err(sanvi_advertising::domain::AdvertisingError::Validation(violations)) => {
            assert_eq!(violations.len(), 1);
            assert_eq!(violations[0].field_path, "ad_groups[0].ads[0].creative.id");
            assert_eq!(violations[0].code, "invalid");
        }
        other => panic!("expected field-addressed Validation error, got {other:?}"),
    }

    // 1b. A wizard payload rejected at step three names the step-three
    //     field: the same malformed creative id in the *third* ad group
    //     must come back as `ad_groups[2]…`, not the envelope.
    let groups = (0..3)
        .map(|index| sanvi_advertising::domain::AdGroup {
            id: Uuid::now_v7().to_string(),
            name: format!("Group {index}"),
            targeting: Targeting::default(),
            bid: BidSettings {
                strategy: "manual_cpc".to_string(),
                maximum_bid: None,
            },
            // Only the third group carries an ad, so the violation cannot
            // be attributed to any earlier step.
            ads: if index == 2 {
                vec![sanvi_advertising::domain::Ad {
                    id: Uuid::now_v7().to_string(),
                    external_id: None,
                    creative: Creative {
                        id: "not-a-uuid".to_string(),
                        placement: "feed".to_string(),
                        texts: vec![],
                        asset_references: vec![],
                    },
                    landing_url: "https://example.com".to_string(),
                    tracking_template: None,
                    review: Default::default(),
                }]
            } else {
                Vec::new()
            },
        })
        .collect::<Vec<_>>();
    let err_step_three = patch
        .execute(
            tenant_id,
            draft.id,
            draft.revision,
            Uuid::now_v7(),
            actor_id,
            CampaignFields {
                ad_groups: Some(groups),
                ..Default::default()
            },
        )
        .await;
    match err_step_three {
        Err(sanvi_advertising::domain::AdvertisingError::Validation(violations)) => {
            assert_eq!(violations.len(), 1);
            assert_eq!(violations[0].field_path, "ad_groups[2].ads[0].creative.id");
        }
        other => panic!("expected step-three field violation, got {other:?}"),
    }

    // 2. Unpersisted snowflake creative id -> CreativeNotFound.
    let missing_uuid = CreativeId::from_snowflake(sanvi_testsupport::test_ids().next_id().unwrap());
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
                        external_id: None,
                        creative: Creative {
                            id: missing_uuid.to_string(),
                            placement: "feed".to_string(),
                            texts: vec![],
                            asset_references: vec![],
                        },
                        landing_url: "https://example.com".to_string(),
                        tracking_template: None,
                        review: Default::default(),
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
    let meta_creative_id =
        CreativeId::from_snowflake(sanvi_testsupport::test_ids().next_id().unwrap());
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
    let mut scope = manager.begin_for(tenant_id.as_i64()).await.unwrap();
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
                        external_id: None,
                        creative: Creative {
                            id: meta_creative_id.to_string(),
                            placement: "feed".to_string(),
                            texts: vec![],
                            asset_references: vec![],
                        },
                        landing_url: "https://example.com".to_string(),
                        tracking_template: None,
                        review: Default::default(),
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
    let google_creative_id =
        CreativeId::from_snowflake(sanvi_testsupport::test_ids().next_id().unwrap());
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
    let mut scope = manager.begin_for(tenant_id.as_i64()).await.unwrap();
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
                        external_id: None,
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
                        review: Default::default(),
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
    let unknown_creative_id =
        CreativeId::from_snowflake(sanvi_testsupport::test_ids().next_id().unwrap());
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
                            external_id: None,
                            creative: Creative {
                                id: unknown_creative_id.to_string(),
                                placement: "feed".to_string(),
                                texts: vec![],
                                asset_references: vec![],
                            },
                            landing_url: "https://example.com".to_string(),
                            tracking_template: None,
                            review: Default::default(),
                        },
                        sanvi_advertising::domain::Ad {
                            id: Uuid::now_v7().to_string(),
                            external_id: None,
                            creative: Creative {
                                id: meta_creative_id.to_string(),
                                placement: "feed".to_string(),
                                texts: vec![],
                                asset_references: vec![],
                            },
                            landing_url: "https://example.com".to_string(),
                            tracking_template: None,
                            review: Default::default(),
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

#[tokio::test]
async fn dry_run_and_patch_agree_on_authoritative_stored_creative_content() {
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
    let tenant_id = seed_tenant(&pool, "dry-run-patch-agreement").await;
    let manager = TxManager::new(pool, sanvi_testsupport::test_ids());
    let actor_id = test_actor();

    let connection_repo = SqlxAdvertisingConnectionRepository::new();
    let connection = AdvertisingConnection::new(
        ConnectionId::from_snowflake(sanvi_testsupport::test_ids().next_id().expect("test id")),
        tenant_id,
        PlatformKey::parse("google_ads").unwrap(),
        "888-000-8888",
        "JPY",
        "Asia/Tokyo",
        SecretRef::new("vault-ref-dry-run-agree"),
        Some(actor_id),
        Utc::now(),
    )
    .unwrap();
    let connection_id = connection.id();
    let mut scope = manager.begin_for(tenant_id.as_i64()).await.unwrap();
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
        Arc::new(OutboxEventPublisher::new(sanvi_testsupport::test_ids()));
    let registry = Arc::new(FakeAdPlatform::registered_catalog());

    let create = CreateCampaign::new(
        manager.clone(),
        campaigns.clone(),
        connections,
        registry.clone(),
        events.clone(),
        Arc::new(SystemIdGenerator::new(sanvi_testsupport::test_ids())),
    );
    let patch = PatchCampaign::new(
        manager.clone(),
        campaigns.clone(),
        Arc::new(SqlxAdvertisingConnectionRepository::new()),
        creatives.clone(),
        events,
        registry.clone(),
        Arc::new(SystemIdGenerator::new(sanvi_testsupport::test_ids())),
    );
    let validate = ValidateCampaign::new(manager.clone(), campaigns, creatives.clone(), registry);

    // Seed 1: A valid stored creative with a headline well within limits (<= 30 chars for Google Ads).
    let valid_creative_id =
        CreativeId::from_snowflake(sanvi_testsupport::test_ids().next_id().unwrap());
    let valid_creative = StoredCreative {
        id: valid_creative_id,
        tenant_id,
        connection_id,
        platform: PlatformKey::parse("google_ads").unwrap(),
        external_id: None,
        creative: Creative {
            id: valid_creative_id.to_string(),
            placement: "feed".to_string(),
            texts: vec![CreativeText {
                locale: "en".to_string(),
                headline: "Valid Short Headline".to_string(),
                body: "Valid Google Ads Body".to_string(),
            }],
            asset_references: vec!["asset-1".to_string()],
        },
        assets: Vec::new(),
        created_at: Utc::now(),
        updated_at: Utc::now(),
    };

    // Seed 2: An invalid stored creative whose headline exceeds Google Ads limit (> 30 chars).
    let invalid_creative_id =
        CreativeId::from_snowflake(sanvi_testsupport::test_ids().next_id().unwrap());
    let invalid_creative = StoredCreative {
        id: invalid_creative_id,
        tenant_id,
        connection_id,
        platform: PlatformKey::parse("google_ads").unwrap(),
        external_id: None,
        creative: Creative {
            id: invalid_creative_id.to_string(),
            placement: "feed".to_string(),
            texts: vec![CreativeText {
                locale: "en".to_string(),
                headline: "This stored headline is way too long for google ads limit".to_string(),
                body: "Valid Google Ads Body".to_string(),
            }],
            asset_references: vec!["asset-2".to_string()],
        },
        assets: Vec::new(),
        created_at: Utc::now(),
        updated_at: Utc::now(),
    };

    let mut scope = manager.begin_for(tenant_id.as_i64()).await.unwrap();
    creatives.insert(&mut scope, &valid_creative).await.unwrap();
    creatives
        .insert(&mut scope, &invalid_creative)
        .await
        .unwrap();
    scope.commit().await.unwrap();

    let draft = create
        .execute(
            tenant_id,
            connection_id,
            Uuid::now_v7(),
            actor_id,
            NewCampaignFields {
                name: "Agreement test campaign".to_string(),
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

    // CASE 1: Client sends INVALID inline text (>30 chars), but references VALID stored creative.
    // Dry run must hydrate the stored creative and report 0 violations.
    // PATCH must hydrate the stored creative and succeed.
    let spoofed_bad_text_fields = CampaignFields {
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
                external_id: None,
                creative: Creative {
                    id: valid_creative_id.to_string(),
                    placement: "feed".to_string(),
                    texts: vec![CreativeText {
                        locale: "en".to_string(),
                        headline: "This client-supplied headline exceeds thirty chars limit"
                            .to_string(),
                        body: "Spoofed Body".to_string(),
                    }],
                    asset_references: vec![],
                },
                landing_url: "https://example.com".to_string(),
                tracking_template: None,
                review: Default::default(),
            }],
        }]),
        ..Default::default()
    };

    let dry_run_violations = validate
        .execute(tenant_id, draft.id, spoofed_bad_text_fields.clone())
        .await
        .unwrap();
    assert!(
        dry_run_violations.is_empty(),
        "dry-run should use stored valid creative and report 0 violations, got {dry_run_violations:?}"
    );

    let patched = patch
        .execute(
            tenant_id,
            draft.id,
            draft.revision,
            Uuid::now_v7(),
            actor_id,
            spoofed_bad_text_fields,
        )
        .await
        .expect("PATCH should succeed with stored authoritative creative");
    assert_eq!(
        patched.intent.ad_groups[0].ads[0].creative.texts[0].headline,
        "Valid Short Headline"
    );

    // CASE 2: Client sends VALID inline text (<=30 chars), but references INVALID stored creative.
    // Dry run must hydrate the stored creative and report validation violation.
    // PATCH must hydrate the stored creative and reject with validation error.
    let spoofed_good_text_fields = CampaignFields {
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
                external_id: None,
                creative: Creative {
                    id: invalid_creative_id.to_string(),
                    placement: "feed".to_string(),
                    texts: vec![CreativeText {
                        locale: "en".to_string(),
                        headline: "Short Valid".to_string(),
                        body: "Valid Body".to_string(),
                    }],
                    asset_references: vec![],
                },
                landing_url: "https://example.com".to_string(),
                tracking_template: None,
                review: Default::default(),
            }],
        }]),
        ..Default::default()
    };

    let dry_run_violations_invalid_stored = validate
        .execute(tenant_id, draft.id, spoofed_good_text_fields.clone())
        .await
        .unwrap();
    assert!(
        dry_run_violations_invalid_stored.iter().any(|v| {
            v.field_path == "ad_groups[0].ads[0].creative.texts[0].headline"
                && v.message.contains("exceeds the 30-character en limit")
        }),
        "dry-run should report violation from stored creative text, got {dry_run_violations_invalid_stored:?}"
    );

    let patch_result = patch
        .execute(
            tenant_id,
            draft.id,
            patched.revision,
            Uuid::now_v7(),
            actor_id,
            spoofed_good_text_fields,
        )
        .await;
    match patch_result {
        Err(sanvi_advertising::domain::AdvertisingError::Validation(violations)) => {
            assert!(
                violations.iter().any(|v| {
                    v.field_path == "ad_groups[0].ads[0].creative.texts[0].headline"
                        && v.message.contains("exceeds the 30-character en limit")
                }),
                "PATCH should fail on stored creative text limit, got {violations:?}"
            );
        }
        other => panic!("expected Validation error from stored creative text, got {other:?}"),
    }
}

#[tokio::test]
async fn deleting_referenced_creative_fails_publish() {
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
    let tenant_id = seed_tenant(&pool, "delete-creative-publish-fail").await;
    let manager = TxManager::new(pool, sanvi_testsupport::test_ids());
    let actor_id = test_actor();

    let connection_repo = SqlxAdvertisingConnectionRepository::new();
    let connection = AdvertisingConnection::new(
        ConnectionId::from_snowflake(sanvi_testsupport::test_ids().next_id().expect("test id")),
        tenant_id,
        PlatformKey::parse("google_ads").unwrap(),
        "999-000-9999",
        "JPY",
        "Asia/Tokyo",
        SecretRef::new("vault-ref-del-creative"),
        Some(actor_id),
        Utc::now(),
    )
    .unwrap();
    let connection_id = connection.id();
    let mut scope = manager.begin_for(tenant_id.as_i64()).await.unwrap();
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
        Arc::new(OutboxEventPublisher::new(sanvi_testsupport::test_ids()));
    let registry = Arc::new(FakeAdPlatform::registered_catalog());

    let create = CreateCampaign::new(
        manager.clone(),
        campaigns.clone(),
        connections,
        registry.clone(),
        events.clone(),
        Arc::new(SystemIdGenerator::new(sanvi_testsupport::test_ids())),
    );
    let patch = PatchCampaign::new(
        manager.clone(),
        campaigns.clone(),
        Arc::new(SqlxAdvertisingConnectionRepository::new()),
        creatives.clone(),
        events.clone(),
        registry.clone(),
        Arc::new(SystemIdGenerator::new(sanvi_testsupport::test_ids())),
    );
    let publish = PublishCampaign::new(
        manager.clone(),
        campaigns.clone(),
        Arc::new(SqlxAdvertisingConnectionRepository::new()),
        creatives.clone(),
        events.clone(),
        registry.clone(),
        Arc::new(SystemIdGenerator::new(sanvi_testsupport::test_ids())),
    );
    let delete_creative = DeleteCreative::new(manager.clone(), creatives.clone());
    let update_ad = UpdateAd::new(
        manager.clone(),
        campaigns.clone(),
        Arc::new(SqlxAdvertisingConnectionRepository::new()),
        creatives.clone(),
        events.clone(),
        registry.clone(),
        Arc::new(SystemIdGenerator::new(sanvi_testsupport::test_ids())),
    );

    // 1. Create creative and campaign draft.
    let creative_id = CreativeId::from_snowflake(sanvi_testsupport::test_ids().next_id().unwrap());
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
                headline: "Valid Headline".to_string(),
                body: "Valid Body".to_string(),
            }],
            asset_references: vec!["ref-1".to_string()],
        },
        assets: Vec::new(),
        created_at: Utc::now(),
        updated_at: Utc::now(),
    };
    let mut scope = manager.begin_for(tenant_id.as_i64()).await.unwrap();
    creatives
        .insert(&mut scope, &stored_creative)
        .await
        .unwrap();
    scope.commit().await.unwrap();

    let draft = create
        .execute(
            tenant_id,
            connection_id,
            Uuid::now_v7(),
            actor_id,
            NewCampaignFields {
                name: "Delete creative publish test".to_string(),
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

    let ad_group_id = Uuid::now_v7().to_string();
    let ad_id = Uuid::now_v7().to_string();
    let patched_draft = patch
        .execute(
            tenant_id,
            draft.id,
            draft.revision,
            Uuid::now_v7(),
            actor_id,
            CampaignFields {
                ad_groups: Some(vec![sanvi_advertising::domain::AdGroup {
                    id: ad_group_id.clone(),
                    name: "Group 1".to_string(),
                    targeting: Targeting::default(),
                    bid: BidSettings {
                        strategy: "manual_cpc".to_string(),
                        maximum_bid: None,
                    },
                    ads: vec![sanvi_advertising::domain::Ad {
                        id: ad_id.clone(),
                        external_id: None,
                        creative: Creative {
                            id: creative_id.to_string(),
                            placement: "feed".to_string(),
                            texts: vec![],
                            asset_references: vec![],
                        },
                        landing_url: "https://example.com".to_string(),
                        tracking_template: None,
                        review: Default::default(),
                    }],
                }]),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    // 2. Delete the referenced creative while the campaign draft references it.
    delete_creative
        .execute(tenant_id, creative_id)
        .await
        .unwrap();

    // 3. Attempting to publish now must fail with CreativeNotFound.
    let publish_err = publish
        .execute(tenant_id, draft.id, Uuid::now_v7(), actor_id)
        .await;
    assert!(
        matches!(
            publish_err,
            Err(sanvi_advertising::domain::AdvertisingError::CreativeNotFound)
        ),
        "expected CreativeNotFound when publishing campaign with deleted creative, got {publish_err:?}"
    );

    // 4. Verify campaign remains unpublished.
    let get = GetCampaign::new(manager.clone(), campaigns.clone());
    let stored_after_fail = get.execute(tenant_id, draft.id).await.unwrap();
    assert!(stored_after_fail.external_id.is_none());
    assert_eq!(stored_after_fail.intent.status, CampaignStatus::Draft);

    // 5. Attempting to update the ad (without re-supplying creative) must also fail with CreativeNotFound.
    let update_ad_err = update_ad
        .execute(
            tenant_id,
            draft.id,
            ad_group_id,
            ad_id,
            patched_draft.revision,
            Uuid::now_v7(),
            actor_id,
            AdFields {
                landing_url: Some("https://new-url.com".to_string()),
                ..Default::default()
            },
        )
        .await;
    assert!(
        matches!(
            update_ad_err,
            Err(sanvi_advertising::domain::AdvertisingError::CreativeNotFound)
        ),
        "expected CreativeNotFound on UpdateAd with deleted creative, got {update_ad_err:?}"
    );

    // 6. Attempting to patch campaign fields (e.g. name only, leaving ad_groups unchanged) must also fail with CreativeNotFound.
    let patch_name_err = patch
        .execute(
            tenant_id,
            draft.id,
            patched_draft.revision,
            Uuid::now_v7(),
            actor_id,
            CampaignFields {
                name: Some("Renamed Campaign".to_string()),
                ..Default::default()
            },
        )
        .await;
    assert!(
        matches!(
            patch_name_err,
            Err(sanvi_advertising::domain::AdvertisingError::CreativeNotFound)
        ),
        "expected CreativeNotFound on PatchCampaign with deleted creative, got {patch_name_err:?}"
    );
}

// ---------------------------------------------------------------------
// Metrics ingestion, rollups & the ROAS query layer (TASK-016, slice 10.7)
// ---------------------------------------------------------------------

/// Inserts a connection and publishes one campaign against it through the
/// real `CreateCampaign`/`PublishCampaign` use cases and the given fake
/// adapter registry, so the campaign has a real external id and satisfies
/// `metrics_daily`'s FK — mirrors
/// `campaign_lifecycle_draft_publish_patch_pause_resume_and_changes`'s
/// setup. Takes the registry as a parameter (rather than building its own)
/// because `FakeAdPlatform` keeps campaigns in an in-process map: a caller
/// that later fetches metrics through the same registry needs the exact
/// instance the campaign was published into, not a fresh empty one.
async fn create_connection_and_published_campaign(
    pool: &sanvi_persistence::PgPool,
    tenant_id: TenantId,
    registry: Arc<sanvi_advertising::application::PlatformRegistry>,
    currency: &str,
    name: &str,
) -> (AdvertisingConnection, sanvi_advertising::domain::CampaignId) {
    let manager = TxManager::new(pool.clone(), sanvi_testsupport::test_ids());
    let actor_id = test_actor();

    let connections_repo = SqlxAdvertisingConnectionRepository::new();
    let connection = AdvertisingConnection::new(
        ConnectionId::from_snowflake(sanvi_testsupport::test_ids().next_id().expect("test id")),
        tenant_id,
        PlatformKey::parse("google_ads").unwrap(),
        format!("acct-{}", Uuid::now_v7()),
        currency,
        "Asia/Tokyo",
        SecretRef::new("vault-ref"),
        Some(actor_id),
        Utc::now(),
    )
    .unwrap();
    let mut scope = manager.begin_for(tenant_id.as_i64()).await.unwrap();
    connections_repo
        .upsert(&mut scope, &connection)
        .await
        .unwrap();
    scope.commit().await.unwrap();

    let campaigns: Arc<dyn sanvi_advertising::domain::CampaignRepository> =
        Arc::new(SqlxCampaignRepository::new());
    let connections: Arc<dyn AdvertisingConnectionRepository> = Arc::new(connections_repo);
    let events: Arc<dyn sanvi_advertising::domain::EventPublisher> =
        Arc::new(OutboxEventPublisher::new(sanvi_testsupport::test_ids()));

    let create = CreateCampaign::new(
        manager.clone(),
        campaigns.clone(),
        connections,
        registry.clone(),
        events.clone(),
        Arc::new(SystemIdGenerator::new(sanvi_testsupport::test_ids())),
    );
    let publish = PublishCampaign::new(
        manager,
        campaigns,
        Arc::new(SqlxAdvertisingConnectionRepository::new()),
        Arc::new(SqlxCreativeRepository::new()),
        events,
        registry,
        Arc::new(SystemIdGenerator::new(sanvi_testsupport::test_ids())),
    );

    let draft = create
        .execute(
            tenant_id,
            connection.id(),
            Uuid::now_v7(),
            actor_id,
            NewCampaignFields {
                name: name.to_string(),
                objective: Objective::parse("sales").unwrap(),
                budget: Budget {
                    kind: BudgetType::Daily,
                    amount: Money::new(5_000, currency).unwrap(),
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
    assert!(published.external_id.is_some());

    (connection, draft.id)
}

async fn insert_revenue_conversion_event(
    pool: &sanvi_persistence::PgPool,
    tenant_id: TenantId,
    platform: &PlatformKey,
    occurred_at: DateTime<Utc>,
    value_minor: i64,
    currency: &str,
) {
    let manager = TxManager::new(pool.clone(), sanvi_testsupport::test_ids());
    let mut scope = manager.begin_for(tenant_id.as_i64()).await.unwrap();
    let tx = scope.tx_mut().as_inner();
    let event_id = format!("event-{}", Uuid::now_v7());
    let order_ref = format!("order-{}", Uuid::now_v7());

    sqlx::query(
        "INSERT INTO advertising.conversion_event_dedup
           (tenant_id, event_id, dedupe_key, occurred_at)
         VALUES ($1, $2, $2, $3)",
    )
    .bind(tenant_id.as_i64())
    .bind(&event_id)
    .bind(occurred_at)
    .execute(&mut **tx)
    .await
    .unwrap();

    let mut upload_states = std::collections::BTreeMap::new();
    upload_states.insert(
        platform.as_str().to_string(),
        serde_json::json!({"status": "pending", "attempt_count": 0}),
    );
    let consent = serde_json::json!({
        "resolver_version": "v1",
        "jurisdiction": "US",
        "purposes_asked": [],
        "answers": {},
        "signal_source": "test"
    });

    sqlx::query(
        "INSERT INTO advertising.conversion_events
         (id, tenant_id, event_id, dedupe_key, name, occurred_at, value_minor, currency,
          value_source, order_ref, click_ids, hashed_ids, consent, upload_states, subject_key)
         VALUES ($1, $2, $3, $3, 'purchase', $4, $5, $6, 'payment_record', $7, '{}'::jsonb,
                 '{}'::jsonb, $8, $9, '{\"kind\": \"contact\"}'::jsonb)",
    )
    .bind(sanvi_testsupport::test_ids().next_id().unwrap().as_i64())
    .bind(tenant_id.as_i64())
    .bind(&event_id)
    .bind(occurred_at)
    .bind(value_minor)
    .bind(currency)
    .bind(order_ref)
    .bind(consent)
    .bind(serde_json::to_value(upload_states).unwrap())
    .execute(&mut **tx)
    .await
    .unwrap();
    scope.commit().await.unwrap();
}

/// The task's own stated priority: re-ingesting a day must replace the row,
/// never add to it — at the `metrics_daily` grain and at every rollup level.
#[tokio::test]
async fn metrics_daily_upsert_is_idempotent_at_row_and_rollup_grain() {
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
    let tenant_id = seed_tenant(&pool, "advertising-metrics-idempotency").await;
    let manager = TxManager::new(pool.clone(), sanvi_testsupport::test_ids());
    let registry = Arc::new(FakeAdPlatform::registered_catalog());
    let (connection, campaign_id) = create_connection_and_published_campaign(
        &pool,
        tenant_id,
        registry,
        "JPY",
        "Idempotency check",
    )
    .await;

    let repo = SqlxMetricsRepository::new();
    let date = crate::tenant_today();
    let platform = PlatformKey::parse("google_ads").unwrap();

    let first_pass = MetricsDailyRow {
        tenant_id,
        platform: platform.clone(),
        connection_id: connection.id(),
        campaign_id,
        date,
        impressions: 1_000,
        clicks: 10,
        spend: Money::new(5_000, "JPY").unwrap(),
        conversions: 1.0,
        conversion_value: Money::new(6_000, "JPY").unwrap(),
    };
    let mut scope = manager.begin_for(tenant_id.as_i64()).await.unwrap();
    repo.upsert_daily(&mut scope, std::slice::from_ref(&first_pass))
        .await
        .unwrap();
    scope.commit().await.unwrap();

    // A restated pull for the same day arrives with different numbers.
    let restated = MetricsDailyRow {
        impressions: 4_000,
        clicks: 40,
        spend: Money::new(20_000, "JPY").unwrap(),
        conversions: 3.0,
        conversion_value: Money::new(24_000, "JPY").unwrap(),
        ..first_pass
    };
    let mut scope = manager.begin_for(tenant_id.as_i64()).await.unwrap();
    repo.upsert_daily(&mut scope, std::slice::from_ref(&restated))
        .await
        .unwrap();
    scope.commit().await.unwrap();

    // Row grain: exactly one row, holding only the restated values.
    let mut scope = manager.begin_for(tenant_id.as_i64()).await.unwrap();
    let row_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM advertising.metrics_daily WHERE tenant_id = $1 AND campaign_id = $2 AND date = $3",
    )
    .bind(tenant_id.as_i64())
    .bind(campaign_id.as_i64())
    .bind(date)
    .fetch_one(&mut **scope.tx_mut().as_inner())
    .await
    .unwrap();
    assert_eq!(
        row_count, 1,
        "re-ingesting a day must never append a second row"
    );
    let (impressions, spend_minor): (i64, i64) = sqlx::query_as(
        "SELECT impressions, spend_minor FROM advertising.metrics_daily WHERE tenant_id = $1 AND campaign_id = $2 AND date = $3",
    )
    .bind(tenant_id.as_i64())
    .bind(campaign_id.as_i64())
    .bind(date)
    .fetch_one(&mut **scope.tx_mut().as_inner())
    .await
    .unwrap();
    assert_eq!(
        impressions, 4_000,
        "must hold only the restated value, never 1_000 + 4_000"
    );
    assert_eq!(spend_minor, 20_000);

    // Rollup grain: the platform-level aggregate agrees — never 24_000 (=
    // 5_000 + 20_000, the append-bug's telltale sum).
    let query = MetricsRangeQuery {
        tenant_id,
        from: date,
        to: date,
        group_by: MetricsGroupBy::Platform,
        platform: None,
        campaign_id: None,
    };
    let rollup = repo.query_range(&mut scope, &query, date, 7).await.unwrap();
    assert_eq!(rollup.len(), 1);
    assert_eq!(rollup[0].spend_minor, 20_000);
    assert_eq!(rollup[0].impressions, 4_000);
}

/// Review fix: a restatement that no longer reports a previously ingested
/// day for a connection must delete that row, not leave it stale.
/// `IngestConnectionMetrics` always replaces the whole requested window
/// (`MetricsRepository::replace_window`); this exercises the repository
/// primitive directly.
#[tokio::test]
async fn replace_window_deletes_a_day_the_restatement_no_longer_reports() {
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
    let tenant_id = seed_tenant(&pool, "advertising-metrics-replace-window").await;
    let manager = TxManager::new(pool.clone(), sanvi_testsupport::test_ids());
    let registry = Arc::new(FakeAdPlatform::registered_catalog());
    let (connection, campaign_id) = create_connection_and_published_campaign(
        &pool,
        tenant_id,
        registry,
        "JPY",
        "Replace window check",
    )
    .await;

    let repo = SqlxMetricsRepository::new();
    let platform = PlatformKey::parse("google_ads").unwrap();
    let day_one = crate::tenant_today();
    let day_two = day_one.succ_opt().unwrap();

    let row = |date: NaiveDate| MetricsDailyRow {
        tenant_id,
        platform: platform.clone(),
        connection_id: connection.id(),
        campaign_id,
        date,
        impressions: 1_000,
        clicks: 10,
        spend: Money::new(5_000, "JPY").unwrap(),
        conversions: 1.0,
        conversion_value: Money::new(6_000, "JPY").unwrap(),
    };

    // First pass: the platform reports both days.
    let mut scope = manager.begin_for(tenant_id.as_i64()).await.unwrap();
    repo.replace_window(
        &mut scope,
        tenant_id,
        &platform,
        connection.id(),
        day_one,
        day_two,
        &[row(day_one), row(day_two)],
    )
    .await
    .unwrap();
    scope.commit().await.unwrap();

    let mut scope = manager.begin_for(tenant_id.as_i64()).await.unwrap();
    let count_before: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM advertising.metrics_daily WHERE tenant_id = $1 AND connection_id = $2",
    )
    .bind(tenant_id.as_i64())
    .bind(connection.id().as_i64())
    .fetch_one(&mut **scope.tx_mut().as_inner())
    .await
    .unwrap();
    assert_eq!(count_before, 2);
    scope.rollback().await.unwrap();

    // Restatement: the campaign was paused, so day two no longer appears in
    // the fetch at all — `rows` only covers day one.
    let mut scope = manager.begin_for(tenant_id.as_i64()).await.unwrap();
    repo.replace_window(
        &mut scope,
        tenant_id,
        &platform,
        connection.id(),
        day_one,
        day_two,
        &[row(day_one)],
    )
    .await
    .unwrap();
    scope.commit().await.unwrap();

    let mut scope = manager.begin_for(tenant_id.as_i64()).await.unwrap();
    let count_after: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM advertising.metrics_daily WHERE tenant_id = $1 AND connection_id = $2",
    )
    .bind(tenant_id.as_i64())
    .bind(connection.id().as_i64())
    .fetch_one(&mut **scope.tx_mut().as_inner())
    .await
    .unwrap();
    assert_eq!(
        count_after, 1,
        "day two must be deleted, not left behind with stale non-zero values"
    );
    let remaining_date: NaiveDate = sqlx::query_scalar(
        "SELECT date FROM advertising.metrics_daily WHERE tenant_id = $1 AND connection_id = $2",
    )
    .bind(tenant_id.as_i64())
    .bind(connection.id().as_i64())
    .fetch_one(&mut **scope.tx_mut().as_inner())
    .await
    .unwrap();
    assert_eq!(remaining_date, day_one);
}

/// End-to-end: `fetch_metrics` → campaign external-id mapping → idempotent
/// upsert → the ROAS query layer, against the fake adapter (real Google/Meta
/// reporting adapters are blocked on live credentials, same as every other
/// gap in this phase).
#[tokio::test]
async fn ingest_connection_metrics_round_trips_through_the_query_layer() {
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
    let tenant_id = seed_tenant(&pool, "advertising-metrics-ingest").await;
    let manager = TxManager::new(pool.clone(), sanvi_testsupport::test_ids());
    let platforms = Arc::new(FakeAdPlatform::registered_catalog());
    let (connection, campaign_id) = create_connection_and_published_campaign(
        &pool,
        tenant_id,
        platforms.clone(),
        "JPY",
        "Ingest round trip",
    )
    .await;

    let campaigns: Arc<dyn sanvi_advertising::domain::CampaignRepository> =
        Arc::new(SqlxCampaignRepository::new());
    let metrics_repo: Arc<dyn MetricsRepository> = Arc::new(SqlxMetricsRepository::new());
    let ingest =
        IngestConnectionMetrics::new(manager.clone(), platforms, campaigns, metrics_repo.clone());

    let date = crate::tenant_today();
    let first = ingest.execute(&connection, date, date).await.unwrap();
    assert_eq!(first, 1, "one campaign, one day");
    // A second pass over the identical window must succeed and never
    // duplicate the row (the fake adapter is deterministic per day/campaign).
    let second = ingest.execute(&connection, date, date).await.unwrap();
    assert_eq!(second, 1);

    let query_metrics = QueryMetrics::new(manager, metrics_repo, 92, 7);
    let rows = query_metrics
        .execute(
            MetricsRangeQuery {
                tenant_id,
                from: date,
                to: date,
                group_by: MetricsGroupBy::Campaign,
                platform: None,
                campaign_id: Some(campaign_id),
            },
            date,
        )
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].campaign_id, Some(campaign_id));
    assert!(
        rows[0].spend_minor > 0,
        "fake adapter always seeds non-zero spend"
    );
    assert!(
        rows[0].restating,
        "today is inside every restatement window"
    );
}

/// The revenue-attribution rule this slice exists to get right: exactly one
/// active campaign on the connection attributes deterministically; more than
/// one is never distributed by a guess and lands at platform level instead.
#[tokio::test]
async fn revenue_join_attributes_single_campaign_and_parks_ambiguous_revenue_at_platform_level() {
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
    let tenant_id = seed_tenant(&pool, "advertising-revenue-join").await;
    let manager = TxManager::new(pool.clone(), sanvi_testsupport::test_ids());
    let platforms = Arc::new(FakeAdPlatform::registered_catalog());
    let google_ads = PlatformKey::parse("google_ads").unwrap();

    // Scenario A: exactly one active campaign on the connection.
    let (connection_a, campaign_a) = create_connection_and_published_campaign(
        &pool,
        tenant_id,
        platforms.clone(),
        "JPY",
        "Single campaign",
    )
    .await;
    let campaigns: Arc<dyn sanvi_advertising::domain::CampaignRepository> =
        Arc::new(SqlxCampaignRepository::new());
    let connections: Arc<dyn AdvertisingConnectionRepository> =
        Arc::new(SqlxAdvertisingConnectionRepository::new());
    let conversions: Arc<dyn sanvi_advertising::domain::ConversionEventRepository> =
        Arc::new(SqlxCampaignRepository::new());
    let metrics_repo: Arc<dyn MetricsRepository> = Arc::new(SqlxMetricsRepository::new());
    let date = crate::tenant_today();

    // Metrics must be ingested first so the target `metrics_daily` row
    // exists — `set_campaign_revenue` is an UPDATE, not an upsert, by
    // design (see `MetricsRepository::set_campaign_revenue`'s doc comment).
    IngestConnectionMetrics::new(
        manager.clone(),
        platforms.clone(),
        campaigns.clone(),
        metrics_repo.clone(),
    )
    .execute(&connection_a, date, date)
    .await
    .unwrap();

    let occurred_at = date.and_hms_opt(12, 0, 0).unwrap().and_utc();
    insert_revenue_conversion_event(&pool, tenant_id, &google_ads, occurred_at, 15_000, "JPY")
        .await;

    let revenue_join = JoinCampaignRevenue::new(
        manager.clone(),
        campaigns.clone(),
        connections.clone(),
        conversions.clone(),
        metrics_repo.clone(),
    );
    revenue_join
        .execute(tenant_id, &google_ads, date, date)
        .await
        .unwrap();

    let mut scope = manager.begin_for(tenant_id.as_i64()).await.unwrap();
    let attributed: i64 = sqlx::query_scalar(
        "SELECT sanvi_revenue_minor FROM advertising.metrics_daily WHERE tenant_id = $1 AND campaign_id = $2 AND date = $3",
    )
    .bind(tenant_id.as_i64())
    .bind(campaign_a.as_i64())
    .bind(date)
    .fetch_one(&mut **scope.tx_mut().as_inner())
    .await
    .unwrap();
    assert_eq!(
        attributed, 15_000,
        "the sole active campaign must receive the revenue"
    );
    scope.rollback().await.unwrap();

    // Review fix: the join clears the window before rebuilding it. Simulate
    // the event no longer qualifying (`order_ref` cleared — conversion_events
    // rows are never actually deleted in production; retraction is a state
    // change, not a row deletion) and re-run — the campaign's revenue must
    // reset to zero, not keep the stale 15,000.
    {
        let mut scope = manager.begin_for(tenant_id.as_i64()).await.unwrap();
        sqlx::query(
            "UPDATE advertising.conversion_events SET order_ref = NULL WHERE tenant_id = $1",
        )
        .bind(tenant_id.as_i64())
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .unwrap();
        scope.commit().await.unwrap();
    }
    revenue_join
        .execute(tenant_id, &google_ads, date, date)
        .await
        .unwrap();
    let mut scope = manager.begin_for(tenant_id.as_i64()).await.unwrap();
    let reset: i64 = sqlx::query_scalar(
        "SELECT sanvi_revenue_minor FROM advertising.metrics_daily WHERE tenant_id = $1 AND campaign_id = $2 AND date = $3",
    )
    .bind(tenant_id.as_i64())
    .bind(campaign_a.as_i64())
    .bind(date)
    .fetch_one(&mut **scope.tx_mut().as_inner())
    .await
    .unwrap();
    assert_eq!(
        reset, 0,
        "a retracted event's revenue must reset to zero, not linger from the prior pass"
    );
    scope.rollback().await.unwrap();

    // Scenario B: a second active campaign on a second connection makes
    // attribution ambiguous — revenue must land at platform level, never
    // split across the two candidates. A separate tenant keeps this isolated
    // from scenario A's event: `ConversionEvent.upload_states` is keyed by
    // platform, not connection, so two same-platform connections under one
    // tenant would otherwise both match the same event (a pre-existing
    // TASK-014/015 modeling constraint this test works around rather than
    // one this slice introduces).
    let tenant_b = seed_tenant(&pool, "advertising-revenue-join-ambiguous").await;
    let manager_b = TxManager::new(pool.clone(), sanvi_testsupport::test_ids());
    let campaigns_b: Arc<dyn sanvi_advertising::domain::CampaignRepository> =
        Arc::new(SqlxCampaignRepository::new());
    let connections_b: Arc<dyn AdvertisingConnectionRepository> =
        Arc::new(SqlxAdvertisingConnectionRepository::new());
    let conversions_b: Arc<dyn sanvi_advertising::domain::ConversionEventRepository> =
        Arc::new(SqlxCampaignRepository::new());
    let metrics_repo_b: Arc<dyn MetricsRepository> = Arc::new(SqlxMetricsRepository::new());
    let platforms_b = Arc::new(FakeAdPlatform::registered_catalog());

    let (connection_b, campaign_b1) = create_connection_and_published_campaign(
        &pool,
        tenant_b,
        platforms_b.clone(),
        "JPY",
        "Ambiguous 1",
    )
    .await;
    // A second campaign on the SAME connection, so `active_campaigns.len() == 2`.
    let create = CreateCampaign::new(
        manager_b.clone(),
        campaigns_b.clone(),
        Arc::new(SqlxAdvertisingConnectionRepository::new()),
        platforms_b.clone(),
        Arc::new(OutboxEventPublisher::new(sanvi_testsupport::test_ids())),
        Arc::new(SystemIdGenerator::new(sanvi_testsupport::test_ids())),
    );
    let publish = PublishCampaign::new(
        manager_b.clone(),
        campaigns_b.clone(),
        Arc::new(SqlxAdvertisingConnectionRepository::new()),
        Arc::new(SqlxCreativeRepository::new()),
        Arc::new(OutboxEventPublisher::new(sanvi_testsupport::test_ids())),
        platforms_b.clone(),
        Arc::new(SystemIdGenerator::new(sanvi_testsupport::test_ids())),
    );
    let draft_2 = create
        .execute(
            tenant_b,
            connection_b.id(),
            Uuid::now_v7(),
            test_actor(),
            NewCampaignFields {
                name: "Ambiguous 2".to_string(),
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
    publish
        .execute(tenant_b, draft_2.id, Uuid::now_v7(), test_actor())
        .await
        .unwrap();
    let _ = campaign_b1;

    IngestConnectionMetrics::new(
        manager_b.clone(),
        platforms_b,
        campaigns_b.clone(),
        metrics_repo_b.clone(),
    )
    .execute(&connection_b, date, date)
    .await
    .unwrap();
    insert_revenue_conversion_event(&pool, tenant_b, &google_ads, occurred_at, 9_000, "JPY").await;
    JoinCampaignRevenue::new(
        manager_b.clone(),
        campaigns_b,
        connections_b,
        conversions_b,
        metrics_repo_b,
    )
    .execute(tenant_b, &google_ads, date, date)
    .await
    .unwrap();

    let mut scope = manager_b.begin_for(tenant_b.as_i64()).await.unwrap();
    let unattributed_total: i64 = sqlx::query_scalar(
        "SELECT COALESCE(SUM(sanvi_revenue_minor), 0)::bigint FROM advertising.metrics_daily
         WHERE tenant_id = $1 AND connection_id = $2 AND date = $3",
    )
    .bind(tenant_b.as_i64())
    .bind(connection_b.id().as_i64())
    .bind(date)
    .fetch_one(&mut **scope.tx_mut().as_inner())
    .await
    .unwrap();
    assert_eq!(
        unattributed_total, 0,
        "ambiguous revenue must never be distributed across candidate campaigns"
    );
    let unattributed_bucket: i64 = sqlx::query_scalar(
        "SELECT revenue_minor FROM advertising.metrics_unattributed_revenue_daily
         WHERE tenant_id = $1 AND platform = $2 AND date = $3",
    )
    .bind(tenant_b.as_i64())
    .bind(google_ads.as_str())
    .bind(date)
    .fetch_one(&mut **scope.tx_mut().as_inner())
    .await
    .unwrap();
    assert_eq!(unattributed_bucket, 9_000);
}

#[tokio::test]
async fn test_platform_ads_health_reports_aggregations_and_freshness() {
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
    let tenant_1 = seed_tenant(&pool, "platform-health-1").await;
    let tenant_2 = seed_tenant(&pool, "platform-health-2").await;
    let platforms = Arc::new(FakeAdPlatform::registered_catalog());
    let connections_repo = Arc::new(SqlxAdvertisingConnectionRepository::new());
    let manager = TxManager::new(pool.clone(), sanvi_testsupport::test_ids());

    let platform = FakeAdPlatform::fixtures()[0].key.clone();
    let now = Utc::now();

    // Tenant 1: active connection with granted scopes
    let mut conn1 = AdvertisingConnection::new(
        sanvi_advertising::domain::model::ConnectionId::try_from(1_111_001_i64).unwrap(),
        tenant_1,
        platform.clone(),
        "ext-act-1",
        "JPY",
        "Asia/Tokyo",
        SecretRef::new("vault-ref-1"),
        Some(test_actor()),
        now,
    )
    .unwrap();
    conn1.apply_credentials(
        Some("Account 1".to_string()),
        vec!["ads.read".to_string(), "ads.manage".to_string()],
        None,
    );
    conn1.record_metrics_sync(now - Duration::hours(1));

    // Tenant 2: disconnected connection with missing scopes
    let mut conn2 = AdvertisingConnection::new(
        sanvi_advertising::domain::model::ConnectionId::try_from(1_111_002_i64).unwrap(),
        tenant_2,
        platform.clone(),
        "ext-act-2",
        "USD",
        "America/New_York",
        SecretRef::new("vault-ref-2"),
        Some(test_actor()),
        now,
    )
    .unwrap();
    conn2.apply_credentials(
        Some("Account 2".to_string()),
        vec!["ads.read".to_string()],
        None,
    );
    conn2.record_permanent_refresh_failure(
        SyncError::new(
            "revoked",
            Some("token revoked".to_string()),
            serde_json::Map::new(),
        )
        .unwrap(),
    );

    let mut scope1 = manager.begin_for(tenant_1.as_i64()).await.unwrap();
    connections_repo.upsert(&mut scope1, &conn1).await.unwrap();
    scope1.commit().await.unwrap();

    let mut scope2 = manager.begin_for(tenant_2.as_i64()).await.unwrap();
    connections_repo.upsert(&mut scope2, &conn2).await.unwrap();
    scope2.commit().await.unwrap();

    let health_service = GetPlatformAdsHealth::new(
        manager.clone(),
        connections_repo.clone(),
        platforms.clone(),
        6.0,
    );

    let report = health_service.execute().await.unwrap();

    // Verify platform status counts
    let count = report
        .platform_counts
        .iter()
        .find(|c| c.platform == platform.as_str())
        .expect("platform count present");
    assert_eq!(count.active, 1);
    assert_eq!(count.disconnected, 1);
    assert_eq!(count.total, 2);

    // Verify connections needing attention (conn2 is disconnected and missing scopes)
    assert!(
        report
            .connections_needing_attention
            .iter()
            .any(|a| a.connection_id == conn2.id() && a.reconnect_required)
    );

    // Verify freshness summary
    assert_eq!(report.freshness_summary.total_connections, 2);
    assert!(report.freshness_summary.worst_lag_hours.is_some());
}

#[tokio::test]
async fn test_scheduled_token_refresh_sweep_success_and_revocation() {
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
    let tenant_id = seed_tenant(&pool, "token-refresh-tenant").await;
    let connections_repo = Arc::new(SqlxAdvertisingConnectionRepository::new());
    let manager = TxManager::new(pool.clone(), sanvi_testsupport::test_ids());
    let metrics = Arc::new(sanvi_advertising::adapters::AdvertisingMetrics::default());

    let platform = FakeAdPlatform::fixtures()[0].key.clone();
    let now = Utc::now();

    // Active connection whose last_synced_at is 2 hours ago (> 1 hour stale threshold)
    let mut conn = AdvertisingConnection::new(
        sanvi_advertising::domain::model::ConnectionId::try_from(1_222_001_i64).unwrap(),
        tenant_id,
        platform.clone(),
        "ext-act-refresh",
        "JPY",
        "Asia/Tokyo",
        SecretRef::new("vault-ref-refresh"),
        Some(test_actor()),
        now - Duration::hours(2),
    )
    .unwrap();
    conn.apply_credentials(
        Some("Account Refresh".to_string()),
        vec!["ads.read".to_string(), "ads.manage".to_string()],
        None,
    );

    let mut scope = manager.begin_for(tenant_id.as_i64()).await.unwrap();
    connections_repo.upsert(&mut scope, &conn).await.unwrap();
    scope.commit().await.unwrap();

    // A provider that mints, then one that reports the credential is gone.
    struct StubProvider(Result<String, sanvi_advertising::domain::AdvertisingError>);
    #[async_trait::async_trait]
    impl sanvi_advertising::domain::ConnectionAccessTokenProvider for StubProvider {
        async fn access_token(
            &self,
            _connection: &sanvi_advertising::domain::ConnectionHandle,
        ) -> Result<String, sanvi_advertising::domain::AdvertisingError> {
            self.0.clone()
        }
    }

    let providers_ok: std::collections::HashMap<
        sanvi_advertising::PlatformKey,
        Arc<dyn sanvi_advertising::domain::ConnectionAccessTokenProvider>,
    > = std::collections::HashMap::from([(
        platform.clone(),
        Arc::new(StubProvider(Ok("ya29.minted".to_string())))
            as Arc<dyn sanvi_advertising::domain::ConnectionAccessTokenProvider>,
    )]);

    let sweep = RunTokenRefresh::new(
        manager.clone(),
        connections_repo.clone(),
        providers_ok,
        metrics.clone(),
        3600,
    );

    let report = sweep.execute(10).await.unwrap();
    assert_eq!(report.checked, 1);
    assert_eq!(report.refreshed, 1);
    assert_eq!(report.failed, 0);
    assert_eq!(report.skipped, 0);

    let mut scope = manager.begin_for(tenant_id.as_i64()).await.unwrap();
    let updated = connections_repo
        .find_by_id(&mut scope, tenant_id, conn.id())
        .await
        .unwrap()
        .unwrap();
    scope.commit().await.unwrap();
    assert!(updated.last_synced_at().unwrap() >= now - Duration::seconds(10));

    // A platform with no configured provider is skipped, never counted as a
    // failing credential.
    let bare = RunTokenRefresh::new(
        manager.clone(),
        connections_repo.clone(),
        std::collections::HashMap::new(),
        metrics.clone(),
        0,
    );
    let report = bare.execute(10).await.unwrap();
    assert_eq!(report.skipped, report.checked);
    assert_eq!(report.failed, 0);

    // A missing stored credential is the one failure that disconnects, pauses
    // dependent jobs, and marks the tenant notified.
    // Backdate the sync first: the sweeps below use a zero-second staleness
    // threshold, whose due predicate compares the host-clock timestamp the
    // last refresh wrote against the database's own `now()`. A row refreshed
    // milliseconds ago is not reliably due yet, so the sweep can race the
    // DB clock and see nothing to check.
    let mut scope = manager.begin_for(tenant_id.as_i64()).await.unwrap();
    let mut due_again = connections_repo
        .find_by_id(&mut scope, tenant_id, conn.id())
        .await
        .unwrap()
        .unwrap();
    due_again.record_refresh_success(None, Utc::now() - Duration::seconds(5));
    connections_repo
        .update(&mut scope, &due_again)
        .await
        .unwrap();
    scope.commit().await.unwrap();

    let providers_missing: std::collections::HashMap<
        sanvi_advertising::PlatformKey,
        Arc<dyn sanvi_advertising::domain::ConnectionAccessTokenProvider>,
    > = std::collections::HashMap::from([(
        platform.clone(),
        Arc::new(StubProvider(Err(
            sanvi_advertising::domain::AdvertisingError::ConnectionTokenMissing,
        ))) as Arc<dyn sanvi_advertising::domain::ConnectionAccessTokenProvider>,
    )]);
    let revoking = RunTokenRefresh::new(
        manager.clone(),
        connections_repo.clone(),
        providers_missing,
        metrics.clone(),
        0,
    );
    let report = revoking.execute(10).await.unwrap();
    assert_eq!(report.failed, 1);
    assert_eq!(report.refreshed, 0);

    let mut scope = manager.begin_for(tenant_id.as_i64()).await.unwrap();
    let after = connections_repo
        .find_by_id(&mut scope, tenant_id, conn.id())
        .await
        .unwrap()
        .unwrap();
    scope.commit().await.unwrap();
    assert_eq!(
        after.status(),
        sanvi_advertising::domain::ConnectionStatus::Disconnected
    );

    // An outage must NOT disconnect: the grant cannot tell invalid_grant from
    // a 503 yet, so only the unambiguous signal above may pause a tenant.
    // Backdate the sync for the same zero-threshold clock race as above.
    let mut recovered = after;
    recovered.record_refresh_success(None, Utc::now() - Duration::seconds(5));
    let mut scope = manager.begin_for(tenant_id.as_i64()).await.unwrap();
    connections_repo
        .update(&mut scope, &recovered)
        .await
        .unwrap();
    scope.commit().await.unwrap();

    let providers_outage: std::collections::HashMap<
        sanvi_advertising::PlatformKey,
        Arc<dyn sanvi_advertising::domain::ConnectionAccessTokenProvider>,
    > = std::collections::HashMap::from([(
        platform.clone(),
        Arc::new(StubProvider(Err(
            sanvi_advertising::domain::AdvertisingError::PlatformUnavailable("503".to_string()),
        ))) as Arc<dyn sanvi_advertising::domain::ConnectionAccessTokenProvider>,
    )]);
    let outage = RunTokenRefresh::new(
        manager.clone(),
        connections_repo.clone(),
        providers_outage,
        metrics.clone(),
        0,
    );
    assert_eq!(outage.execute(10).await.unwrap().failed, 1);

    let mut scope = manager.begin_for(tenant_id.as_i64()).await.unwrap();
    let still_active = connections_repo
        .find_by_id(&mut scope, tenant_id, conn.id())
        .await
        .unwrap()
        .unwrap();
    scope.commit().await.unwrap();
    assert_eq!(
        still_active.status(),
        sanvi_advertising::domain::ConnectionStatus::Active
    );
}

/// The vault-backed provider is the seam a real adapter uses on every call:
/// tenant from the handle, refresh token from the vault, access token from the
/// platform's grant — and never the connection id as a credential.
#[tokio::test]
async fn vault_access_token_provider_mints_and_caches_a_token_per_connection() {
    use sanvi_advertising::adapters::{SqlxConnectionTokenRepository, SqlxTenantEnvelopeCrypto};
    use sanvi_advertising::application::VaultAccessTokenProvider;
    use sanvi_advertising::domain::{
        ConnectionAccessTokenProvider, ConnectionHandle, GrantedAccessToken, RefreshTokenGrant,
        TokenVault,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct CountingGrant {
        calls: AtomicUsize,
        seen_refresh_tokens: std::sync::Mutex<Vec<String>>,
    }

    #[async_trait::async_trait]
    impl RefreshTokenGrant for CountingGrant {
        async fn exchange_refresh_token(
            &self,
            refresh_token: &str,
        ) -> Result<GrantedAccessToken, sanvi_advertising::domain::AdvertisingError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.seen_refresh_tokens
                .lock()
                .unwrap()
                .push(refresh_token.to_string());
            Ok(GrantedAccessToken {
                access_token: format!("ya29.minted_from_{refresh_token}"),
                expires_at: Some(Utc::now() + Duration::seconds(3600)),
            })
        }
    }

    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
    let tenant_id = seed_tenant(&pool, "advertising-access-tokens").await;
    let tx = TxManager::new(pool.clone(), sanvi_testsupport::test_ids());

    let vault = Arc::new(TokenVault::new(
        Arc::new(SqlxTenantEnvelopeCrypto::new(
            [7u8; 32],
            "test-kek".to_string(),
        )),
        Arc::new(SqlxConnectionTokenRepository::new()),
    ));

    let connection_id =
        ConnectionId::from_snowflake(sanvi_testsupport::test_ids().next_id().unwrap());
    let mut scope = tx.begin_for(tenant_id.as_i64()).await.unwrap();
    sqlx::query(
        "INSERT INTO advertising.connections
         (id, tenant_id, platform, external_account_id, currency, timezone, status, token_ref)
         VALUES ($1, $2, 'google_ads', $3, 'JPY', 'Asia/Tokyo', 'active', 'secret-ref')",
    )
    .bind(connection_id.as_i64())
    .bind(tenant_id.as_i64())
    .bind(format!("acct-{connection_id}"))
    .execute(&mut **scope.tx_mut().as_inner())
    .await
    .unwrap();
    vault
        .store_refresh_token(
            &mut scope,
            tenant_id,
            connection_id,
            "1//04_stored_refresh_token",
        )
        .await
        .unwrap();
    scope.commit().await.unwrap();

    let grant = Arc::new(CountingGrant {
        calls: AtomicUsize::new(0),
        seen_refresh_tokens: std::sync::Mutex::new(Vec::new()),
    });
    let provider = VaultAccessTokenProvider::new(tx.clone(), vault.clone(), grant.clone());
    let handle = ConnectionHandle {
        id: connection_id.to_string(),
        tenant_id,
        external_account_id: connection_id.to_string(),
    };

    let first = provider.access_token(&handle).await.unwrap();
    assert_eq!(first, "ya29.minted_from_1//04_stored_refresh_token");
    // The decrypted refresh token reached the grant, and only the grant.
    assert_eq!(
        grant.seen_refresh_tokens.lock().unwrap().as_slice(),
        ["1//04_stored_refresh_token"]
    );

    // A second call inside the token's lifetime is served from cache: no
    // second vault read, no second grant.
    let second = provider.access_token(&handle).await.unwrap();
    assert_eq!(second, first);
    assert_eq!(grant.calls.load(Ordering::SeqCst), 1);

    // An id with no stored token fails instead of returning something usable.
    let unknown = ConnectionHandle {
        id: ConnectionId::from_snowflake(sanvi_testsupport::test_ids().next_id().unwrap())
            .to_string(),
        tenant_id,
        external_account_id: String::new(),
    };
    assert!(provider.access_token(&unknown).await.is_err());
}

#[tokio::test]
async fn publishing_intent_is_persisted_before_remote_call_and_reconciliation_works() {
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
    let tenant_id = seed_tenant(&pool, "advertising-publish-intent").await;
    let manager = TxManager::new(pool, sanvi_testsupport::test_ids());
    let actor_id = test_actor();

    let connections = SqlxAdvertisingConnectionRepository::new();
    let connection = AdvertisingConnection::new(
        ConnectionId::from_snowflake(sanvi_testsupport::test_ids().next_id().expect("test id")),
        tenant_id,
        PlatformKey::parse("google_ads").unwrap(),
        "123-456-7890",
        "USD",
        "America/New_York",
        SecretRef::new("vault-ref"),
        Some(actor_id),
        Utc::now(),
    )
    .unwrap();
    let connection_id = connection.id();
    let mut scope = manager.begin_for(tenant_id.as_i64()).await.unwrap();
    connections.upsert(&mut scope, &connection).await.unwrap();
    scope.commit().await.unwrap();

    let campaigns: Arc<dyn sanvi_advertising::domain::CampaignRepository> =
        Arc::new(SqlxCampaignRepository::new());
    let connections_repo: Arc<dyn AdvertisingConnectionRepository> = Arc::new(connections);
    let events: Arc<dyn sanvi_advertising::domain::EventPublisher> =
        Arc::new(OutboxEventPublisher::new(sanvi_testsupport::test_ids()));
    let registry = Arc::new(FakeAdPlatform::registered_catalog());
    let creatives: Arc<dyn CreativeRepository> = Arc::new(SqlxCreativeRepository::new());

    let create = CreateCampaign::new(
        manager.clone(),
        campaigns.clone(),
        connections_repo.clone(),
        registry.clone(),
        events.clone(),
        Arc::new(SystemIdGenerator::new(sanvi_testsupport::test_ids())),
    );
    let get = GetCampaign::new(manager.clone(), campaigns.clone());
    let publish = PublishCampaign::new(
        manager.clone(),
        campaigns.clone(),
        connections_repo.clone(),
        creatives.clone(),
        events.clone(),
        registry.clone(),
        Arc::new(SystemIdGenerator::new(sanvi_testsupport::test_ids())),
    );
    let changes = ListCampaignChanges::new(manager.clone(), campaigns.clone());

    // 1. Create a draft campaign.
    let draft = create
        .execute(
            tenant_id,
            connection_id,
            Uuid::now_v7(),
            actor_id,
            NewCampaignFields {
                name: "Fall promo".to_string(),
                objective: Objective::parse("sales").unwrap(),
                budget: Budget {
                    kind: BudgetType::Daily,
                    amount: Money::new(10_000, "JPY").unwrap(),
                },
                schedule: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(draft.intent.status, CampaignStatus::Draft);
    assert!(draft.publishing_mutation_id.is_none());

    // 2. Preflight error handling: validating non-existent campaign fails at Preflight stage.
    // Notice remote_may_have_occurred() is FALSE for preflight failures.
    let preflight_err = publish
        .execute_with_safety(
            tenant_id,
            sanvi_advertising::domain::CampaignId::from_snowflake(
                sanvi_testsupport::test_ids().next_id().unwrap(),
            ),
            Uuid::now_v7(),
            actor_id,
        )
        .await
        .unwrap_err();
    assert!(!preflight_err.remote_may_have_occurred());
    assert!(matches!(
        preflight_err,
        PublishError::Preflight(sanvi_advertising::domain::AdvertisingError::CampaignNotFound)
    ));

    // 3. Normal publish persists Publishing intent and finalizes to Active.
    let publish_mid = Uuid::now_v7();
    let published = publish
        .execute_with_safety(tenant_id, draft.id, publish_mid, actor_id)
        .await
        .unwrap();
    assert_eq!(published.intent.status, CampaignStatus::Active);
    assert!(published.external_id.is_some());
    assert!(published.publishing_mutation_id.is_none());

    // 4. Test re-driving an operation left in `publishing`:
    // Simulate a crash/failure right after Tx 1 by creating another campaign and setting its status to Publishing with a stable mutation id.
    let draft2 = create
        .execute(
            tenant_id,
            connection_id,
            Uuid::now_v7(),
            actor_id,
            NewCampaignFields {
                name: "Winter clearance".to_string(),
                objective: Objective::parse("sales").unwrap(),
                budget: Budget {
                    kind: BudgetType::Daily,
                    amount: Money::new(15_000, "JPY").unwrap(),
                },
                schedule: None,
            },
        )
        .await
        .unwrap();

    let initial_publishing_mid = Uuid::now_v7();
    {
        let mut scope = manager.begin_for(tenant_id.as_i64()).await.unwrap();
        let mut in_flight = draft2.clone();
        in_flight.intent.status = CampaignStatus::Publishing;
        in_flight.publishing_mutation_id = Some(initial_publishing_mid);
        in_flight.revision += 1;
        campaigns
            .update(&mut scope, &in_flight, draft2.revision)
            .await
            .unwrap();
        scope.commit().await.unwrap();
    }

    // Verify it is indeed persisted in Publishing state in the database.
    let stored_in_flight = get.execute(tenant_id, draft2.id).await.unwrap();
    assert_eq!(stored_in_flight.intent.status, CampaignStatus::Publishing);
    assert_eq!(
        stored_in_flight.publishing_mutation_id,
        Some(initial_publishing_mid)
    );

    // Now re-drive it via execute_with_safety with a DIFFERENT mutation id (e.g. caller retried with fresh id).
    let retried_mid = Uuid::now_v7();
    let redriven = publish
        .execute_with_safety(tenant_id, draft2.id, retried_mid, actor_id)
        .await
        .unwrap();
    assert_eq!(redriven.intent.status, CampaignStatus::Active);
    assert!(redriven.external_id.is_some());
    assert!(redriven.publishing_mutation_id.is_none());

    // Verify the change record used the initial stable mutation id, NOT the retried one!
    let campaign2_changes = changes
        .execute(tenant_id, draft2.id, Some(50), None)
        .await
        .unwrap();
    let publish_change = campaign2_changes
        .iter()
        .find(|c| c.revision == redriven.revision)
        .expect("publish change must exist");
    assert_eq!(publish_change.mutation_id, Some(initial_publishing_mid));

    // 5. Test reconcile_publishing on a campaign left in `Publishing`:
    let draft3 = create
        .execute(
            tenant_id,
            connection_id,
            Uuid::now_v7(),
            actor_id,
            NewCampaignFields {
                name: "Spring preview".to_string(),
                objective: Objective::parse("sales").unwrap(),
                budget: Budget {
                    kind: BudgetType::Daily,
                    amount: Money::new(20_000, "JPY").unwrap(),
                },
                schedule: None,
            },
        )
        .await
        .unwrap();

    let recovery_mid = Uuid::now_v7();
    {
        let mut scope = manager.begin_for(tenant_id.as_i64()).await.unwrap();
        let mut in_flight3 = draft3.clone();
        in_flight3.intent.status = CampaignStatus::Publishing;
        in_flight3.publishing_mutation_id = Some(recovery_mid);
        in_flight3.revision += 1;
        campaigns
            .update(&mut scope, &in_flight3, draft3.revision)
            .await
            .unwrap();
        scope.commit().await.unwrap();
    }

    let reconciled = publish
        .reconcile_publishing(tenant_id, draft3.id, actor_id)
        .await
        .unwrap();
    assert_eq!(reconciled.intent.status, CampaignStatus::Active);
    assert!(reconciled.external_id.is_some());
    assert!(reconciled.publishing_mutation_id.is_none());
}

#[tokio::test]
async fn review_status_poll_merges_platform_verdicts_into_intent() {
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
    let tenant_id = seed_tenant(&pool, "ad-review-status").await;
    let manager = TxManager::new(pool, sanvi_testsupport::test_ids());
    let actor_id = test_actor();

    let connection_repo = SqlxAdvertisingConnectionRepository::new();
    let connection = AdvertisingConnection::new(
        ConnectionId::from_snowflake(sanvi_testsupport::test_ids().next_id().expect("test id")),
        tenant_id,
        PlatformKey::parse("google_ads").unwrap(),
        "777-001-0001",
        "JPY",
        "Asia/Tokyo",
        SecretRef::new("vault-ref-review-status"),
        Some(actor_id),
        Utc::now(),
    )
    .unwrap();
    let connection_id = connection.id();
    let mut scope = manager.begin_for(tenant_id.as_i64()).await.unwrap();
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
        Arc::new(OutboxEventPublisher::new(sanvi_testsupport::test_ids()));
    let registry = Arc::new(FakeAdPlatform::registered_catalog());

    let create = CreateCampaign::new(
        manager.clone(),
        campaigns.clone(),
        connections.clone(),
        registry.clone(),
        events.clone(),
        Arc::new(SystemIdGenerator::new(sanvi_testsupport::test_ids())),
    );
    let add_ad_group = AddAdGroup::new(
        manager.clone(),
        campaigns.clone(),
        connections.clone(),
        events.clone(),
        registry.clone(),
        Arc::new(SystemIdGenerator::new(sanvi_testsupport::test_ids())),
    );
    let add_ad = AddAd::new(
        manager.clone(),
        campaigns.clone(),
        connections.clone(),
        creatives.clone(),
        events.clone(),
        registry.clone(),
        Arc::new(SystemIdGenerator::new(sanvi_testsupport::test_ids())),
    );
    let publish = PublishCampaign::new(
        manager.clone(),
        campaigns.clone(),
        connections.clone(),
        creatives.clone(),
        events.clone(),
        registry.clone(),
        Arc::new(SystemIdGenerator::new(sanvi_testsupport::test_ids())),
    );
    let pause = PauseCampaign::new(
        manager.clone(),
        campaigns.clone(),
        connections.clone(),
        events.clone(),
        registry.clone(),
        Arc::new(SystemIdGenerator::new(sanvi_testsupport::test_ids())),
    );
    let get = GetCampaign::new(manager.clone(), campaigns.clone());
    let poll = PollCampaignReviewStatus::new(
        manager.clone(),
        campaigns.clone(),
        connections.clone(),
        registry.clone(),
    );

    // Draft → group → ad (fresh creative) → publish. The fake adapter stores
    // the whole tree, so the poll has ads to report verdicts for.
    let creative_id = CreativeId::from_snowflake(sanvi_testsupport::test_ids().next_id().unwrap());
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
                headline: "Review status headline".to_string(),
                body: "Review status body".to_string(),
            }],
            asset_references: Vec::new(),
        },
        assets: Vec::new(),
        created_at: Utc::now(),
        updated_at: Utc::now(),
    };
    let mut scope = manager.begin_for(tenant_id.as_i64()).await.unwrap();
    creatives
        .insert(&mut scope, &stored_creative)
        .await
        .unwrap();
    scope.commit().await.unwrap();

    let draft = create
        .execute(
            tenant_id,
            connection_id,
            Uuid::now_v7(),
            actor_id,
            NewCampaignFields {
                name: "Review status campaign".to_string(),
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
                        headline: "Review status headline".to_string(),
                        body: "Review status body".to_string(),
                    }],
                    asset_references: Vec::new(),
                },
                landing_url: "https://example.com/review".to_string(),
                tracking_template: None,
            },
        )
        .await
        .unwrap();

    let published = publish
        .execute(tenant_id, draft.id, Uuid::now_v7(), actor_id)
        .await
        .unwrap();
    assert!(published.external_id.is_some());

    // Pre-poll, the ad carries the default (pending, never checked) verdict.
    assert_eq!(
        get.execute(tenant_id, draft.id)
            .await
            .unwrap()
            .intent
            .ad_groups[0]
            .ads[0]
            .review
            .state,
        AdReviewState::Pending
    );

    // Poll against the fake platform: an Active campaign's ads review clean.
    // The fake assigned the ad a platform id at create and publish persisted
    // it onto the intent — that recorded id is what the verdict matched on.
    let published_ad = &published.intent.ad_groups[0].ads[0];
    assert!(
        published_ad.external_id.is_some(),
        "publish must persist the platform's id for the ad"
    );
    let (views, unmatched) = poll.execute(tenant_id, draft.id).await.unwrap();
    assert_eq!(views.len(), 1);
    assert_eq!(views[0].ad_id, with_ad.intent.ad_groups[0].ads[0].id);
    assert_eq!(views[0].review.state, AdReviewState::Approved);
    assert!(views[0].review.checked_at.is_some());
    assert!(
        unmatched.platform_ad_ids.is_empty() && unmatched.ads_without_platform_id == 0,
        "a fully correlated poll reports no gap"
    );

    // The verdict persisted onto the intent, not just into the response.
    let stored = get.execute(tenant_id, draft.id).await.unwrap();
    assert_eq!(
        stored.intent.ad_groups[0].ads[0].review.state,
        AdReviewState::Approved
    );

    // A later poll overwrites the earlier verdict — the platform's latest
    // word is always what the dashboard shows.
    pause
        .execute(tenant_id, draft.id, Uuid::now_v7(), actor_id)
        .await
        .unwrap();
    let (after_pause, _) = poll.execute(tenant_id, draft.id).await.unwrap();
    assert_eq!(after_pause[0].review.state, AdReviewState::Pending);
}

#[tokio::test]
async fn retention_enforcement_anonymises_conversions_and_drops_aged_metrics() {
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();

    // Aged conversion partition (well past the 730d window) and an aged
    // metrics partition (well inside its own 1095d window at 2020-01): both
    // are past their class cutoffs. Both are minted through the SECURITY
    // DEFINER ensure function — the app role has no DDL rights on the
    // schema, exactly like production.
    sqlx::query("SELECT advertising.ensure_monthly_partitions('2020-01-15')")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("SELECT advertising.ensure_monthly_partitions(current_date)")
        .execute(&pool)
        .await
        .unwrap();

    // A still-personal conversion event in the aged partition. The class is
    // registered `anonymise`: the sweep must strip the personal-data columns
    // (as the erasure path does) and KEEP the partition — the anonymised
    // rows are the retained consent proof, not fodder for a drop. The insert
    // carries tenant session context, exactly like a production write
    // under RLS.
    let tenant_id = seed_tenant(&pool, "advertising-retention-sweep").await;
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("SELECT set_config('sanvi.tenant_id', $1, true)")
        .bind(tenant_id.as_i64().to_string())
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO advertising.conversion_event_dedup (tenant_id, event_id, dedupe_key, occurred_at)
         VALUES ($1, 'aged-event', 'aged-event', '2020-01-15T00:00:00Z')",
    )
    .bind(tenant_id.as_i64())
    .execute(&mut *tx)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO advertising.conversion_events
           (id, tenant_id, event_id, dedupe_key, name, occurred_at, click_ids, hashed_ids,
            consent, subject_key)
         VALUES (1, $1, 'aged-event', 'aged-event', 'purchase', '2020-01-15T00:00:00Z',
                 '{\"gclid\": \"x\"}'::jsonb, '{\"em\": \"hash\"}'::jsonb,
                 '{\"resolver_version\": \"v1\"}'::jsonb,
                 '{\"kind\": \"end_user\", \"external_id\": \"u1\"}'::jsonb)",
    )
    .bind(tenant_id.as_i64())
    .execute(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();

    // First pass: the conversion partition is anonymised, the aged metrics
    // partition (a `delete` class) is dropped.
    let acted: (i32,) = sqlx::query_as(
        "SELECT advertising.enforce_retention(
           current_date - 730, current_date - 1095, true)",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(acted.0, 2, "both aged 2020-01 partitions are acted on");

    // The conversion partition survives, reduced to its anonymised,
    // consent-proof form: subject link, identifier hashes and click ids are
    // gone; the consent snapshot is retained. Read back under the tenant's
    // session context, exactly like the insert above.
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("SELECT set_config('sanvi.tenant_id', $1, true)")
        .bind(tenant_id.as_i64().to_string())
        .execute(&mut *tx)
        .await
        .unwrap();
    let anonymised: (
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
    ) = sqlx::query_as(
        "SELECT click_ids->>'gclid', hashed_ids->>'em',
                    subject_key->>'external_id', subject_key->>'kind'
             FROM advertising.conversion_events_202001
             WHERE event_id = 'aged-event'",
    )
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    assert_eq!(anonymised.0, None, "click ids must be stripped");
    assert_eq!(anonymised.1, None, "identifier hashes must be stripped");
    assert_eq!(anonymised.2, None, "the subject link must be stripped");
    assert_eq!(
        anonymised.3.as_deref(),
        Some("contact"),
        "the row keeps the anonymised marker"
    );
    let consent: (Option<String>,) = sqlx::query_as(
        "SELECT consent->>'resolver_version' FROM advertising.conversion_events_202001
         WHERE event_id = 'aged-event'",
    )
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    tx.rollback().await.unwrap();
    assert_eq!(
        consent.0.as_deref(),
        Some("v1"),
        "the consent snapshot is retained as proof"
    );
    let conversions_kept: Option<(String,)> = sqlx::query_as(
        "SELECT 'x' WHERE to_regclass('advertising.conversion_events_202001') IS NOT NULL",
    )
    .fetch_optional(&pool)
    .await
    .unwrap();
    assert!(
        conversions_kept.is_some(),
        "an `anonymise` class keeps its partition — the retained rows are the point"
    );

    // The aged metrics partition (a `delete` class) is gone; the fresh one
    // survives. Its name moves with the clock: minted from current_date
    // above, so asserted the same way instead of a hard-coded month that
    // breaks when the suite runs in a later month.
    let metrics_gone: Option<(String,)> = sqlx::query_as(
        "SELECT 'x' WHERE to_regclass('advertising.metrics_daily_202001') IS NOT NULL",
    )
    .fetch_optional(&pool)
    .await
    .unwrap();
    assert!(
        metrics_gone.is_none(),
        "aged metrics partition must be dropped"
    );
    let kept: Option<(String,)> = sqlx::query_as(
        "SELECT 'x' WHERE to_regclass('advertising.metrics_daily_' || to_char(current_date, 'YYYYMM')) IS NOT NULL",
    )
    .fetch_optional(&pool)
    .await
    .unwrap();
    assert!(kept.is_some(), "fresh partition must survive");

    // Every action is recorded, so retention reporting reads facts, not
    // hopes — and the two actions read differently.
    let log: Vec<(String, String)> = sqlx::query_as(
        "SELECT partition_name, action FROM advertising.partition_retention_log ORDER BY partition_name",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        log,
        vec![
            (
                "conversion_events_202001".to_string(),
                "anonymise".to_string()
            ),
            ("metrics_daily_202001".to_string(), "drop".to_string()),
        ]
    );

    // Idempotent: the anonymised rows are never rewritten and the dropped
    // partition is gone — a second pass acts on nothing.
    let second: (i32,) = sqlx::query_as(
        "SELECT advertising.enforce_retention(
           current_date - 730, current_date - 1095, true)",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(second.0, 0);

    // A NULL cutoff skips its class — a missing registry rule must stop the
    // actions for that class, not fall back to a hard-coded window. A
    // different aged month keeps the log's PK uninvolved.
    sqlx::query("SELECT advertising.ensure_monthly_partitions('2019-12-15')")
        .execute(&pool)
        .await
        .unwrap();
    let skipped: (i32,) =
        sqlx::query_as("SELECT advertising.enforce_retention(NULL, current_date - 1095, true)")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        skipped.0, 1,
        "only the metrics partition is acted on; conversions are skipped"
    );
    let conversion_survives: Option<(String,)> = sqlx::query_as(
        "SELECT 'x' WHERE to_regclass('advertising.conversion_events_201912') IS NOT NULL",
    )
    .fetch_optional(&pool)
    .await
    .unwrap();
    assert!(
        conversion_survives.is_some(),
        "conversion partition is untouched when its class is skipped"
    );
    let metrics_dropped: Option<(String,)> = sqlx::query_as(
        "SELECT 'x' WHERE to_regclass('advertising.metrics_daily_201912') IS NOT NULL",
    )
    .fetch_optional(&pool)
    .await
    .unwrap();
    assert!(
        metrics_dropped.is_none(),
        "the other class still drops on its own cutoff"
    );
}

/// OAuth must insert the parent before its encrypted token. Exercise actual
/// SQL repositories, so mocks cannot hide the immediate foreign key.
#[tokio::test]
async fn oauth_callbacks_store_connection_before_token_for_both_platforms() {
    use sanvi_advertising::{
        adapters::{SqlxConnectionTokenRepository, SqlxTenantEnvelopeCrypto},
        application::HandleCallback,
        domain::{ConnectionStatus, TokenVault},
    };
    let db = sanvi_testsupport::test_db().await;
    let pool = db.app_pool();
    let tenant_id = seed_tenant(&pool, "oauth-callback-fk").await;
    let manager = TxManager::new(pool, sanvi_testsupport::test_ids());
    let connections = Arc::new(SqlxAdvertisingConnectionRepository::new());
    let platforms = Arc::new(FakeAdPlatform::registered_catalog());
    let oauth = OAuthStateManager::with_store(
        "x".repeat(32),
        Arc::new(SqlxOAuthStateStore::new(manager.clone())),
    );
    let vault = Arc::new(TokenVault::new(
        Arc::new(SqlxTenantEnvelopeCrypto::new(
            [42; 32],
            "test-kek".to_string(),
        )),
        Arc::new(SqlxConnectionTokenRepository::new()),
    ));
    let callback = HandleCallback::new(
        manager.clone(),
        connections.clone(),
        platforms,
        oauth.clone(),
        vault.clone(),
        Arc::new(SystemIdGenerator::new(sanvi_testsupport::test_ids())),
    );
    for descriptor in FakeAdPlatform::fixtures().into_iter().take(2) {
        let platform = descriptor.key;
        let state = oauth
            .issue(
                tenant_id,
                platform.clone(),
                Utc::now(),
                Duration::minutes(10),
            )
            .await
            .unwrap();
        let pending = callback
            .execute(
                tenant_id,
                platform.clone(),
                &state,
                format!("test-code-{}", platform.as_str()),
                "https://console.example.com/callback".to_string(),
                Some(test_actor()),
            )
            .await
            .unwrap();
        assert_eq!(pending.status(), ConnectionStatus::Pending);
        let mut scope = manager.begin_for(tenant_id.as_i64()).await.unwrap();
        let stored = connections
            .find_by_id(&mut scope, tenant_id, pending.id())
            .await
            .unwrap()
            .unwrap();
        let token = vault
            .resolve_refresh_token(&mut scope, tenant_id, pending.id())
            .await
            .unwrap();
        assert_eq!(stored.id(), pending.id());
        assert_eq!(
            token,
            format!("fake-refresh-token-for-test-code-{}", platform.as_str())
        );
        scope.commit().await.unwrap();
        let selected_account = pending.pending_accounts().unwrap()[0].external_id.clone();
        let active = CreateConnection::new(manager.clone(), connections.clone())
            .execute(
                tenant_id,
                pending.id(),
                &selected_account,
                "JPY".to_string(),
                "Asia/Tokyo".to_string(),
            )
            .await
            .unwrap();
        assert_eq!(active.id(), pending.id());
        assert_eq!(active.status(), ConnectionStatus::Active);
        assert!(
            callback
                .execute(
                    tenant_id,
                    platform,
                    &state,
                    "replay".to_string(),
                    "https://console.example.com/callback".to_string(),
                    Some(test_actor()),
                )
                .await
                .is_err()
        );
    }
}
