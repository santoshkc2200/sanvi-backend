//! The checkout aggregate: one provider checkout session per merchant
//! order attempt. Direct charges on the connected account — the amount
//! belongs to the tenant, and Sanvi's fee (if ever configured) rides the
//! payment intent, never the session total.
//!
//! Persisted since 09.0 (the schema landed first); 09.4 drives the
//! lifecycle: the create use case opens the session, and the payment
//! webhooks — never the browser redirect — close it.

use chrono::{DateTime, Utc};
use sanvi_kernel::{Id, TenantId};
use uuid::Uuid;

use super::connection::ConnectionId;
use super::error::PaymentsError;
use super::money::Money;

pub enum CheckoutTag {}
pub type CheckoutId = Id<CheckoutTag>;

/// The tenant's own order reference, carried through to the provider and
/// echoed on every read model — the join key between the tenant's order
/// book and our payments. Opaque to us beyond validation: non-empty,
/// bounded (providers reject long client reference ids at the HTTP layer,
/// and unbounded strings are a log-injection vector).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MerchantOrderRef(String);

impl MerchantOrderRef {
    const MAX_LEN: usize = 200;

    pub fn parse(raw: &str) -> Result<Self, PaymentsError> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(PaymentsError::InvalidConnection(
                "merchant order reference must not be empty".to_string(),
            ));
        }
        if trimmed.len() > Self::MAX_LEN {
            return Err(PaymentsError::InvalidConnection(format!(
                "merchant order reference exceeds {} characters",
                Self::MAX_LEN
            )));
        }
        Ok(Self(trimmed.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The checkout lifecycle. Values are fixed by the 09.0 schema CHECK on
/// `payments.checkouts` — this enum and that constraint must never
/// disagree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckoutStatus {
    /// Created, session not yet confirmed by the provider.
    Pending,
    /// Provider session live; the customer may pay.
    Open,
    /// Payment succeeded (the webhook said so — never the redirect).
    Complete,
    /// No payment happened before the deadline.
    Expired,
    /// The customer abandoned at the provider.
    Canceled,
}

impl CheckoutStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Open => "open",
            Self::Complete => "complete",
            Self::Expired => "expired",
            Self::Canceled => "canceled",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        Some(match raw {
            "pending" => Self::Pending,
            "open" => Self::Open,
            "complete" => Self::Complete,
            "expired" => Self::Expired,
            "canceled" => Self::Canceled,
            _ => return None,
        })
    }

    /// Still awaiting money: session not yet created, or created and
    /// payable. Webhook handlers may drive any transition *into* a
    /// terminal state from here; nothing leaves terminal states.
    pub const fn is_live(self) -> bool {
        matches!(self, Self::Pending | Self::Open)
    }
}

/// A checkout for one merchant order on the tenant's connected account.
#[derive(Debug, Clone, PartialEq)]
pub struct Checkout {
    pub id: CheckoutId,
    pub tenant_id: TenantId,
    pub connection_id: ConnectionId,
    /// The tenant's own order reference (opaque to Sanvi).
    pub reference: MerchantOrderRef,
    /// What the order is for, as the tenant's bookkeeping states it
    /// (free-form JSON, provider-passthrough; no card data ever).
    pub customer_ref: Option<serde_json::Value>,
    pub amount: Money,
    pub status: CheckoutStatus,
    /// The provider's session id (`cs_…` for Stripe), once created.
    pub external_session_id: Option<String>,
    /// The caller's `Idempotency-Key` — the double-clicked buy button's
    /// owner. Unique per tenant at the schema; replay returns this row.
    pub idempotency_key: Option<Uuid>,
    /// Presentation locale for the provider-hosted page (phase 06).
    pub locale: Option<String>,
    pub expires_at: Option<DateTime<Utc>>,
    /// When a webhook first reported success (never set by the redirect).
    pub completed_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

impl Checkout {
    /// A checkout awaiting its provider session. The amount must be
    /// positive: a zero-money checkout is a programming error, and the
    /// provider would reject it after the round-trip we can catch here.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: CheckoutId,
        tenant_id: TenantId,
        connection_id: ConnectionId,
        reference: MerchantOrderRef,
        amount: Money,
        now: DateTime<Utc>,
    ) -> Result<Self, PaymentsError> {
        if amount.is_zero() {
            return Err(PaymentsError::InvalidMoney(format!(
                "checkout amount must be positive, got 0 {}",
                amount.currency.as_str()
            )));
        }
        Ok(Self {
            id,
            tenant_id,
            connection_id,
            reference,
            customer_ref: None,
            amount,
            status: CheckoutStatus::Pending,
            external_session_id: None,
            idempotency_key: None,
            locale: None,
            expires_at: None,
            completed_at: None,
            created_at: now,
        })
    }

