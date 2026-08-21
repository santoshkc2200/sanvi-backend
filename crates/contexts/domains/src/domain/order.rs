//! The domain order aggregate: the purchase journey's durable record.
//!
//! An order survives the saga it drives: charge → register → configure →
//! active, with `failed` / `refunded` terminals. Registration is idempotent
//! on the order id (the registrar refuses double-registration), so a crash
//! at any step resumes instead of duplicating.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::domain::{DomainsError, Hostname, Money};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum OrderStatus {
    /// Created and quoted; nothing charged yet.
    Pending,
    /// The tenant was charged; registration not yet confirmed.
    Charged,
    /// The registrar accepted the registration.
    Registered,
    /// DNS zone/records still being applied (worker retries).
    Configuring,
    /// DNS applied; the domain claim exists and flows through verification.
    Active,
    /// The charge failed or the order was cancelled before registration.
    Failed,
    /// Charged and then refunded (registration failed after the charge).
    Refunded,
}

impl OrderStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Charged => "charged",
            Self::Registered => "registered",
            Self::Configuring => "configuring",
            Self::Active => "active",
            Self::Failed => "failed",
            Self::Refunded => "refunded",
        }
    }

    pub fn parse(raw: &str) -> Result<Self, DomainsError> {
        match raw {
            "pending" => Ok(Self::Pending),
            "charged" => Ok(Self::Charged),
            "registered" => Ok(Self::Registered),
            "configuring" => Ok(Self::Configuring),
            "active" => Ok(Self::Active),
            "failed" => Ok(Self::Failed),
            "refunded" => Ok(Self::Refunded),
            other => Err(DomainsError::OrderNotModifiable(format!(
                "invalid order status: {other}"
            ))),
        }
    }
}

/// The registrant contact data (purchase journey). Personal data: minimized,
/// registered in the phase-05 data map with the registrar as sub-processor,
/// and erasable subject to ICANN retention rules.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct RegistrantContact {
    pub name: String,
    pub email: String,
    pub phone: String,
    pub organization: Option<String>,
    pub country_code: String,
}

