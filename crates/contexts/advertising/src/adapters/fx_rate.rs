//! FX rate lookup for display-only currency conversion (TASK-016, slice 10.7).
//!
//! No live FX data source is wired up yet — real-time/daily rate ingestion
//! from a provider is out of scope for this slice, same "blocked" shape as
//! every other external-data gap in this phase. `rendered_*` fields are
//! `Option` for exactly this reason: an unavailable rate is a normal
//! outcome, not an error, and every caller already treats it as such.

use async_trait::async_trait;
use chrono::NaiveDate;

use crate::domain::{AdvertisingError, FxRate, FxRatePort};

/// Always reports no rate available. Same-currency pairs never need one —
/// the query layer only calls this port when `base != quote`.
#[derive(Debug, Clone, Copy, Default)]
pub struct UnavailableFxRate;

impl UnavailableFxRate {
    pub const fn new() -> Self {
        Self
    }
}

#[async_trait]
impl FxRatePort for UnavailableFxRate {
    async fn rate(
        &self,
        _base: &str,
        _quote: &str,
        _date: NaiveDate,
    ) -> Result<Option<FxRate>, AdvertisingError> {
        Ok(None)
    }
}
