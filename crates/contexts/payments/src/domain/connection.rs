//! The provider-connection aggregate.
//!
//! One row per (tenant, provider) while connected — enforced by the
//! partial unique index `one_active_provider_per_tenant` on
//! `disconnected_at IS NULL`. The aggregate carries identifiers and status
//! only: capabilities/requirements arrive as the provider's own JSON
//! (mapped views land in 09.2), and no onboarding PII is ever stored here.

use chrono::{DateTime, Utc};
use serde_json::Value;
use uuid::Uuid;

use super::error::PaymentsError;
use super::provider::{ConnectionStatus, ProviderKind};

/// A tenant's connection to a payment provider.
#[derive(Debug, Clone, PartialEq)]
pub struct ProviderConnection {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub provider: ProviderKind,
    /// The provider's account identifier (`acct_…` for Stripe).
    pub external_account_id: String,
    pub status: ConnectionStatus,
    /// The provider's capability map, verbatim from the last sync
    /// (`{"card_payments": {"status": "active"}, …}` for Stripe v2).
    /// Read-model views over this land with 09.2/09.3.
    pub capabilities: Value,
    /// The provider's outstanding-requirements summary, verbatim.
    pub requirements: Value,
    pub country: Option<String>,
    pub default_currency: Option<String>,
    pub connected_at: Option<DateTime<Utc>>,
    pub last_synced_at: Option<DateTime<Utc>>,
    pub disconnected_at: Option<DateTime<Utc>>,
}

impl ProviderConnection {
    /// A connection ready for onboarding (09.2 creates the external
    /// account and starts here).
    pub fn pending(
        id: Uuid,
        tenant_id: Uuid,
        provider: ProviderKind,
        external_account_id: String,
        now: DateTime<Utc>,
    ) -> Result<Self, PaymentsError> {
        if external_account_id.trim().is_empty() {
            return Err(PaymentsError::InvalidConnection(
                "external_account_id must not be empty".to_string(),
            ));
        }
        Ok(Self {
            id,
            tenant_id,
            provider,
            external_account_id,
            status: ConnectionStatus::Pending,
            capabilities: serde_json::json!({}),
            requirements: serde_json::json!({}),
            country: None,
            default_currency: None,
            connected_at: None,
            last_synced_at: Some(now),
            disconnected_at: None,
        })
    }
}
