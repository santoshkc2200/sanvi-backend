//! Domain events for the privacy context, published through the
//! transactional outbox. Consumers (phase 10's conversion pipeline, the
//! notifications context, tenant operators) react to these; the directive
//! materialisation itself is synchronous in the submitting transaction.

use sanvi_kernel::{EventEnvelope, EventId, SnowflakeId, TenantId, UserId};
use serde::Serialize;

use crate::domain::types::{ProcessingPurpose, SubjectRef};
use crate::domain::{AppealId, BreachIncidentId, DsrId, LegalHoldId};

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PrivacyEvent {
    /// Consent was granted (or re-granted after withdrawal).
    ConsentGranted {
        subject: SubjectRef,
        purpose: ProcessingPurpose,
        notice_version: String,
        method: String,
        jurisdiction: String,
    },
    /// Consent was withdrawn (opt-in jurisdictions).
    ConsentWithdrawn {
        subject: SubjectRef,
        purpose: ProcessingPurpose,
        notice_version: String,
        jurisdiction: String,
    },
    /// An opt-out was exercised (or a universal signal observed). Phase 10's
    /// conversion pipeline MUST honour this: no conversion upload without
    /// `ads_measurement`; no audience upload without `sale_or_share` and
    /// `targeted_advertising`.
    ProcessingOptedOut {
        subject: SubjectRef,
        purposes: Vec<ProcessingPurpose>,
        source: String,
        jurisdiction: String,
    },
    /// "Limit the use of my sensitive personal information" (CPRA).
    SensitiveUseLimited {
        subject: SubjectRef,
        jurisdiction: String,
    },
    /// A data subject request was received.
    DsrReceived {
        request_id: DsrId,
        subject: SubjectRef,
        dsr_kind: String,
        jurisdiction: String,
    },
    DsrExtended {
        request_id: DsrId,
        subject: SubjectRef,
        deadline: chrono::DateTime<chrono::Utc>,
        reason: String,
    },
    /// A DSR reached a terminal state.
    DsrClosed {
        request_id: DsrId,
        subject: SubjectRef,
        dsr_kind: String,
        status: String,
        jurisdiction: String,
    },
    /// A subject's data was erased (crypto-shred + provider erasure done).
    SubjectErased {
        subject: SubjectRef,
        tenant_id: Option<TenantId>,
    },
    /// A US appeal was decided; the outcome must include the authority
    /// complaint route.
    AppealDecided {
        request_id: DsrId,
        appeal_id: AppealId,
        outcome: String,
        jurisdiction: String,
    },
    /// A legal hold was applied.
    LegalHoldApplied {
        hold_id: LegalHoldId,
        scope: serde_json::Value,
    },
    /// A legal hold was released.
    LegalHoldReleased { hold_id: LegalHoldId },
    /// A breach incident was recorded.
    BreachRecorded {
        incident_id: BreachIncidentId,
        jurisdictions: Vec<String>,
    },
    /// The jurisdiction resolution changed for a subject.
    JurisdictionResolved {
        subject: SubjectRef,
        jurisdiction: String,
    },
}

