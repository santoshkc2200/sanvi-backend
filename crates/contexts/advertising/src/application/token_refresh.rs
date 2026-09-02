//! Cross-tenant OAuth token refresh sweep (TASK-018).

use std::sync::Arc;

use chrono::Utc;
use sanvi_persistence::TxManager;

use crate::adapters::AdvertisingMetrics;
use crate::application::PlatformRegistry;
use crate::domain::{
    AdvertisingConnectionRepository, AdvertisingError, ConnectionStatus, RefreshFailureKind,
    SyncError, connection_lifecycle::refresh_failure_transition, model::ConnectionHandle,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TokenRefreshSweepReport {
    pub checked: u32,
    pub refreshed: u32,
    pub failed: u32,
    pub errors: u32,
}

pub struct RunTokenRefresh {
    tx: TxManager,
    connections: Arc<dyn AdvertisingConnectionRepository>,
    platforms: Arc<PlatformRegistry>,
    metrics: Arc<AdvertisingMetrics>,
    stale_after_secs: i64,
}

impl RunTokenRefresh {
    pub fn new(
        tx: TxManager,
        connections: Arc<dyn AdvertisingConnectionRepository>,
        platforms: Arc<PlatformRegistry>,
        metrics: Arc<AdvertisingMetrics>,
        stale_after_secs: i64,
    ) -> Self {
        Self {
            tx,
            connections,
            platforms,
            metrics,
            stale_after_secs,
        }
    }

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

        let now = Utc::now();

        for mut conn in due_connections {
            let tenant_id = conn.tenant_id();
            let platform = conn.platform().clone();
            let adapter = match self.platforms.adapter(&platform) {
                Ok(a) => a,
                Err(err) => {
                    tracing::warn!(
                        connection_id = %conn.id(),
                        platform = platform.as_str(),
                        error = %err,
                        "advertising token refresh skipped: adapter not found"
                    );
                    report.errors += 1;
                    continue;
                }
            };

            let handle = ConnectionHandle {
                id: conn.id().to_string(),
            };

            // Attempt refresh / credential verification through existing adapter list_accounts
            match adapter.list_accounts(&handle).await {
                Ok(_accounts) => {
                    conn.record_refresh_success(None, now);
                    let mut scope = match self.tx.begin_for(tenant_id.as_i64()).await {
                        Ok(s) => s,
                        Err(err) => {
                            tracing::error!(
                                connection_id = %conn.id(),
                                error = %err,
                                "failed to open tx for token refresh success update"
                            );
                            report.errors += 1;
                            continue;
                        }
                    };
                    if let Err(err) = self.connections.update(&mut scope, &conn).await {
                        tracing::error!(
                            connection_id = %conn.id(),
                            error = %err,
                            "failed to persist token refresh success"
                        );
                        report.errors += 1;
                        continue;
                    }
                    if let Err(err) = scope.commit().await {
                        tracing::error!(
                            connection_id = %conn.id(),
                            error = %err,
                            "failed to commit token refresh success"
                        );
                        report.errors += 1;
                        continue;
                    }
                    report.refreshed += 1;
                }
                Err(err) => {
                    let failure_kind = match &err {
                        AdvertisingError::PlatformUnavailable(_) => RefreshFailureKind::Retryable,
                        _ => RefreshFailureKind::Revoked,
                    };
                    let transition = refresh_failure_transition(
                        failure_kind,
                        conn.disconnected_notified_at().is_some(),
                    );

                    let sync_error = SyncError {
                        code: match failure_kind {
                            RefreshFailureKind::Retryable => "platform_unavailable".to_string(),
                            RefreshFailureKind::Revoked | RefreshFailureKind::PermissionRevoked => {
                                "token_revoked".to_string()
                            }
                        },
                        message: Some(err.to_string()),
                        context: serde_json::Map::new(),
                    };

                    if transition.status == ConnectionStatus::Disconnected {
                        conn.record_permanent_refresh_failure(sync_error);
                    } else {
                        conn.record_sync_error(sync_error);
                    }

                    if transition.pause_dependent_jobs {
                        conn.mark_dependent_jobs_paused(now);
                    }
                    if transition.notify_tenant {
                        conn.mark_disconnected_notified(now);
                    }

                    let mut scope = match self.tx.begin_for(tenant_id.as_i64()).await {
                        Ok(s) => s,
                        Err(tx_err) => {
                            tracing::error!(
                                connection_id = %conn.id(),
                                error = %tx_err,
                                "failed to open tx for token refresh failure update"
                            );
                            report.errors += 1;
                            continue;
                        }
                    };
                    if let Err(update_err) = self.connections.update(&mut scope, &conn).await {
                        tracing::error!(
                            connection_id = %conn.id(),
                            error = %update_err,
                            "failed to persist token refresh failure"
                        );
                        report.errors += 1;
                        continue;
                    }
                    if let Err(commit_err) = scope.commit().await {
                        tracing::error!(
                            connection_id = %conn.id(),
                            error = %commit_err,
                            "failed to commit token refresh failure"
                        );
                        report.errors += 1;
                        continue;
                    }

                    let reason_label = match failure_kind {
                        RefreshFailureKind::Retryable => "retryable",
                        _ => "revoked",
                    };
                    self.metrics
                        .ads_token_refresh_failures()
                        .with_label_values(&[platform.as_str(), reason_label])
                        .inc();

                    report.failed += 1;
                }
            }
        }

        Ok(report)
    }
}
