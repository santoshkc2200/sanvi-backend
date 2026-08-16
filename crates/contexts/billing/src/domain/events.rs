//! Domain events emitted by the billing context: subscription lifecycle,
//! dunning, invoices and entitlement recomputation. Tenant-scoped events
//! carry the tenant id in envelope metadata so consumers (cache
//! invalidation, notifications) can route without parsing payloads.

use chrono::{DateTime, Utc};
use sanvi_kernel::EventEnvelope;
use serde::Serialize;
use uuid::Uuid;

/// Machine-readable event names, stable across versions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BillingEventName {
    SubscriptionStarted,
    SubscriptionRenewed,
    SubscriptionPlanChanged,
    SubscriptionPastDue,
    SubscriptionCanceled,
    SubscriptionSuspended,
    TrialEnding,
    InvoicePaid,
    InvoicePaymentFailed,
    EntitlementsRecomputed,
}

impl BillingEventName {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SubscriptionStarted => "billing.subscription_started",
            Self::SubscriptionRenewed => "billing.subscription_renewed",
            Self::SubscriptionPlanChanged => "billing.subscription_plan_changed",
            Self::SubscriptionPastDue => "billing.subscription_past_due",
            Self::SubscriptionCanceled => "billing.subscription_canceled",
            Self::SubscriptionSuspended => "billing.subscription_suspended",
            Self::TrialEnding => "billing.trial_ending",
            Self::InvoicePaid => "billing.invoice_paid",
            Self::InvoicePaymentFailed => "billing.invoice_payment_failed",
            Self::EntitlementsRecomputed => "billing.entitlements_recomputed",
        }
    }

    pub fn aggregate(self) -> &'static str {
        match self {
            Self::EntitlementsRecomputed => "billing.entitlements",
            Self::InvoicePaid | Self::InvoicePaymentFailed => "billing.invoice",
            _ => "billing.subscription",
        }
    }
}

/// Events the billing context publishes.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BillingEvent {
    SubscriptionStarted {
        tenant_id: Uuid,
        plan_key: String,
        stripe_subscription_id: String,
    },
    SubscriptionRenewed {
        tenant_id: Uuid,
        period_end: DateTime<Utc>,
    },
    SubscriptionPlanChanged {
        tenant_id: Uuid,
        from_plan_key: String,
        to_plan_key: String,
    },
    SubscriptionPastDue {
        tenant_id: Uuid,
        stripe_subscription_id: String,
    },
    SubscriptionCanceled {
        tenant_id: Uuid,
    },
    /// Dunning grace expired: entitlements downgraded, tenant suspended.
    SubscriptionSuspended {
        tenant_id: Uuid,
        stripe_subscription_id: String,
    },
    TrialEnding {
        tenant_id: Uuid,
        trial_end: DateTime<Utc>,
    },
    InvoicePaid {
        tenant_id: Uuid,
        number: Option<String>,
        total_minor: i64,
    },
    InvoicePaymentFailed {
        tenant_id: Uuid,
        number: Option<String>,
        total_minor: i64,
    },
    /// Subscription-source entitlement rows were rewritten for a tenant;
    /// consumers invalidate their entitlement caches.
    EntitlementsRecomputed {
        tenant_id: Uuid,
    },
}

impl BillingEvent {
    pub fn name(&self) -> BillingEventName {
        match self {
            Self::SubscriptionStarted { .. } => BillingEventName::SubscriptionStarted,
            Self::SubscriptionRenewed { .. } => BillingEventName::SubscriptionRenewed,
            Self::SubscriptionPlanChanged { .. } => BillingEventName::SubscriptionPlanChanged,
            Self::SubscriptionPastDue { .. } => BillingEventName::SubscriptionPastDue,
            Self::SubscriptionCanceled { .. } => BillingEventName::SubscriptionCanceled,
            Self::SubscriptionSuspended { .. } => BillingEventName::SubscriptionSuspended,
            Self::TrialEnding { .. } => BillingEventName::TrialEnding,
            Self::InvoicePaid { .. } => BillingEventName::InvoicePaid,
            Self::InvoicePaymentFailed { .. } => BillingEventName::InvoicePaymentFailed,
            Self::EntitlementsRecomputed { .. } => BillingEventName::EntitlementsRecomputed,
        }
    }

    pub fn tenant_id(&self) -> Uuid {
        match self {
            Self::SubscriptionStarted { tenant_id, .. }
            | Self::SubscriptionRenewed { tenant_id, .. }
            | Self::SubscriptionPlanChanged { tenant_id, .. }
            | Self::SubscriptionPastDue { tenant_id, .. }
            | Self::SubscriptionCanceled { tenant_id }
            | Self::SubscriptionSuspended { tenant_id, .. }
            | Self::TrialEnding { tenant_id, .. }
            | Self::InvoicePaid { tenant_id, .. }
            | Self::InvoicePaymentFailed { tenant_id, .. }
            | Self::EntitlementsRecomputed { tenant_id } => *tenant_id,
        }
    }

    /// The outbox envelope for this event, tagged with the tenant id.
    pub fn envelope(&self) -> EventEnvelope {
        let payload = serde_json::to_value(self).expect("billing events serialize infallibly");
        EventEnvelope::new(
            self.name().aggregate(),
            self.tenant_id(),
            self.name().as_str(),
            payload,
        )
        .with_metadata("tenant_id", serde_json::json!(self.tenant_id().to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_names_and_aggregates_are_stable() {
        assert_eq!(
            BillingEventName::SubscriptionStarted.as_str(),
            "billing.subscription_started"
        );
        assert_eq!(
            BillingEventName::EntitlementsRecomputed.aggregate(),
            "billing.entitlements"
        );
        assert_eq!(BillingEventName::InvoicePaid.aggregate(), "billing.invoice");
    }

    #[test]
    fn every_event_carries_tenant_id_metadata() {
        let events = vec![
            BillingEvent::SubscriptionStarted {
                tenant_id: Uuid::now_v7(),
                plan_key: "starter".to_string(),
                stripe_subscription_id: "sub_x".to_string(),
            },
            BillingEvent::EntitlementsRecomputed {
                tenant_id: Uuid::now_v7(),
            },
        ];
        for event in events {
            let envelope = event.envelope();
            assert_eq!(
                envelope.metadata["tenant_id"],
                event.tenant_id().to_string()
            );
            assert_eq!(envelope.aggregate_id, event.tenant_id());
        }
    }
}