impl PrivacyEvent {
    pub fn name(&self) -> &'static str {
        match self {
            Self::ConsentGranted { .. } => "privacy.consent_granted",
            Self::ConsentWithdrawn { .. } => "privacy.consent_withdrawn",
            Self::ProcessingOptedOut { .. } => "privacy.processing_opted_out",
            Self::SensitiveUseLimited { .. } => "privacy.sensitive_use_limited",
            Self::DsrReceived { .. } => "privacy.dsr_received",
            Self::DsrClosed { .. } => "privacy.dsr_closed",
            Self::DsrExtended { .. } => "privacy.dsr_extended",
            Self::SubjectErased { .. } => "privacy.subject_erased",
            Self::AppealDecided { .. } => "privacy.appeal_decided",
            Self::LegalHoldApplied { .. } => "privacy.legal_hold_applied",
            Self::LegalHoldReleased { .. } => "privacy.legal_hold_released",
            Self::BreachRecorded { .. } => "privacy.breach_recorded",
            Self::JurisdictionResolved { .. } => "privacy.jurisdiction_resolved",
        }
    }

    pub fn tenant_id(&self) -> Option<TenantId> {
        match self {
            Self::ConsentGranted { subject, .. }
            | Self::ConsentWithdrawn { subject, .. }
            | Self::ProcessingOptedOut { subject, .. }
            | Self::SensitiveUseLimited { subject, .. }
            | Self::DsrReceived { subject, .. }
            | Self::DsrClosed { subject, .. }
            | Self::DsrExtended { subject, .. }
            | Self::JurisdictionResolved { subject, .. } => subject.tenant_id,
            Self::SubjectErased { tenant_id, .. } => *tenant_id,
            Self::AppealDecided { .. }
            | Self::LegalHoldApplied { .. }
            | Self::LegalHoldReleased { .. }
            | Self::BreachRecorded { .. } => None,
        }
    }

    pub fn envelope(&self, id: EventId, tenant_id: Option<TenantId>) -> EventEnvelope {
        let aggregate_id = match self {
            Self::ConsentGranted { .. }
            | Self::ConsentWithdrawn { .. }
            | Self::ProcessingOptedOut { .. }
            | Self::SensitiveUseLimited { .. }
            | Self::SubjectErased { .. }
            | Self::JurisdictionResolved { .. } => {
                subject_aggregate(self).unwrap_or_else(|| id.as_snowflake())
            }
            Self::DsrReceived { request_id, .. }
            | Self::DsrClosed { request_id, .. }
            | Self::DsrExtended { request_id, .. }
            | Self::AppealDecided { request_id, .. } => request_id.as_snowflake(),
            Self::LegalHoldApplied { hold_id, .. } | Self::LegalHoldReleased { hold_id } => {
                hold_id.as_snowflake()
            }
            Self::BreachRecorded { incident_id, .. } => incident_id.as_snowflake(),
        };
        let mut envelope = EventEnvelope::new(
            id,
            "privacy.subject",
            aggregate_id,
            self.name(),
            serde_json::to_value(self).expect("privacy events always serialize"),
        );
        if let Some(tenant_id) = tenant_id {
            envelope =
                envelope.with_metadata("tenant_id", serde_json::json!(tenant_id.to_string()));
        }
        envelope
    }
}

fn subject_aggregate(event: &PrivacyEvent) -> Option<SnowflakeId> {
    let key = match event {
        PrivacyEvent::ConsentGranted { subject, .. }
        | PrivacyEvent::ConsentWithdrawn { subject, .. }
        | PrivacyEvent::ProcessingOptedOut { subject, .. }
        | PrivacyEvent::SensitiveUseLimited { subject, .. }
        | PrivacyEvent::SubjectErased { subject, .. }
        | PrivacyEvent::JurisdictionResolved { subject, .. } => subject.canonical_key(),
        _ => return None,
    };
    key.strip_prefix("user:")
        .and_then(|raw| raw.parse::<UserId>().ok())
        .map(UserId::as_snowflake)
}

#[cfg(test)]
mod tests {
    use sanvi_kernel::{EventId, TenantId};

    use super::*;
    use crate::domain::DsrId;

    #[test]
    fn dsr_event_envelope_preserves_snowflake_ids_as_decimal_strings() {
        let request_id = DsrId::try_from(41_i64).unwrap();
        let tenant_id = TenantId::try_from(42_i64).unwrap();
        let event = PrivacyEvent::DsrReceived {
            request_id,
            subject: SubjectRef::contact("privacy@example.com"),
            dsr_kind: "access".to_string(),
            jurisdiction: "eu".to_string(),
        };

        let envelope = event.envelope(EventId::try_from(43_i64).unwrap(), Some(tenant_id));

        assert_eq!(envelope.aggregate_id, request_id.as_snowflake());
        assert_eq!(envelope.payload["request_id"], "41");
        assert_eq!(envelope.metadata["tenant_id"], "42");
    }
}
