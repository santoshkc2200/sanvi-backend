//! Integration tests for TASK-017 (10.8 Budget guardrails & spend alerts).

use std::sync::Arc;

use chrono::{Duration, Utc};
use sanvi_advertising::adapters::{
    AdvertisingMetrics, FakeAdPlatform, LoggingBudgetNotifier, OutboxEventPublisher,
    SqlxAdvertisingConnectionRepository, SqlxBudgetAlertRepository, SqlxBudgetCapRepository,
    SqlxCampaignRepository, SqlxMetricsRepository, SystemIdGenerator, UnavailableFxRate,
};
use sanvi_advertising::application::{
    AcknowledgeBudgetAlert, BudgetRolloverSweep, GetBudgetCaps, GetCampaignBudgetCap,
    GetSpendStatus, ListBudgetAlerts, PauseCampaign, PutBudgetCap, PutBudgetCapCommand,
    SpendAnomalyDetector, ThresholdEvaluator,
};
use sanvi_advertising::domain::model::{
    AlertCondition, BudgetPeriod, CampaignId, CampaignStatus, DataFreshness, Money,
};
use sanvi_advertising::domain::{
    AdvertisingConnectionRepository, AdvertisingError, BudgetAlertRepository, CampaignRepository,
    FxRatePort, IdGenerator, MetricsDailyRow, MetricsRepository, PlatformKey,
};
use sanvi_kernel::{TenantId, UserId};
use sanvi_persistence::{Migrator, TxManager};

const ADVERTISING_BUDGET_MIGRATION: i64 = 20_260_831_000_000;

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

async fn seed_connection_and_campaign(
    pool: &sanvi_persistence::PgPool,
    platforms: &sanvi_advertising::application::PlatformRegistry,
    tenant_id: TenantId,
    currency: &str,
) -> (sanvi_advertising::domain::ConnectionId, CampaignId) {
    let tx = TxManager::new(pool.clone(), sanvi_testsupport::test_ids());
    let mut scope = tx.begin_for(tenant_id.as_i64()).await.unwrap();

    let conn_id = sanvi_advertising::domain::ConnectionId::try_from(
        sanvi_testsupport::test_ids().next_id().unwrap().as_i64(),
    )
    .unwrap();
    let camp_id =
        CampaignId::try_from(sanvi_testsupport::test_ids().next_id().unwrap().as_i64()).unwrap();

    let platform = PlatformKey::parse("google_ads").unwrap();
    let mut conn = sanvi_advertising::domain::AdvertisingConnection::new(
        conn_id,
        tenant_id,
        platform.clone(),
        format!("acct-{conn_id}"),
        currency,
        "Asia/Tokyo",
        sanvi_advertising::domain::SecretRef::new("secret-ref"),
        Some(test_actor()),
        Utc::now(),
    )
    .unwrap();
    conn.record_refresh_success(None, Utc::now());
    conn.record_metrics_sync(Utc::now());
    SqlxAdvertisingConnectionRepository::new()
        .upsert(&mut scope, &conn)
        .await
        .unwrap();

    let campaign_intent = sanvi_advertising::domain::Campaign {
        id: camp_id.to_string(),
        name: "Test Campaign".to_string(),
        objective: sanvi_advertising::domain::Objective::parse("sales").unwrap(),
        status: CampaignStatus::Active,
        budget: sanvi_advertising::domain::Budget {
            amount: Money::new(100_000, currency).unwrap(),
            kind: sanvi_advertising::domain::BudgetType::Daily,
        },
        schedule: None,
        ad_groups: vec![],
        drift: sanvi_advertising::domain::model::DriftState {
            drifted: false,
            changed_fields: vec![],
        },
    };

    let stored = sanvi_advertising::domain::StoredCampaign {
        id: camp_id,
        tenant_id,
        connection_id: conn_id,
        platform: platform.clone(),
        external_id: Some(camp_id.to_string()),
        revision: 1,
        intent: campaign_intent.clone(),
        remote_snapshot: campaign_intent.clone(),
    };

    SqlxCampaignRepository::new()
        .insert(&mut scope, &stored)
        .await
        .unwrap();

    scope.commit().await.unwrap();

    if let Ok(adapter) = platforms.adapter(&platform) {
        let _ = adapter
            .create_campaign(
                &sanvi_advertising::domain::ConnectionHandle {
                    id: format!("acct-{conn_id}"),
                    tenant_id,
                    external_account_id: format!("acct-{conn_id}"),
                },
                campaign_intent,
                uuid::Uuid::new_v4(),
            )
            .await;
    }

    (conn_id, camp_id)
}

struct TestHarness {
    _db: sanvi_testsupport::TestDb,
    pool: sanvi_persistence::PgPool,
    tx: TxManager,
    alerts_repo: Arc<SqlxBudgetAlertRepository>,
    metrics_repo: Arc<SqlxMetricsRepository>,
    campaigns_repo: Arc<SqlxCampaignRepository>,
    connections_repo: Arc<SqlxAdvertisingConnectionRepository>,
    ids: Arc<SystemIdGenerator>,
    platforms: Arc<sanvi_advertising::application::PlatformRegistry>,
    status_query: Arc<GetSpendStatus>,
    evaluator: Arc<ThresholdEvaluator>,
    put_cap: Arc<PutBudgetCap>,
    get_caps: Arc<GetBudgetCaps>,
    get_camp_cap: Arc<GetCampaignBudgetCap>,
    list_alerts: Arc<ListBudgetAlerts>,
    ack_alert: Arc<AcknowledgeBudgetAlert>,
    rollover_sweep: Arc<BudgetRolloverSweep>,
}

