//! Joins a captured conversion's order reference to the phase-09 payment
//! record. The payment record is the trustworthy value (NFR-1010): a
//! client-reported amount is only ever a fallback hint, never a source of
//! truth for ROAS.

use std::sync::Arc;

use async_trait::async_trait;
use sanvi_payments::domain::CheckoutRepository;
use uuid::Uuid;

use crate::domain::{AdvertisingError, Money, PaymentLookupPort};

pub struct CheckoutPaymentLookup {
    checkouts: Arc<dyn CheckoutRepository>,
}

impl CheckoutPaymentLookup {
    pub fn new(checkouts: Arc<dyn CheckoutRepository>) -> Self {
        Self { checkouts }
    }
}

#[async_trait]
impl PaymentLookupPort for CheckoutPaymentLookup {
    async fn lookup_payment_value(
        &self,
        scope: &mut sanvi_persistence::ScopedTx<'_>,
        tenant_id: Uuid,
        order_ref: &str,
    ) -> Result<Option<Money>, AdvertisingError> {
        let checkout = self
            .checkouts
            .find_completed_by_reference(scope, tenant_id, order_ref)
            .await
            .map_err(|err| AdvertisingError::Storage(err.to_string()))?;

        // A straight field copy — no decimal arithmetic — so a
        // zero-decimal currency like JPY carries through unchanged.
        Ok(checkout.map(|checkout| Money {
            amount_minor: checkout.amount.amount_minor,
            currency: checkout.amount.currency.as_str().to_string(),
        }))
    }
}
