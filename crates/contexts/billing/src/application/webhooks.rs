//! Webhook receiving and dispatch.
//!
//! Store-first: the receiver verifies the signature, persists the raw event
//! (PK on `stripe_event_id` = free deduplication) and returns immediately —
//! Stripe's redelivery contract is satisfied with a 2xx before processing
//! even starts. The worker's dispatcher drains unprocessed rows, applies
//! them idempotently, and guards against out-of-order delivery by comparing
//! event `created` timestamps with local `updated_at`.
//!
//! Webhooks are the truth for status; API responses are never used to grant
//! access.

use std::net::IpAddr;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use sanvi_persistence::TxManager;
use serde::Serialize;
use tracing::{error, warn};
use utoipa::ToSchema;

use crate::application::{entitlements_sync::sync_action, storage};
use crate::domain::{
    BillingError, Clock, EntitlementSyncer, EventPublisher, IdGenerator, InvoiceRef,
    InvoiceRepository, InvoiceStatus, PlanEntitlementRepository, PlanPriceRepository,
    PlanRepository, StripeClient, StripeInvoiceData, StripeSubscriptionData, Subscription,
    SubscriptionRepository, SubscriptionSource, SubscriptionStatus, TenantLifecycle,
    WebhookEventRecord, WebhookEventRepository,
};

const ACTOR: &str = "stripe-webhook";

/// How many processing attempts before a webhook row becomes dead-letter.
pub const MAX_WEBHOOK_ATTEMPTS: i32 = 5;

// ---------------------------------------------------------------------------
// Receiving (HTTP boundary → storage)
// ---------------------------------------------------------------------------

/// Verify the signature, store the event, answer Stripe immediately.
pub struct WebhookReceiver {
    tx_manager: TxManager,
    events: Arc<dyn WebhookEventRepository>,
    clock: Arc<dyn Clock>,
    secret: String,
    tolerance_secs: i64,
    allowed_clients: Vec<String>,
}

/// CIDR or exact match (same vocabulary as the identity context's webhook
/// receivers; empty allow-list = fail closed, never open).
fn client_allowed(allowed: &[String], client_ip: Option<&str>) -> bool {
    if allowed.is_empty() {
        return false;
    }
    let Some(client_ip) = client_ip else {
        return false;
    };
    allowed.iter().any(|entry| {
        if let Some((network, bits)) = entry.split_once('/') {
            match (network.parse::<std::net::Ipv4Addr>(), bits.parse::<u8>()) {
                (Ok(network), Ok(bits)) if bits <= 32 => {
                    let mask = u32::MAX << (32 - bits);
                    let network_u32 = u32::from(network) & mask;
                    client_ip
                        .parse::<std::net::Ipv4Addr>()
                        .map(|ip| u32::from(ip) & mask == network_u32)
                        .unwrap_or(false)
                }
                _ => false,
            }
        } else {
            entry == client_ip
        }
    })
}

impl WebhookReceiver {
    pub fn new(
        tx_manager: TxManager,
        events: Arc<dyn WebhookEventRepository>,
        clock: Arc<dyn Clock>,
        secret: String,
        tolerance_secs: i64,
        allowed_clients: Vec<String>,
    ) -> Self {
        Self {
            tx_manager,
            events,
            clock,
            secret,
            tolerance_secs,
            allowed_clients,
        }
    }

