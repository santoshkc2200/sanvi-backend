//! The billing context's `PersonalDataProvider`.
//!
//! Local personal data: `stripe_customer_id` / `stripe_subscription_id`
//! pointers and — the real leak — the raw Stripe event payloads in
//! `billing.webhook_events` (they embed email, name and address). Erasure:
//! delete the Stripe customer (Stripe-side tombstone), null the local
//! pointers, scrub the stored payloads. Invoices are statutory records and
//! are retained with that explanation; the subscription row survives as an
//! account record once its personal pointers are gone.

use async_trait::async_trait;

use sanvi_persistence::TxManager;
use sanvi_privacy::domain::{
    DATA_CLASSES, DataBundle, DataClass, DataClassDef, DataEntry, EraseMode, EraseReport,
    PersonalDataProvider, PrivacyError, RectifyPatch, RetainedItem, RetentionReason, SubjectRef,
};

use crate::domain::{
    InvoiceRepository, StripeClient, SubscriptionRepository, WebhookEventRepository,
};

pub struct BillingPrivacyProvider {
    tx_manager: TxManager,
    subscriptions: std::sync::Arc<dyn SubscriptionRepository>,
    invoices: std::sync::Arc<dyn InvoiceRepository>,
    webhooks: std::sync::Arc<dyn WebhookEventRepository>,
    stripe: std::sync::Arc<dyn StripeClient>,
}

impl BillingPrivacyProvider {
    pub fn new(
        tx_manager: TxManager,
        subscriptions: std::sync::Arc<dyn SubscriptionRepository>,
        invoices: std::sync::Arc<dyn InvoiceRepository>,
        webhooks: std::sync::Arc<dyn WebhookEventRepository>,
        stripe: std::sync::Arc<dyn StripeClient>,
    ) -> Self {
        Self {
            tx_manager,
            subscriptions,
            invoices,
            webhooks,
            stripe,
        }
    }
}

#[async_trait]
impl PersonalDataProvider for BillingPrivacyProvider {
    fn context_name(&self) -> &'static str {
        "billing"
    }

    fn data_classes(&self) -> Vec<DataClassDef> {
        DATA_CLASSES
            .iter()
            .filter(|def| {
                matches!(
                    def.class,
                    DataClass::Billing | DataClass::Invoice | DataClass::Contact
                )
            })
            .copied()
            .collect()
    }

    async fn collect(&self, subject: &SubjectRef) -> Result<DataBundle, PrivacyError> {
        let Some(tenant_id) = subject.tenant_id else {
            return Ok(DataBundle {
                context: "billing".to_string(),
                entries: Vec::new(),
            });
        };
        let mut scope = self
            .tx_manager
            .begin_unscoped("billing_privacy_provider", "billing")
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;
        let mut entries = Vec::new();
        if let Some(subscription) = self
            .subscriptions
            .find_by_tenant_platform(&mut scope, tenant_id)
            .await
            .map_err(storage)?
        {
            entries.push(DataEntry {
                data_class: DataClass::Billing,
                category: "Financial".to_string(),
                label: "subscription".to_string(),
                value: serde_json::json!({
                    "status": subscription.status().as_str(),
                    "plan_id": subscription.plan_id().as_uuid(),
                    "stripe_customer_id": subscription.stripe_customer_id(),
                }),
            });
        }
        for invoice in self
            .invoices
            .list_for_tenant_platform(&mut scope, tenant_id, 100)
            .await
            .map_err(storage)?
        {
            entries.push(DataEntry {
                data_class: DataClass::Invoice,
                category: "Financial".to_string(),
                label: "invoice".to_string(),
                value: serde_json::json!({
                    "number": invoice.number(),
                    "status": invoice.status().as_str(),
                    "total_minor": invoice.total_minor(),
                    "currency": invoice.currency(),
                }),
            });
        }
        scope
            .commit()
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;
        Ok(DataBundle {
            context: "billing".to_string(),
            entries,
        })
    }

    async fn erase(
        &self,
        subject: &SubjectRef,
        _mode: EraseMode,
    ) -> Result<EraseReport, PrivacyError> {
        let Some(tenant_id) = subject.tenant_id else {
            return Ok(EraseReport {
                context: "billing".to_string(),
                erased: 0,
                anonymised: 0,
                retained: Vec::new(),
            });
        };
        let mut scope = self
            .tx_manager
            .begin_unscoped("billing_privacy_erase", "billing")
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;

        let mut anonymised = 0;
        let mut retained = Vec::new();

        // Stripe-side tombstone first (the processor holds the real data).
        if let Some(subscription) = self
            .subscriptions
            .find_by_tenant_platform(&mut scope, tenant_id)
            .await
            .map_err(storage)?
            && let Some(customer_id) = subscription.stripe_customer_id()
        {
            if !customer_id.is_empty() {
                self.stripe
                    .delete_customer(customer_id)
                    .await
                    .map_err(|err| PrivacyError::Upstream(err.to_string()))?;
            }
            // Scrub every stored webhook payload that mentions the
            // customer (email/name/address leak).
            anonymised += self
                .webhooks
                .scrub_payloads_for_customer(&mut scope, customer_id)
                .await
                .map_err(storage)?;
        }

        // Null the local pointers: what remains is an account/entitlement
        // record, not personal data.
        anonymised += self
            .subscriptions
            .anonymise_stripe_ids(&mut scope, tenant_id)
            .await
            .map_err(storage)?;

        retained.push(RetainedItem {
            data_class: DataClass::Invoice,
            reason: RetentionReason::StatutoryRetention,
            detail: "invoices retained: statutory accounting records with a retention rule that overrides erasure requests".to_string(),
        });
        retained.push(RetainedItem {
            data_class: DataClass::Billing,
            reason: RetentionReason::Operational,
            detail: "subscription row retained as the tenant's access record; Stripe identifiers removed".to_string(),
        });

        scope
            .commit()
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;
        Ok(EraseReport {
            context: "billing".to_string(),
            erased: 0,
            anonymised,
            retained,
        })
    }

    async fn rectify(
        &self,
        _subject: &SubjectRef,
        _patch: &RectifyPatch,
    ) -> Result<(), PrivacyError> {
        // Billing facts are Stripe's; corrections go through Stripe support
        // flows, never through DSR rectification.
        Ok(())
    }
}

fn storage(err: crate::domain::BillingError) -> PrivacyError {
    PrivacyError::Storage(err.to_string())
}
