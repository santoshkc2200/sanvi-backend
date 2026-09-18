//! The create-connection use case: the least reversible request in the
//! whole phase (sub-phase 09.2).

use std::net::IpAddr;
use std::sync::Arc;

use chrono::Utc;
use uuid::Uuid;

use sanvi_audit::domain::{ActorType, AuditEntryData, AuditWriter};
use sanvi_kernel::{TenantId, UserId};
use sanvi_persistence::TxManager;

use crate::domain::connection::ProviderConnection;
use crate::domain::error::PaymentsError;
use crate::domain::events::{EventPublisher, PaymentsEvent};
use crate::domain::money::Currency;
use crate::domain::ports::{CreateAccountRequest, IdGenerator, ProviderConnectionRepository};
use crate::domain::provider::{CountryCode, ProviderKind};

/// A command to start (or resume) a tenant's connection to a provider.
#[derive(Debug, Clone)]
pub struct StartConnectionCommand {
    pub tenant_id: TenantId,
    pub provider: ProviderKind,
    pub country: Option<CountryCode>,
    pub default_currency: Option<Currency>,
    /// Business display name (tenant data) prefilled at the provider.
    pub display_name: Option<String>,
    /// The caller's `Idempotency-Key` — required on every mutating
    /// payments endpoint (NFR-904) and the root of the provider-side
    /// idempotency chain here.
    pub idempotency_key: Uuid,
    /// Who asked (for the audit entry).
    pub actor: UserId,
    pub ip: Option<IpAddr>,
}

pub struct StartProviderConnection {
    tx_manager: TxManager,
    registry: Arc<super::registry::ProviderRegistry>,
    connections: Arc<dyn ProviderConnectionRepository>,
    events: Arc<dyn EventPublisher>,
    audit: Arc<dyn AuditWriter>,
    ids: Arc<dyn IdGenerator>,
}

impl StartProviderConnection {
    pub fn new(
        tx_manager: TxManager,
        registry: Arc<super::registry::ProviderRegistry>,
        connections: Arc<dyn ProviderConnectionRepository>,
        events: Arc<dyn EventPublisher>,
        audit: Arc<dyn AuditWriter>,
        ids: Arc<dyn IdGenerator>,
    ) -> Self {
        Self {
            tx_manager,
            registry,
            connections,
            events,
            audit,
            ids,
        }
    }

