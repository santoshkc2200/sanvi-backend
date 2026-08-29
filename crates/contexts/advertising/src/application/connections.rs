//! Connections: OAuth start/callback, account selection, list, disconnect
//! (TASK-011). Campaign CRUD and later per-request token use by a real
//! adapter stay out of scope here — this owns only "does a healthy
//! connection exist," which is exactly what campaigns.rs already depends on.

use std::sync::Arc;

use chrono::{Duration, Utc};
use sanvi_kernel::{TenantId, UserId};
use sanvi_persistence::TxManager;
use uuid::Uuid;

use crate::domain::model::ConnectionId;
use crate::{
    application::PlatformRegistry,
    domain::{
        AdvertisingConnection, AdvertisingConnectionRepository, AdvertisingError,
        AuthorizationExchange, AuthorizationRequest, ConnectionHandle, ConnectionHealth,
        OAuthStateManager, PlatformKey, TokenVault,
    },
};

const OAUTH_STATE_TTL_MINUTES: i64 = 10;

/// `begin_authorization`/`exchange_authorization` take a handle for
/// parity with every other port method, but neither call has a persisted
/// connection yet — a fresh id is a harmless placeholder no adapter is
/// expected to interpret as anything durable.
fn transient_handle(
    ids: &dyn crate::domain::IdGenerator,
) -> Result<ConnectionHandle, crate::domain::AdvertisingError> {
    Ok(ConnectionHandle {
        id: ids.next_connection_id()?.to_string(),
    })
}

/// Redirect the tenant to the platform's consent screen. Returns the URL and
/// the opaque state the frontend must echo back unmodified — it never
/// constructs the URL itself and never sees a token.
pub struct StartAuthorization {
    platforms: Arc<PlatformRegistry>,
    oauth_state: OAuthStateManager,
    ids: Arc<dyn crate::domain::IdGenerator>,
}

impl StartAuthorization {
    pub fn new(
        platforms: Arc<PlatformRegistry>,
        oauth_state: OAuthStateManager,
        ids: Arc<dyn crate::domain::IdGenerator>,
    ) -> Self {
        Self {
            platforms,
            oauth_state,
            ids,
        }
    }

    pub async fn execute(
        &self,
        tenant_id: TenantId,
        platform: PlatformKey,
        redirect_uri: String,
    ) -> Result<(String, String), AdvertisingError> {
        if redirect_uri.trim().is_empty() {
            return Err(AdvertisingError::InvalidInput(
                "advertising OAuth redirect_uri is required".to_string(),
            ));
        }
        let adapter = self.platforms.adapter(&platform)?;
        let state = self
            .oauth_state
            .issue(
                tenant_id,
                platform,
                Utc::now(),
                Duration::minutes(OAUTH_STATE_TTL_MINUTES),
            )
            .await?;
        let start = adapter
            .begin_authorization(
                &transient_handle(self.ids.as_ref())?,
                &AuthorizationRequest {
                    redirect_uri,
                    state: state.clone(),
                },
            )
            .await?;
        Ok((start.authorization_url, state))
    }
}

/// Redeem the callback: verify the signed state, exchange the (single-use)
/// authorization code, encrypt and store the refresh token, and persist a
/// `Pending` connection holding the fetched account list for the picker.
/// Nothing here is `Active` yet — [`CreateConnection`] finalizes it.
pub struct HandleCallback {
    tx: TxManager,
    connections: Arc<dyn AdvertisingConnectionRepository>,
    platforms: Arc<PlatformRegistry>,
    oauth_state: OAuthStateManager,
    tokens: Arc<TokenVault>,
    ids: Arc<dyn crate::domain::IdGenerator>,
}

impl HandleCallback {
    pub fn new(
        tx: TxManager,
        connections: Arc<dyn AdvertisingConnectionRepository>,
        platforms: Arc<PlatformRegistry>,
        oauth_state: OAuthStateManager,
        tokens: Arc<TokenVault>,
        ids: Arc<dyn crate::domain::IdGenerator>,
    ) -> Self {
        Self {
            tx,
            connections,
            platforms,
            oauth_state,
            tokens,
            ids,
        }
    }

