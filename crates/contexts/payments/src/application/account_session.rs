//! The account-session use case (sub-phase 09.2): mint the short-lived
//! credential the tenant's frontend renders the provider's embedded
//! onboarding component with.
//!
//! The session is fetched per render, never persisted and never logged;
//! its only trace is the audit entry recording that it was issued. A
//! `Disconnected` (or `Rejected`) connection gets nothing — there is no
//! onboarding left to do on a dead connection.

use std::net::IpAddr;
use std::sync::Arc;

use chrono::Utc;

use sanvi_audit::domain::{ActorType, AuditEntryData, AuditWriter};
use sanvi_kernel::{TenantId, UserId};
use sanvi_persistence::TxManager;

use crate::domain::connection::ProviderConnection;
use crate::domain::error::PaymentsError;
use crate::domain::ports::{ProviderAccountSession, ProviderConnectionRepository};
use crate::domain::provider::ConnectionStatus;

pub struct CreateAccountSession {
    tx_manager: TxManager,
    registry: Arc<super::registry::ProviderRegistry>,
    connections: Arc<dyn ProviderConnectionRepository>,
    audit: Arc<dyn AuditWriter>,
}

impl CreateAccountSession {
    pub fn new(
        tx_manager: TxManager,
        registry: Arc<super::registry::ProviderRegistry>,
        connections: Arc<dyn ProviderConnectionRepository>,
        audit: Arc<dyn AuditWriter>,
    ) -> Self {
        Self {
            tx_manager,
            registry,
            connections,
            audit,
        }
    }

    /// Mint a session for the connection's external account.
    ///
    /// Returns the session plus the connection *when its status moved*
    /// (`Pending → Onboarding` on first mint) — the caller persists that
    /// transition. The session secret itself is never persisted.
    #[tracing::instrument(name = "payments.create_account_session", skip_all, fields(tenant_id = %connection.tenant_id))]
    pub async fn execute(
        &self,
        actor: UserId,
        ip: Option<IpAddr>,
        tenant_id: TenantId,
        connection: &ProviderConnection,
    ) -> Result<(ProviderAccountSession, Option<ProviderConnection>), PaymentsError> {
        if connection.tenant_id != tenant_id {
            // Tenant isolation is enforced by RLS on the read; this is the
            // explicit belt to its braces.
            return Err(PaymentsError::ConnectionNotActive(
                "connection belongs to another tenant".to_string(),
            ));
        }
        if matches!(
            connection.status,
            ConnectionStatus::Disconnected | ConnectionStatus::Rejected
        ) {
            return Err(PaymentsError::ConnectionNotActive(format!(
                "connection is {}",
                connection.status.as_str()
            )));
        }

        let adapter = self.registry.get(connection.provider).ok_or_else(|| {
            PaymentsError::InvalidConnection(format!(
                "provider {:?} is not available in this environment",
                connection.provider.as_str()
            ))
        })?;

        let session = adapter
            .create_account_session(&connection.external_account_id)
            .await?;
        if session.expires_at <= Utc::now() {
            // The provider minted an already-expired credential: refuse to
            // hand it on rather than let the frontend render a dead
            // component.
            return Err(PaymentsError::ProviderUnavailable(
                "account session expired before issuance".to_string(),
            ));
        }

        // First mint moves the connection `Pending → Onboarding` so the
        // settings page reflects the flow in flight. Later mints (a
        // re-render) leave the status alone — the state machine forbids
        // same-state transitions, and 09.3's webhooks own everything past
        // Onboarding.
        let mut transitioned = None;
        if connection.status == ConnectionStatus::Pending {
            let snapshot = adapter.fetch_status(&connection.external_account_id).await;
            match snapshot {
                Ok(snapshot) => {
                    let mut updated = connection.clone();
                    if updated
                        .apply_status(&snapshot, ConnectionStatus::Onboarding, Utc::now())
                        .is_ok()
                    {
                        let mut scope = self
                            .tx_manager
                            .begin_for(tenant_id.as_i64())
                            .await
                            .map_err(|err| PaymentsError::Storage(err.to_string()))?;
                        self.connections.update(&mut scope, &updated).await?;
                        scope
                            .commit()
                            .await
                            .map_err(|err| PaymentsError::Storage(err.to_string()))?;
                        transitioned = Some(updated);
                    }
                }
                // The session is already minted — a status-fetch failure
                // must not fail the request; 09.3's sync reconciles.
                Err(err) => {
                    tracing::warn!(error = %err, "session minted; status fetch for Pending → Onboarding failed")
                }
            }
        }

        self.audit_issuance(actor, ip, tenant_id, connection).await;
        Ok((session, transitioned))
    }

    async fn audit_issuance(
        &self,
        actor: UserId,
        ip: Option<IpAddr>,
        tenant_id: TenantId,
        connection: &ProviderConnection,
    ) {
        let Ok(mut scope) = self
            .tx_manager
            .begin_unscoped("audit_payments_account_session_issued", "payments")
            .await
        else {
            tracing::error!(
                connection_id = %connection.id,
                "failed to open audit scope for account-session issuance"
            );
            return;
        };
        if let Err(err) = self
            .audit
            .append(
                &mut scope,
                &AuditEntryData {
                    occurred_at: Utc::now(),
                    actor_type: ActorType::User,
                    actor_id: Some(actor),
                    on_behalf_of: None,
                    tenant_id: Some(tenant_id),
                    action: "payments.account_session_issued".to_string(),
                    resource_type: Some("payments.provider_connection".to_string()),
                    resource_id: Some(connection.id.to_string()),
                    before: None,
                    // The secret is never in the audit entry — only the
                    // fact and the account it was for.
                    after: Some(serde_json::json!({
                        "provider": connection.provider.as_str(),
                        "external_account_id": connection.external_account_id,
                    })),
                    request_id: None,
                    ip,
                    user_agent: None,
                },
            )
            .await
        {
            tracing::error!(
                connection_id = %connection.id,
                error = %err,
                "failed to audit account-session issuance"
            );
        }
        if let Err(err) = scope.commit().await {
            tracing::error!(
                connection_id = %connection.id,
                error = %err,
                "failed to commit account-session audit entry"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    // The use cases' real exercise is the integration suite (DB-backed,
    // outbox/audit asserted end to end); unit tests here would need a
    // scoped-tx fake that reproduces sqlx, which proves nothing.
}
