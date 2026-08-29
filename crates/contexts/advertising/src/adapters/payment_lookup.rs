//! Joins a captured conversion's order reference to the phase-09 payment
//! record. The payment record is the trustworthy value (NFR-1010): a
//! client-reported amount is only ever a fallback hint, never a source of
//! truth for ROAS.

use std::sync::Arc;

use async_trait::async_trait;
use sanvi_kernel::TenantId;
use sanvi_payments::domain::CheckoutRepository;

use crate::domain::{AdvertisingError, Money, PaymentLookup, PaymentLookupPort};

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
        tenant_id: TenantId,
        order_ref: &str,
    ) -> Result<Option<PaymentLookup>, AdvertisingError> {
        let checkout = self
            .checkouts
            .find_by_reference(scope, tenant_id, order_ref)
            .await
            .map_err(|err| AdvertisingError::Storage(err.to_string()))?;

        let Some(checkout) = checkout else {
            return Ok(None);
        };
        let Some(payment) = self
            .checkouts
            .latest_payment_for_checkout(scope, tenant_id, checkout.id)
            .await
            .map_err(|err| AdvertisingError::Storage(err.to_string()))?
        else {
            return Ok(None);
        };

        // A straight field copy — no decimal arithmetic — so a
        // zero-decimal currency like JPY carries through unchanged. The
        // conversion id is deliberately derived at this adapter boundary
        // from PaymentRecord.id, never from the checkout row.
        Ok(Some(PaymentLookup {
            conversion_event_id: sanvi_payments::application::conversion_event_id(
                tenant_id, payment.id,
            )
            .to_string(),
            value: Money {
                amount_minor: payment.amount.amount_minor,
                currency: payment.amount.currency.as_str().to_string(),
            },
        }))
    }
}