    /// Verify + store. Returns `true` when the event was newly stored
    /// (redeliveries return `false` without error — both answer 2xx).
    #[tracing::instrument(name = "billing.receive", skip_all)]
    pub async fn receive(
        &self,
        payload: &[u8],
        signature_header: Option<&str>,
        client_ip: Option<IpAddr>,
    ) -> Result<bool, BillingError> {
        if self.secret.is_empty() {
            return Err(BillingError::InvalidWebhookSignature(
                "no webhook secret configured".to_string(),
            ));
        }
        let Some(signature) = signature_header else {
            return Err(BillingError::InvalidWebhookSignature(
                "missing stripe-signature header".to_string(),
            ));
        };
        if !client_allowed(
            &self.allowed_clients,
            client_ip.map(|ip| ip.to_string()).as_deref(),
        ) {
            return Err(BillingError::InvalidWebhookSignature(
                "client address not allow-listed".to_string(),
            ));
        }
        crate::adapters::stripe::verify_webhook_signature(
            self.secret.as_bytes(),
            payload,
            signature,
            self.tolerance_secs,
        )?;

        let event: serde_json::Value = serde_json::from_slice(payload)
            .map_err(|err| BillingError::InvalidWebhookPayload(format!("not json: {err}")))?;
        let stripe_event_id = event
            .get("id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| BillingError::InvalidWebhookPayload("missing event id".to_string()))?
            .to_string();
        let event_type = event
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();
        let created = event
            .get("created")
            .and_then(|v| v.as_i64())
            .and_then(|epoch| DateTime::from_timestamp(epoch, 0))
            .unwrap_or_else(|| self.clock.now());

        let record = WebhookEventRecord {
            stripe_event_id,
            event_type,
            api_version: event
                .get("api_version")
                .and_then(|v| v.as_str())
                .map(str::to_string),
            payload: event,
            created,
            received_at: self.clock.now(),
            attempts: 0,
        };

        let mut scope = self
            .tx_manager
            .begin_unscoped("store_stripe_webhook", ACTOR)
            .await
            .map_err(storage)?;
        let inserted = self.events.insert_ignore(&mut scope, &record).await?;
        scope.commit().await.map_err(storage)?;
        Ok(inserted)
    }
}

// ---------------------------------------------------------------------------
// Dispatch (worker)
// ---------------------------------------------------------------------------

/// What one drain pass accomplished (logs + metrics).
#[derive(Debug, Clone, Copy, Default, Serialize, ToSchema)]
pub struct DispatchReport {
    pub processed: u32,
    /// Stored and acknowledged without state change (unknown types, stale
    /// redeliveries).
    pub skipped: u32,
    /// Attempts recorded against the dead-letter policy.
    pub failed: u32,
}

/// The parsed shape of one event, reduced to the fields handlers need.
enum ParsedEvent {
    CheckoutSessionCompleted {
        subscription_id: Option<String>,
        client_reference_id: Option<String>,
        created: DateTime<Utc>,
    },
    SubscriptionEvent {
        data: StripeSubscriptionData,
        created: DateTime<Utc>,
        deleted: bool,
    },
    InvoicePaid {
        data: StripeInvoiceData,
        created: DateTime<Utc>,
    },
    InvoicePaymentFailed {
        data: StripeInvoiceData,
        created: DateTime<Utc>,
    },
    InvoiceFinalized {
        data: StripeInvoiceData,
        created: DateTime<Utc>,
    },
    /// customer.updated, payment_method.*, charge.dispute.* — stored and
    /// acknowledged; no local state change.
    AcknowledgedOnly,
    Unknown,
}

fn parse_event(record: &WebhookEventRecord) -> Result<ParsedEvent, BillingError> {
    let payload = &record.payload;
    let object = payload
        .get("data")
        .and_then(|data| data.get("object"))
        .ok_or_else(|| BillingError::InvalidWebhookPayload("missing data.object".to_string()))?;
    let created = payload
        .get("created")
        .and_then(|v| v.as_i64())
        .and_then(|epoch| DateTime::from_timestamp(epoch, 0))
        .unwrap_or(record.created);

    let parse_subscription =
        |object: &serde_json::Value| -> Result<StripeSubscriptionData, BillingError> {
            let period = {
                let start = object.get("current_period_start").and_then(|v| v.as_i64());
                let end = object.get("current_period_end").and_then(|v| v.as_i64());
                crate::domain::period_from_range(start, end)
            };
            let stripe_price_id = object
                .get("items")
                .and_then(|items| items.get("data"))
                .and_then(|data| data.as_array())
                .and_then(|items| items.first())
                .and_then(|item| item.get("price"))
                .and_then(|price| price.get("id"))
                .and_then(|v| v.as_str())
                .map(str::to_string);
            let status = object
                .get("status")
                .and_then(|v| v.as_str())
                .ok_or_else(|| BillingError::InvalidWebhookPayload("missing status".to_string()))?;
            Ok(StripeSubscriptionData {
                id: object
                    .get("id")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| BillingError::InvalidWebhookPayload("missing id".to_string()))?
                    .to_string(),
                customer_id: object
                    .get("customer")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string(),
                status: SubscriptionStatus::parse(status)?,
                stripe_price_id,
                current_period: period,
                cancel_at_period_end: object
                    .get("cancel_at_period_end")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
                trial_end: object
                    .get("trial_end")
                    .and_then(|v| v.as_i64())
                    .and_then(|epoch| DateTime::from_timestamp(epoch, 0)),
                metadata: object
                    .get("metadata")
                    .cloned()
                    .unwrap_or(serde_json::json!({})),
                created,
            })
        };