/// The domain order aggregate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct DomainOrder {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub hostname: Hostname,
    pub term_years: u8,
    pub price: Money,
    pub status: OrderStatus,
    pub auto_renew: bool,
    pub whois_privacy: bool,
    pub registrant_contact: RegistrantContact,
    pub ordered_by: Option<Uuid>,
    pub idempotency_key: Option<Uuid>,
    pub external_order_ref: Option<String>,
    pub charge_id: Option<String>,
    pub registered_at: Option<DateTime<Utc>>,
    pub expires_at: Option<DateTime<Utc>>,
    pub renewal_attempts: u32,
    pub next_renewal_at: Option<DateTime<Utc>>,
    pub failure_detail: Option<serde_json::Value>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl DomainOrder {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: Uuid,
        tenant_id: Uuid,
        hostname: Hostname,
        term_years: u8,
        price: Money,
        auto_renew: bool,
        whois_privacy: bool,
        registrant_contact: RegistrantContact,
        ordered_by: Option<Uuid>,
        idempotency_key: Option<Uuid>,
        now: DateTime<Utc>,
    ) -> Self {
        Self {
            id,
            tenant_id,
            hostname,
            term_years,
            price,
            status: OrderStatus::Pending,
            auto_renew,
            whois_privacy,
            registrant_contact,
            ordered_by,
            idempotency_key,
            external_order_ref: None,
            charge_id: None,
            registered_at: None,
            expires_at: None,
            renewal_attempts: 0,
            next_renewal_at: None,
            failure_detail: None,
            created_at: now,
            updated_at: now,
        }
    }

    fn require(&self, allowed: &[OrderStatus], action: &str) -> Result<(), DomainsError> {
        if allowed.contains(&self.status) {
            return Ok(());
        }
        Err(DomainsError::OrderNotModifiable(format!(
            "{action} from {} is not allowed",
            self.status.as_str()
        )))
    }

    /// The charge succeeded.
    pub fn mark_charged(
        &mut self,
        charge_id: String,
        now: DateTime<Utc>,
    ) -> Result<(), DomainsError> {
        self.require(&[OrderStatus::Pending], "charge")?;
        self.status = OrderStatus::Charged;
        self.charge_id = Some(charge_id);
        self.updated_at = now;
        Ok(())
    }

    /// The registrar accepted the registration.
    pub fn mark_registered(
        &mut self,
        external_ref: String,
        registered_at: DateTime<Utc>,
        expires_at: DateTime<Utc>,
        now: DateTime<Utc>,
    ) -> Result<(), DomainsError> {
        self.require(
            &[
                OrderStatus::Charged,
                OrderStatus::Registered,
                OrderStatus::Configuring,
            ],
            "register",
        )?;
        self.status = OrderStatus::Registered;
        self.external_order_ref = Some(external_ref);
        self.registered_at = Some(registered_at);
        self.expires_at = Some(expires_at);
        self.failure_detail = None;
        self.updated_at = now;
        Ok(())
    }

    /// Registration accepted; DNS application in progress.
    pub fn mark_configuring(&mut self, now: DateTime<Utc>) -> Result<(), DomainsError> {
        self.require(
            &[
                OrderStatus::Registered,
                OrderStatus::Configuring,
                OrderStatus::Charged,
            ],
            "configure",
        )?;
        self.status = OrderStatus::Configuring;
        self.updated_at = now;
        Ok(())
    }

    /// DNS applied and the claim created: the journey continues in the
    /// domain aggregate.
    pub fn mark_active(&mut self, now: DateTime<Utc>) -> Result<(), DomainsError> {
        self.require(
            &[OrderStatus::Configuring, OrderStatus::Registered],
            "activate",
        )?;
        self.status = OrderStatus::Active;
        self.updated_at = now;
        Ok(())
    }

    /// Terminal failure before registration (charge failed / refused).
    pub fn mark_failed(&mut self, detail: String, now: DateTime<Utc>) -> Result<(), DomainsError> {
        self.require(&[OrderStatus::Pending, OrderStatus::Charged], "fail")?;
        self.status = OrderStatus::Failed;
        self.failure_detail = Some(serde_json::json!({ "detail": detail }));
        self.updated_at = now;
        Ok(())
    }

    /// Charged, registration failed, and the refund call itself failed:
    /// the order closes as `failed` with `refund_error` recorded, which is
    /// the marker the order sweep keys its refund retries on.
    pub fn mark_failed_pending_refund(
        &mut self,
        detail: String,
        refund_error: String,
        now: DateTime<Utc>,
    ) -> Result<(), DomainsError> {
        self.require(&[OrderStatus::Pending, OrderStatus::Charged], "fail")?;
        self.status = OrderStatus::Failed;
        self.failure_detail = Some(serde_json::json!({
            "detail": detail,
            "refund_error": refund_error,
        }));
        self.updated_at = now;
        Ok(())
    }

    /// Charged, then the registrar failed: refunded and closed.
    ///
    /// Also legal from `failed` — that is the refund-retry path: the refund
    /// call itself failed, the order sweep retried it until it landed.
    pub fn mark_refunded(
        &mut self,
        detail: String,
        now: DateTime<Utc>,
    ) -> Result<(), DomainsError> {
        self.require(
            &[
                OrderStatus::Charged,
                OrderStatus::Registered,
                OrderStatus::Failed,
            ],
            "refund",
        )?;
        self.status = OrderStatus::Refunded;
        self.failure_detail = Some(serde_json::json!({ "detail": detail }));
        self.updated_at = now;
        Ok(())
    }

    /// Toggle auto-renew (any non-terminal state).
    pub fn set_auto_renew(
        &mut self,
        enabled: bool,
        now: DateTime<Utc>,
    ) -> Result<(), DomainsError> {
        self.require(
            &[
                OrderStatus::Pending,
                OrderStatus::Charged,
                OrderStatus::Registered,
                OrderStatus::Configuring,
                OrderStatus::Active,
            ],
            "auto-renew",
        )?;
        self.auto_renew = enabled;
        self.updated_at = now;
        Ok(())
    }

    /// The registry expiry moved (renewal accepted).
    pub fn mark_renewed(
        &mut self,
        new_expiry: DateTime<Utc>,
        now: DateTime<Utc>,
    ) -> Result<(), DomainsError> {
        self.require(&[OrderStatus::Active], "renew")?;
        self.expires_at = Some(new_expiry);
        self.renewal_attempts = 0;
        self.next_renewal_at = None;
        self.updated_at = now;
        Ok(())
    }

    /// A renewal attempt failed: count it and schedule the retry.
    pub fn mark_renewal_failed(
        &mut self,
        detail: String,
        now: DateTime<Utc>,
        retry_secs: u64,
    ) -> Result<(), DomainsError> {
        self.require(&[OrderStatus::Active], "renew")?;
        self.renewal_attempts += 1;
        self.next_renewal_at = Some(now + chrono::Duration::seconds(retry_secs as i64));
        self.failure_detail = Some(serde_json::json!({ "renewal_detail": detail }));
        self.updated_at = now;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn order() -> DomainOrder {
        DomainOrder::new(
            Uuid::now_v7(),
            Uuid::now_v7(),
            Hostname::parse("example.com").unwrap(),
            1,
            Money::new(1299, "usd"),
            true,
            true,
            RegistrantContact {
                name: "Acme".to_string(),
                email: "ops@acme.test".to_string(),
                phone: "+81-3-0000-0000".to_string(),
                organization: None,
                country_code: "jp".to_string(),
            },
            None,
            None,
            Utc::now(),
        )
    }

    #[test]
    fn saga_path() {
        let mut order = order();
        let now = Utc::now();
        order.mark_charged("ch_1".to_string(), now).unwrap();
        assert_eq!(order.status, OrderStatus::Charged);
        order
            .mark_registered("reg-1".to_string(), now, now, now)
            .unwrap();
        assert_eq!(order.status, OrderStatus::Registered);
        order.mark_configuring(now).unwrap();
        order.mark_active(now).unwrap();
        assert_eq!(order.status, OrderStatus::Active);
    }

    #[test]
    fn registration_is_idempotent() {
        let mut order = order();
        let now = Utc::now();
        order.mark_charged("ch_1".to_string(), now).unwrap();
        order
            .mark_registered("reg-1".to_string(), now, now, now)
            .unwrap();
        order
            .mark_registered("reg-1".to_string(), now, now, now)
            .expect("re-register after crash resumes, not fails");
    }

    #[test]
    fn refund_only_after_charge() {
        let mut order = order();
        let now = Utc::now();
        assert!(order.mark_refunded("x".to_string(), now).is_err());
        order.mark_charged("ch_1".to_string(), now).unwrap();
        order.mark_refunded("x".to_string(), now).unwrap();
        assert_eq!(order.status, OrderStatus::Refunded);
    }

    #[test]
    fn a_failed_refund_is_retried_by_the_order_sweep() {
        let mut order = order();
        let now = Utc::now();
        order.mark_charged("ch_1".to_string(), now).unwrap();
        // Registration failed and the refund call itself failed: the order
        // closes as `failed` carrying the refund_error marker the sweep
        // keys its retries on.
        order
            .mark_failed_pending_refund(
                "registration failed; refund failed".to_string(),
                "connection reset".to_string(),
                now,
            )
            .unwrap();
        assert_eq!(order.status, OrderStatus::Failed);
        assert!(
            order
                .failure_detail
                .as_ref()
                .unwrap()
                .get("refund_error")
                .is_some()
        );
        // The retry lands: failed → refunded is legal.
        order
            .mark_refunded("refunded on retry".to_string(), now)
            .unwrap();
        assert_eq!(order.status, OrderStatus::Refunded);
    }
}
