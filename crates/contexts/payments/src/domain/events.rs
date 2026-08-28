//! The payments context's outbox events.
//!
//! Naming: `payments.<snake_case>` event types; `tenant_id` rides in
//! envelope metadata, same as every other context. 09.1 declares the full
//! phase-09 vocabulary — connection lifecycle *and* money events — and
//! wires every one to the phase-01 outbox, so later slices emit events
//! instead of inventing them. The money events first *fire* in 09.4–09.6;
//! their payloads are fixed now (ids, provider, minor-unit amounts) and
//! carry no PII and no card data — identifiers, statuses and amounts only,
//! per the phase's storage invariant.

use sanvi_kernel::events::EventEnvelope;
use sanvi_kernel::{EventId, SnowflakeId, TenantId};
use serde::Serialize;

use super::checkout::CheckoutId;
use super::connection::ConnectionId;
use super::dispute::DisputeId;
use super::error::PaymentsError;
use super::payment::{PaymentId, RefundId};
use super::payout::PayoutId;

pub const CONNECTION_AGGREGATE: &str = "payments.provider_connection";
pub const CHECKOUT_AGGREGATE: &str = "payments.checkout";
pub const PAYMENT_AGGREGATE: &str = "payments.payment";
pub const REFUND_AGGREGATE: &str = "payments.refund";
pub const DISPUTE_AGGREGATE: &str = "payments.dispute";
pub const PAYOUT_AGGREGATE: &str = "payments.payout";
pub const DEFAULT_LOCALE: &str = "en";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaymentsEventName {
    ProviderConnectionStarted,
    ProviderConnectionActivated,
    ProviderConnectionRestricted,
    ProviderDisconnected,
    CheckoutCreated,
    PaymentSucceeded,
    PaymentFailed,
    RefundIssued,
    DisputeOpened,
    PayoutPaid,
}

impl PaymentsEventName {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ProviderConnectionStarted => "payments.provider_connection_started",
            Self::ProviderConnectionActivated => "payments.provider_connection_activated",
            Self::ProviderConnectionRestricted => "payments.provider_connection_restricted",
            Self::ProviderDisconnected => "payments.provider_disconnected",
            Self::CheckoutCreated => "payments.checkout_created",
            Self::PaymentSucceeded => "payments.payment_succeeded",
            Self::PaymentFailed => "payments.payment_failed",
            Self::RefundIssued => "payments.refund_issued",
            Self::DisputeOpened => "payments.dispute_opened",
            Self::PayoutPaid => "payments.payout_paid",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        Some(match raw {
            "payments.provider_connection_started" => Self::ProviderConnectionStarted,
            "payments.provider_connection_activated" => Self::ProviderConnectionActivated,
            "payments.provider_connection_restricted" => Self::ProviderConnectionRestricted,
            "payments.provider_disconnected" => Self::ProviderDisconnected,
            "payments.checkout_created" => Self::CheckoutCreated,
            "payments.payment_succeeded" => Self::PaymentSucceeded,
            "payments.payment_failed" => Self::PaymentFailed,
            "payments.refund_issued" => Self::RefundIssued,
            "payments.dispute_opened" => Self::DisputeOpened,
            "payments.payout_paid" => Self::PayoutPaid,
            _ => return None,
        })
    }
}

/// The minor-unit amount every money event carries — integer plus
/// ISO-4217 code, never a float (NFR-907).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EventAmount {
    pub amount_minor: i64,
    pub currency: String,
}

impl EventAmount {
    pub fn new(amount: &super::money::Money) -> Self {
        Self {
            amount_minor: amount.amount_minor,
            currency: amount.currency.as_str().to_string(),
        }
    }
}

/// Every payments outbox event, internally tagged. Aggregate routing is
/// per variant (see [`PaymentsEvent::envelope`]) — money events belong to
/// their own aggregates, not to the connection that happened to be open.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PaymentsEvent {
    ProviderConnectionStarted {
        connection_id: ConnectionId,
        provider: String,
    },
    ProviderConnectionActivated {
        connection_id: ConnectionId,
        provider: String,
    },
    ProviderConnectionRestricted {
        connection_id: ConnectionId,
        provider: String,
        detail: String,
    },
    ProviderDisconnected {
        connection_id: ConnectionId,
        provider: String,
        reason: String,
    },
    CheckoutCreated {
        checkout_id: CheckoutId,
        connection_id: ConnectionId,
        provider: String,
        reference: String,
        amount: EventAmount,
    },
    PaymentSucceeded {
        payment_id: PaymentId,
        checkout_id: Option<CheckoutId>,
        provider: String,
        amount: EventAmount,
    },
    PaymentFailed {
        payment_id: PaymentId,
        checkout_id: Option<CheckoutId>,
        provider: String,
        failure_code: String,
    },
    RefundIssued {
        refund_id: RefundId,
        payment_id: PaymentId,
        provider: String,
        amount: EventAmount,
        reason: Option<String>,
    },
    DisputeOpened {
        dispute_id: DisputeId,
        payment_id: PaymentId,
        provider: String,
        amount: EventAmount,
    },
    PayoutPaid {
        payout_id: PayoutId,
        connection_id: ConnectionId,
        provider: String,
        amount: EventAmount,
    },
}

