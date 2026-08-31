//! ROAS query layer (TASK-016, slice 10.7): bounded date-range reads over
//! the metrics rollup, backing `/ads/metrics`, `/ads/metrics/summary` and
//! `/ads/metrics/export`.

use std::sync::Arc;

use chrono::NaiveDate;
use sanvi_persistence::TxManager;

use crate::domain::{AdvertisingError, MetricsAggregateRow, MetricsRangeQuery, MetricsRepository};

pub struct QueryMetrics {
    tx: TxManager,
    metrics_repo: Arc<dyn MetricsRepository>,
    max_span_days: i64,
    restatement_days: i64,
}

impl QueryMetrics {
    pub fn new(
        tx: TxManager,
        metrics_repo: Arc<dyn MetricsRepository>,
        max_span_days: i64,
        restatement_days: i64,
    ) -> Self {
        Self {
            tx,
            metrics_repo,
            max_span_days,
            restatement_days,
        }
    }

    /// Bounded, tenant-scoped read. Rejects an over-long span outright
    /// rather than letting a raw-partition scan run long and time out.
    pub async fn execute(
        &self,
        query: MetricsRangeQuery,
        today: NaiveDate,
    ) -> Result<Vec<MetricsAggregateRow>, AdvertisingError> {
        validate_span(query.from, query.to, self.max_span_days)?;
        let mut scope = self
            .tx
            .begin_for(query.tenant_id.as_i64())
            .await
            .map_err(|error| AdvertisingError::Storage(error.to_string()))?;
        let rows = self
            .metrics_repo
            .query_range(&mut scope, &query, today, self.restatement_days)
            .await?;
        scope
            .commit()
            .await
            .map_err(|error| AdvertisingError::Storage(error.to_string()))?;
        Ok(rows)
    }

    pub fn max_span_days(&self) -> i64 {
        self.max_span_days
    }
}

fn validate_span(
    from: NaiveDate,
    to: NaiveDate,
    max_span_days: i64,
) -> Result<(), AdvertisingError> {
    if to < from {
        return Err(AdvertisingError::InvalidInput(
            "metrics query 'to' must not precede 'from'".to_string(),
        ));
    }
    let span_days = (to - from).num_days() + 1;
    if span_days > max_span_days {
        return Err(AdvertisingError::InvalidInput(format!(
            "metrics query spans {span_days} days, exceeding the maximum of {max_span_days}"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_inverted_range() {
        let from = NaiveDate::from_ymd_opt(2026, 8, 10).unwrap();
        let to = NaiveDate::from_ymd_opt(2026, 8, 1).unwrap();
        assert!(matches!(
            validate_span(from, to, 90),
            Err(AdvertisingError::InvalidInput(_))
        ));
    }

    #[test]
    fn rejects_over_long_span() {
        let from = NaiveDate::from_ymd_opt(2026, 1, 1).unwrap();
        let to = NaiveDate::from_ymd_opt(2026, 12, 31).unwrap();
        assert!(matches!(
            validate_span(from, to, 90),
            Err(AdvertisingError::InvalidInput(_))
        ));
    }

    #[test]
    fn accepts_span_at_the_max() {
        let from = NaiveDate::from_ymd_opt(2026, 1, 1).unwrap();
        let to = NaiveDate::from_ymd_opt(2026, 3, 31).unwrap();
        let span = (to - from).num_days() + 1;
        assert!(validate_span(from, to, span).is_ok());
    }
}
