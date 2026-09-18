//! Cross-tenant OAuth token refresh sweep (TASK-018, slice 10.9).
//!
//! `refresh_failure_transition` and the connection methods it drives
//! (`record_permanent_refresh_failure`, `mark_disconnected_notified`,
//! `mark_dependent_jobs_paused`) were tested domain code with no caller: a
//! revoked credential only ever reached `disconnected` when the tenant deleted
//! the connection. This sweep is that caller.
//!
//! Refreshing means minting through the tenant's own vault — resolve the
//! stored refresh token, trade it at the platform's grant — which is exactly
//! what [`ConnectionAccessTokenProvider`] does on a live request path. Reusing
//! it keeps one refresh path in the crate; probing some read endpoint instead
//! would test reachability, not the credential.
//!
//! A platform with no registered provider is skipped, not failed: the fake
//! platforms and any adapter whose OAuth client is unconfigured have no grant
//! to mint against, and reporting those as refresh failures would bury the
//! real ones.

use std::collections::HashMap;
use std::sync::Arc;

use chrono::Utc;
use sanvi_persistence::TxManager;

use crate::adapters::AdvertisingMetrics;
use crate::domain::{
    AdvertisingConnectionRepository, AdvertisingError, ConnectionAccessTokenProvider,
    ConnectionHandle, ConnectionStatus, PlatformKey, RefreshFailureKind, SyncError,
    connection_lifecycle::{classify_refresh_failure, refresh_failure_transition},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TokenRefreshSweepReport {
    pub checked: u32,
    pub refreshed: u32,
    /// Credentials the platform would not renew.
    pub failed: u32,
    /// Connections whose platform has no token provider wired.
    pub skipped: u32,
    /// Storage or transaction failures — the sweep's own trouble, not the
    /// tenant's.
    pub errors: u32,
}

pub struct RunTokenRefresh {
    tx: TxManager,
    connections: Arc<dyn AdvertisingConnectionRepository>,
    providers: HashMap<PlatformKey, Arc<dyn ConnectionAccessTokenProvider>>,
    metrics: Arc<AdvertisingMetrics>,
    stale_after_secs: i64,
}

impl RunTokenRefresh {
    pub fn new(
        tx: TxManager,
        connections: Arc<dyn AdvertisingConnectionRepository>,
        providers: HashMap<PlatformKey, Arc<dyn ConnectionAccessTokenProvider>>,
        metrics: Arc<AdvertisingMetrics>,
        stale_after_secs: i64,
    ) -> Self {
        Self {
            tx,
            connections,
            providers,
            metrics,
            stale_after_secs,
        }
    }

    #[tracing::instrument(name = "advertising.run_token_refresh", skip_all)]
    pub async fn execute(&self, limit: i64) -> Result<TokenRefreshSweepReport, AdvertisingError> {
        let mut scope = self
            .tx
            .begin_unscoped(
                "advertising_token_refresh_sweep",
                "advertising-token-refresh",
            )
            .await
            .map_err(|error| AdvertisingError::Storage(error.to_string()))?;

        let due_connections = self
            .connections
            .list_due_for_token_refresh(&mut scope, self.stale_after_secs, limit)
            .await?;

        scope
            .commit()
            .await
            .map_err(|error| AdvertisingError::Storage(error.to_string()))?;

        let mut report = TokenRefreshSweepReport {
            checked: due_connections.len() as u32,
            ..Default::default()
        };

        for mut connection in due_connections {
            let tenant_id = connection.tenant_id();
            let platform = connection.platform().clone();

            let Some(provider) = self.providers.get(&platform) else {
                report.skipped += 1;
                continue;
            };

            let handle = ConnectionHandle {
                id: connection.id().to_string(),
                tenant_id,
                external_account_id: connection.external_account_id().to_string(),
            };

            let now = Utc::now();
            let outcome = provider.access_token(&handle).await;
            let refreshed = outcome.is_ok();

            match outcome {
                Ok(_access_token) => {
                    // The grant's own expiry governs the minted token; the
                    // connection records that its credential still works.
                    connection.record_refresh_success(connection.token_expires_at(), now);
                }
                Err(error) => {
                    let failure = classify_refresh_failure(&error);
                    let transition = refresh_failure_transition(
                        failure,
                        connection.disconnected_notified_at().is_some(),
                    );

                    let reason = match failure {
                        RefreshFailureKind::Retryable => "retryable",
                        RefreshFailureKind::Revoked | RefreshFailureKind::PermissionRevoked => {
                            "revoked"
                        }
                    };
                    self.metrics
                        .ads_token_refresh_failures()
                        .with_label_values(&[platform.as_str(), reason])
                        .inc();

                    let sync_error = SyncError {
                        code: reason.to_string(),
                        message: Some(error.to_string()),
                        context: serde_json::Map::new(),
                    };

                    if transition.status == ConnectionStatus::Disconnected {
                        connection.record_permanent_refresh_failure(sync_error);
                    } else {
                        connection.record_sync_error(sync_error);
                    }
                    if transition.pause_dependent_jobs {
                        connection.mark_dependent_jobs_paused(now);
                    }
                    if transition.notify_tenant {
                        connection.mark_disconnected_notified(now);
                    }

                    tracing::warn!(
                        connection_id = %connection.id(),
                        platform = platform.as_str(),
                        reason,
                        error = %error,
                        "advertising token refresh failed"
                    );
                }
            }

            match self.persist(tenant_id, &connection).await {
                Ok(()) if refreshed => report.refreshed += 1,
                Ok(()) => report.failed += 1,
                Err(error) => {
                    tracing::error!(
                        connection_id = %connection.id(),
                        error = %error,
                        "advertising token refresh outcome could not be persisted"
                    );
                    report.errors += 1;
                }
            }
        }

        Ok(report)
    }

    async fn persist(
        &self,
        tenant_id: sanvi_kernel::TenantId,
        connection: &crate::domain::AdvertisingConnection,
    ) -> Result<(), AdvertisingError> {
        let mut scope = self
            .tx
            .begin_for(tenant_id.as_i64())
            .await
            .map_err(|error| AdvertisingError::Storage(error.to_string()))?;
        self.connections.update(&mut scope, connection).await?;
        scope
            .commit()
            .await
            .map_err(|error| AdvertisingError::Storage(error.to_string()))
    }
}
