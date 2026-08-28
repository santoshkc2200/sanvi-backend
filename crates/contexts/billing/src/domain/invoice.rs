//! Invoice references: the tenant-facing mirror of Stripe invoices.
//! Personal data → registered in the phase 05 data map with a legal
//! retention rule that overrides erasure requests.

use chrono::{DateTime, Utc};
use sanvi_kernel::{Id, TenantId};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::domain::BillingError;
use crate::domain::subscription::Period;

pub enum InvoiceTag {}
pub type InvoiceId = Id<InvoiceTag>;

/// Mirrors Stripe's invoice status one-to-one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum InvoiceStatus {
    Draft,
    Open,
    Paid,
    Uncollectible,
    Void,
}

impl InvoiceStatus {
    pub fn parse(raw: &str) -> Result<Self, BillingError> {
        match raw {
            "draft" => Ok(Self::Draft),
            "open" => Ok(Self::Open),
            "paid" => Ok(Self::Paid),
            "uncollectible" => Ok(Self::Uncollectible),
            "void" => Ok(Self::Void),
            other => Err(BillingError::InvalidWebhookPayload(format!(
                "unknown invoice status: {other}"
            ))),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Draft => "draft",
            Self::Open => "open",
            Self::Paid => "paid",
            Self::Uncollectible => "uncollectible",
            Self::Void => "void",
        }
    }
}

/// A stored invoice reference.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InvoiceRef {
    id: InvoiceId,
    tenant_id: TenantId,
    stripe_invoice_id: String,
    number: Option<String>,
    status: InvoiceStatus,
    total_minor: i64,
    currency: String,
    hosted_url: Option<String>,
    pdf_url: Option<String>,
    period: Option<Period>,
    created_at: DateTime<Utc>,
    paid_at: Option<DateTime<Utc>>,
}

impl InvoiceRef {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: InvoiceId,
        tenant_id: TenantId,
        stripe_invoice_id: String,
        number: Option<String>,
        status: InvoiceStatus,
        total_minor: i64,
        currency: String,
        hosted_url: Option<String>,
        pdf_url: Option<String>,
        period: Option<Period>,
        created_at: DateTime<Utc>,
        paid_at: Option<DateTime<Utc>>,
    ) -> Result<Self, BillingError> {
        if stripe_invoice_id.trim().is_empty() {
            return Err(BillingError::InvalidWebhookPayload(
                "stripe_invoice_id must not be empty".to_string(),
            ));
        }
        let currency = currency.to_ascii_lowercase();
        if currency.len() != 3 {
            return Err(BillingError::InvalidCurrency(currency));
        }
        if total_minor < 0 {
            return Err(BillingError::InvalidWebhookPayload(
                "negative invoice total".to_string(),
            ));
        }
        Ok(Self {
            id,
            tenant_id,
            stripe_invoice_id,
            number,
            status,
            total_minor,
            currency,
            hosted_url,
            pdf_url,
            period,
            created_at,
            paid_at,
        })
    }

    /// Re-hydration constructor.
    #[allow(clippy::too_many_arguments)]
    pub fn from_parts(
        id: InvoiceId,
        tenant_id: TenantId,
        stripe_invoice_id: String,
        number: Option<String>,
        status: InvoiceStatus,
        total_minor: i64,
        currency: String,
        hosted_url: Option<String>,
        pdf_url: Option<String>,
        period: Option<Period>,
        created_at: DateTime<Utc>,
        paid_at: Option<DateTime<Utc>>,
    ) -> Self {
        Self {
            id,
            tenant_id,
            stripe_invoice_id,
            number,
            status,
            total_minor,
            currency,
            hosted_url,
            pdf_url,
            period,
            created_at,
            paid_at,
        }
    }

    pub fn id(&self) -> InvoiceId {
        self.id
    }
    pub fn tenant_id(&self) -> TenantId {
        self.tenant_id
    }
    pub fn stripe_invoice_id(&self) -> &str {
        &self.stripe_invoice_id
    }
    pub fn number(&self) -> Option<&str> {
        self.number.as_deref()
    }
    pub fn status(&self) -> InvoiceStatus {
        self.status
    }
    pub fn total_minor(&self) -> i64 {
        self.total_minor
    }
    pub fn currency(&self) -> &str {
        &self.currency
    }
    pub fn hosted_url(&self) -> Option<&str> {
        self.hosted_url.as_deref()
    }
    pub fn pdf_url(&self) -> Option<&str> {
        self.pdf_url.as_deref()
    }
    pub fn period(&self) -> Option<Period> {
        self.period
    }
    pub fn created_at(&self) -> DateTime<Utc> {
        self.created_at
    }
    pub fn paid_at(&self) -> Option<DateTime<Utc>> {
        self.paid_at
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicI64, Ordering};

    use super::*;

    static NEXT_ID: AtomicI64 = AtomicI64::new(1);

    fn test_id<Tag>() -> Id<Tag> {
        Id::try_from(NEXT_ID.fetch_add(1, Ordering::Relaxed)).unwrap()
    }

    #[test]
    fn invoice_status_mirrors_stripe() {
        for (raw, expected) in [
            ("draft", InvoiceStatus::Draft),
            ("open", InvoiceStatus::Open),
            ("paid", InvoiceStatus::Paid),
            ("uncollectible", InvoiceStatus::Uncollectible),
            ("void", InvoiceStatus::Void),
        ] {
            assert_eq!(InvoiceStatus::parse(raw).unwrap(), expected);
            assert_eq!(expected.as_str(), raw);
        }
    }

    #[test]
    fn invoices_validate_identity_and_amounts() {
        let ok = InvoiceRef::new(
            test_id(),
            test_id(),
            "in_x".to_string(),
            Some("3A2F".to_string()),
            InvoiceStatus::Paid,
            4900,
            "USD".to_string(),
            None,
            None,
            None,
            Utc::now(),
            Some(Utc::now()),
        );
        assert!(ok.is_ok());
        assert!(
            InvoiceRef::new(
                test_id(),
                test_id(),
                " ".to_string(),
                None,
                InvoiceStatus::Open,
                0,
                "usd".to_string(),
                None,
                None,
                None,
                Utc::now(),
                None,
            )
            .is_err()
        );
    }
}
