//! The disconnect use case (sub-phase 09.7): a tenant ends their
//! connection with a payment provider.
//!
//! Semantics are deliberately narrow:
//!
//! - **Guards first** (`DisconnectBlocker`): refuse while any payment is
//!   in flight, any dispute is open, or any payout is pending — exactly
//!   the states where losing the live connection would cost the tenant
//!   their ability to resolve something with their own money. The refusal
//!   carries the blocker list so the UI can name what to wait for.
//! - **Local, reversible-by-reconnect**: mark the row `Disconnected`,
//!   stop new checkouts (the checkout gate skips disconnected rows), keep
//!   every historical record. The Stripe account itself is *not* closed
//!   or deleted here — it is the tenant's account; closing it is theirs
//!   to do in their own Dashboard.
//! - **Idempotent**: deleting an already-disconnected connection returns
//!   success — a retried DELETE must not turn into an error after the
//!   first one landed.
//! - **Money truth never stops**: the webhook handlers route by account
//!   through [`crate::domain::ports::ConnectionLifecycleStore::
//! find_by_external_account`], so a payment whose session was minted
//!   before the disconnect is still recorded when it completes.
//!
//! One tenant-scoped transaction covers the guards, the status move and
//! the outbox event, so a concurrent refund or payout webhook cannot slip
//! between "looks clean" and "disconnected".

use std::net::IpAddr;
use std::sync::Arc;

use chrono::Utc;

use sanvi_audit::domain::{ActorType, AuditEntryData, AuditWriter};
use sanvi_kernel::{TenantId, UserId};
use sanvi_persistence::TxManager;

use crate::domain::connection::{ConnectionId, ProviderConnection};
use crate::domain::disconnect::DisconnectBlocker;
use crate::domain::error::PaymentsError;
use crate::domain::events::{EventPublisher, PaymentsEvent};
use crate::domain::ports::{
    DisputeRepository, PaymentRepository, PayoutRepository, ProviderConnectionRepository,
};

/// The reason recorded on `payments.provider_disconnected` for a
/// tenant-initiated disconnect.
pub const TENANT_REQUEST_REASON: &str = "tenant request";

#[derive(Debug, Clone)]
pub struct DisconnectCommand {
    pub tenant_id: TenantId,
    /// Which of the tenant's connections to disconnect (a reconnect
    /// history can hold several).
    pub connection_id: ConnectionId,
    /// Who asked — the audit entry's actor.
    pub actor: UserId,
    pub ip: Option<IpAddr>,
}

pub struct DisconnectConnection {
    tx_manager: TxManager,
    connections: Arc<dyn ProviderConnectionRepository>,
    payments: Arc<dyn PaymentRepository>,
    disputes: Arc<dyn DisputeRepository>,
    payouts: Arc<dyn PayoutRepository>,
    events: Arc<dyn EventPublisher>,
    audit: Arc<dyn AuditWriter>,
}

