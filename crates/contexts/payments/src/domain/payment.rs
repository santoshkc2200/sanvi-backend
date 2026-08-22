//! The payment and refund read models.
//!
//! Truth about money arrives by webhook (09.4/09.5); these records are
//! the projection of that truth, never its source. Amounts are minor
//! units + ISO-4217; fees are recorded because the provider reports
//! them, not because Sanvi collects them (direct charges — the connected
//! account pays its own processor fees).
//!
//! Persistence lands in 09.5; the aggregates and invariants exist now so
//! the ports (this slice) already speak them.

use chrono::{DateTime, Utc};
use uuid::Uuid;

use super::error::PaymentsError;
use super::money::Money;

/// The payment lifecycle as webhooks report it. Values are fixed by the
/// 09.0 schema CHECK on `payments.payments`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaymentStatus {
    RequiresAction,
    Processing,
    Succeeded,
    Failed,
    Canceled,
}

impl PaymentStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RequiresAction => "requires_action",
            Self::Processing => "processing",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Canceled => "canceled",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        Some(match raw {
            "requires_action" => Self::RequiresAction,
            "processing" => Self::Processing,
            "succeeded" => Self::Succeeded,
            "failed" => Self::Failed,
            "canceled" => Self::Canceled,
            _ => return None,
        })
    }
}

/// Why a payment failed, as the provider reported it — a machine code
/// (`card_declined`) plus a human message. No card data, ever.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaymentFailure {
    pub code: String,
    pub message: String,
}

/// One payment on the tenant's connected account.
#[derive(Debug, Clone, PartialEq)]
pub struct PaymentRecord {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub checkout_id: Option<Uuid>,
    /// The provider's payment id (`pi_…` / `pay_…`), unique per tenant.
    pub external_payment_id: String,
    pub status: PaymentStatus,
    pub amount: Money,
    /// Processor fee as the provider reported it (same currency).
    pub fee: Option<Money>,
    pub failure: Option<PaymentFailure>,
    pub captured_at: Option<DateTime<Utc>>,
    pub created_at: Option<DateTime<Utc>>,
}

impl PaymentRecord {
    pub fn new(
        id: Uuid,
        tenant_id: Uuid,
        checkout_id: Option<Uuid>,
        external_payment_id: String,
        amount: Money,
    ) -> Result<Self, PaymentsError> {
        if external_payment_id.trim().is_empty() {
            return Err(PaymentsError::InvalidConnection(
                "external_payment_id must not be empty".to_string(),
            ));
        }
        if amount.is_zero() {
            return Err(PaymentsError::InvalidMoney(format!(
                "payment amount must be positive, got 0 {}",
                amount.currency.as_str()
            )));
        }
        Ok(Self {
            id,
            tenant_id,
            checkout_id,
            external_payment_id,
            status: PaymentStatus::Processing,
            amount,
            fee: None,
            failure: None,
            captured_at: None,
            created_at: None,
        })
    }

    /// The amount still refundable: total minus nothing yet (refund
    /// accounting lands with the 09.5 read model); the guard that matters
    /// lives on [`RefundRecord::against`].
    pub fn refundable(&self) -> Money {
        self.amount.clone()
    }
}

/// The refund lifecycle. Values are fixed by the 09.0 schema CHECK on
/// `payments.refunds`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefundStatus {
    Pending,
    Succeeded,
    Failed,
}

impl RefundStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        Some(match raw {
            "pending" => Self::Pending,
            "succeeded" => Self::Succeeded,
            "failed" => Self::Failed,
            _ => return None,
        })
    }
}

/// One refund against a payment. Partial refunds are the normal case
/// (¥2,000 order, ¥500 refund); the domain guard is `Money::portion_of`:
/// currency-checked, minor-unit exact, JPY-correct by construction.
#[derive(Debug, Clone, PartialEq)]
pub struct RefundRecord {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub payment_id: Uuid,
    pub external_refund_id: Option<String>,
    pub amount: Money,
    pub reason: Option<String>,
    pub status: RefundStatus,
    pub created_by: Option<Uuid>,
    pub created_at: DateTime<Utc>,
    pub idempotency_key: Option<Uuid>,
}

