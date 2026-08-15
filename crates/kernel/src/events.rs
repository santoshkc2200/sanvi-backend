use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// The canonical shape of every domain event that crosses a context boundary.
///
/// Events are written to the transactional outbox together with the state
/// change that produced them, then relayed by the worker to Redis Streams.
/// Consumers deduplicate on `id`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventEnvelope {
    /// Unique event id (UUIDv7); doubles as the consumer dedup key.
    pub id: Uuid,
    /// Aggregate type, e.g. `"tenancy.tenant"`.
    pub aggregate: String,
    /// Id of the aggregate instance that raised the event.
    pub aggregate_id: Uuid,
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
        aggregate: impl Into<String>,
        aggregate_id: Uuid,
        event_type: impl Into<String>,
        payload: serde_json::Value,
    ) -> Self {
        Self {
            id: Uuid::now_v7(),
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_envelope_carries_all_fields() {
        let envelope = EventEnvelope::new(
            "tenancy.tenant",
            Uuid::now_v7(),
            "tenancy.tenant_created",
            serde_json::json!({ "slug": "acme" }),
        )
        .with_metadata("request_id", serde_json::json!("req-123"));

        assert_eq!(envelope.aggregate, "tenancy.tenant");
        assert_eq!(envelope.event_type, "tenancy.tenant_created");
        assert_eq!(envelope.payload, serde_json::json!({ "slug": "acme" }));
        assert_eq!(envelope.metadata["request_id"], "req-123");
        assert_eq!(envelope.id.get_version_num(), 7);
        assert!(!envelope.aggregate_id.is_nil());
    }

    #[test]
    fn serde_roundtrip_preserves_content() {
        let envelope = EventEnvelope::new(
            "billing.invoice",
            Uuid::now_v7(),
            "billing.invoice_paid",
            serde_json::json!({ "total": 4900 }),
        );
        let json = serde_json::to_string(&envelope).unwrap();
        let back: EventEnvelope = serde_json::from_str(&json).unwrap();
        assert_eq!(back, envelope);
    }
}
