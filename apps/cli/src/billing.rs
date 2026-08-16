//! Phase-04 operator commands: billing plans, Stripe Tax check,
//! reconciliation and webhook backlog. All commands run under the audited
//! platform scope with the `platform-admin` CLI actor.

use std::sync::Arc;

use sanvi_billing::{
    adapters::{
        AccessEntitlementSyncer, HttpStripeClient, LoggingNotifications, OutboxEventPublisher,
        SqlxInvoiceRepository, SqlxPlanEntitlementRepository, SqlxPlanPriceRepository,
        SqlxPlanRepository, SqlxSubscriptionRepository, SqlxWebhookEventRepository, SystemClock,
        SystemIdGenerator, TenancyTenantLifecycle,
    },
    application::{
        CheckTaxRegistrations, DunningSweep, ListPlans, ReconcileSubscriptions, WebhookDispatcher,
    },
    domain::{
        Clock, EntitlementSyncer, EventPublisher, IdGenerator, InvoiceRepository, Notifications,
        PlanEntitlementRepository, PlanPriceRepository, PlanRepository, StripeClient,
        SubscriptionRepository, TenantLifecycle, WebhookEventRepository,
    },
};
use sanvi_persistence::TxManager;

/// Load configuration and connect.
async fn connect(
    environment: &str,
) -> anyhow::Result<(sanvi_persistence::PgPool, sanvi_billing::BillingConfig)> {
    let config = crate::config::load(environment)?;
    let pool = sanvi_persistence::build_pool(&config.database).await?;
    Ok((pool, config.billing))
}

/// Build the full billing stack (repos + stripe + syncer + lifecycle +
/// dispatcher) for CLI commands.
#[allow(clippy::too_many_arguments)]
async fn build_stack(
    pool: sanvi_persistence::PgPool,
    config: &sanvi_billing::BillingConfig,
) -> anyhow::Result<(
    TxManager,
    Arc<WebhookDispatcher>,
    Arc<ListPlans>,
    Arc<CheckTaxRegistrations>,
    Arc<ReconcileSubscriptions>,
    Arc<DunningSweep>,
    Arc<dyn WebhookEventRepository>,
)> {
    let tx = TxManager::new(pool.clone());
    let plans: Arc<dyn PlanRepository> = Arc::new(SqlxPlanRepository::new());
    let prices: Arc<dyn PlanPriceRepository> = Arc::new(SqlxPlanPriceRepository::new());
    let grants: Arc<dyn PlanEntitlementRepository> = Arc::new(SqlxPlanEntitlementRepository::new());
    let subscriptions: Arc<dyn SubscriptionRepository> =
        Arc::new(SqlxSubscriptionRepository::new());
    let invoices: Arc<dyn InvoiceRepository> = Arc::new(SqlxInvoiceRepository::new());
    let webhooks: Arc<dyn WebhookEventRepository> = Arc::new(SqlxWebhookEventRepository::new());
    let events: Arc<dyn EventPublisher> = Arc::new(OutboxEventPublisher::new());
    let notifications: Arc<dyn Notifications> = Arc::new(LoggingNotifications::new());
    let ids: Arc<dyn IdGenerator> = Arc::new(SystemIdGenerator);
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let entitlements: Arc<dyn sanvi_access::domain::EntitlementRepository> =
        Arc::new(sanvi_access::adapters::SqlxEntitlementRepository::new());
    let syncer: Arc<dyn EntitlementSyncer> = Arc::new(AccessEntitlementSyncer::new(entitlements));
    let tenants: Arc<dyn sanvi_tenancy::domain::TenantRepository> =
        Arc::new(sanvi_tenancy::adapters::repo::SqlxTenantRepository::new());
    let lifecycle: Arc<dyn TenantLifecycle> = Arc::new(TenancyTenantLifecycle::new(
        tx.clone(),
        tenants,
        Arc::new(sanvi_tenancy::adapters::OutboxEventPublisher::new()),
        Arc::new(sanvi_tenancy::adapters::SystemClock),
    ));
    let stripe: Arc<dyn StripeClient> = Arc::new(HttpStripeClient::new(
        config.stripe_api_base.clone(),
        config.stripe_api_key.clone(),
        config.stripe_api_version.clone(),
        None,
    ));
    let dispatcher = Arc::new(WebhookDispatcher::new(
        tx.clone(),
        webhooks.clone(),
        subscriptions.clone(),
        invoices.clone(),
        prices.clone(),
        plans.clone(),
        grants.clone(),
        syncer.clone(),
        lifecycle.clone(),
        notifications.clone(),
        events.clone(),
        stripe.clone(),
        ids,
        clock.clone(),
    ));
    let list_plans = Arc::new(ListPlans::new(tx.clone(), plans, prices, grants));
    let tax_check = Arc::new(CheckTaxRegistrations::new(
        stripe.clone(),
        config.automatic_tax_enabled,
    ));
    let reconciliation = Arc::new(ReconcileSubscriptions::new(
        tx.clone(),
        subscriptions.clone(),
        stripe.clone(),
        dispatcher.clone(),
    ));
    let dunning = Arc::new(DunningSweep::new(
        tx.clone(),
        subscriptions,
        syncer,
        lifecycle,
        notifications,
        events,
        clock,
        config.dunning_grace_days,
        config.trial_reminder_days,
    ));
    Ok((
        tx,
        dispatcher,
        list_plans,
        tax_check,
        reconciliation,
        dunning,
        webhooks,
    ))
}