impl PaymentsEvent {
    pub fn name(&self) -> PaymentsEventName {
        match self {
            Self::ProviderConnectionStarted { .. } => PaymentsEventName::ProviderConnectionStarted,
            Self::ProviderConnectionActivated { .. } => {
                PaymentsEventName::ProviderConnectionActivated
            }
            Self::ProviderConnectionRestricted { .. } => {
                PaymentsEventName::ProviderConnectionRestricted
            }
            Self::ProviderDisconnected { .. } => PaymentsEventName::ProviderDisconnected,
            Self::CheckoutCreated { .. } => PaymentsEventName::CheckoutCreated,
            Self::PaymentSucceeded { .. } => PaymentsEventName::PaymentSucceeded,
            Self::PaymentFailed { .. } => PaymentsEventName::PaymentFailed,
            Self::RefundIssued { .. } => PaymentsEventName::RefundIssued,
            Self::DisputeOpened { .. } => PaymentsEventName::DisputeOpened,
            Self::PayoutPaid { .. } => PaymentsEventName::PayoutPaid,
        }
    }

    /// The aggregate (type + instance id) the event belongs to. Connection
    /// events belong to connections; money events to their own aggregates
    /// — a payment is not "part of" the connection that happened to be
    /// open when it was taken.
    pub fn aggregate(&self) -> (&'static str, SnowflakeId) {
        match self {
            Self::ProviderConnectionStarted { connection_id, .. }
            | Self::ProviderConnectionActivated { connection_id, .. }
            | Self::ProviderConnectionRestricted { connection_id, .. }
            | Self::ProviderDisconnected { connection_id, .. } => {
                (CONNECTION_AGGREGATE, connection_id.as_snowflake())
            }
            Self::CheckoutCreated { checkout_id, .. } => {
                (CHECKOUT_AGGREGATE, checkout_id.as_snowflake())
            }
            Self::PaymentSucceeded { payment_id, .. } | Self::PaymentFailed { payment_id, .. } => {
                (PAYMENT_AGGREGATE, payment_id.as_snowflake())
            }
            Self::RefundIssued { refund_id, .. } => (REFUND_AGGREGATE, refund_id.as_snowflake()),
            Self::DisputeOpened { dispute_id, .. } => {
                (DISPUTE_AGGREGATE, dispute_id.as_snowflake())
            }
            Self::PayoutPaid { payout_id, .. } => (PAYOUT_AGGREGATE, payout_id.as_snowflake()),
        }
    }

    pub fn envelope(&self, id: EventId, tenant_id: TenantId) -> EventEnvelope {
        let (aggregate, aggregate_id) = self.aggregate();
        EventEnvelope::new(
            id,
            aggregate,
            aggregate_id,
            self.name().as_str(),
            serde_json::to_value(self).expect("payments events serialize"),
        )
        .with_metadata("tenant_id", serde_json::json!(tenant_id.to_string()))
        .with_locale(DEFAULT_LOCALE)
    }
}

/// The outbox port (implemented by the adapters layer).
#[async_trait::async_trait]
pub trait EventPublisher: Send + Sync {
    async fn publish(
        &self,
        scope: &mut sanvi_persistence::PlatformScope<'_>,
        tenant_id: TenantId,
        events: &[PaymentsEvent],
    ) -> Result<(), PaymentsError>;

    async fn publish_scoped(
        &self,
        scope: &mut sanvi_persistence::ScopedTx<'_>,
        tenant_id: TenantId,
        events: &[PaymentsEvent],
    ) -> Result<(), PaymentsError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::money::Money;