impl TestHarness {
    async fn new() -> (Self, TenantId) {
        let db = sanvi_testsupport::test_db().await;
        let pool = db.app_pool();

        let today = Utc::now().date_naive();
        let yesterday = today - Duration::days(1);
        let _ = sqlx::query("SELECT advertising.ensure_monthly_partitions($1)")
            .bind(yesterday)
            .execute(&pool)
            .await;
        let _ = sqlx::query("SELECT advertising.ensure_monthly_partitions($1)")
            .bind(today)
            .execute(&pool)
            .await;
        let _ = sqlx::query("SELECT advertising.ensure_monthly_partitions($1)")
            .bind(today + Duration::days(31))
            .execute(&pool)
            .await;

        let tenant_id = seed_tenant(&pool, "budget-guardrails-test").await;
        let tx = TxManager::new(pool.clone(), sanvi_testsupport::test_ids());
        let caps_repo = Arc::new(SqlxBudgetCapRepository::new());
        let alerts_repo = Arc::new(SqlxBudgetAlertRepository::new());
        let metrics_repo = Arc::new(SqlxMetricsRepository::new());
        let campaigns_repo = Arc::new(SqlxCampaignRepository::new());
        let connections_repo = Arc::new(SqlxAdvertisingConnectionRepository::new());
        let events = Arc::new(OutboxEventPublisher::new(sanvi_testsupport::test_ids()));
        let ids = Arc::new(SystemIdGenerator::new(sanvi_testsupport::test_ids()));
        let notifier = Arc::new(LoggingBudgetNotifier::new());
        let registry = prometheus::Registry::new();
        let metrics = Arc::new(AdvertisingMetrics::new(&registry));
        let fx_rates: Arc<dyn FxRatePort> = Arc::new(UnavailableFxRate::new());

        let platforms = Arc::new(sanvi_advertising::adapters::rate_limited_registry(
            &FakeAdPlatform::registered_catalog(),
            |_| (10, 2),
        ));

        let status_query = Arc::new(GetSpendStatus::new(
            tx.clone(),
            metrics_repo.clone(),
            caps_repo.clone(),
            campaigns_repo.clone(),
            connections_repo.clone(),
            fx_rates.clone(),
            7,
        ));

        let pause_campaign = Arc::new(PauseCampaign::new(
            tx.clone(),
            campaigns_repo.clone(),
            Arc::new(SqlxAdvertisingConnectionRepository::new()),
            events.clone(),
            platforms.clone(),
            ids.clone(),
        ));

        let evaluator = Arc::new(ThresholdEvaluator::new(
            tx.clone(),
            caps_repo.clone(),
            alerts_repo.clone(),
            campaigns_repo.clone(),
            connections_repo.clone(),
            pause_campaign.clone(),
            status_query.clone(),
            events.clone(),
            ids.clone(),
            notifier.clone(),
            metrics.clone(),
        ));

        let put_cap = Arc::new(PutBudgetCap::new(
            tx.clone(),
            caps_repo.clone(),
            ids.clone(),
            status_query.clone(),
            evaluator.clone(),
        ));

        let get_caps = Arc::new(GetBudgetCaps::new(tx.clone(), caps_repo.clone()));
        let get_camp_cap = Arc::new(GetCampaignBudgetCap::new(tx.clone(), caps_repo.clone()));
        let list_alerts = Arc::new(ListBudgetAlerts::new(tx.clone(), alerts_repo.clone()));
        let ack_alert = Arc::new(AcknowledgeBudgetAlert::new(tx.clone(), alerts_repo.clone()));
        let rollover_sweep = Arc::new(BudgetRolloverSweep::new(tx.clone(), caps_repo.clone()));

        (
            Self {
                _db: db,
                pool,
                tx,
                alerts_repo,
                metrics_repo,
                campaigns_repo,
                connections_repo,
                ids,
                platforms,
                status_query,
                evaluator,
                put_cap,
                get_caps,
                get_camp_cap,
                list_alerts,
                ack_alert,
                rollover_sweep,
            },
            tenant_id,
        )
    }
}

#[tokio::test]
async fn test_budget_cap_crud_and_optimistic_locking() {
    let (harness, tenant_id) = TestHarness::new().await;
    let (_conn_id, camp_id) =
        seed_connection_and_campaign(&harness.pool, &harness.platforms, tenant_id, "JPY").await;

    // 1. Put Tenant Cap
    let cmd = PutBudgetCapCommand {
        period: BudgetPeriod::Daily,
        amount: Money::new(100_000, "JPY").unwrap(),
        declared_fx_basis: None,
        fx_rate_date: None,
        auto_pause: true,
        auto_resume_on_rollover: false,
        confirm_below_current_spend: false,
        dry_run: false,
    };
    let res = harness.put_cap.execute(tenant_id, None, cmd).await.unwrap();
    let tenant_cap = match res {
        sanvi_advertising::application::PutBudgetCapResponse::Applied(c) => c,
        _ => panic!("expected applied"),
    };
    assert_eq!(tenant_cap.amount.amount_minor, 100_000);
    assert_eq!(tenant_cap.version, 1);
    assert!(tenant_cap.auto_pause);

    // 2. Update with version increment
    let update_cmd = PutBudgetCapCommand {
        period: BudgetPeriod::Daily,
        amount: Money::new(120_000, "JPY").unwrap(),
        declared_fx_basis: None,
        fx_rate_date: None,
        auto_pause: false,
        auto_resume_on_rollover: true,
        confirm_below_current_spend: false,
        dry_run: false,
    };
    let res2 = harness
        .put_cap
        .execute(tenant_id, None, update_cmd)
        .await
        .unwrap();
    let updated_cap = match res2 {
        sanvi_advertising::application::PutBudgetCapResponse::Applied(c) => c,
        _ => panic!("expected applied"),
    };
    assert_eq!(updated_cap.amount.amount_minor, 120_000);
    assert_eq!(updated_cap.version, 2);
    assert!(!updated_cap.auto_pause);
    assert!(updated_cap.auto_resume_on_rollover);

    // 3. Put Campaign-level Cap
    let camp_cmd = PutBudgetCapCommand {
        period: BudgetPeriod::Daily,
        amount: Money::new(50_000, "JPY").unwrap(),
        declared_fx_basis: None,
        fx_rate_date: None,
        auto_pause: true,
        auto_resume_on_rollover: false,
        confirm_below_current_spend: false,
        dry_run: false,
    };
    let res_camp = harness
        .put_cap
        .execute(tenant_id, Some(camp_id), camp_cmd)
        .await
        .unwrap();
    let camp_cap = match res_camp {
        sanvi_advertising::application::PutBudgetCapResponse::Applied(c) => c,
        _ => panic!("expected applied"),
    };
    assert_eq!(camp_cap.campaign_id, Some(camp_id));
    assert_eq!(camp_cap.amount.amount_minor, 50_000);

    // 4. List Caps
    let all_caps = harness.get_caps.execute(tenant_id).await.unwrap();
    assert_eq!(all_caps.len(), 2);

    // 5. Get Campaign Cap
    let fetched_camp_cap = harness
        .get_camp_cap
        .execute(tenant_id, camp_id, BudgetPeriod::Daily)
        .await
        .unwrap();
    assert_eq!(fetched_camp_cap.id, camp_cap.id);
}

