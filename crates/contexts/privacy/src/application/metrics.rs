//! Annual request metrics (CCPA disclosure): generated from the request
//! ledger by a yearly job, never hand-counted; the public endpoint reads the
//! generated rows.

use std::sync::Arc;

use serde::Serialize;
use utoipa::ToSchema;

use sanvi_persistence::TxManager;

use crate::domain::{Clock, DsrRepository, PrivacyError, RequestMetricsRepository};

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct PublicMetricsRow {
    pub year: i32,
    pub jurisdiction: String,
    pub kind: String,
    pub received: i32,
    pub complied: i32,
    pub denied: i32,
    pub median_days: Option<f64>,
}

pub struct PublicMetrics {
    tx_manager: TxManager,
    metrics: Arc<dyn RequestMetricsRepository>,
}

impl PublicMetrics {
    pub fn new(tx_manager: TxManager, metrics: Arc<dyn RequestMetricsRepository>) -> Self {
        Self {
            tx_manager,
            metrics,
        }
    }

    #[tracing::instrument(name = "privacy.public_metrics", skip_all)]
    pub async fn execute(&self, year: Option<i32>) -> Result<Vec<PublicMetricsRow>, PrivacyError> {
        let year =
            year.unwrap_or_else(|| chrono::Datelike::year(&chrono::Utc::now().date_naive()) - 1);
        let mut scope = self
            .tx_manager
            .begin_unscoped("privacy_metrics_public", "privacy")
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;
        let rows = self.metrics.list_yearly(&mut scope, year).await?;
        scope
            .commit()
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;
        Ok(rows
            .into_iter()
            .map(|row| PublicMetricsRow {
                year: row.year,
                jurisdiction: row.jurisdiction,
                kind: row.kind,
                received: row.received,
                complied: row.complied,
                denied: row.denied,
                median_days: row.median_days,
            })
            .collect())
    }
}

pub struct GenerateAnnualMetrics {
    tx_manager: TxManager,
    requests: Arc<dyn DsrRepository>,
    metrics: Arc<dyn RequestMetricsRepository>,
    clock: Arc<dyn Clock>,
}

impl GenerateAnnualMetrics {
    pub fn new(
        tx_manager: TxManager,
        requests: Arc<dyn DsrRepository>,
        metrics: Arc<dyn RequestMetricsRepository>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            tx_manager,
            requests,
            metrics,
            clock,
        }
    }

    /// Regenerate the completed calendar year (or the one requested).
    #[tracing::instrument(name = "privacy.generate_annual_metrics", skip_all)]
    pub async fn execute(&self, year: Option<i32>) -> Result<usize, PrivacyError> {
        let year =
            year.unwrap_or_else(|| chrono::Datelike::year(&self.clock.now().date_naive()) - 1);
        let mut scope = self
            .tx_manager
            .begin_unscoped("privacy_metrics_generate", "privacy")
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;
        let rows = self.requests.metrics_by_kind(&mut scope, year).await?;
        let count = rows.len();
        self.metrics.upsert_yearly(&mut scope, year, &rows).await?;
        scope
            .commit()
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;
        Ok(count)
    }
}