    pub async fn execute(
        &self,
        tenant_id: TenantId,
        platform: PlatformKey,
        state: &str,
        code: String,
        redirect_uri: String,
        actor_id: Option<UserId>,
    ) -> Result<AdvertisingConnection, AdvertisingError> {
        self.oauth_state
            .redeem(state, tenant_id, &platform, Utc::now())
            .await?;
        let adapter = self.platforms.adapter(&platform)?;
        let exchanged = adapter
            .exchange_authorization(&AuthorizationExchange { code, redirect_uri })
            .await?;

        let connection_id = self.ids.next_connection_id()?;
        let mut scope = self
            .tx
            .begin_for(tenant_id.as_i64())
            .await
            .map_err(|error| AdvertisingError::Storage(error.to_string()))?;
        let token_ref = self
            .tokens
            .store_refresh_token(
                &mut scope,
                tenant_id,
                connection_id,
                &exchanged.refresh_token,
            )
            .await?;
        let pending = AdvertisingConnection::new_pending(
            connection_id,
            tenant_id,
            platform,
            token_ref,
            exchanged.granted_scopes,
            exchanged.expires_at,
            exchanged.accounts,
            actor_id,
            Utc::now(),
        )?;
        let stored = self.connections.upsert(&mut scope, &pending).await?;
        scope
            .commit()
            .await
            .map_err(|error| AdvertisingError::Storage(error.to_string()))?;
        Ok(stored)
    }
}

/// The tenant picked an account from the list `HandleCallback` fetched.
/// `currency`/`timezone` come from the frontend's own account picker (the
/// platform's picker UI already shows them) since [`PlatformAccount`] itself
/// is a cross-platform identity, not a full account profile.
pub struct CreateConnection {
    tx: TxManager,
    connections: Arc<dyn AdvertisingConnectionRepository>,
}

impl CreateConnection {
    pub fn new(tx: TxManager, connections: Arc<dyn AdvertisingConnectionRepository>) -> Self {
        Self { tx, connections }
    }

    pub async fn execute(
        &self,
        tenant_id: TenantId,
        connection_id: ConnectionId,
        chosen_external_id: &str,
        currency: String,
        timezone: String,
    ) -> Result<AdvertisingConnection, AdvertisingError> {
        let mut scope = self
            .tx
            .begin_for(tenant_id.as_i64())
            .await
            .map_err(|error| AdvertisingError::Storage(error.to_string()))?;
        let mut connection = self
            .connections
            .find_by_id(&mut scope, tenant_id, connection_id)
            .await?
            .ok_or(AdvertisingError::ConnectionNotFound)?;
        let chosen = connection
            .pending_accounts()
            .and_then(|accounts| {
                accounts
                    .iter()
                    .find(|a| a.external_id == chosen_external_id)
            })
            .cloned()
            .ok_or_else(|| {
                AdvertisingError::InvalidInput(
                    "chosen advertising account was not among the fetched accounts".to_string(),
                )
            })?;
        connection.finalize(&chosen, currency, timezone)?;
        let stored = self
            .connections
            .finalize_pending(&mut scope, &connection)
            .await?;
        scope
            .commit()
            .await
            .map_err(|error| AdvertisingError::Storage(error.to_string()))?;
        Ok(stored)
    }
}

/// Every connection with its server-computed health.
pub struct ListConnections {
    tx: TxManager,
    connections: Arc<dyn AdvertisingConnectionRepository>,
    platforms: Arc<PlatformRegistry>,
}

impl ListConnections {
    pub fn new(
        tx: TxManager,
        connections: Arc<dyn AdvertisingConnectionRepository>,
        platforms: Arc<PlatformRegistry>,
    ) -> Self {
        Self {
            tx,
            connections,
            platforms,
        }
    }

