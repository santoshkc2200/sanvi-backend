//! The payments context's outbox events.
//!
//! Naming: `payments.<snake_case>` event types on the
//! `payments.provider_connection` aggregate; `tenant_id` rides in envelope
//! metadata, same as every other context. 09.0 fixes the connection
//! lifecycle names (the aggregate and state machine exist); the money
//! events (`checkout_created`, `payment_succeeded`, `payment_failed`,
//! `refund_issued`, `dispute_opened`, `payout_paid`) are specified by the
//! parent plan and land with the slices that first emit them (09.4–09.6),
//! where their payloads stop being guesses.

use serde::Serialize;
use uuid::Uuid;

use sanvi_kernel::events::EventEnvelope;

use super::error::PaymentsError;

pub const CONNECTION_AGGREGATE: &str = "payments.provider_connection";
pub const DEFAULT_LOCALE: &str = "en";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaymentsEventName {
    ProviderConnectionStarted,
    ProviderConnectionActivated,
    ProviderConnectionRestricted,
    ProviderDisconnected,
}

impl PaymentsEventName {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ProviderConnectionStarted => "payments.provider_connection_started",
            Self::ProviderConnectionActivated => "payments.provider_connection_activated",
            Self::ProviderConnectionRestricted => "payments.provider_connection_restricted",
            Self::ProviderDisconnected => "payments.provider_disconnected",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        Some(match raw {
            "payments.provider_connection_started" => Self::ProviderConnectionStarted,
            "payments.provider_connection_activated" => Self::ProviderConnectionActivated,
            "payments.provider_connection_restricted" => Self::ProviderConnectionRestricted,
            "payments.provider_disconnected" => Self::ProviderDisconnected,
            _ => return None,
        })
    }
}

/// The connection-lifecycle event payloads, internally tagged.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PaymentsEvent {
    ProviderConnectionStarted {
        connection_id: Uuid,
        provider: String,
    },
    ProviderConnectionActivated {
        connection_id: Uuid,
        provider: String,
    },
    ProviderConnectionRestricted {
        connection_id: Uuid,
        provider: String,
        detail: String,
    },
    ProviderDisconnected {
        connection_id: Uuid,
        provider: String,
        reason: String,
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
        }
    }

    pub fn connection_id(&self) -> Uuid {
        match self {
            Self::ProviderConnectionStarted { connection_id, .. }
            | Self::ProviderConnectionActivated { connection_id, .. }
            | Self::ProviderConnectionRestricted { connection_id, .. }
            | Self::ProviderDisconnected { connection_id, .. } => *connection_id,
        }
    }

    pub fn envelope(&self, tenant_id: Uuid) -> EventEnvelope {
        EventEnvelope::new(
            CONNECTION_AGGREGATE,
            self.connection_id(),
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
        tenant_id: Uuid,
        events: &[PaymentsEvent],
    ) -> Result<(), PaymentsError>;

    async fn publish_scoped(
        &self,
        scope: &mut sanvi_persistence::ScopedTx<'_>,
        tenant_id: Uuid,
        events: &[PaymentsEvent],
    ) -> Result<(), PaymentsError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_round_trip() {
        for event in [
            PaymentsEvent::ProviderConnectionStarted {
                connection_id: Uuid::now_v7(),
                provider: "stripe_connect".to_string(),
            },
            PaymentsEvent::ProviderDisconnected {
                connection_id: Uuid::now_v7(),
                provider: "stripe_connect".to_string(),
                reason: "tenant request".to_string(),
            },
        ] {
            assert_eq!(
                PaymentsEventName::parse(event.name().as_str()),
                Some(event.name())
            );
        }
        assert_eq!(PaymentsEventName::parse("payments.payout_paid"), None);
    }

    #[test]
    fn envelope_carries_tenant_metadata() {
        let event = PaymentsEvent::ProviderConnectionActivated {
            connection_id: Uuid::now_v7(),
            provider: "stripe_connect".to_string(),
        };
        let tenant_id = Uuid::now_v7();
        let envelope = event.envelope(tenant_id);
        assert_eq!(
            envelope.event_type,
            "payments.provider_connection_activated"
        );
        assert_eq!(
            envelope.metadata.get("tenant_id").and_then(|v| v.as_str()),
            Some(tenant_id.to_string().as_str())
        );
    }
}
