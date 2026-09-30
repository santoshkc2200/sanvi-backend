//! Phase-09 operator commands: the Connect webhook queue's status, its
//! dead-letter triage/replay, and manual connection reconciliation. All
//! commands run under the audited platform scope with the `platform-admin`
//! CLI actor.

use std::sync::Arc;

use sanvi_kernel::RawIdGenerator;
use sanvi_payments::{
    adapters::{
        OutboxEventPublisher as PaymentsEvents, SqlxProviderConnectionRepository,
        SqlxWebhookEventRepository,
    },
    application::{
        ProviderRegistry as PaymentsProviderRegistry,
        RuntimeEnvironment as PaymentsRuntimeEnvironment, SyncConnections,
    },
    domain::{
        ConnectionLifecycleStore, EventPublisher as PaymentsEventPublisher, WebhookEventRepository,
    },
};
use sanvi_persistence::TxManager;

/// Load configuration and connect.
async fn connect(
    environment: &str,
) -> anyhow::Result<(sanvi_persistence::PgPool, sanvi_payments::PaymentsConfig)> {
    let config = crate::config::load(environment)?;
    let pool = sanvi_persistence::build_pool(&config.database).await?;
    Ok((pool, config.payments))
}

/// The provider registry for provider-calling commands: the fake joins in
/// non-production environments, Stripe Connect when a restricted key is
/// configured — mirroring the API and worker composition roots.
fn build_registry(
    config: &sanvi_payments::PaymentsConfig,
    environment: &str,
) -> Arc<PaymentsProviderRegistry> {
    let mut providers = PaymentsProviderRegistry::new();
    if PaymentsRuntimeEnvironment::from_config(environment)
        == PaymentsRuntimeEnvironment::NonProduction
    {
        providers
            .register(Arc::new(sanvi_payments::adapters::FakeProvider::new()))
            .expect("the fake provider is the first registration");
    }
    if !config.connect_api_key.is_empty() {
        providers
            .register(Arc::new(
                sanvi_payments::adapters::StripeConnectProvider::new(
                    config.connect_api_base.clone(),
                    config.connect_api_key.expose_secret().clone(),
                    config.stripe_api_version.clone(),
                    config.connect_webhook_secret.expose_secret().clone(),
                    config.webhook_tolerance_secs,
                ),
            ))
            .expect("the stripe connect adapter registers");
    }
    Arc::new(providers)
}

/// `payments webhooks` — the store-first queue's backlog plus its
/// dead-letter head.
pub async fn webhooks(environment: &str, ids: Arc<dyn RawIdGenerator>) -> anyhow::Result<()> {
    let (pool, _) = connect(environment).await?;
    let tx = TxManager::new(pool, ids);
    let webhooks: Arc<dyn WebhookEventRepository> = Arc::new(SqlxWebhookEventRepository::new());
    let mut scope = tx
        .begin_unscoped("payments_webhook_status", "platform-admin")
        .await?;
    let pending = webhooks
        .unprocessed_count(&mut scope)
        .await
        .map_err(|err| anyhow::anyhow!(err))?;
    let dead = webhooks
        .dead_letter_count(&mut scope)
        .await
        .map_err(|err| anyhow::anyhow!(err))?;
    println!("unprocessed={pending} dead_letter={dead}");
    if dead > 0 {
        println!("event_id\taccount\ttype\tattempts\terror");
        for record in webhooks
            .dead_letters(&mut scope, 20)
            .await
            .map_err(|err| anyhow::anyhow!(err))?
        {
            println!(
                "{}\t{}\t{}\t{}\t{}",
                record.stripe_event_id,
                record.account_id.as_deref().unwrap_or("-"),
                record.event_type,
                record.attempts,
                record.last_error.as_deref().unwrap_or("-")
            );
        }
    }
    scope.commit().await?;
    Ok(())
}

/// `payments replay <event-id>` — requeue one dead-lettered event after
/// the root cause is fixed. A poison event never blocks the queue; this
/// is how it gets back in once it is no longer poison.
pub async fn replay(
    environment: &str,
    event_id: &str,
    ids: Arc<dyn RawIdGenerator>,
) -> anyhow::Result<()> {
    let (pool, _) = connect(environment).await?;
    let tx = TxManager::new(pool, ids);
    let webhooks: Arc<dyn WebhookEventRepository> = Arc::new(SqlxWebhookEventRepository::new());
    let mut scope = tx
        .begin_unscoped("replay_connect_webhook", "platform-admin")
        .await?;
    let requeued = webhooks
        .replay(&mut scope, event_id)
        .await
        .map_err(|err| anyhow::anyhow!(err))?;
    scope.commit().await?;
    if requeued {
        println!("requeued {event_id}");
    } else {
        anyhow::bail!("{event_id} is not dead-lettered (nothing to replay)");
    }
    Ok(())
}

/// `payments sync` — one manual reconciliation pass (the worker does this
/// hourly; the runbook uses this for incident repair and the lost-webhook
/// drill).
pub async fn sync(environment: &str, ids: Arc<dyn RawIdGenerator>) -> anyhow::Result<()> {
    let (pool, config) = connect(environment).await?;
    let tx = TxManager::new(pool, ids.clone());
    let lifecycle: Arc<dyn ConnectionLifecycleStore> =
        Arc::new(SqlxProviderConnectionRepository::new());
    let events: Arc<dyn PaymentsEventPublisher> = Arc::new(PaymentsEvents::new(ids));
    let registry = build_registry(&config, environment);
    // One full pass regardless of staleness: an operator-invoked sync is
    // explicit about wanting fresh truth.
    let sync = SyncConnections::new(tx, lifecycle, registry, events, chrono::Duration::zero());
    let report = sync
        .execute(1000)
        .await
        .map_err(|err| anyhow::anyhow!(err))?;
    println!(
        "checked={} updated={} errors={}",
        report.checked, report.updated, report.errors
    );
    if report.errors > 0 {
        anyhow::bail!("reconciliation had errors; see logs");
    }
    Ok(())
}