    /// Record the provider session: the checkout goes `Open` and learns
    /// its provider id and expiry. Idempotent in shape only — callers
    /// route replays through the idempotency key, not through re-calling
    /// this with a different session.
    pub fn session_created(
        &mut self,
        external_session_id: String,
        expires_at: Option<DateTime<Utc>>,
    ) -> Result<(), PaymentsError> {
        if self.status != CheckoutStatus::Pending {
            return Err(PaymentsError::InvalidConnection(format!(
                "checkout {} cannot open from status {}",
                self.id,
                self.status.as_str()
            )));
        }
        self.external_session_id = Some(external_session_id);
        self.expires_at = expires_at;
        self.status = CheckoutStatus::Open;
        Ok(())
    }

    /// A payment succeeded (webhook truth only). Terminal: `Open` and
    /// `Pending` may complete — a lost `session.created` path must not
    /// strand a paid order in limbo — but nothing leaves a terminal
    /// state, and completing twice never moves `completed_at`.
    pub fn complete(&mut self, now: DateTime<Utc>) -> Result<(), PaymentsError> {
        if matches!(
            self.status,
            CheckoutStatus::Expired | CheckoutStatus::Canceled
        ) {
            return Err(self.illegal("complete"));
        }
        if self.status == CheckoutStatus::Complete {
            return Ok(()); // idempotent redelivery
        }
        self.completed_at.get_or_insert(now);
        self.status = CheckoutStatus::Complete;
        Ok(())
    }

    /// No payment happened before the deadline (provider event or our
    /// own expiry sweep). Idempotent like [`Checkout::complete`].
    pub fn expire(&mut self) -> Result<(), PaymentsError> {
        match self.status {
            CheckoutStatus::Pending | CheckoutStatus::Open => {
                self.status = CheckoutStatus::Expired;
                Ok(())
            }
            CheckoutStatus::Expired => Ok(()),
            _ => Err(self.illegal("expire")),
        }
    }

    /// The customer abandoned at the provider (`payment_intent.canceled`
    /// / an explicit cancel at the hosted page).
    pub fn cancel(&mut self) -> Result<(), PaymentsError> {
        match self.status {
            CheckoutStatus::Pending | CheckoutStatus::Open => {
                self.status = CheckoutStatus::Canceled;
                Ok(())
            }
            CheckoutStatus::Canceled => Ok(()),
            _ => Err(self.illegal("cancel")),
        }
    }