    /// Create the provider account and the `Pending` connection row, and
    /// emit `ProviderConnectionStarted` — the row and the outbox event
    /// commit or roll back together in one tenant-scoped transaction.
    ///
    /// Idempotent: the same `(tenant, Idempotency-Key)` returns the same
    /// connection — a replay after a successful create, an adoption after
    /// a crash mid-create, or (concurrently) whichever insert won the
    /// race. A *different* key while an active connection exists is a
    /// `409 payments/connection-exists`.
    #[tracing::instrument(name = "payments.start_provider_connection", skip_all, fields(tenant_id = %command.tenant_id))]
    pub async fn execute(
        &self,
        command: StartConnectionCommand,
    ) -> Result<ProviderConnection, PaymentsError> {
        let adapter = self.registry.get(command.provider).ok_or_else(|| {
            PaymentsError::InvalidConnection(format!(
                "provider {:?} is not available in this environment",
                command.provider.as_str()
            ))
        })?;

        // Replay/conflict check. A concurrent create with the same key
        // falls through to the insert, which the unique indexes arbitrate.
        {
            let mut scope = self
                .tx_manager
                .begin_for(command.tenant_id.as_i64())
                .await
                .map_err(|err| PaymentsError::Storage(err.to_string()))?;
            if let Some(existing) = self
                .connections
                .find_active(&mut scope, command.tenant_id, adapter.kind().as_str())
                .await?
            {
                let _ = scope.rollback().await;
                return Err(PaymentsError::ConnectionExists(format!(
                    "tenant already has a {} connection in status {}",
                    existing.provider.as_str(),
                    existing.status.as_str()
                )));
            }
        }

        let connection_id = self.ids.next_connection_id()?;

        let account = adapter
            .create_account(&CreateAccountRequest {
                tenant_id: command.tenant_id,
                country: command.country.clone(),
                default_currency: command.default_currency.clone(),
                display_name: command.display_name.clone(),
                idempotency_key: command.idempotency_key,
            })
            .await?;

        // Best-effort snapshot of the freshly created account: the tenant
        // sees outstanding requirements immediately instead of after the
        // first webhook (09.3 owns keeping this truthful).
        let snapshot = match adapter.fetch_status(&account.external_account_id).await {
            Ok(snapshot) => Some(snapshot),
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    "connection created; initial status fetch failed (09.3 sync will reconcile)"
                );
                None
            }
        };

        let now = Utc::now();
        let mut connection = ProviderConnection::pending(
            connection_id,
            command.tenant_id,
            adapter.kind(),
            account.external_account_id,
            now,
        )?;
        if let Some(snapshot) = &snapshot {
            connection.capabilities = serde_json::to_value(&snapshot.capabilities)
                .map_err(|err| PaymentsError::Storage(err.to_string()))?;
            connection.requirements = serde_json::to_value(&snapshot.requirements)
                .map_err(|err| PaymentsError::Storage(err.to_string()))?;
            connection.country = snapshot.country.as_ref().map(|c| c.as_str().to_string());
            connection.default_currency = snapshot
                .default_currency
                .as_ref()
                .map(|c| c.as_str().to_string());
        }

        match self.persist(&command, &connection).await {
            Ok(()) => {}
            Err(persist_err) => {
                // A concurrent same-key create may have won the insert race
                // between our guard and our insert. Ids are now generated,
                // not derived, so the winner's id is not `connection_id`:
                // look it up the same way the guard did, by the
                // one-active-connection-per-provider constraint.
                let mut scope = self
                    .tx_manager
                    .begin_for(command.tenant_id.as_i64())
                    .await
                    .map_err(|_| persist_err.clone())?;
                let adopted = self
                    .connections
                    .find_active(&mut scope, command.tenant_id, adapter.kind().as_str())
                    .await?;
                let _ = scope.rollback().await;
                match adopted {
                    Some(winner) => return Ok(winner),
                    None => return Err(persist_err),
                }
            }
        }

        self.audit_creation(&command, &connection).await;
        Ok(connection)
    }

    /// The tenant-scoped transaction: connection row + outbox event.
    async fn persist(
        &self,
        command: &StartConnectionCommand,
        connection: &ProviderConnection,
    ) -> Result<(), PaymentsError> {
        let mut scope = self
            .tx_manager
            .begin_for(command.tenant_id.as_i64())
            .await
            .map_err(|err| PaymentsError::Storage(err.to_string()))?;

        self.connections.insert(&mut scope, connection).await?;
        self.events
            .publish_scoped(
                &mut scope,
                command.tenant_id,
                &[PaymentsEvent::ProviderConnectionStarted {
                    connection_id: connection.id,
                    provider: connection.provider.as_str().to_string(),
                }],
            )
            .await?;

        scope
            .commit()
            .await
            .map_err(|err| PaymentsError::Storage(err.to_string()))?;
        Ok(())
    }

    /// Audit after the commit: a payment-affecting action is on the
    /// record with actor, tenant and IP (phase-09 security bar). Audit
    /// failure must not undo a committed provider account — it is logged
    /// loudly instead.
    async fn audit_creation(
        &self,
        command: &StartConnectionCommand,
        connection: &ProviderConnection,
    ) {
        let Ok(mut scope) = self
            .tx_manager
            .begin_unscoped("audit_payments_connection_created", "payments")
            .await
        else {
            tracing::error!(
                connection_id = %connection.id,
                "failed to open audit scope for connection creation"
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
                    action: "payments.connection_created".to_string(),
                    resource_type: Some("payments.provider_connection".to_string()),
                    resource_id: Some(connection.id.to_string()),
                    before: None,
                    after: Some(serde_json::json!({
                        "provider": connection.provider.as_str(),
                        "external_account_id": connection.external_account_id,
                        "status": connection.status.as_str(),
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
                "failed to audit connection creation"
            );
        }
        if let Err(err) = scope.commit().await {
            tracing::error!(connection_id = %connection.id, error = %err, "failed to commit audit entry");
        }
    }
}
