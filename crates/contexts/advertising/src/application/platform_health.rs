//! Cross-tenant advertising connection health and freshness overview for platform admins (TASK-018).

use std::collections::BTreeMap;
use std::sync::Arc;

use chrono::Utc;
use sanvi_kernel::TenantId;
use sanvi_persistence::TxManager;
use serde::Serialize;

use crate::application::PlatformRegistry;
use crate::domain::{
    AdvertisingConnectionRepository, AdvertisingError, ConnectionStatus, model::ConnectionId,
};

/// A platform-admin overview reads every tenant's connections in one request,
/// so it is bounded like any other cross-tenant read rather than growing with
/// the tenant table.
const CONNECTION_SCAN_LIMIT: i64 = 5_000;

#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct PlatformAdsHealthReport {
    pub platform_counts: Vec<PlatformConnectionCountView>,
    pub connections_needing_attention: Vec<ConnectionAttentionView>,
    pub freshness_summary: AdsFreshnessSummaryView,
}

#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct PlatformConnectionCountView {
    pub platform: String,
    pub active: u32,
    pub pending: u32,
    pub disconnected: u32,
    pub expired: u32,
    pub total: u32,
}

#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct ConnectionAttentionView {
    #[schema(value_type = String)]
    pub tenant_id: TenantId,
    #[schema(value_type = String)]
    pub connection_id: ConnectionId,
    pub platform: String,
    pub status: String,
    pub reconnect_required: bool,
    pub scopes_missing: Vec<String>,
    pub last_error: Option<String>,
    pub stalled: bool,
}

#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct AdsFreshnessSummaryView {
    pub total_connections: u32,
    pub stalled_connections: u32,
    pub worst_lag_hours: Option<f64>,
}

pub struct GetPlatformAdsHealth {
    tx: TxManager,
    connections: Arc<dyn AdvertisingConnectionRepository>,
    platforms: Arc<PlatformRegistry>,
    stalled_after_hours: f64,
}

impl GetPlatformAdsHealth {
    pub fn new(
        tx: TxManager,
        connections: Arc<dyn AdvertisingConnectionRepository>,
        platforms: Arc<PlatformRegistry>,
        stalled_after_hours: f64,
    ) -> Self {
        Self {
            tx,
            connections,
            platforms,
            stalled_after_hours,
        }
    }

    pub async fn execute(&self) -> Result<PlatformAdsHealthReport, AdvertisingError> {
        let mut scope = self
            .tx
            .begin_unscoped("advertising_platform_health", "platform_admin")
            .await
            .map_err(|error| AdvertisingError::Storage(error.to_string()))?;
        let connections = self
            .connections
            .list_all_for_platform_scope(&mut scope, CONNECTION_SCAN_LIMIT)
            .await?;
        scope
            .commit()
            .await
            .map_err(|error| AdvertisingError::Storage(error.to_string()))?;

        let now = Utc::now();

        // 1. Status counts per platform
        let mut counts_by_platform: BTreeMap<String, (u32, u32, u32, u32, u32)> = BTreeMap::new();
        // Seed registered platforms with 0 counts
        for adapter in self.platforms.adapters() {
            counts_by_platform.insert(
                adapter.descriptor().key.as_str().to_string(),
                (0, 0, 0, 0, 0),
            );
        }

        let mut connections_needing_attention = Vec::new();
        let mut stalled_count = 0u32;
        let mut worst_lag: Option<f64> = None;

        for conn in &connections {
            let platform_str = conn.platform().as_str().to_string();
            let entry = counts_by_platform
                .entry(platform_str.clone())
                .or_insert((0, 0, 0, 0, 0));
            entry.4 += 1; // total
            match conn.status() {
                ConnectionStatus::Active => entry.0 += 1,
                ConnectionStatus::Pending => entry.1 += 1,
                ConnectionStatus::Disconnected => entry.2 += 1,
                ConnectionStatus::Expired => entry.3 += 1,
            }

            // Derive health using existing logic with adapter required_scopes
            let required_scopes = self
                .platforms
                .adapter(conn.platform())
                .map(|adapter| adapter.required_scopes())
                .unwrap_or_default();
            let health = conn.health(&required_scopes, now);

            // Derive freshness signal
            let last_ingested_at = conn.last_metrics_ingested_at();
            let lag_hours =
                last_ingested_at.map(|synced_at| (now - synced_at).num_seconds() as f64 / 3600.0);
            let stalled = conn.status() == ConnectionStatus::Active
                && lag_hours.is_none_or(|hours| hours > self.stalled_after_hours);

            if stalled {
                stalled_count += 1;
            }
            if let Some(lag) = lag_hours {
                worst_lag = Some(match worst_lag {
                    Some(prev) => prev.max(lag),
                    None => lag,
                });
            }

            let needs_attention = health.reconnect_required
                || !health.scopes_missing.is_empty()
                || conn.last_error().is_some()
                || conn.status() != ConnectionStatus::Active
                || stalled;

            if needs_attention {
                connections_needing_attention.push(ConnectionAttentionView {
                    tenant_id: conn.tenant_id(),
                    connection_id: conn.id(),
                    platform: platform_str,
                    status: conn.status().as_str().to_string(),
                    reconnect_required: health.reconnect_required,
                    scopes_missing: health.scopes_missing,
                    last_error: conn.last_error().and_then(|e| e.message.clone()),
                    stalled,
                });
            }
        }

        let platform_counts = counts_by_platform
            .into_iter()
            .map(
                |(platform, (active, pending, disconnected, expired, total))| {
                    PlatformConnectionCountView {
                        platform,
                        active,
                        pending,
                        disconnected,
                        expired,
                        total,
                    }
                },
            )
            .collect();

        let freshness_summary = AdsFreshnessSummaryView {
            total_connections: connections.len() as u32,
            stalled_connections: stalled_count,
            worst_lag_hours: worst_lag,
        };

        Ok(PlatformAdsHealthReport {
            platform_counts,
            connections_needing_attention,
            freshness_summary,
        })
    }
}
