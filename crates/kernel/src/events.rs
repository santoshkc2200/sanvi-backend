use crate::{Id, SnowflakeId};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub enum EventTag {}
pub type EventId = Id<EventTag>;

/// The canonical shape of every domain event that crosses a context boundary.
///
/// Events are written to the transactional outbox together with the state
/// change that produced them, then relayed by the worker to Redis Streams.
/// Consumers deduplicate on `id`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventEnvelope {
    /// Unique event id; doubles as the consumer dedup key.
    pub id: EventId,
    /// Aggregate type, e.g. `"tenancy.tenant"`.
    pub aggregate: String,
    /// Id of the aggregate instance that raised the event.
    pub aggregate_id: SnowflakeId,
    /// Machine-readable event name, e.g. `"tenancy.tenant_created"`.
    pub event_type: String,
    /// Event payload as defined by the publishing context.
    pub payload: serde_json::Value,
    /// Free-form metadata (idempotency keys, causation ids, …).
    pub metadata: serde_json::Value,
    /// When the event happened (set by the publisher).
    pub occurred_at: DateTime<Utc>,
}

impl EventEnvelope {
    pub fn new(
        id: EventId,
        aggregate: impl Into<String>,
        aggregate_id: SnowflakeId,
        event_type: impl Into<String>,
        payload: serde_json::Value,
    ) -> Self {
        Self {
            id,
            aggregate: aggregate.into(),
            aggregate_id,
            event_type: event_type.into(),
            payload,
            metadata: serde_json::Value::Object(Default::default()),
            occurred_at: Utc::now(),
        }
    }

    pub fn with_metadata(mut self, key: impl Into<String>, value: serde_json::Value) -> Self {
        self.metadata
            .as_object_mut()
            .map(|m| m.insert(key.into(), value));
        self
    }

    /// Record the locale the event happened under.
    ///
    /// Every event that produces user-visible text downstream (notifications,
    /// invoices, exports) must carry this; workers never re-negotiate a
    /// locale from a missing request. The key is the stable contract:
    /// `metadata.locale` (canonical BCP-47, e.g. `ja` or `en-GB`).
    pub fn with_locale(self, locale: impl Into<String>) -> Self {
        self.with_metadata("locale", serde_json::json!(locale.into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_serializes_event_and_aggregate_ids_as_strings() {
        let envelope = EventEnvelope::new(
            EventId::try_from(101_i64).unwrap(),
            "tenancy.tenant",
            SnowflakeId::try_from(202_i64).unwrap(),
            "tenancy.tenant_created",
            serde_json::json!({}),
        );
        let json = serde_json::to_value(envelope).unwrap();

        assert_eq!(json["id"], "101");
        assert_eq!(json["aggregate_id"], "202");
    }

    #[test]
    fn new_envelope_carries_all_fields() {
        let envelope = EventEnvelope::new(
            EventId::try_from(101_i64).unwrap(),
            "tenancy.tenant",
            SnowflakeId::try_from(202_i64).unwrap(),
            "tenancy.tenant_created",
            serde_json::json!({ "slug": "acme" }),
        )
        .with_metadata("request_id", serde_json::json!("req-123"));

        assert_eq!(envelope.aggregate, "tenancy.tenant");
        assert_eq!(envelope.event_type, "tenancy.tenant_created");
        assert_eq!(envelope.payload, serde_json::json!({ "slug": "acme" }));
        assert_eq!(envelope.metadata["request_id"], "req-123");
        assert_eq!(envelope.id.as_i64(), 101);
        assert_eq!(envelope.aggregate_id.as_i64(), 202);
    }

    #[test]
    fn serde_roundtrip_preserves_content() {
        let envelope = EventEnvelope::new(
            EventId::try_from(101_i64).unwrap(),
            "billing.invoice",
            SnowflakeId::try_from(202_i64).unwrap(),
            "billing.invoice_paid",
            serde_json::json!({ "total": 4900 }),
        );
        let json = serde_json::to_string(&envelope).unwrap();
        let back: EventEnvelope = serde_json::from_str(&json).unwrap();
        assert_eq!(back, envelope);
    }

    #[test]
    fn with_locale_records_the_canonical_metadata_key() {
        let envelope = EventEnvelope::new(
            EventId::try_from(101_i64).unwrap(),
            "privacy.dsr",
            SnowflakeId::try_from(202_i64).unwrap(),
            "privacy.dsr_submitted",
            serde_json::json!({}),
        )
        .with_locale("ja");
        assert_eq!(envelope.metadata["locale"], serde_json::json!("ja"));
    }
}