#[tokio::test]
async fn test_cap_below_current_spend_confirmation() {
    let (harness, tenant_id) = TestHarness::new().await;
    let (conn_id, camp_id) =
        seed_connection_and_campaign(&harness.pool, &harness.platforms, tenant_id, "JPY").await;
    let today = Utc::now().date_naive();

    // Insert 50,000 JPY spend today
    let mut scope = harness.tx.begin_for(tenant_id.as_i64()).await.unwrap();
    harness
        .metrics_repo
        .upsert_daily(
            &mut scope,
            &[MetricsDailyRow {
                tenant_id,
                connection_id: conn_id,
                platform: PlatformKey::parse("google_ads").unwrap(),
                campaign_id: camp_id,
                date: today,
                impressions: 1000,
                clicks: 100,
                spend: Money::new(50_000, "JPY").unwrap(),
                conversions: 5.0,
                conversion_value: Money::new(100_000, "JPY").unwrap(),
            }],
        )
        .await
        .unwrap();
    scope.commit().await.unwrap();

    // Try to set cap to 30,000 without confirmation -> should fail with CapBelowCurrentSpend
    let cmd = PutBudgetCapCommand {
        period: BudgetPeriod::Daily,
        amount: Money::new(30_000, "JPY").unwrap(),
        declared_fx_basis: None,
        fx_rate_date: None,
        auto_pause: true,
        auto_resume_on_rollover: false,
        confirm_below_current_spend: false,
        dry_run: false,
    };
    let err = harness
        .put_cap
        .execute(tenant_id, Some(camp_id), cmd)
        .await
        .unwrap_err();
    assert!(matches!(err, AdvertisingError::CapBelowCurrentSpend));

    // With confirmation -> should succeed
    let confirm_cmd = PutBudgetCapCommand {
        period: BudgetPeriod::Daily,
        amount: Money::new(30_000, "JPY").unwrap(),
        declared_fx_basis: None,
        fx_rate_date: None,
        auto_pause: true,
        auto_resume_on_rollover: false,
        confirm_below_current_spend: true,
        dry_run: false,
    };
    let res = harness
        .put_cap
        .execute(tenant_id, Some(camp_id), confirm_cmd)
        .await
        .unwrap();
    assert!(matches!(
        res,
        sanvi_advertising::application::PutBudgetCapResponse::Applied(_)
    ));
}

#[tokio::test]
async fn test_dry_run_simulation() {
    let (harness, tenant_id) = TestHarness::new().await;
    let (conn_id, camp_id) =
        seed_connection_and_campaign(&harness.pool, &harness.platforms, tenant_id, "JPY").await;
    let today = Utc::now().date_naive();

    // Insert 85,000 JPY spend today
    let mut scope = harness.tx.begin_for(tenant_id.as_i64()).await.unwrap();
    harness
        .metrics_repo
        .upsert_daily(
            &mut scope,
            &[MetricsDailyRow {
                tenant_id,
                connection_id: conn_id,
                platform: PlatformKey::parse("google_ads").unwrap(),
                campaign_id: camp_id,
                date: today,
                impressions: 1000,
                clicks: 100,
                spend: Money::new(85_000, "JPY").unwrap(),
                conversions: 5.0,
                conversion_value: Money::new(100_000, "JPY").unwrap(),
            }],
        )
        .await
        .unwrap();
    scope.commit().await.unwrap();

    // Dry-run setting 100,000 JPY cap (spend is 85%):
    let cmd = PutBudgetCapCommand {
        period: BudgetPeriod::Daily,
        amount: Money::new(100_000, "JPY").unwrap(),
        declared_fx_basis: None,
        fx_rate_date: None,
        auto_pause: true,
        auto_resume_on_rollover: false,
        confirm_below_current_spend: false,
        dry_run: true,
    };
    let res = harness
        .put_cap
        .execute(tenant_id, Some(camp_id), cmd)
        .await
        .unwrap();
    let sim = match res {
        sanvi_advertising::application::PutBudgetCapResponse::DryRun(sim) => sim,
        _ => panic!("expected dry run"),
    };
    assert_eq!(sim.current_spend.amount_minor, 85_000);
    assert!(sim.would_breach_80);
    assert!(!sim.would_breach_100);
    assert!(!sim.would_auto_pause);

    // Verify cap was NOT persisted
    let caps = harness.get_caps.execute(tenant_id).await.unwrap();
    assert!(caps.is_empty());
}

