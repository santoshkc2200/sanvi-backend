//! Domain events for the privacy context, published through the
//! transactional outbox. Consumers (phase 10's conversion pipeline, the
//! notifications context, tenant operators) react to these; the directive
//! materialisation itself is synchronous in the submitting transaction.

use serde::Serialize;
use sha2::Digest;
use uuid::Uuid;

use crate::domain::types::{ProcessingPurpose, SubjectRef};

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
        request_id: Uuid,
        subject: SubjectRef,
        dsr_kind: String,
        jurisdiction: String,
    },
    /// A DSR reached a terminal state.
    DsrClosed {
        request_id: Uuid,
        subject: SubjectRef,
        dsr_kind: String,
        status: String,
        jurisdiction: String,
    },
    /// A subject's data was erased (crypto-shred + provider erasure done).
    SubjectErased {
        subject: SubjectRef,
        tenant_id: Option<Uuid>,
    },
    /// A US appeal was decided; the outcome must include the authority
    /// complaint route.
    AppealDecided {
        request_id: Uuid,
        appeal_id: Uuid,
        outcome: String,
        jurisdiction: String,
    },
    /// A legal hold was applied.
    LegalHoldApplied {
        hold_id: Uuid,
        scope: serde_json::Value,
    },
    /// A legal hold was released.
    LegalHoldReleased { hold_id: Uuid },
    /// A breach incident was recorded.
    BreachRecorded {
        incident_id: Uuid,
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
            Self::SubjectErased { .. } => "privacy.subject_erased",
            Self::AppealDecided { .. } => "privacy.appeal_decided",
            Self::LegalHoldApplied { .. } => "privacy.legal_hold_applied",
            Self::LegalHoldReleased { .. } => "privacy.legal_hold_released",
            Self::BreachRecorded { .. } => "privacy.breach_recorded",
            Self::JurisdictionResolved { .. } => "privacy.jurisdiction_resolved",
        }
    }

    pub fn envelope(&self, tenant_id: Option<Uuid>) -> sanvi_kernel::EventEnvelope {
        let aggregate_id = match self {
            Self::ConsentGranted { .. }
            | Self::ConsentWithdrawn { .. }
            | Self::ProcessingOptedOut { .. }
            | Self::SensitiveUseLimited { .. }
            | Self::SubjectErased { .. }
            | Self::JurisdictionResolved { .. } => subject_uuid(self),
            Self::DsrReceived { request_id, .. }
            | Self::DsrClosed { request_id, .. }
            | Self::AppealDecided { request_id, .. } => *request_id,
            Self::LegalHoldApplied { hold_id, .. } | Self::LegalHoldReleased { hold_id } => {
                *hold_id
            }
            Self::BreachRecorded { incident_id, .. } => *incident_id,
        };
        let mut envelope = sanvi_kernel::EventEnvelope::new(
            "privacy.subject",
            aggregate_id,
            self.name(),
            serde_json::to_value(self).expect("privacy events always serialize"),
        );
        if let Some(tenant_id) = tenant_id {
            envelope = envelope.with_metadata("tenant_id", serde_json::json!(tenant_id));
        }
        envelope
    }
}

fn subject_uuid(event: &PrivacyEvent) -> Uuid {
    // Subject refs are UUID-shaped only for user-based keys; derive a
    // deterministic UUID (v5 of the canonical key, DNS namespace) so every
    // event has a stable aggregate id without parsing the key.
    let key = match event {
        PrivacyEvent::ConsentGranted { subject, .. }
        | PrivacyEvent::ConsentWithdrawn { subject, .. }
        | PrivacyEvent::ProcessingOptedOut { subject, .. }
        | PrivacyEvent::SensitiveUseLimited { subject, .. }
        | PrivacyEvent::SubjectErased { subject, .. }
        | PrivacyEvent::JurisdictionResolved { subject, .. } => subject.canonical_key(),
        _ => "privacy",
    };
    // Deterministic UUID from the canonical key (SHA-256, first 16 bytes,
    // version/variant bits set).
    let digest = sha2::Sha256::digest(key.as_bytes());
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x50; // UUIDv5-ish version
    bytes[8] = (bytes[8] & 0x3f) | 0x80; // variant
    Uuid::from_bytes(bytes)
}
