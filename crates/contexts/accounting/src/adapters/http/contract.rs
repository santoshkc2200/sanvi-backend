//! The Accounting HTTP contract the platform publishes in `api/accounting.yaml`.
//! The standalone Accounting service produces and consumes these bodies; the
//! gateway relays them unchanged, so these types are never constructed here.
//! They must stay in step with the service's own definitions.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::domain::{AccountMappings, BookId, ConnectionId, Provider};

/// RFC-9457 problem type slugs emitted by accounting endpoints.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub enum AccountingProblemType {
    #[serde(rename = "accounting/authorization-rejected")]
    AuthorizationRejected,
    #[serde(rename = "accounting/reauthorization-required")]
    ReauthorizationRequired,
    #[serde(rename = "accounting/verification-failed")]
    VerificationFailed,
    #[serde(rename = "accounting/provider-unavailable")]
    ProviderUnavailable,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateBookRequest {
    pub legal_entity_key: String,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RegisterConnectionRequest {
    pub provider: Provider,
    pub organization_id: String,
    pub mappings: AccountMappings,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CompleteAuthorizationRequest {
    pub state: String,
    pub code: String,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ActivateBookRequest {
    #[schema(value_type = String)]
    pub connection_id: ConnectionId,
    pub starts_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ScheduleSwitchRequest {
    #[schema(value_type = String)]
    pub connection_id: ConnectionId,
    pub cutover_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct BookView {
    #[schema(value_type = String)]
    pub id: BookId,
    pub legal_entity_key: String,
    pub revision: i64,
    pub connections: Vec<ConnectionView>,
    pub periods: Vec<PeriodView>,
    pub scheduled_switch: Option<PeriodView>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
#[schema(as = AccountingConnectionView)]
pub struct ConnectionView {
    #[schema(value_type = String)]
    pub id: ConnectionId,
    pub provider: Provider,
    pub organization_id: String,
    pub mappings: AccountMappings,
    pub credential_status: CredentialStatusView,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum CredentialStatusView {
    None,
    Active,
    ReauthorizationRequired,
    Revoked,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct PeriodView {
    #[schema(value_type = String)]
    pub connection_id: ConnectionId,
    pub starts_at: DateTime<Utc>,
    pub ends_at: Option<DateTime<Utc>>,
    pub state: PeriodStateView,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum PeriodStateView {
    Active,
    Scheduled,
    Draining,
    Closed,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct StartAuthorizationResponse {
    pub authorization_url: String,
}
