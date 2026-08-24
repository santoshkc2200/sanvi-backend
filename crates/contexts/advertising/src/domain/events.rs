use serde::{Deserialize, Serialize};
use uuid::Uuid;

use sanvi_kernel::events::EventEnvelope;

/// Event names registered for the shared transactional outbox.
pub const EVENT_TYPES: &[&str] = &[
    "advertising.account_connected",
    "advertising.account_disconnected",
    "advertising.campaign_created",
    "advertising.campaign_updated",
    "advertising.campaign_paused",
    "advertising.campaign_resumed",
    "advertising.campaign_drifted",
    "advertising.conversion_recorded",
    "advertising.conversion_uploaded",
    "advertising.conversion_upload_failed",
    "advertising.spend_threshold_reached",
    "advertising.metrics_ingested",
];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
pub enum AdEvent {
    AccountConnected {
        connection_id: Uuid,
    },
    AccountDisconnected {
        connection_id: Uuid,
    },
    CampaignCreated {
        campaign_id: Uuid,
    },
    CampaignUpdated {
        campaign_id: Uuid,
    },
    CampaignPaused {
        campaign_id: Uuid,
    },
    CampaignResumed {
        campaign_id: Uuid,
    },
    CampaignDrifted {
        campaign_id: Uuid,
        changed_fields: Vec<String>,
    },
    ConversionRecorded {
        event_id: Uuid,
    },
    ConversionUploaded {
        event_id: Uuid,
    },
    ConversionUploadFailed {
        event_id: Uuid,
        reason: String,
    },
    SpendThresholdReached {
        campaign_id: Uuid,
    },
    MetricsIngested {
        connection_id: Uuid,
    },
}

impl AdEvent {
    pub const fn event_type(&self) -> &'static str {
        match self {
            Self::AccountConnected { .. } => EVENT_TYPES[0],
            Self::AccountDisconnected { .. } => EVENT_TYPES[1],
            Self::CampaignCreated { .. } => EVENT_TYPES[2],
            Self::CampaignUpdated { .. } => EVENT_TYPES[3],
            Self::CampaignPaused { .. } => EVENT_TYPES[4],
            Self::CampaignResumed { .. } => EVENT_TYPES[5],
            Self::CampaignDrifted { .. } => EVENT_TYPES[6],
            Self::ConversionRecorded { .. } => EVENT_TYPES[7],
            Self::ConversionUploaded { .. } => EVENT_TYPES[8],
            Self::ConversionUploadFailed { .. } => EVENT_TYPES[9],
            Self::SpendThresholdReached { .. } => EVENT_TYPES[10],
            Self::MetricsIngested { .. } => EVENT_TYPES[11],
        }
    }

    fn aggregate(&self) -> (&'static str, Uuid) {
        match self {
            Self::AccountConnected { connection_id }
            | Self::AccountDisconnected { connection_id }
            | Self::MetricsIngested { connection_id } => ("advertising.connection", *connection_id),
            Self::CampaignCreated { campaign_id }
            | Self::CampaignUpdated { campaign_id }
            | Self::CampaignPaused { campaign_id }
            | Self::CampaignResumed { campaign_id }
            | Self::CampaignDrifted { campaign_id, .. }
            | Self::SpendThresholdReached { campaign_id } => ("advertising.campaign", *campaign_id),
            Self::ConversionRecorded { event_id }
            | Self::ConversionUploaded { event_id }
            | Self::ConversionUploadFailed { event_id, .. } => {
                ("advertising.conversion_event", *event_id)
            }
        }
    }

    pub fn envelope(&self, tenant_id: Uuid) -> EventEnvelope {
        let (aggregate, aggregate_id) = self.aggregate();
        EventEnvelope::new(
            aggregate,
            aggregate_id,
            self.event_type(),
            serde_json::to_value(self).expect("advertising events serialize"),
        )
        .with_metadata("tenant_id", serde_json::json!(tenant_id.to_string()))
        .with_locale("en")
    }
}

#[async_trait::async_trait]
pub trait EventPublisher: Send + Sync {
    async fn publish_scoped(
        &self,
        scope: &mut sanvi_persistence::ScopedTx<'_>,
        tenant_id: Uuid,
        events: &[AdEvent],
    ) -> Result<(), super::AdvertisingError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registered_events_round_trip_through_json_and_outbox_envelopes() {
        let id = Uuid::now_v7();
        let tenant_id = Uuid::now_v7();
        let events = vec![
            AdEvent::AccountConnected { connection_id: id },
            AdEvent::AccountDisconnected { connection_id: id },
            AdEvent::CampaignCreated { campaign_id: id },
            AdEvent::CampaignUpdated { campaign_id: id },
            AdEvent::CampaignPaused { campaign_id: id },
            AdEvent::CampaignResumed { campaign_id: id },
            AdEvent::CampaignDrifted {
                campaign_id: id,
                changed_fields: vec!["status".to_string()],
            },
            AdEvent::ConversionRecorded { event_id: id },
            AdEvent::ConversionUploaded { event_id: id },
            AdEvent::ConversionUploadFailed {
                event_id: id,
                reason: "rejected".to_string(),
            },
            AdEvent::SpendThresholdReached { campaign_id: id },
            AdEvent::MetricsIngested { connection_id: id },
        ];
        assert_eq!(events.len(), EVENT_TYPES.len());
        for event in events {
            assert!(EVENT_TYPES.contains(&event.event_type()));
            let json = serde_json::to_string(&event).unwrap();
            assert_eq!(serde_json::from_str::<AdEvent>(&json).unwrap(), event);
            assert_eq!(event.envelope(tenant_id).event_type, event.event_type());
        }
    }
}