impl DisconnectConnection {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        tx_manager: TxManager,
        connections: Arc<dyn ProviderConnectionRepository>,
        payments: Arc<dyn PaymentRepository>,
        disputes: Arc<dyn DisputeRepository>,
        payouts: Arc<dyn PayoutRepository>,
        events: Arc<dyn EventPublisher>,
        audit: Arc<dyn AuditWriter>,
    ) -> Self {
        Self {
            tx_manager,
            connections,
            payments,
            disputes,
            payouts,
            events,
            audit,
        }
    }

    /// Guard, transition and emit in one tenant-scoped transaction.
    /// Returns the disconnected aggregate.
    #[tracing::instrument(
        name = "payments.connection.disconnect",
        skip(self, command),
        fields(tenant_id = %command.tenant_id, connection_id = %command.connection_id)
    )]
    pub async fn execute(
        &self,
        command: DisconnectCommand,
    ) -> Result<ProviderConnection, PaymentsError> {
        let mut scope = self
            .tx_manager
            .begin_for(command.tenant_id.as_i64())
            .await
            .map_err(|err| PaymentsError::Storage(err.to_string()))?;

        let Some(mut connection) = self
            .connections
            .find_by_id(&mut scope, command.tenant_id, command.connection_id)
            .await?
        else {
            // RLS-scoped read: another tenant's id resolves to nothing.
            return Err(PaymentsError::InvalidConnection(format!(
                "connection {} not found",
                command.connection_id
            )));
        };

        // Already disconnected: the retry of a landed DELETE. Answer from
        // the record instead of failing on an illegal second transition.
        if connection.disconnected_at.is_some()
            || connection.status == crate::domain::ConnectionStatus::Disconnected
        {
            let _ = scope.rollback().await;
            return Ok(connection);
        }

        // ---- the money guards -----------------------------------------
        let mut blockers = Vec::new();
        if self
            .payments
            .count_in_flight_for_connection(&mut scope, command.tenant_id, command.connection_id)
            .await?
            > 0
        {
            blockers.push(DisconnectBlocker::InFlightPayments);
        }
        if self
            .disputes
            .count_open_for_connection(&mut scope, command.tenant_id, command.connection_id)
            .await?
            > 0
        {
            blockers.push(DisconnectBlocker::OpenDisputes);
        }
        if self
            .payouts
            .count_pending_for_connection(&mut scope, command.tenant_id, command.connection_id)
            .await?
            > 0
        {
            blockers.push(DisconnectBlocker::PendingPayouts);
        }
        if !blockers.is_empty() {
            let _ = scope.rollback().await;
            return Err(PaymentsError::DisconnectBlocked { blockers });
        }

        // The domain's transition table has the final word (a Rejected row
        // refuses here too — the provider already ended that account).
        connection.disconnect(Utc::now())?;
        self.connections.update(&mut scope, &connection).await?;

        self.events
            .publish_scoped(
                &mut scope,
                command.tenant_id,
                &[PaymentsEvent::ProviderDisconnected {
                    connection_id: connection.id,
                    provider: connection.provider.as_str().to_string(),
                    reason: TENANT_REQUEST_REASON.to_string(),
                }],
            )
            .await?;

        scope
            .commit()
            .await
            .map_err(|err| PaymentsError::Storage(err.to_string()))?;

        self.audit_disconnect(&command, &connection).await;
        Ok(connection)
    }

    /// Audit after the commit, mirroring connection creation: a failed
    /// audit write is logged loudly, never a rollback of a committed
    /// disconnect.
    async fn audit_disconnect(&self, command: &DisconnectCommand, connection: &ProviderConnection) {
        let Ok(mut scope) = self
            .tx_manager
            .begin_unscoped("audit_payments_connection_disconnected", "payments")
            .await
        else {
            tracing::error!(
                connection_id = %connection.id,
                "failed to open audit scope for connection disconnect"
            );
            return;
        };
        let result = self
            .audit
            .append(
                &mut scope,
                &AuditEntryData {
                    occurred_at: Utc::now(),
                    actor_type: ActorType::User,
                    actor_id: Some(command.actor),
                    on_behalf_of: None,
                    tenant_id: Some(command.tenant_id),
                    action: "payments.connection_disconnected".to_string(),
                    resource_type: Some("payments.provider_connection".to_string()),
                    resource_id: Some(connection.id.to_string()),
                    before: None,
                    after: Some(serde_json::json!({
                        "provider": connection.provider.as_str(),
                        "external_account_id": connection.external_account_id,
                        "status": connection.status.as_str(),
                        "reason": TENANT_REQUEST_REASON,
                    })),
                    request_id: None,
                    ip: command.ip,
                    user_agent: None,
                },
            )
            .await;
        if let Err(err) = result {
            tracing::error!(
                connection_id = %connection.id,
                error = %err,
                "failed to audit connection disconnect"
            );
        }
        if let Err(err) = scope.commit().await {
            tracing::error!(
                connection_id = %connection.id,
                error = %err,
                "failed to commit audit entry"
            );
        }
    }
}