    match record.event_type.as_str() {
        "checkout.session.completed" => Ok(ParsedEvent::CheckoutSessionCompleted {
            subscription_id: object
                .get("subscription")
                .and_then(|v| v.as_str())
                .map(str::to_string),
            client_reference_id: object
                .get("client_reference_id")
                .and_then(|v| v.as_str())
                .map(str::to_string),
            created,
        }),
        "customer.subscription.created"
        | "customer.subscription.updated"
        | "customer.subscription.paused"
        | "customer.subscription.resumed" => Ok(ParsedEvent::SubscriptionEvent {
            data: parse_subscription(object)?,
            created,
            deleted: false,
        }),
        "customer.subscription.deleted" => Ok(ParsedEvent::SubscriptionEvent {
            data: parse_subscription(object)?,
            created,
            deleted: true,
        }),
        "invoice.paid" => Ok(ParsedEvent::InvoicePaid {
            data: parse_invoice(object)?,
            created,
        }),
        "invoice.payment_failed" => Ok(ParsedEvent::InvoicePaymentFailed {
            data: parse_invoice(object)?,
            created,
        }),
        "invoice.finalized" => Ok(ParsedEvent::InvoiceFinalized {
            data: parse_invoice(object)?,
            created,
        }),
        "customer.updated"
        | "payment_method.attached"
        | "payment_method.detached"
        | "payment_method.updated"
        | "charge.dispute.created" => Ok(ParsedEvent::AcknowledgedOnly),
        _ => Ok(ParsedEvent::Unknown),
    }
}