#[tokio::test]
async fn test_threshold_evaluator_dedup_and_auto_pause() {
    let (harness, tenant_id) = TestHarness::new().await;
    let (conn_id, camp_id) =
        seed_connection_and_campaign(&harness.pool, &harness.platforms, tenant_id, "JPY").await;
    let today = Utc::now().date_naive();

    // Put a daily cap of 100,000 JPY with auto_pause = true
    let cmd = PutBudgetCapCommand {
        period: BudgetPeriod::Daily,
        amount: Money::new(100_000, "JPY").unwrap(),
        declared_fx_basis: None,
        fx_rate_date: None,
        auto_pause: true,
        auto_resume_on_rollover: false,
        confirm_below_current_spend: false,
        dry_run: false,
    };
    harness
        .put_cap
        .execute(tenant_id, Some(camp_id), cmd)
        .await
        .unwrap();

    // 1. Spend is 85,000 JPY (85% -> triggers 80% warning)
    let mut scope = harness.tx.begin_for(tenant_id.as_i64()).await.unwrap();
    harness
        .metrics_repo
        .upsert_daily(
            &mut scope,
            &[MetricsDailyRow {
                tenant_id,
                connection_id: conn_id,
                platform: PlatformKey::parse("google_ads").unwrap(),
                campaign_id: camp_id,
                date: today,
                impressions: 1000,
                clicks: 100,
                spend: Money::new(85_000, "JPY").unwrap(),
                conversions: 5.0,
                conversion_value: Money::new(100_000, "JPY").unwrap(),
            }],
        )
        .await
        .unwrap();
    scope.commit().await.unwrap();

    harness.evaluator.evaluate_tenant(tenant_id).await.unwrap();

    let alerts = harness
        .list_alerts
        .execute(tenant_id, Some(camp_id), false, None)
        .await
        .unwrap();
    assert_eq!(alerts.len(), 1);
    assert_eq!(alerts[0].threshold, 80);
    assert!(!alerts[0].auto_paused);

    // Run evaluator again -> verify deduplication (still 1 alert)
    harness.evaluator.evaluate_tenant(tenant_id).await.unwrap();
    let alerts_after = harness
        .list_alerts
        .execute(tenant_id, Some(camp_id), false, None)
        .await
        .unwrap();
    assert_eq!(alerts_after.len(), 1);

    // 2. Spend is 130,000 JPY against a 100,000 cap on a freshly synced connection.
    // For a Daily cap those rows are settled (the restatement window covers the whole
    // period, so freshness is the gate), so this is a SettledBreach and AUTO-PAUSE.
    let mut scope = harness.tx.begin_for(tenant_id.as_i64()).await.unwrap();
    harness
        .metrics_repo
        .upsert_daily(
            &mut scope,
            &[MetricsDailyRow {
                tenant_id,
                connection_id: conn_id,
                platform: PlatformKey::parse("google_ads").unwrap(),
                campaign_id: camp_id,
                date: today,
                impressions: 1000,
                clicks: 100,
                spend: Money::new(130_000, "JPY").unwrap(),
                conversions: 5.0,
                conversion_value: Money::new(100_000, "JPY").unwrap(),
            }],
        )
        .await
        .unwrap();
    scope.commit().await.unwrap();

    harness.evaluator.evaluate_tenant(tenant_id).await.unwrap();

    let alerts_100 = harness
        .list_alerts
        .execute(tenant_id, Some(camp_id), false, None)
        .await
        .unwrap();
    let alert_100 = alerts_100
        .iter()
        .find(|a| a.threshold == 100)
        .expect("100% alert");
    assert_eq!(alert_100.condition, AlertCondition::SettledBreach);
    assert!(alert_100.auto_paused);

    // Verify campaign is now Paused with change log attribution
    let mut scope = harness.tx.begin_for(tenant_id.as_i64()).await.unwrap();
    let updated_camp = harness
        .campaigns_repo
        .find(&mut scope, tenant_id, camp_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(updated_camp.intent.status, CampaignStatus::Paused);

    let changes = harness
        .campaigns_repo
        .list_changes(&mut scope, tenant_id, camp_id, 10)
        .await
        .unwrap();
    let pause_change = changes
        .iter()
        .find(|c| c.after_state.status == CampaignStatus::Paused)
        .expect("pause change log");
    assert_eq!(
        pause_change.attribution.as_deref(),
        Some("system:budget_guardrail")
    );
    assert!(pause_change.metadata.is_some());
}

#[tokio::test]
async fn test_stale_data_guard_no_pause_on_stale_connections() {
    let (harness, tenant_id) = TestHarness::new().await;
    let (conn_id, camp_id) =
        seed_connection_and_campaign(&harness.pool, &harness.platforms, tenant_id, "JPY").await;
    let today = Utc::now().date_naive();

    // Mark connection as stale (last synced 10 hours ago)
    let mut scope = harness.tx.begin_for(tenant_id.as_i64()).await.unwrap();
    let mut conn = harness
        .connections_repo
        .find_by_id(&mut scope, tenant_id, conn_id)
        .await
        .unwrap()
        .unwrap();
    conn.record_refresh_success(None, Utc::now() - Duration::hours(10));
    conn.record_metrics_sync(Utc::now() - Duration::hours(10));
    harness
        .connections_repo
        .upsert(&mut scope, &conn)
        .await
        .unwrap();

    // Insert 150,000 JPY settled spend
    harness
        .metrics_repo
        .upsert_daily(
            &mut scope,
            &[MetricsDailyRow {
                tenant_id,
                connection_id: conn_id,
                platform: PlatformKey::parse("google_ads").unwrap(),
                campaign_id: camp_id,
                date: today,
                impressions: 1000,
                clicks: 100,
                spend: Money::new(150_000, "JPY").unwrap(),
                conversions: 5.0,
                conversion_value: Money::new(100_000, "JPY").unwrap(),
            }],
        )
        .await
        .unwrap();
    scope.commit().await.unwrap();

    // Put cap with auto_pause = true
    let cmd = PutBudgetCapCommand {
        period: BudgetPeriod::Daily,
        amount: Money::new(100_000, "JPY").unwrap(),
        declared_fx_basis: None,
        fx_rate_date: None,
        auto_pause: true,
        auto_resume_on_rollover: false,
        confirm_below_current_spend: true,
        dry_run: false,
    };
    harness
        .put_cap
        .execute(tenant_id, Some(camp_id), cmd)
        .await
        .unwrap();

    harness.evaluator.evaluate_tenant(tenant_id).await.unwrap();

    let alerts = harness
        .list_alerts
        .execute(tenant_id, Some(camp_id), false, None)
        .await
        .unwrap();
    let alert = alerts
        .iter()
        .find(|a| a.threshold == 100)
        .expect("100% alert");
    assert_eq!(alert.condition, AlertCondition::StaleDataWarning);
    assert!(!alert.auto_paused);

    // Verify campaign is NOT paused
    let mut scope = harness.tx.begin_for(tenant_id.as_i64()).await.unwrap();
    let camp = harness
        .campaigns_repo
        .find(&mut scope, tenant_id, camp_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(camp.intent.status, CampaignStatus::Active);
}

/// Regression test for the 100% dedupe key.
///
/// A `StaleDataWarning` alert fired early in the period must not suppress the
/// auto-pause that a later, settled breach of the same cap requires. Before the
/// fix, `has_alert_fired(cap, period_start, 100)` returned true on the second
/// evaluation and the pause branch was never reached, so the campaign spent
/// unbounded for the rest of the period.
#[tokio::test]
async fn test_stale_alert_does_not_suppress_later_settled_pause() {
    let (harness, tenant_id) = TestHarness::new().await;
    let (conn_id, camp_id) =
        seed_connection_and_campaign(&harness.pool, &harness.platforms, tenant_id, "JPY").await;
    let today = Utc::now().date_naive();

    // First evaluation runs against a stale connection and over-cap spend.
    let mut scope = harness.tx.begin_for(tenant_id.as_i64()).await.unwrap();
    let mut conn = harness
        .connections_repo
        .find_by_id(&mut scope, tenant_id, conn_id)
        .await
        .unwrap()
        .unwrap();
    conn.record_refresh_success(None, Utc::now() - Duration::hours(10));
    conn.record_metrics_sync(Utc::now() - Duration::hours(10));
    harness
        .connections_repo
        .upsert(&mut scope, &conn)
        .await
        .unwrap();

    harness
        .metrics_repo
        .upsert_daily(
            &mut scope,
            &[MetricsDailyRow {
                tenant_id,
                connection_id: conn_id,
                platform: PlatformKey::parse("google_ads").unwrap(),
                campaign_id: camp_id,
                date: today,
                impressions: 1000,
                clicks: 100,
                spend: Money::new(150_000, "JPY").unwrap(),
                conversions: 5.0,
                conversion_value: Money::new(100_000, "JPY").unwrap(),
            }],
        )
        .await
        .unwrap();
    scope.commit().await.unwrap();

    let cmd = PutBudgetCapCommand {
        period: BudgetPeriod::Daily,
        amount: Money::new(100_000, "JPY").unwrap(),
        declared_fx_basis: None,
        fx_rate_date: None,
        auto_pause: true,
        auto_resume_on_rollover: false,
        confirm_below_current_spend: true,
        dry_run: false,
    };
    harness
        .put_cap
        .execute(tenant_id, Some(camp_id), cmd)
        .await
        .unwrap();

    harness.evaluator.evaluate_tenant(tenant_id).await.unwrap();

    // Precondition: the stale guard fired and did not pause.
    let alerts = harness
        .list_alerts
        .execute(tenant_id, Some(camp_id), false, None)
        .await
        .unwrap();
    let stale_alert = alerts
        .iter()
        .find(|a| a.threshold == 100)
        .expect("100% stale alert");
    assert_eq!(stale_alert.condition, AlertCondition::StaleDataWarning);
    assert!(!stale_alert.auto_paused);

    let mut scope = harness.tx.begin_for(tenant_id.as_i64()).await.unwrap();
    let camp = harness
        .campaigns_repo
        .find(&mut scope, tenant_id, camp_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        camp.intent.status,
        CampaignStatus::Active,
        "stale data must not pause"
    );

    // Data settles: the same connection syncs successfully, nothing else changes.
    let mut conn = harness
        .connections_repo
        .find_by_id(&mut scope, tenant_id, conn_id)
        .await
        .unwrap()
        .unwrap();
    conn.record_refresh_success(None, Utc::now());
    conn.record_metrics_sync(Utc::now());
    harness
        .connections_repo
        .upsert(&mut scope, &conn)
        .await
        .unwrap();
    scope.commit().await.unwrap();

    harness.evaluator.evaluate_tenant(tenant_id).await.unwrap();

    let mut scope = harness.tx.begin_for(tenant_id.as_i64()).await.unwrap();
    let camp_after = harness
        .campaigns_repo
        .find(&mut scope, tenant_id, camp_id)
        .await
        .unwrap()
        .unwrap();
    scope.commit().await.unwrap();
    assert_eq!(
        camp_after.intent.status,
        CampaignStatus::Paused,
        "a prior StaleDataWarning must not suppress the settled-breach auto-pause"
    );
}

/// Regression test for daily-cap settled classification.
///
/// Every metrics row for a daily cap is dated today, so classifying `date == today`
/// as provisional made `settled` permanently zero: the settled-breach branch was
/// unreachable and a daily cap could only pause via the provisional lower bound,
/// i.e. at 125% of the cap. Spend here is exactly 100% of the cap.
#[tokio::test]
async fn test_daily_cap_pauses_at_one_hundred_percent() {
    let (harness, tenant_id) = TestHarness::new().await;
    let (conn_id, camp_id) =
        seed_connection_and_campaign(&harness.pool, &harness.platforms, tenant_id, "JPY").await;
    let today = Utc::now().date_naive();

    // Fresh connection (seeded as synced now) and spend exactly at the cap.
    let mut scope = harness.tx.begin_for(tenant_id.as_i64()).await.unwrap();
    harness
        .metrics_repo
        .upsert_daily(
            &mut scope,
            &[MetricsDailyRow {
                tenant_id,
                connection_id: conn_id,
                platform: PlatformKey::parse("google_ads").unwrap(),
                campaign_id: camp_id,
                date: today,
                impressions: 1000,
                clicks: 100,
                spend: Money::new(100_000, "JPY").unwrap(),
                conversions: 5.0,
                conversion_value: Money::new(100_000, "JPY").unwrap(),
            }],
        )
        .await
        .unwrap();
    scope.commit().await.unwrap();

    let cmd = PutBudgetCapCommand {
        period: BudgetPeriod::Daily,
        amount: Money::new(100_000, "JPY").unwrap(),
        declared_fx_basis: None,
        fx_rate_date: None,
        auto_pause: true,
        auto_resume_on_rollover: false,
        confirm_below_current_spend: true,
        dry_run: false,
    };
    harness
        .put_cap
        .execute(tenant_id, Some(camp_id), cmd)
        .await
        .unwrap();

    harness.evaluator.evaluate_tenant(tenant_id).await.unwrap();

    let alerts = harness
        .list_alerts
        .execute(tenant_id, Some(camp_id), false, None)
        .await
        .unwrap();
    let alert = alerts
        .iter()
        .find(|a| a.threshold == 100)
        .expect("100% alert");
    assert_eq!(
        alert.condition,
        AlertCondition::SettledBreach,
        "today's non-restating rows must count as settled for a daily cap"
    );
    assert!(alert.auto_paused, "settled breach at 100% must auto-pause");

    let mut scope = harness.tx.begin_for(tenant_id.as_i64()).await.unwrap();
    let camp = harness
        .campaigns_repo
        .find(&mut scope, tenant_id, camp_id)
        .await
        .unwrap()
        .unwrap();
    scope.commit().await.unwrap();
    assert_eq!(
        camp.intent.status,
        CampaignStatus::Paused,
        "daily cap must pause at 100%, not 125%"
    );
}

#[tokio::test]
async fn test_spend_status_and_run_rate_projection() {
    let (harness, tenant_id) = TestHarness::new().await;
    let (conn_id, camp_id) =
        seed_connection_and_campaign(&harness.pool, &harness.platforms, tenant_id, "JPY").await;
    let today = Utc::now().date_naive();

    // Put a daily cap of 100,000 JPY
    let cmd = PutBudgetCapCommand {
        period: BudgetPeriod::Daily,
        amount: Money::new(100_000, "JPY").unwrap(),
        declared_fx_basis: None,
        fx_rate_date: None,
        auto_pause: true,
        auto_resume_on_rollover: false,
        confirm_below_current_spend: false,
        dry_run: false,
    };
    harness
        .put_cap
        .execute(tenant_id, Some(camp_id), cmd)
        .await
        .unwrap();

    // Seed 40,000 JPY spend
    let mut scope = harness.tx.begin_for(tenant_id.as_i64()).await.unwrap();
    harness
        .metrics_repo
        .upsert_daily(
            &mut scope,
            &[MetricsDailyRow {
                tenant_id,
                connection_id: conn_id,
                platform: PlatformKey::parse("google_ads").unwrap(),
                campaign_id: camp_id,
                date: today,
                impressions: 1000,
                clicks: 100,
                spend: Money::new(40_000, "JPY").unwrap(),
                conversions: 5.0,
                conversion_value: Money::new(80_000, "JPY").unwrap(),
            }],
        )
        .await
        .unwrap();
    scope.commit().await.unwrap();

    let report = harness
        .status_query
        .execute(tenant_id, Some(BudgetPeriod::Daily))
        .await
        .unwrap();
    assert_eq!(report.items.len(), 2); // 1 tenant-level, 1 campaign-level

    let camp_item = report
        .items
        .iter()
        .find(|i| i.campaign_id == Some(camp_id))
        .expect("camp item");
    assert_eq!(camp_item.spend_to_date.amount_minor, 40_000);
    assert_eq!(camp_item.percentage, Some(40.0));
    assert!(camp_item.projected_spend.amount_minor >= 40_000);
}

#[tokio::test]
/// Finding 3 / FR-1013: guardrails never auto-resume a paused campaign on rollover.
///
/// This is the mandatory test required by the REVIEW-FINDINGS.md working rules:
/// "manually paused campaign is NOT resumed by rollover".
async fn test_rollover_sweep_never_auto_resumes() {
    let (harness, tenant_id) = TestHarness::new().await;
    let (_conn_id, camp_id) =
        seed_connection_and_campaign(&harness.pool, &harness.platforms, tenant_id, "JPY").await;

    // Manually pause campaign (simulating a guardrail-caused pause)
    let mut scope = harness.tx.begin_for(tenant_id.as_i64()).await.unwrap();
    let mut camp = harness
        .campaigns_repo
        .find(&mut scope, tenant_id, camp_id)
        .await
        .unwrap()
        .unwrap();
    camp.intent.status = CampaignStatus::Paused;
    camp.remote_snapshot.status = CampaignStatus::Paused;
    harness
        .campaigns_repo
        .update(&mut scope, &camp, 1)
        .await
        .unwrap();
    scope.commit().await.unwrap();

    // Case 1: cap with auto_resume_on_rollover = false → sweep returns 0
    let cmd_no_resume = PutBudgetCapCommand {
        period: BudgetPeriod::Daily,
        amount: Money::new(100_000, "JPY").unwrap(),
        declared_fx_basis: None,
        fx_rate_date: None,
        auto_pause: true,
        auto_resume_on_rollover: false,
        confirm_below_current_spend: false,
        dry_run: false,
    };
    harness
        .put_cap
        .execute(tenant_id, Some(camp_id), cmd_no_resume)
        .await
        .unwrap();

    let boundary_recorded = harness
        .rollover_sweep
        .sweep_tenant(tenant_id, BudgetPeriod::Daily)
        .await
        .unwrap();
    assert!(
        boundary_recorded,
        "first sweep of a period records the boundary"
    );

    let mut scope = harness.tx.begin_for(tenant_id.as_i64()).await.unwrap();
    let camp = harness
        .campaigns_repo
        .find(&mut scope, tenant_id, camp_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        camp.intent.status,
        CampaignStatus::Paused,
        "campaign must remain paused after rollover sweep"
    );
    scope.commit().await.unwrap();

    // Case 2: cap with auto_resume_on_rollover = true → sweep STILL returns 0 (FR-1013)
    let cmd_with_resume = PutBudgetCapCommand {
        period: BudgetPeriod::Daily,
        amount: Money::new(100_000, "JPY").unwrap(),
        declared_fx_basis: None,
        fx_rate_date: None,
        auto_pause: true,
        auto_resume_on_rollover: true,
        confirm_below_current_spend: false,
        dry_run: false,
    };
    harness
        .put_cap
        .execute(tenant_id, Some(camp_id), cmd_with_resume)
        .await
        .unwrap();

    // FR-1013: "never auto-resumes" — the auto_resume_on_rollover flag is a no-op.
    // This second sweep lands inside the period already recorded above, so it is
    // also the boundary-guard regression test: a tick that crosses no boundary
    // must do nothing.
    let boundary_recorded_again = harness
        .rollover_sweep
        .sweep_tenant(tenant_id, BudgetPeriod::Daily)
        .await
        .unwrap();
    assert!(
        !boundary_recorded_again,
        "a second sweep within the same period must be a no-op"
    );

    let mut scope = harness.tx.begin_for(tenant_id.as_i64()).await.unwrap();
    let camp_still_paused = harness
        .campaigns_repo
        .find(&mut scope, tenant_id, camp_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        camp_still_paused.intent.status,
        CampaignStatus::Paused,
        "campaign must still be paused after rollover sweep regardless of auto_resume_on_rollover"
    );
}

#[tokio::test]
async fn test_alert_acknowledgment() {
    let (harness, tenant_id) = TestHarness::new().await;
    let (_conn_id, camp_id) =
        seed_connection_and_campaign(&harness.pool, &harness.platforms, tenant_id, "JPY").await;
    let user_id = test_actor();

    // Record an alert
    let mut scope = harness.tx.begin_for(tenant_id.as_i64()).await.unwrap();
    let alert_id = harness.ids.next_budget_alert_id().unwrap();
    let alert = sanvi_advertising::domain::model::BudgetAlert {
        id: alert_id,
        tenant_id,
        cap_id: None,
        campaign_id: Some(camp_id),
        period: BudgetPeriod::Daily,
        period_start: Utc::now().date_naive(),
        threshold: 80,
        condition: AlertCondition::Threshold80,
        spend: Money::new(80_000, "JPY").unwrap(),
        cap_amount: Money::new(100_000, "JPY").unwrap(),
        data_freshness: DataFreshness {
            last_synced_at: Some(Utc::now()),
            lag_hours: Some(0.1),
            is_settled: true,
            is_stale: false,
        },
        auto_paused: false,
        acknowledged_at: None,
        acknowledged_by: None,
        created_at: Utc::now(),
    };
    harness
        .alerts_repo
        .record_alert(&mut scope, &alert)
        .await
        .unwrap();
    scope.commit().await.unwrap();

    // List unacknowledged -> contains alert
    let unacked = harness
        .list_alerts
        .execute(tenant_id, Some(camp_id), true, None)
        .await
        .unwrap();
    assert_eq!(unacked.len(), 1);

    // Acknowledge alert
    let acked = harness
        .ack_alert
        .execute(tenant_id, alert_id, user_id)
        .await
        .unwrap();
    assert!(acked.acknowledged_at.is_some());
    assert_eq!(acked.acknowledged_by, Some(user_id));

    // List unacknowledged -> empty now
    let unacked_after = harness
        .list_alerts
        .execute(tenant_id, Some(camp_id), true, None)
        .await
        .unwrap();
    assert!(unacked_after.is_empty());
}

#[tokio::test]
async fn test_budget_guardrails_migration_down_and_up() {
    let db = sanvi_testsupport::test_db().await;
    let migrator_pool = db.migrator_pool().await;
    let migrator = Migrator::new(migrations_dir());

    // Rollback budget guardrails migration
    migrator
        .undo(&migrator_pool, ADVERTISING_BUDGET_MIGRATION - 1)
        .await
        .unwrap();

    // Verify budget_caps table is gone
    let caps_exists = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (
            SELECT FROM information_schema.tables 
            WHERE table_schema = 'advertising' AND table_name = 'budget_caps'
        )",
    )
    .fetch_one(&migrator_pool)
    .await
    .unwrap();
    assert!(!caps_exists);

    // Re-run migration
    migrator.run(&migrator_pool).await.unwrap();

    let caps_exists_again = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (
            SELECT FROM information_schema.tables 
            WHERE table_schema = 'advertising' AND table_name = 'budget_caps'
        )",
    )
    .fetch_one(&migrator_pool)
    .await
    .unwrap();
    assert!(caps_exists_again);
}

