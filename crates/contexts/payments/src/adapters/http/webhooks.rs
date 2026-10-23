//! The Connect webhook receiver: `/internal/webhooks/stripe/connect`.
//!
//! A **separate** endpoint from the phase-04 platform receiver, with its
//! own signing secret — the two are distinct trust domains (the cross-
//! secret tests in [`crate::domain::webhook`] and the composition root's
//! boot check keep them that way). Like its sibling it sits under
//! `/internal`, outside the authn middleware by design and authenticated
//! by the Stripe signature instead.
//!
//! Order is the whole game (NFR-905): raw body → signature → parse. The
//! handler takes [`web::Bytes`] so nothing has touched the payload before
//! verification; the global payload limit caps body size; the receiver's
//! allow-list checks the client IP before the secret is even consulted.

use actix_web::{HttpRequest, HttpResponse, web};
use sanvi_http::ApiError;

use crate::adapters::http::PaymentsState;
use crate::domain::error::PaymentsError;

/// Mounts the internal Connect webhook route (flag-gated with the rest of
/// the payments context; deliberately absent from the public OpenAPI spec).
pub fn payments_internal_routes(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::resource("/internal/webhooks/stripe/connect")
            .route(web::post().to(stripe_connect_webhook)),
    );
}

/// `POST /internal/webhooks/stripe/connect` — Stripe's connected-account
/// event receiver. Signature verified on the raw body, stored first,
/// processed asynchronously by the worker; redeliveries dedupe on the
/// event id and still answer 2xx.
async fn stripe_connect_webhook(
    state: web::Data<PaymentsState>,
    body: web::Bytes,
    req: HttpRequest,
) -> Result<HttpResponse, actix_web::Error> {
    let signature = req
        .headers()
        .get("stripe-signature")
        .and_then(|value| value.to_str().ok());
    let client_ip = sanvi_http::client_ip(&req);

    match state
        .webhook_receiver
        .receive(&body, signature, client_ip)
        .await
    {
        // Both fresh stores and redeliveries answer 2xx — a redelivery is
        // a no-op, not an error.
        Ok(_) => Ok(HttpResponse::Ok().json(serde_json::json!({ "received": true }))),
        Err(PaymentsError::WebhookRejected(detail)) => {
            tracing::warn!(detail = %detail, "connect webhook rejected");
            Err(actix_web::Error::from(ApiError::Unauthorized))
        }
        Err(err) => {
            tracing::error!(error = %err, "connect webhook storage failure");
            Err(actix_web::Error::from(ApiError::internal(
                "webhook storage failure",
            )))
        }
    }
}