impl RefundRecord {
    /// Build a refund against a payment, enforcing portion ≤ total at the
    /// type boundary — before any provider call.
    pub fn against(
        id: Uuid,
        payment: &PaymentRecord,
        portion_minor: i64,
        reason: Option<String>,
        created_by: Option<Uuid>,
        now: DateTime<Utc>,
    ) -> Result<Self, PaymentsError> {
        let amount = payment.amount.portion_of(portion_minor)?;
        if amount.is_zero() {
            return Err(PaymentsError::InvalidMoney(
                "refund amount must be positive".to_string(),
            ));
        }
        Ok(Self {
            id,
            tenant_id: payment.tenant_id,
            payment_id: payment.id,
            external_refund_id: None,
            amount,
            reason,
            status: RefundStatus::Pending,
            created_by,
            created_at: now,
            idempotency_key: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payment(amount_minor: i64, currency: &str) -> PaymentRecord {
        PaymentRecord::new(
            Uuid::now_v7(),
            Uuid::now_v7(),
            Some(Uuid::now_v7()),
            format!("pi_{}", Uuid::now_v7().simple()),
            Money::new(amount_minor, currency).unwrap(),
        )
        .unwrap()
    }

    #[test]
    fn payment_statuses_round_trip_the_schema_check() {
        for status in [
            PaymentStatus::RequiresAction,
            PaymentStatus::Processing,
            PaymentStatus::Succeeded,
            PaymentStatus::Failed,
            PaymentStatus::Canceled,
        ] {
            assert_eq!(PaymentStatus::parse(status.as_str()), Some(status));
        }
        assert_eq!(PaymentStatus::parse("disputed"), None);

        for status in [
            RefundStatus::Pending,
            RefundStatus::Succeeded,
            RefundStatus::Failed,
        ] {
            assert_eq!(RefundStatus::parse(status.as_str()), Some(status));
        }
        assert_eq!(RefundStatus::parse("canceled"), None);
    }

    #[test]
    fn payments_require_identity_and_a_positive_amount() {
        assert!(
            PaymentRecord::new(
                Uuid::now_v7(),
                Uuid::now_v7(),
                None,
                "  ".to_string(),
                Money::new(100, "USD").unwrap(),
            )
            .is_err()
        );
        assert!(
            PaymentRecord::new(
                Uuid::now_v7(),
                Uuid::now_v7(),
                None,
                "pi_x".to_string(),
                Money::new(0, "USD").unwrap(),
            )
            .is_err()
        );
    }

    #[test]
    fn partial_refunds_are_portion_guarded_in_jpy_and_usd() {
        let usd = payment(1999, "USD");
        let refund = RefundRecord::against(
            Uuid::now_v7(),
            &usd,
            999,
            Some("duplicate".to_string()),
            None,
            Utc::now(),
        )
        .unwrap();
        assert_eq!(refund.amount.amount_minor, 999);
        assert_eq!(refund.status, RefundStatus::Pending);

        // Zero and over-total portions are refused.
        assert!(RefundRecord::against(Uuid::now_v7(), &usd, 0, None, None, Utc::now()).is_err());
        assert!(RefundRecord::against(Uuid::now_v7(), &usd, 2000, None, None, Utc::now()).is_err());

        // JPY: ¥2,000 payment, ¥500 refund — minor units, no ÷100.
        let jpy = payment(2000, "JPY");
        let refund =
            RefundRecord::against(Uuid::now_v7(), &jpy, 500, None, None, Utc::now()).unwrap();
        assert_eq!(refund.amount.amount_minor, 500);
        assert_eq!(refund.amount.currency.as_str(), "JPY");
        assert!(RefundRecord::against(Uuid::now_v7(), &jpy, 2001, None, None, Utc::now()).is_err());
    }
}