    #[test]
    fn names_round_trip() {
        let events = [
            PaymentsEvent::ProviderConnectionStarted {
                connection_id: ConnectionId::try_from(1_i64).unwrap(),
                provider: "stripe_connect".to_string(),
            },
            PaymentsEvent::ProviderDisconnected {
                connection_id: ConnectionId::try_from(2_i64).unwrap(),
                provider: "stripe_connect".to_string(),
                reason: "tenant request".to_string(),
            },
            PaymentsEvent::CheckoutCreated {
                checkout_id: CheckoutId::try_from(3_i64).unwrap(),
                connection_id: ConnectionId::try_from(4_i64).unwrap(),
                provider: "fake".to_string(),
                reference: "order-1".to_string(),
                amount: EventAmount::new(&Money::new(2000, "JPY").unwrap()),
            },
            PaymentsEvent::PaymentSucceeded {
                payment_id: PaymentId::try_from(5_i64).unwrap(),
                checkout_id: Some(CheckoutId::try_from(3_i64).unwrap()),
                provider: "fake".to_string(),
                amount: EventAmount::new(&Money::new(2000, "JPY").unwrap()),
            },
            PaymentsEvent::PaymentFailed {
                payment_id: PaymentId::try_from(6_i64).unwrap(),
                checkout_id: None,
                provider: "fake".to_string(),
                failure_code: "card_declined".to_string(),
            },
            PaymentsEvent::RefundIssued {
                refund_id: RefundId::try_from(7_i64).unwrap(),
                payment_id: PaymentId::try_from(5_i64).unwrap(),
                provider: "fake".to_string(),
                amount: EventAmount::new(&Money::new(500, "JPY").unwrap()),
                reason: Some("duplicate".to_string()),
            },
            PaymentsEvent::DisputeOpened {
                dispute_id: DisputeId::try_from(8_i64).unwrap(),
                payment_id: PaymentId::try_from(5_i64).unwrap(),
                provider: "fake".to_string(),
                amount: EventAmount::new(&Money::new(2000, "JPY").unwrap()),
            },
            PaymentsEvent::PayoutPaid {
                payout_id: PayoutId::try_from(9_i64).unwrap(),
                connection_id: ConnectionId::try_from(1_i64).unwrap(),
                provider: "fake".to_string(),
                amount: EventAmount::new(&Money::new(1500, "JPY").unwrap()),
            },
        ];
        for event in events {
            assert_eq!(
                PaymentsEventName::parse(event.name().as_str()),
                Some(event.name())
            );
        }
        assert_eq!(PaymentsEventName::parse("payments.payout_failed"), None);
    }

    #[test]
    fn money_events_route_to_their_own_aggregates() {
        let checkout_id = CheckoutId::try_from(10_i64).unwrap();
        let payment_id = PaymentId::try_from(20_i64).unwrap();
        let refund_id = RefundId::try_from(30_i64).unwrap();

        let event = PaymentsEvent::CheckoutCreated {
            checkout_id,
            connection_id: ConnectionId::try_from(40_i64).unwrap(),
            provider: "fake".to_string(),
            reference: "order-1".to_string(),
            amount: EventAmount::new(&Money::new(2000, "JPY").unwrap()),
        };
        assert_eq!(
            event.aggregate(),
            (CHECKOUT_AGGREGATE, checkout_id.as_snowflake())
        );

        let event = PaymentsEvent::PaymentSucceeded {
            payment_id,
            checkout_id: Some(checkout_id),
            provider: "fake".to_string(),
            amount: EventAmount::new(&Money::new(2000, "JPY").unwrap()),
        };
        assert_eq!(
            event.aggregate(),
            (PAYMENT_AGGREGATE, payment_id.as_snowflake())
        );

        let event = PaymentsEvent::RefundIssued {
            refund_id,
            payment_id,
            provider: "fake".to_string(),
            amount: EventAmount::new(&Money::new(500, "JPY").unwrap()),
            reason: None,
        };
        assert_eq!(
            event.aggregate(),
            (REFUND_AGGREGATE, refund_id.as_snowflake())
        );
    }

    #[test]
    fn envelope_carries_tenant_metadata_and_minor_unit_amounts() {
        let event = PaymentsEvent::ProviderConnectionActivated {
            connection_id: ConnectionId::try_from(1_i64).unwrap(),
            provider: "stripe_connect".to_string(),
        };
        let tenant_id = TenantId::try_from(2_i64).unwrap();
        let event_id = EventId::try_from(3_i64).unwrap();
        let envelope = event.envelope(event_id, tenant_id);
        assert_eq!(
            envelope.event_type,
            "payments.provider_connection_activated"
        );
        assert_eq!(
            envelope.metadata.get("tenant_id").and_then(|v| v.as_str()),
            Some(tenant_id.to_string().as_str())
        );

        // Money payloads stay minor units + ISO code under serialization.
        let event = PaymentsEvent::PaymentSucceeded {
            payment_id: PaymentId::try_from(5_i64).unwrap(),
            checkout_id: None,
            provider: "fake".to_string(),
            amount: EventAmount::new(&Money::new(2000, "JPY").unwrap()),
        };
        let payload = serde_json::to_value(&event).unwrap();
        assert_eq!(payload["amount"]["amount_minor"], 2000);
        assert_eq!(payload["amount"]["currency"], "JPY");
        assert!(!payload.to_string().contains("200000"));
    }
}
