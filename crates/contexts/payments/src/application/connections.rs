//! The connection use cases.
//!
//! 09.1 ships the first one: `StartProviderConnection` — ask a registered
//! provider to create the tenant's account, persist the connection, and
//! emit `ProviderConnectionStarted` through the outbox in the same
//! transaction. It has no route yet (that is 09.2's `POST
//! /api/v1/tenant/payments/connections`); it exists now because the
//! outbox end-to-end acceptance criterion runs through it, and because
//! building the route later means adding a handler, not a use case.

use std::sync::Arc;

use chrono::Utc;
use uuid::Uuid;

use sanvi_persistence::TxManager;

use crate::domain::connection::ProviderConnection;
use crate::domain::error::PaymentsError;
use crate::domain::events::{EventPublisher, PaymentsEvent};
use crate::domain::money::Currency;
use crate::domain::ports::{CreateAccountRequest, ProviderConnectionRepository};
use crate::domain::provider::{ConnectionStatus, CountryCode, ProviderKind};

/// A command to start (or resume) a tenant's connection to a provider.
#[derive(Debug, Clone)]
pub struct StartConnectionCommand {
    pub tenant_id: Uuid,
    pub provider: ProviderKind,
    pub country: Option<CountryCode>,
    pub default_currency: Option<Currency>,
}

pub struct StartProviderConnection {
    tx_manager: TxManager,
    registry: Arc<super::registry::ProviderRegistry>,
    connections: Arc<dyn ProviderConnectionRepository>,
    events: Arc<dyn EventPublisher>,
}

impl StartProviderConnection {
    pub fn new(
        tx_manager: TxManager,
        registry: Arc<super::registry::ProviderRegistry>,
        connections: Arc<dyn ProviderConnectionRepository>,
        events: Arc<dyn EventPublisher>,
    ) -> Self {
        Self {
            tx_manager,
            registry,
            connections,
            events,
        }
    }

    /// Create the provider account and the `Pending` connection row, and
    /// emit `ProviderConnectionStarted` — all in one tenant-scoped
    /// transaction, so the outbox row and the connection row commit or
    /// roll back together.
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

        let account = adapter
            .create_account(&CreateAccountRequest {
                tenant_id: command.tenant_id,
                country: command.country.clone(),
                default_currency: command.default_currency.clone(),
            })
            .await?;

        let now = Utc::now();
        let connection = ProviderConnection::pending(
            Uuid::now_v7(),
            command.tenant_id,
            adapter.kind(),
            account.external_account_id,
            now,
        )?;

        let mut scope = self
            .tx_manager
            .begin_for(command.tenant_id)
            .await
            .map_err(|err| PaymentsError::Storage(err.to_string()))?;

        // Idempotency at the aggregate boundary: the partial unique index
        // one_active_provider_per_tenant rejects a second live row, so a
        // retried command surfaces as a conflict, not a duplicate.
        if let Some(existing) = self
            .connections
            .find_active(&mut scope, command.tenant_id, adapter.kind().as_str())
            .await?
        {
            let _ = scope.rollback().await;
            return Err(PaymentsError::InvalidConnection(format!(
                "tenant already has a {} connection in status {}",
                existing.provider.as_str(),
                existing.status.as_str()
            )));
        }

        self.connections.insert(&mut scope, &connection).await?;
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
        Ok(connection)
    }
}

/// Resume onboarding: mint an account session for the embedded component
/// and move the connection `Pending → Onboarding`. The session secret is
/// returned to the caller and never persisted — it is ephemeral by
/// design, and 09.2's route hands it straight to the frontend.
pub struct ResumeProviderOnboarding {
    registry: Arc<super::registry::ProviderRegistry>,
}

impl ResumeProviderOnboarding {
    pub fn new(registry: Arc<super::registry::ProviderRegistry>) -> Self {
        Self { registry }
    }

    pub async fn execute(
        &self,
        connection: &ProviderConnection,
    ) -> Result<
        (
            crate::domain::ports::ProviderAccountSession,
            ProviderConnection,
        ),
        PaymentsError,
    > {
        let adapter = self.registry.get(connection.provider).ok_or_else(|| {
            PaymentsError::InvalidConnection(format!(
                "provider {:?} is not available in this environment",
                connection.provider.as_str()
            ))
        })?;
        let session = adapter
            .create_account_session(&connection.external_account_id)
            .await?;
        let mut resumed = connection.clone();
        resumed.apply_status(
            &adapter
                .fetch_status(&connection.external_account_id)
                .await?,
            ConnectionStatus::Onboarding,
            Utc::now(),
        )?;
        Ok((session, resumed))
    }
}

#[cfg(test)]
mod tests {
    // The use cases' real exercise is the integration suite (DB-backed,
    // outbox asserted end to end); unit tests here would need a scoped-tx
    // fake that reproduces sqlx, which proves nothing.
}