#[tokio::test]
async fn test_spend_anomaly_detector_persists_budget_alert_row_and_events() {
    let db = sanvi_testsupport::test_db().await;
    let migrator_pool = db.migrator_pool().await;
    sqlx::query(
        "SELECT advertising.ensure_monthly_partitions((CURRENT_DATE - INTERVAL '1 month')::date)",
    )
    .execute(&migrator_pool)
    .await
    .unwrap();
    sqlx::query("SELECT advertising.ensure_monthly_partitions(CURRENT_DATE)")
        .execute(&migrator_pool)
        .await
        .unwrap();

    let pool = db.app_pool();
    let tenant_id = seed_tenant(&pool, "anomaly-tenant").await;
    let platforms = FakeAdPlatform::registered_catalog();
    let (conn_id, campaign_id) =
        seed_connection_and_campaign(&pool, &platforms, tenant_id, "JPY").await;

    let tx = TxManager::new(pool.clone(), sanvi_testsupport::test_ids());
    let metrics_repo = Arc::new(SqlxMetricsRepository::new());
    let campaigns = Arc::new(SqlxCampaignRepository::new());
    let alerts = Arc::new(SqlxBudgetAlertRepository::new());
    let events = Arc::new(OutboxEventPublisher::new(sanvi_testsupport::test_ids()));
    let ids = Arc::new(SystemIdGenerator::new(sanvi_testsupport::test_ids()));
    let notifier = Arc::new(LoggingBudgetNotifier::new());

    let detector = SpendAnomalyDetector::new(
        tx.clone(),
        alerts.clone(),
        metrics_repo.clone(),
        campaigns.clone(),
        events.clone(),
        ids.clone(),
        notifier.clone(),
        7,
    );

    let now = Utc::now();
    let today = now.date_naive();
    let platform = FakeAdPlatform::fixtures()[0].key.clone();

    // Seed historical spend rows: 5 days with spend 1000 JPY
    let mut daily_rows = Vec::new();
    for day_offset in 1..=5 {
        let date = today - chrono::Duration::days(day_offset);
        daily_rows.push(MetricsDailyRow {
            tenant_id,
            connection_id: conn_id,
            campaign_id,
            platform: platform.clone(),
            date,
            impressions: 100,
            clicks: 10,
            spend: Money::new(1000, "JPY").unwrap(),
            conversions: 1.0,
            conversion_value: Money::new(5000, "JPY").unwrap(),
        });
    }
    // Seed today with 10,000 JPY (huge variance > 3sigma vs mean of 1000 with std_dev ~0)
    daily_rows.push(MetricsDailyRow {
        tenant_id,
        connection_id: conn_id,
        campaign_id,
        platform: platform.clone(),
        date: today,
        impressions: 1000,
        clicks: 100,
        spend: Money::new(10_000, "JPY").unwrap(),
        conversions: 10.0,
        conversion_value: Money::new(50_000, "JPY").unwrap(),
    });

    let mut scope = tx.begin_for(tenant_id.as_i64()).await.unwrap();
    metrics_repo
        .upsert_daily(&mut scope, &daily_rows)
        .await
        .unwrap();
    scope.commit().await.unwrap();

    // Run anomaly detection
    detector
        .detect_anomalies_for_tenant(tenant_id)
        .await
        .unwrap();

    // Verify alert row is persisted
    let mut scope = tx.begin_for(tenant_id.as_i64()).await.unwrap();
    let alert_rows = alerts
        .list_alerts(&mut scope, tenant_id, Some(campaign_id), false, 10)
        .await
        .unwrap();
    scope.commit().await.unwrap();

    assert_eq!(
        alert_rows.len(),
        1,
        "expected exactly one anomaly alert persisted"
    );
    let alert = &alert_rows[0];
    assert_eq!(alert.condition, AlertCondition::SpendAnomaly);
    assert_eq!(alert.spend.amount_minor, 10_000);
    assert_eq!(alert.cap_amount.amount_minor, 0);
    assert_eq!(alert.threshold, 100);
    assert_eq!(alert.period_start, today);
    assert_eq!(alert.campaign_id, Some(campaign_id));
    assert_eq!(alert.cap_id, None);

    // Verify outbox event was published
    let event_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM platform.outbox WHERE aggregate_id = $1 AND event_type = 'advertising.spend_threshold_reached'",
    )
    .bind(campaign_id.as_i64())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(
        event_count >= 1,
        "expected spend threshold reached event in outbox"
    );

    // Run anomaly detection again on same day - deduplication should prevent second row
    detector
        .detect_anomalies_for_tenant(tenant_id)
        .await
        .unwrap();

    let mut scope = tx.begin_for(tenant_id.as_i64()).await.unwrap();
    let alert_rows_after = alerts
        .list_alerts(&mut scope, tenant_id, Some(campaign_id), false, 10)
        .await
        .unwrap();
    scope.commit().await.unwrap();

    assert_eq!(
        alert_rows_after.len(),
        1,
        "deduplication must prevent duplicate alerts on same day"
    );
}