    fn illegal(&self, action: &str) -> PaymentsError {
        PaymentsError::InvalidConnection(format!(
            "checkout {} cannot {action} from status {}",
            self.id,
            self.status.as_str()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn order_references_are_validated_and_bounded() {
        assert_eq!(
            MerchantOrderRef::parse(" order-123 ").unwrap().as_str(),
            "order-123"
        );
        assert!(MerchantOrderRef::parse("   ").is_err());
        assert!(MerchantOrderRef::parse(&"x".repeat(201)).is_err());
        assert!(MerchantOrderRef::parse(&"x".repeat(200)).is_ok());
    }

    #[test]
    fn checkout_statuses_round_trip_the_schema_check() {
        for status in [
            CheckoutStatus::Pending,
            CheckoutStatus::Open,
            CheckoutStatus::Complete,
            CheckoutStatus::Expired,
            CheckoutStatus::Canceled,
        ] {
            assert_eq!(CheckoutStatus::parse(status.as_str()), Some(status));
        }
        assert_eq!(CheckoutStatus::parse("failed"), None);
    }

    #[test]
    fn zero_amount_checkouts_are_refused_and_sessions_open_once() {
        let mut checkout = Checkout::new(
            CheckoutId::try_from(1_i64).unwrap(),
            TenantId::try_from(2_i64).unwrap(),
            ConnectionId::try_from(3_i64).unwrap(),
            MerchantOrderRef::parse("order-1").unwrap(),
            Money::new(2000, "JPY").unwrap(),
            Utc::now(),
        )
        .unwrap();
        assert_eq!(checkout.status, CheckoutStatus::Pending);

        assert!(
            Checkout::new(
                CheckoutId::try_from(4_i64).unwrap(),
                TenantId::try_from(5_i64).unwrap(),
                ConnectionId::try_from(6_i64).unwrap(),
                MerchantOrderRef::parse("order-2").unwrap(),
                Money::new(0, "JPY").unwrap(),
                Utc::now(),
            )
            .is_err()
        );

        checkout
            .session_created("cs_fake_1".to_string(), None)
            .unwrap();
        assert_eq!(checkout.status, CheckoutStatus::Open);
        assert_eq!(checkout.external_session_id.as_deref(), Some("cs_fake_1"));
        // Opening twice is an error — the webhook drives what happens next.
        assert!(
            checkout
                .session_created("cs_fake_2".to_string(), None)
                .is_err()
        );
        assert_eq!(checkout.external_session_id.as_deref(), Some("cs_fake_1"));
    }

    #[test]
    fn webhook_transitions_close_the_checkout_forward_only() {
        let paid_at = Utc::now();

        // Open → Complete, stamped once; a redelivered completion is a no-op.
        let mut paid = open_checkout();
        paid.complete(paid_at).unwrap();
        assert_eq!(paid.status, CheckoutStatus::Complete);
        assert_eq!(paid.completed_at, Some(paid_at));
        paid.complete(paid_at).unwrap();
        assert_eq!(paid.completed_at, Some(paid_at));
        // Terminal states never reopen or re-cancel.
        assert!(paid.expire().is_err());
        assert!(paid.cancel().is_err());

        // A paid order is never stranded by a lost session-open event:
        // Pending → Complete is legal too.
        let mut unopened = new_checkout();
        unopened.complete(paid_at).unwrap();
        assert_eq!(unopened.status, CheckoutStatus::Complete);

        // Expired and canceled are terminal from Pending/Open.
        for closer in [CheckoutCloser::Expire, CheckoutCloser::Cancel] {
            let mut fresh = open_checkout();
            match closer {
                CheckoutCloser::Expire => fresh.expire().unwrap(),
                CheckoutCloser::Cancel => fresh.cancel().unwrap(),
            }
            assert!(fresh.complete(paid_at).is_err());
            // Idempotent redelivery of the same closing event.
            match closer {
                CheckoutCloser::Expire => fresh.expire().unwrap(),
                CheckoutCloser::Cancel => fresh.cancel().unwrap(),
            }
        }
    }

    enum CheckoutCloser {
        Expire,
        Cancel,
    }

    fn new_checkout() -> Checkout {
        Checkout::new(
            CheckoutId::try_from(10_i64).unwrap(),
            TenantId::try_from(20_i64).unwrap(),
            ConnectionId::try_from(30_i64).unwrap(),
            MerchantOrderRef::parse("order-x").unwrap(),
            Money::new(1500, "JPY").unwrap(),
            Utc::now(),
        )
        .unwrap()
    }

    fn open_checkout() -> Checkout {
        let mut checkout = new_checkout();
        checkout
            .session_created("cs_fake_x".to_string(), None)
            .unwrap();
        checkout
    }
}