    pub fn health(
        &self,
        connection: &AdvertisingConnection,
        now: chrono::DateTime<Utc>,
    ) -> Result<ConnectionHealth, AdvertisingError> {
        let adapter = self.platforms.adapter(connection.platform())?;
        Ok(connection.health(&adapter.required_scopes(), now))
    }

    pub async fn execute(
        &self,
        tenant_id: TenantId,
    ) -> Result<Vec<(AdvertisingConnection, ConnectionHealth)>, AdvertisingError> {
        let mut scope = self
            .tx
            .begin_for(tenant_id.as_i64())
            .await
            .map_err(|error| AdvertisingError::Storage(error.to_string()))?;
        let connections = self.connections.list(&mut scope, tenant_id).await?;
        scope
            .commit()
            .await
            .map_err(|error| AdvertisingError::Storage(error.to_string()))?;
        let now = Utc::now();
        connections
            .into_iter()
            .map(|connection| {
                let health = self.health(&connection, now)?;
                Ok((connection, health))
            })
            .collect()
    }
}

/// Disconnects a connection. Dependent campaign syncs stop the next time
/// they check `ConnectionStatus`; nothing here touches the platform side —
/// disconnecting in Sanvi does not pause the tenant's ads there.
pub struct DeleteConnection {
    tx: TxManager,
    connections: Arc<dyn AdvertisingConnectionRepository>,
}

impl DeleteConnection {
    pub fn new(tx: TxManager, connections: Arc<dyn AdvertisingConnectionRepository>) -> Self {
        Self { tx, connections }
    }

    pub async fn execute(
        &self,
        tenant_id: TenantId,
        connection_id: ConnectionId,
    ) -> Result<(), AdvertisingError> {
        let mut scope = self
            .tx
            .begin_for(tenant_id.as_i64())
            .await
            .map_err(|error| AdvertisingError::Storage(error.to_string()))?;
        let mut connection = self
            .connections
            .find_by_id(&mut scope, tenant_id, connection_id)
            .await?
            .ok_or(AdvertisingError::ConnectionNotFound)?;
        connection.disconnect();
        self.connections.update(&mut scope, &connection).await?;
        scope
            .commit()
            .await
            .map_err(|error| AdvertisingError::Storage(error.to_string()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::domain::model::ConnectionId;
    use chrono::Utc;
    use sanvi_kernel::{TenantId, UserId};
    use sanvi_persistence::TxManager;
    use uuid::Uuid;

    use crate::{
        AdvertisingConnection, SecretRef,
        adapters::{FakeAdPlatform, SqlxAdvertisingConnectionRepository},
    };

    use super::ListConnections;

    #[tokio::test]
    async fn health_uses_the_selected_adapters_required_scopes() {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://unused:unused@localhost/unused")
            .unwrap();
        let registry = Arc::new(FakeAdPlatform::registered_catalog());
        let connections = Arc::new(SqlxAdvertisingConnectionRepository::new());
        let list = ListConnections::new(
            TxManager::new(pool, sanvi_testsupport::test_ids()),
            connections,
            registry.clone(),
        );
        let key = registry.adapters().first().unwrap().descriptor().key;
        let mut connection = AdvertisingConnection::new(
            crate::domain::model::ConnectionId::try_from(1_000_100_i64).unwrap(),
            sanvi_kernel::TenantId::try_from(1_000_101_i64).unwrap(),
            key,
            "123-456-7890",
            "JPY",
            "Asia/Tokyo",
            SecretRef::new("vault-ref"),
            None,
            Utc::now(),
        )
        .unwrap();
        connection.apply_credentials(None, vec!["ads.read".to_string()], None);

        let health = list.health(&connection, Utc::now()).unwrap();

        assert!(!health.can_sync);
        assert_eq!(health.scopes_missing, ["ads.manage"]);
    }
}
