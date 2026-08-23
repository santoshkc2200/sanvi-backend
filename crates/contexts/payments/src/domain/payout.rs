//! The payout projection (09.6).
//!
//! Payouts are the provider's money movement — Sanvi never schedules,
//! delays, or retries one. This record is the webhook-driven truth of
//! what the provider reported (`payout.paid` / `payout.failed`), kept so
//! a failed payout is an alert with a row behind it rather than a
//! transient API call, and so the payments list can show payout status.

use chrono::{DateTime, Utc};
use uuid::Uuid;

use super::money::Money;
use super::ports::PayoutStatus;

/// One payout from the tenant's connected account to their bank.
#[derive(Debug, Clone, PartialEq)]
pub struct PayoutRecord {
    pub id: Uuid,
    pub tenant_id: Uuid,
    /// The connection that owned the account when the event arrived
    /// (`SET NULL` on delete — history survives a disconnect).
    pub connection_id: Option<Uuid>,
    pub external_payout_id: String,
    pub amount: Money,
    pub status: PayoutStatus,
    pub arrival_at: Option<DateTime<Utc>>,
    pub created_at: Option<DateTime<Utc>>,
    pub updated_at: Option<DateTime<Utc>>,
}

impl PayoutRecord {
    pub fn new(
        id: Uuid,
        tenant_id: Uuid,
        connection_id: Option<Uuid>,
        external_payout_id: String,
        amount: Money,
        status: PayoutStatus,
    ) -> Result<Self, super::error::PaymentsError> {
        if external_payout_id.trim().is_empty() || !external_payout_id.starts_with("po_") {
            return Err(super::error::PaymentsError::InvalidConnection(
                "external_payout_id must be a provider payout id".to_string(),
            ));
        }
        Ok(Self {
            id,
            tenant_id,
            connection_id,
            external_payout_id,
            amount,
            status,
            arrival_at: None,
            created_at: None,
            updated_at: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payout_records_require_a_provider_payout_id() {
        assert!(
            PayoutRecord::new(
                Uuid::now_v7(),
                Uuid::now_v7(),
                None,
                "po_test_1".to_string(),
                Money::new(1500, "JPY").unwrap(),
                PayoutStatus::Paid,
            )
            .is_ok()
        );
        assert!(
            PayoutRecord::new(
                Uuid::now_v7(),
                Uuid::now_v7(),
                None,
                "pi_not_a_payout".to_string(),
                Money::new(1500, "JPY").unwrap(),
                PayoutStatus::Paid,
            )
            .is_err()
        );
    }
}