fn parse_invoice(object: &serde_json::Value) -> Result<StripeInvoiceData, BillingError> {
    let status = object
        .get("status")
        .and_then(|v| v.as_str())
        .ok_or_else(|| BillingError::InvalidWebhookPayload("missing invoice status".to_string()))?;
    let created = object
        .get("created")
        .and_then(|v| v.as_i64())
        .and_then(|epoch| DateTime::from_timestamp(epoch, 0))
        .ok_or_else(|| {
            BillingError::InvalidWebhookPayload("missing invoice created".to_string())
        })?;
    Ok(StripeInvoiceData {
        id: object
            .get("id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| BillingError::InvalidWebhookPayload("missing invoice id".to_string()))?
            .to_string(),
        number: object
            .get("number")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        status: InvoiceStatus::parse(status)?,
        total_minor: object.get("total").and_then(|v| v.as_i64()).unwrap_or(0),
        currency: object
            .get("currency")
            .and_then(|v| v.as_str())
            .unwrap_or("usd")
            .to_string(),
        hosted_url: object
            .get("hosted_invoice_url")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        pdf_url: object
            .get("invoice_pdf")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        period: crate::domain::period_from_range(
            object.get("period_start").and_then(|v| v.as_i64()),
            object.get("period_end").and_then(|v| v.as_i64()),
        ),
        created,
        paid_at: object
            .get("status_transitions")
            .and_then(|t| t.get("paid_at"))
            .and_then(|v| v.as_i64())
            .and_then(|epoch| DateTime::from_timestamp(epoch, 0)),
        subscription_id: object
            .get("subscription")
            .and_then(|v| v.as_str())
            .map(str::to_string),
    })
}

/// The worker-side event dispatcher: drains unprocessed webhook rows and
/// applies each inside its own audited platform transaction.
pub struct WebhookDispatcher {
    tx_manager: TxManager,
    events_repo: Arc<dyn WebhookEventRepository>,
    subscriptions: Arc<dyn SubscriptionRepository>,
    invoices: Arc<dyn InvoiceRepository>,
    prices: Arc<dyn PlanPriceRepository>,
    plans: Arc<dyn PlanRepository>,
    grants_repo: Arc<dyn PlanEntitlementRepository>,
    syncer: Arc<dyn EntitlementSyncer>,
    lifecycle: Arc<dyn TenantLifecycle>,
    events: Arc<dyn EventPublisher>,
    stripe: Arc<dyn StripeClient>,
    ids: Arc<dyn IdGenerator>,
    clock: Arc<dyn Clock>,
    metrics: Option<crate::adapters::metrics::BillingMetrics>,
}

#[allow(clippy::too_many_arguments)]
impl WebhookDispatcher {
    pub fn new(
        tx_manager: TxManager,
        events_repo: Arc<dyn WebhookEventRepository>,
        subscriptions: Arc<dyn SubscriptionRepository>,
        invoices: Arc<dyn InvoiceRepository>,
        prices: Arc<dyn PlanPriceRepository>,
        plans: Arc<dyn PlanRepository>,
        grants_repo: Arc<dyn PlanEntitlementRepository>,
        syncer: Arc<dyn EntitlementSyncer>,
        lifecycle: Arc<dyn TenantLifecycle>,
        events: Arc<dyn EventPublisher>,
        stripe: Arc<dyn StripeClient>,
        ids: Arc<dyn IdGenerator>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            tx_manager,
            events_repo,
            subscriptions,
            invoices,
            prices,
            plans,
            grants_repo,
            syncer,
            lifecycle,
            events,
            stripe,
            ids,
            clock,
            metrics: None,
        }
    }

    /// Attach billing metrics (the worker does; tests do not need them).
    pub fn with_metrics(mut self, metrics: crate::adapters::metrics::BillingMetrics) -> Self {
        self.metrics = Some(metrics);
        self
    }

    /// Drain one batch. Every row is processed in its own transaction, so a
    /// poisoned event cannot take the batch down with it.
    #[tracing::instrument(name = "billing.dispatch", skip_all)]
    pub async fn dispatch(&self, limit: u32) -> Result<DispatchReport, BillingError> {
        let batch = {
            let mut scope = self
                .tx_manager
                .begin_unscoped("fetch_webhook_batch", ACTOR)
                .await
                .map_err(storage)?;
            let batch = self
                .events_repo
                .fetch_unprocessed(&mut scope, limit)
                .await?;
            scope.commit().await.map_err(storage)?;
            batch
        };

        let mut report = DispatchReport::default();
        for record in batch {
            match self.apply(&record).await {
                Ok(outcome) => {
                    if let Some(metrics) = &self.metrics {
                        metrics.observe_lag(record.received_at, self.clock.now());
                    }
                    match outcome {
                        ApplyOutcome::Applied => report.processed += 1,
                        ApplyOutcome::Skipped => report.skipped += 1,
                    }
                }
                Err(err) => {
                    report.failed += 1;
                    error!(
                        event = %record.stripe_event_id,
                        event_type = %record.event_type,
                        error = %err,
                        "webhook event processing failed",
                    );
                    let mut scope = match self
                        .tx_manager
                        .begin_unscoped("fail_webhook_event", ACTOR)
                        .await
                    {
                        Ok(scope) => scope,
                        Err(scope_err) => {
                            error!(error = %scope_err, "could not record webhook failure");
                            continue;
                        }
                    };
                    let _ = self
                        .events_repo
                        .mark_attempt_failed(&mut scope, &record.stripe_event_id, &err.to_string())
                        .await;
                    let _ = scope.commit().await;
                }
            }
        }
        Ok(report)
    }

    async fn apply(&self, record: &WebhookEventRecord) -> Result<ApplyOutcome, BillingError> {
        let event = parse_event(record)?;
        let mut scope = self
            .tx_manager
            .begin_unscoped("process_webhook_event", ACTOR)
            .await
            .map_err(storage)?;

        let outcome = match event {
            ParsedEvent::Unknown => ApplyOutcome::Skipped,
            ParsedEvent::AcknowledgedOnly => ApplyOutcome::Skipped,
            ParsedEvent::CheckoutSessionCompleted {
                subscription_id,
                client_reference_id,
                created,
            } => {
                self.apply_checkout_completed(
                    &mut scope,
                    subscription_id.as_deref(),
                    client_reference_id.as_deref(),
                    created,
                )
                .await?
            }
            ParsedEvent::SubscriptionEvent {
                data,
                created,
                deleted,
            } => {
                if deleted {
                    self.apply_subscription_deleted(&mut scope, &data, created)
                        .await?
                } else {
                    self.apply_subscription_update(&mut scope, &data, created)
                        .await?
                }
            }
            ParsedEvent::InvoicePaid { data, created } => {
                self.apply_invoice_paid(&mut scope, &data, created).await?
            }
            ParsedEvent::InvoicePaymentFailed { data, created } => {
                self.apply_invoice_failed(&mut scope, &data, created)
                    .await?
            }
            ParsedEvent::InvoiceFinalized { data, created } => {
                self.apply_invoice_finalized(&mut scope, &data, created)
                    .await?
            }
        };

        self.events_repo
            .mark_processed(&mut scope, &record.stripe_event_id)
            .await?;
        scope.commit().await.map_err(storage)?;
        Ok(outcome)
    }

    /// `checkout.session.completed`: the redirect never grants access; the
    /// subscription rows arrive via webhooks. When only the session event
    /// made it, fetch the subscription from Stripe and apply the same update
    /// path (never trust request-time API responses for status — but
    /// `retrieve_subscription` here is the reconciliation-grade path, and
    /// anything we do not see we do not grant).
    async fn apply_checkout_completed(
        &self,
        scope: &mut sanvi_persistence::PlatformScope<'_>,
        subscription_id: Option<&str>,
        client_reference_id: Option<&str>,
        created: DateTime<Utc>,
    ) -> Result<ApplyOutcome, BillingError> {
        let Some(stripe_subscription_id) = subscription_id else {
            return Ok(ApplyOutcome::Skipped);
        };
        if self
            .subscriptions
            .find_by_stripe_subscription_id(scope, stripe_subscription_id)
            .await?
            .is_some()
        {
            // The subscription webhook already created the row.
            return Ok(ApplyOutcome::Skipped);
        }
        // No local row: the subscription.created event may have been lost.
        // Retrieve from Stripe (reconciliation-grade) rather than granting
        // anything from the session payload.
        let view = match self
            .stripe
            .retrieve_subscription(stripe_subscription_id)
            .await
        {
            Ok(view) => view,
            Err(err) => {
                warn!(error = %err, stripe_subscription_id, "checkout completed but subscription retrieval failed; the daily reconciliation will repair it");
                return Ok(ApplyOutcome::Skipped);
            }
        };
        let data = StripeSubscriptionData {
            id: view.id,
            customer_id: view.customer_id,
            status: view.status,
            stripe_price_id: view.stripe_price_id,
            current_period: view.current_period,
            cancel_at_period_end: view.cancel_at_period_end,
            trial_end: view.trial_end,
            metadata: client_reference_id
                .map(|tenant_id| serde_json::json!({ "sanvi_tenant_id": tenant_id }))
                .unwrap_or_else(|| serde_json::json!({})),
            created,
        };
        self.apply_subscription_update(scope, &data, created).await
    }

    /// Upsert a subscription from a Stripe subscription object; recompute
    /// entitlements whenever the entitlement-bearing facts changed.
    /// Crate-visible for the reconciliation job, which reuses this exact
    /// path so live events and repairs can never disagree about semantics.
    pub(crate) async fn apply_subscription_update(
        &self,
        scope: &mut sanvi_persistence::PlatformScope<'_>,
        data: &StripeSubscriptionData,
        event_created: DateTime<Utc>,
    ) -> Result<ApplyOutcome, BillingError> {
        let existing_by_stripe_id = self
            .subscriptions
            .find_by_stripe_subscription_id(scope, &data.id)
            .await?;

        // Tenant identity: the existing row wins; otherwise the metadata we
        // stamped on the subscription at Checkout creation.
        let tenant_id = match existing_by_stripe_id.as_ref().map(|s| s.tenant_id()) {
            Some(tenant_id) => tenant_id,
            None => {
                crate::domain::plan::tenant_id_from_metadata(&data.metadata).ok_or_else(|| {
                    BillingError::InvalidWebhookPayload(format!(
                        "subscription {} carries no tenant metadata",
                        data.id
                    ))
                })?
            }
        };

        // A tenant owns one local projection, but Stripe creates a new
        // subscription id when a canceled/incomplete subscription is
        // replaced. Reuse that tenant row instead of violating its unique
        // constraint; non-terminal subscriptions still reject replacement.
        let existing = match existing_by_stripe_id {
            Some(existing) => Some(existing),
            None => {
                self.subscriptions
                    .find_by_tenant_platform(scope, tenant_id)
                    .await?
            }
        };
        let replacing = existing.as_ref().is_some_and(|subscription| {
            subscription.stripe_subscription_id() != Some(data.id.as_str())
        });
        if replacing
            && existing.as_ref().is_some_and(|subscription| {
                subscription.source() != SubscriptionSource::Stripe
                    || !matches!(
                        subscription.status(),
                        SubscriptionStatus::Canceled | SubscriptionStatus::IncompleteExpired
                    )
            })
        {
            return Err(BillingError::Conflict(format!(
                "tenant {tenant_id} already has a non-terminal subscription"
            )));
        }

        // Provider time is compared only with the preceding provider event,
        // never with local processing time.
        if let Some(existing) = &existing
            && !replacing
            && existing.is_subscription_event_stale(event_created)
        {
            return Ok(ApplyOutcome::Skipped);
        }

        let price = match data.stripe_price_id.as_deref() {
            Some(stripe_price_id) => match self
                .prices
                .find_by_stripe_price_id(scope, stripe_price_id)
                .await?
            {
                Some(price) => Some(price),
                None => {
                    return Err(BillingError::PriceNotFound(stripe_price_id.to_string()));
                }
            },
            None => None,
        };
        let plan_id = match &price {
            Some(price) => price.plan_id(),
            None => match &existing {
                Some(existing) => existing.plan_id(),
                None => {
                    return Err(BillingError::InvalidWebhookPayload(format!(
                        "subscription {} has no price and no local row",
                        data.id
                    )));
                }
            },
        };

        let mut events_to_publish: Vec<crate::domain::BillingEvent> = Vec::new();
        let outcome = match &existing {
            None => {
                let subscription = Subscription::new(
                    self.ids.next_subscription_id()?,
                    tenant_id,
                    plan_id,
                    price.as_ref().map(|p| p.id()),
                    SubscriptionSource::Stripe,
                    Some(data.customer_id.clone()),
                    Some(data.id.clone()),
                    data.status,
                    self.clock.now(),
                )?;
                let mut subscription = subscription;
                subscription.apply_stripe_update(
                    data.status,
                    plan_id,
                    price.as_ref().map(|p| p.id()),
                    data.current_period,
                    data.cancel_at_period_end,
                    data.trial_end,
                    event_created,
                    self.clock.now(),
                );
                let plan = self.plans.find_by_id(scope, plan_id).await?;
                self.subscriptions.save(scope, &subscription).await?;
                events_to_publish.push(crate::domain::BillingEvent::SubscriptionStarted {
                    tenant_id,
                    plan_key: plan
                        .as_ref()
                        .map(|p| p.key().to_string())
                        .unwrap_or_default(),
                    stripe_subscription_id: data.id.clone(),
                });
                ApplyOutcome::Applied
            }
            Some(existing) => {
                let from_plan = existing.plan_id();
                let mut subscription = existing.clone();
                if replacing {
                    subscription
                        .replace_stripe_identity(data.customer_id.clone(), data.id.clone())?;
                }
                let changed = subscription.apply_stripe_update(
                    data.status,
                    plan_id,
                    price.as_ref().map(|p| p.id()),
                    data.current_period,
                    data.cancel_at_period_end,
                    data.trial_end,
                    event_created,
                    self.clock.now(),
                );
                if replacing {
                    let plan_key = self
                        .plans
                        .find_by_id(scope, plan_id)
                        .await?
                        .map(|p| p.key().to_string())
                        .unwrap_or_default();
                    events_to_publish.push(crate::domain::BillingEvent::SubscriptionStarted {
                        tenant_id,
                        plan_key,
                        stripe_subscription_id: data.id.clone(),
                    });
                } else if subscription.plan_id() != from_plan {
                    let from_key = self
                        .plans
                        .find_by_id(scope, from_plan)
                        .await?
                        .map(|p| p.key().to_string())
                        .unwrap_or_default();
                    let to_key = self
                        .plans
                        .find_by_id(scope, plan_id)
                        .await?
                        .map(|p| p.key().to_string())
                        .unwrap_or_default();
                    events_to_publish.push(crate::domain::BillingEvent::SubscriptionPlanChanged {
                        tenant_id,
                        from_plan_key: from_key,
                        to_plan_key: to_key,
                    });
                }
                // Renewal: the period end moved forward on an already-live
                // subscription.
                if !replacing
                    && let (Some(new_period), Some(old_period)) =
                        (subscription.current_period(), existing.current_period())
                    && new_period.end > old_period.end
                    && existing.status().grants_entitlements()
                    && subscription.status().grants_entitlements()
                {
                    events_to_publish.push(crate::domain::BillingEvent::SubscriptionRenewed {
                        tenant_id,
                        period_end: new_period.end,
                    });
                }
                self.subscriptions.save(scope, &subscription).await?;
                if changed || replacing {
                    ApplyOutcome::Applied
                } else {
                    ApplyOutcome::Skipped
                }
            }
        };

        // Entitlements follow the recompute table.
        let subscription = self
            .subscriptions
            .find_by_tenant_platform(scope, tenant_id)
            .await?
            .ok_or_else(|| BillingError::SubscriptionNotFound(tenant_id.to_string()))?;
        match sync_action(subscription.status(), subscription.collection_state()) {
            crate::application::entitlements_sync::SyncAction::Apply => {
                let grants = self.grants_repo.list_for_plan(scope, plan_id).await?;
                let plan_key = self
                    .plans
                    .find_by_id(scope, plan_id)
                    .await?
                    .map(|p| p.key().to_string())
                    .unwrap_or_default();
                self.syncer
                    .sync_subscription(scope, tenant_id, &plan_key, &grants)
                    .await?;
            }
            crate::application::entitlements_sync::SyncAction::Clear => {
                self.syncer.clear_subscription(scope, tenant_id).await?;
            }
        }

        // Past-due transition: notify once (dunning start).
        if subscription.status() == SubscriptionStatus::PastDue
            && subscription.collection_state() == crate::domain::CollectionState::Dunning
        {
            events_to_publish.push(crate::domain::BillingEvent::SubscriptionPastDue {
                tenant_id,
                stripe_subscription_id: data.id.clone(),
            });
        }

        self.events.publish(scope, &events_to_publish).await?;
        Ok(outcome)
    }

    /// `customer.subscription.deleted` (or the scheduled downgrade after
    /// `cancel_at_period_end`): access stops here.
    async fn apply_subscription_deleted(
        &self,
        scope: &mut sanvi_persistence::PlatformScope<'_>,
        data: &StripeSubscriptionData,
        event_created: DateTime<Utc>,
    ) -> Result<ApplyOutcome, BillingError> {
        let Some(existing) = self
            .subscriptions
            .find_by_stripe_subscription_id(scope, &data.id)
            .await?
        else {
            return Ok(ApplyOutcome::Skipped);
        };
        if existing.is_subscription_event_stale(event_created) {
            return Ok(ApplyOutcome::Skipped);
        }
        let tenant_id = existing.tenant_id();
        let mut subscription = existing.clone();
        subscription.apply_stripe_update(
            SubscriptionStatus::Canceled,
            existing.plan_id(),
            existing.price_id(),
            existing.current_period(),
            existing.cancel_at_period_end(),
            existing.trial_end(),
            event_created,
            self.clock.now(),
        );
        self.subscriptions.save(scope, &subscription).await?;
        self.syncer.clear_subscription(scope, tenant_id).await?;
        self.events
            .publish(
                scope,
                &[crate::domain::BillingEvent::SubscriptionCanceled { tenant_id }],
            )
            .await?;
        Ok(ApplyOutcome::Applied)
    }

    async fn apply_invoice_paid(
        &self,
        scope: &mut sanvi_persistence::PlatformScope<'_>,
        data: &StripeInvoiceData,
        event_created: DateTime<Utc>,
    ) -> Result<ApplyOutcome, BillingError> {
        let outcome = self.upsert_invoice(scope, data, event_created).await?;
        let Some(stripe_subscription_id) = data.subscription_id.as_deref() else {
            return Ok(outcome);
        };
        let Some(existing) = self
            .subscriptions
            .find_by_stripe_subscription_id(scope, stripe_subscription_id)
            .await?
        else {
            return Ok(outcome);
        };
        if existing.is_invoice_event_stale(event_created) {
            return Ok(outcome);
        }

        // Payment recovery: undo dunning (entitlements were already cleared
        // by the subscription.update webhook that flips status to active).
        let mut subscription = existing.clone();
        if existing.collection_state() != crate::domain::CollectionState::Ok {
            subscription.restore(self.clock.now());
            let tenant_id = existing.tenant_id();
            self.events
                .publish(
                    scope,
                    &[
                        crate::domain::BillingEvent::InvoicePaid {
                            tenant_id,
                            number: data.number.clone(),
                            total_minor: data.total_minor,
                        },
                        crate::domain::BillingEvent::SubscriptionRestored { tenant_id },
                    ],
                )
                .await?;
        }
        subscription.record_invoice_event(event_created, self.clock.now());
        self.subscriptions.save(scope, &subscription).await?;
        // Resume the tenant when *we* suspended it for billing (the adapter
        // checks the suspension reason).
        self.lifecycle.resume_billing(existing.tenant_id()).await?;
        Ok(ApplyOutcome::Applied)
    }

    async fn apply_invoice_failed(
        &self,
        scope: &mut sanvi_persistence::PlatformScope<'_>,
        data: &StripeInvoiceData,
        event_created: DateTime<Utc>,
    ) -> Result<ApplyOutcome, BillingError> {
        let outcome = self.upsert_invoice(scope, data, event_created).await?;
        let Some(stripe_subscription_id) = data.subscription_id.as_deref() else {
            return Ok(outcome);
        };
        let Some(existing) = self
            .subscriptions
            .find_by_stripe_subscription_id(scope, stripe_subscription_id)
            .await?
        else {
            return Ok(outcome);
        };
        if existing.is_invoice_event_stale(event_created) {
            return Ok(outcome);
        }
        let mut events_to_publish = vec![crate::domain::BillingEvent::InvoicePaymentFailed {
            tenant_id: existing.tenant_id(),
            number: data.number.clone(),
            total_minor: data.total_minor,
        }];
        let mut subscription = existing.clone();
        if subscription.start_dunning_from_invoice(event_created, self.clock.now()) {
            events_to_publish.push(crate::domain::BillingEvent::SubscriptionPastDue {
                tenant_id: existing.tenant_id(),
                stripe_subscription_id: stripe_subscription_id.to_string(),
            });
        }
        self.subscriptions.save(scope, &subscription).await?;
        self.events.publish(scope, &events_to_publish).await?;
        Ok(ApplyOutcome::Applied)
    }

    async fn apply_invoice_finalized(
        &self,
        scope: &mut sanvi_persistence::PlatformScope<'_>,
        data: &StripeInvoiceData,
        event_created: DateTime<Utc>,
    ) -> Result<ApplyOutcome, BillingError> {
        self.upsert_invoice(scope, data, event_created).await
    }

    /// Mirror an invoice row (tenant resolved through the subscription).
    async fn upsert_invoice(
        &self,
        scope: &mut sanvi_persistence::PlatformScope<'_>,
        data: &StripeInvoiceData,
        event_created: DateTime<Utc>,
    ) -> Result<ApplyOutcome, BillingError> {
        let Some(stripe_subscription_id) = data.subscription_id.as_deref() else {
            return Ok(ApplyOutcome::Skipped);
        };
        let Some(subscription) = self
            .subscriptions
            .find_by_stripe_subscription_id(scope, stripe_subscription_id)
            .await?
        else {
            return Ok(ApplyOutcome::Skipped);
        };
        let existing = self
            .invoices
            .find_by_stripe_invoice_id(scope, &data.id)
            .await?;
        let invoice_id = match existing.as_ref().map(|i| i.id()) {
            Some(id) => id,
            None => self.ids.next_invoice_id()?,
        };
        let invoice = InvoiceRef::new(
            invoice_id,
            subscription.tenant_id(),
            data.id.clone(),
            data.number.clone(),
            data.status,
            data.total_minor,
            data.currency.clone(),
            data.hosted_url.clone(),
            data.pdf_url.clone(),
            data.period,
            data.created,
            data.paid_at,
        )?;
        if self.invoices.upsert(scope, &invoice, event_created).await? {
            Ok(ApplyOutcome::Applied)
        } else {
            Ok(ApplyOutcome::Skipped)
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ApplyOutcome {
    Applied,
    Skipped,
}
