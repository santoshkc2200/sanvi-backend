//! Disputes: read-only projection of charge disputes on the tenant's
//! connected account. The tenant is merchant of record (direct charges),
//! so disputes land on their balance and Stripe owns the workflow —
//! Sanvi stores the state (status, amount, reason, response deadline)
//! and a dashboard deep link, but never proxies evidence.
//!
//! The table is `payments.disputes`; rows are written by Connect
//! webhooks (`charge.dispute.created` / `charge.dispute.closed`) and
//! read by the tenant's payments UI. No evidence document ever touches
//! Sanvi's storage — the read model is identifiers, statuses and
//! amounts only.

use chrono::{DateTime, Utc};
use sanvi_kernel::{Id, TenantId};

use super::error::PaymentsError;
use super::money::Money;
use super::payment::PaymentId;

pub enum DisputeTag {}
pub type DisputeId = Id<DisputeTag>;

/// The dispute lifecycle as Stripe reports it. Values are fixed by the
/// 09.5 schema CHECK on `payments.disputes`. Unknown provider values
/// degrade to `Other` rather than failing the webhook handler — the
/// column stays the raw string, this enum is a typed view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DisputeStatus {
    NeedsResponse,
    UnderReview,
    Won,
    Lost,
    WarningNeedsResponse,
    WarningUnderReview,
    WarningClosed,
    ChargeRefunded,
    /// A status the code does not yet know — stored verbatim.
    Other(String),
}

impl DisputeStatus {
    pub fn as_str(&self) -> &str {
        match self {
            Self::NeedsResponse => "needs_response",
            Self::UnderReview => "under_review",
            Self::Won => "won",
            Self::Lost => "lost",
            Self::WarningNeedsResponse => "warning_needs_response",
            Self::WarningUnderReview => "warning_under_review",
            Self::WarningClosed => "warning_closed",
            Self::ChargeRefunded => "charge_refunded",
            Self::Other(raw) => raw.as_str(),
        }
    }

    pub fn parse(raw: &str) -> Self {
        match raw {
            "needs_response" => Self::NeedsResponse,
            "under_review" => Self::UnderReview,
            "won" => Self::Won,
            "lost" => Self::Lost,
            "warning_needs_response" => Self::WarningNeedsResponse,
            "warning_under_review" => Self::WarningUnderReview,
            "warning_closed" => Self::WarningClosed,
            "charge_refunded" => Self::ChargeRefunded,
            other => Self::Other(other.to_string()),
        }
    }

    /// Whether the dispute is still open and needs tenant attention.
    pub fn is_open(&self) -> bool {
        matches!(
            self,
            Self::NeedsResponse
                | Self::WarningNeedsResponse
                | Self::UnderReview
                | Self::WarningUnderReview
        )
    }
}

/// One dispute against a payment on the tenant's connected account.
///
/// Stored fields only: identifiers, status, amount, reason, deadline —
/// no evidence.
#[derive(Debug, Clone, PartialEq)]
pub struct DisputeRecord {
    pub id: DisputeId,
    pub tenant_id: TenantId,
    pub payment_id: PaymentId,
    /// Provider's dispute id (`dp_…` for Stripe).
    pub external_dispute_id: String,
    pub status: DisputeStatus,
    pub amount: Money,
    pub reason: Option<String>,
    /// Evidence submission deadline, when the provider supplies one.
    pub due_by: Option<DateTime<Utc>>,
    /// The charge id the dispute is against (provider id).
    pub charge_id: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl DisputeRecord {
    /// Build a dispute record as webhooks report it — validates ids but
    /// not status (unknown statuses are kept as `Other` so the handler
    /// never drops an event because of a new provider value).
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: DisputeId,
        tenant_id: TenantId,
        payment_id: PaymentId,
        external_dispute_id: String,
        status: DisputeStatus,
        amount: Money,
        reason: Option<String>,
        due_by: Option<DateTime<Utc>>,
        now: DateTime<Utc>,
    ) -> Result<Self, PaymentsError> {
        if external_dispute_id.trim().is_empty() {
            return Err(PaymentsError::InvalidConnection(
                "external_dispute_id must not be empty".to_string(),
            ));
        }
        Ok(Self {
            id,
            tenant_id,
            payment_id,
            external_dispute_id,
            status,
            amount,
            reason,
            due_by,
            charge_id: None,
            created_at: now,
            updated_at: now,
        })
    }

    /// The Stripe Dashboard deep link for this dispute — the tenant's
    /// read-only handling path. Provider-agnostic shape: a URL on Stripe's
    /// dashboard for the tenant's connected account.
    pub fn dashboard_url(&self, external_account_id: &str) -> Option<String> {
        if external_account_id.starts_with("acct_") {
            Some(format!(
                "https://dashboard.stripe.com/{}/disputes/{}",
                external_account_id, self.external_dispute_id
            ))
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_statuses_round_trip_and_open_predicate_holds() {
        for status in [
            DisputeStatus::NeedsResponse,
            DisputeStatus::UnderReview,
            DisputeStatus::Won,
            DisputeStatus::Lost,
            DisputeStatus::WarningNeedsResponse,
            DisputeStatus::WarningUnderReview,
            DisputeStatus::WarningClosed,
            DisputeStatus::ChargeRefunded,
        ] {
            let raw = status.as_str().to_string();
            assert_eq!(DisputeStatus::parse(&raw).as_str(), raw);
        }
        assert!(DisputeStatus::NeedsResponse.is_open());
        assert!(DisputeStatus::UnderReview.is_open());
        assert!(!DisputeStatus::Won.is_open());
        assert!(!DisputeStatus::Lost.is_open());
        assert!(!DisputeStatus::ChargeRefunded.is_open());
        // Unknown status degrades, not fails.
        let other = DisputeStatus::parse("future_status");
        assert_eq!(other.as_str(), "future_status");
        assert!(!other.is_open());
    }

    #[test]
    fn dispute_requires_identity_and_amount() {
        let now = Utc::now();
        assert!(
            DisputeRecord::new(
                DisputeId::try_from(1_i64).unwrap(),
                TenantId::try_from(2_i64).unwrap(),
                PaymentId::try_from(3_i64).unwrap(),
                "  ".to_string(),
                DisputeStatus::NeedsResponse,
                Money::new(2000, "JPY").unwrap(),
                Some("fraudulent".to_string()),
                None,
                now,
            )
            .is_err()
        );
        // Zero amount is allowed for the dispute shape? No — disputes are
        // against a payment, but we still guard that the id exists, not
        // the amount sign (Money is non-negative by type).
        let record = DisputeRecord::new(
            DisputeId::try_from(1_i64).unwrap(),
            TenantId::try_from(2_i64).unwrap(),
            PaymentId::try_from(3_i64).unwrap(),
            "dp_test_123".to_string(),
            DisputeStatus::NeedsResponse,
            Money::new(2000, "JPY").unwrap(),
            Some("fraudulent".to_string()),
            Some(now),
            now,
        )
        .unwrap();
        assert_eq!(record.status, DisputeStatus::NeedsResponse);
        assert_eq!(record.amount.amount_minor, 2000);
    }
}
