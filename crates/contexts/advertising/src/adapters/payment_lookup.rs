//! Joins a captured conversion's order reference to the phase-09 payment
//! record. The payment record is the trustworthy value (NFR-1010): a
//! client-reported amount is only ever a fallback hint, never a source of
//! truth for ROAS.
//!
//! The record lives in the standalone Payments service, so the join is a
//! signed `GET /internal/payments/conversion-value` under a single-use
//! `conversion` delegation. The service derives the conversion ID from the
//! payment record, never from the checkout row.

use async_trait::async_trait;
use sanvi_kernel::TenantId;
use sanvi_payments::adapters::remote::{
    Delegation, RemoteClient, RequestBinding, ServiceConfig, TENANT_AUDIENCE, percent_encode,
};
use serde::Deserialize;

use crate::domain::{AdvertisingError, Money, PaymentLookup, PaymentLookupPort};

pub struct RemotePaymentLookup {
    client: RemoteClient,
}

impl RemotePaymentLookup {
    pub fn new(config: &ServiceConfig) -> anyhow::Result<Self> {
        Ok(Self {
            client: RemoteClient::new(config)?,
        })
    }
}

#[derive(Deserialize)]
struct ConversionValue {
    conversion_event_id: String,
    amount_minor: i64,
    currency: String,
}

#[async_trait]
impl PaymentLookupPort for RemotePaymentLookup {
    async fn lookup_payment_value(
        &self,
        tenant_id: TenantId,
        order_ref: &str,
    ) -> Result<Option<PaymentLookup>, AdvertisingError> {
        let target = format!(
            "/internal/payments/conversion-value?tenant_id={tenant_id}&order_ref={}",
            percent_encode(order_ref)
        );
        let claims = Delegation::new(
            TENANT_AUDIENCE,
            "conversion",
            &RequestBinding::json("GET", &target, b""),
        );
        // An unreachable or failing service is an error, never "no payment
        // record": the caller must not fall back to the client's hint.
        let response = self
            .client
            .send(&claims, Vec::new(), "application/json", "")
            .await
            .map_err(|err| AdvertisingError::Storage(format!("payments lookup: {err}")))?;
        match response.status {
            200 => {
                let value: ConversionValue = serde_json::from_slice(&response.body)
                    .map_err(|err| AdvertisingError::Storage(format!("payments lookup: {err}")))?;
                // A straight field copy — no decimal arithmetic — so a
                // zero-decimal currency like JPY carries through unchanged.
                Ok(Some(PaymentLookup {
                    conversion_event_id: value.conversion_event_id,
                    value: Money {
                        amount_minor: value.amount_minor,
                        currency: value.currency,
                    },
                }))
            }
            404 => Ok(None),
            status => Err(AdvertisingError::Storage(format!(
                "payments lookup answered {status}"
            ))),
        }
    }
}