/// `billing plans` — the operator catalog.
pub async fn plans(environment: &str) -> anyhow::Result<()> {
    let (pool, config) = connect(environment).await?;
    let (_, _, list_plans, _, _, _, _) = build_stack(pool, &config).await?;
    let plans = list_plans
        .execute()
        .await
        .map_err(|err| anyhow::anyhow!(err))?;
    for plan in &plans {
        println!(
            "{}\t{}\t{}\t{}\t{} prices\t{} grants",
            plan.key,
            plan.name,
            plan.stripe_product_id,
            plan.visibility.as_str(),
            plan.prices.len(),
            plan.entitlements.len()
        );
    }
    Ok(())
}

/// `billing tax-check` — fail loudly when automatic_tax is enabled without
/// an active Stripe Tax registration.
pub async fn tax_check(environment: &str) -> anyhow::Result<()> {
    let (pool, config) = connect(environment).await?;
    let (_, _, _, check, _, _, _) = build_stack(pool, &config).await?;
    let registrations = check.execute().await.map_err(|err| anyhow::anyhow!(err))?;
    for registration in &registrations {
        println!("{}\t{}", registration.country, registration.id);
    }
    println!("{} active registrations", registrations.len());
    Ok(())
}

/// `billing reconcile` — one manual reconciliation pass (the worker does
/// this daily; the runbook uses this for incident repair).
pub async fn reconcile(environment: &str) -> anyhow::Result<()> {
    let (pool, config) = connect(environment).await?;
    let (_, _, _, _, reconciliation, _, _) = build_stack(pool, &config).await?;
    let report = reconciliation
        .execute(1000)
        .await
        .map_err(|err| anyhow::anyhow!(err))?;
    println!(
        "checked={} drift_repaired={} errors={}",
        report.checked, report.drift_repaired, report.errors
    );
    if report.errors > 0 {
        anyhow::bail!("reconciliation had errors; see logs");
    }
    Ok(())
}

/// `billing webhooks` — the store-first queue's backlog.
pub async fn webhooks(environment: &str) -> anyhow::Result<()> {
    let (pool, config) = connect(environment).await?;
    let (tx, _, _, _, _, _, webhooks) = build_stack(pool, &config).await?;
    let mut scope = tx
        .begin_unscoped("billing_webhook_status", "platform-admin")
        .await?;
    let pending = webhooks
        .unprocessed_count(&mut scope)
        .await
        .map_err(|err| anyhow::anyhow!(err))?;
    let dead = webhooks
        .dead_letter_count(&mut scope)
        .await
        .map_err(|err| anyhow::anyhow!(err))?;
    scope.commit().await?;
    println!("unprocessed={pending} dead_letter={dead}");
    Ok(())
}
