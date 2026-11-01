//! The public shopper surface: `/api/v1/public/ecommerce/...`. Mounted as
//! full-path resources (never a shared public scope — actix commits to the
//! first matching scope without backtracking), outside tenant resolution
//! and authentication: every handler resolves the tenant from the
//! validated `Host` header (or the `?host=` override the SSR layer
//! passes), and the storefront-facing state decides everything else.
//!
//! All responses carry `cache-control: private, no-store`: prices and
//! availability change without any platform invalidation event, and the
//! no-store posture is this phase's policy.

use actix_web::http::header;
use actix_web::{HttpRequest, HttpResponse, web};
use sanvi_http::{ApiError, CurrentSubject, CurrentTenant, RequiredPermission};
use serde::Deserialize;

use super::ProductAccessState;
use crate::application::Caller;
use crate::application::shopper::{
    PublishStoreRequest, ShopperPlaceOrderRequest, ShopperReleaseCheckoutRequest,
    ShopperStartCheckoutRequest,
};
use crate::domain::{ForwardRequest, ProductPath, ShopperError};

/// The `?host=` override the SSR layer passes (a browser cannot set the
/// `Host` header); absent, the request's own host decides.
#[derive(Debug, Deserialize)]
pub struct HostQuery {
    #[serde(default)]
    pub host: Option<String>,
}

/// The products listing's query: the host override, a bounded page size
/// and the `after` cursor. `IntoParams` gives the OpenAPI schema its
/// bounds (the spec lint requires them on every paginated endpoint).
#[derive(Debug, Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ProductsQuery {
    /// Host override for server-side callers; defaults to the request Host.
    #[serde(default)]
    pub host: Option<String>,
    /// Page size, 1-100 (default 50).
    #[param(minimum = 1, maximum = 100, default = 50)]
    #[serde(default)]
    pub limit: Option<i64>,
    /// Opaque cursor from the previous page's `next_cursor`.
    #[serde(default)]
    pub after: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct CredentialQuery {
    #[serde(default)]
    pub host: Option<String>,
    #[serde(default)]
    pub credential: Option<String>,
}

/// The host a shopper request addresses: the `?host=` override, else the
/// request's `Host` header, else empty (which resolves to nothing).
fn shopper_host(request: &HttpRequest, query: &HostQuery) -> String {
    query
        .host
        .clone()
        .filter(|host| !host.trim().is_empty())
        .unwrap_or_else(|| {
            request
                .headers()
                .get(header::HOST)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned)
                .unwrap_or_default()
        })
}

/// Builds the host argument for order-scoped calls (which take the
/// credential query too).
fn shopper_host_with_credential(request: &HttpRequest, query: &CredentialQuery) -> String {
    query
        .host
        .clone()
        .filter(|host| !host.trim().is_empty())
        .unwrap_or_else(|| {
            request
                .headers()
                .get(header::HOST)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned)
                .unwrap_or_default()
        })
}

fn no_store(mut response: HttpResponse) -> HttpResponse {
    let headers = response.headers_mut();
    headers.insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("private, no-store"),
    );
    headers.insert(header::VARY, header::HeaderValue::from_static("host"));
    response
}

/// Renders a [`ShopperError`] as the shared problem envelope. Upstream
/// typed problems pass through verbatim — status, slug and structured
/// extensions — so the storefront's taxonomy is the service's taxonomy;
/// platform-owned classes use the shopper slugs.
pub fn shopper_problem(error: ShopperError) -> HttpResponse {
    match error {
        ShopperError::Upstream(problem) => {
            // Extension members go flat: the platform's problem envelope
            // captures unknown top-level members as RFC 9457 extensions.
            let mut body = serde_json::Map::new();
            body.insert("type".to_owned(), serde_json::json!(problem.type_uri));
            body.insert("title".to_owned(), serde_json::json!(problem.title));
            body.insert("status".to_owned(), serde_json::json!(problem.status));
            body.insert("detail".to_owned(), serde_json::json!(problem.detail));
            if !problem.fields.is_empty() {
                body.insert(
                    "fields".to_owned(),
                    serde_json::json!(
                        problem
                            .fields
                            .iter()
                            .map(|(field, message)| serde_json::json!({
                                "field": field,
                                "message": message,
                            }))
                            .collect::<Vec<_>>()
                    ),
                );
            }
            if !problem.lines.is_empty() {
                body.insert("lines".to_owned(), serde_json::json!(problem.lines));
            }
            let extensions = [
                (
                    "attempt_id",
                    problem.attempt_id.map(serde_json::Value::String),
                ),
                ("current_version", problem.current_version.map(|v| v.into())),
                ("due_minor", problem.due_minor.map(|v| v.into())),
                ("pending_minor", problem.pending_minor.map(|v| v.into())),
                ("order_id", problem.order_id.map(serde_json::Value::String)),
                (
                    "customer_id",
                    problem.customer_id.map(serde_json::Value::String),
                ),
            ];
            for (key, value) in extensions {
                if let Some(value) = value {
                    body.insert(key.to_owned(), value);
                }
            }
            let status = actix_web::http::StatusCode::from_u16(problem.status)
                .unwrap_or(actix_web::http::StatusCode::BAD_GATEWAY);
            HttpResponse::build(status).json(body)
        }
        other => {
            let status = other.status();
            let slug = other.slug().unwrap_or(
                // The classes without a context slug map onto the shared ones.
                match &other {
                    ShopperError::Unavailable(_) => "service-unavailable",
                    ShopperError::BadGateway(_) => "bad-gateway",
                    ShopperError::Storage(_) => "internal-error",
                    _ => "internal-error",
                },
            );
            let title = match slug {
                "ecommerce/store-unavailable" => "Store Unavailable",
                "ecommerce/credential-invalid" => "Order Access Denied",
                "ecommerce/credential-expired" => "Order Link Expired",
                "ecommerce/checkout-expired" => "Checkout Expired",
                "ecommerce/price-changed" => "Prices Changed",
                "bad-request" => "Bad Request",
                "not-found" => "Not Found",
                "service-unavailable" => "Service Unavailable",
                "bad-gateway" => "Bad Gateway",
                _ => "Internal Server Error",
            };
            let problem = ApiError::problem_type(
                actix_web::http::StatusCode::from_u16(status)
                    .unwrap_or(actix_web::http::StatusCode::INTERNAL_SERVER_ERROR),
                slug,
                title,
                other.to_string(),
            );
            HttpResponse::build(problem.status()).json(problem.problem())
        }
    }
}

macro_rules! shopper_call {
    ($state:expr, $future:expr) => {
        match $future.await {
            Ok(view) => no_store(HttpResponse::Ok().json(view)),
            Err(error) => {
                if matches!(
                    error,
                    ShopperError::Unavailable(_)
                        | ShopperError::BadGateway(_)
                        | ShopperError::Storage(_)
                ) {
                    tracing::warn!(error = %error, "shopper request degraded");
                }
                no_store(shopper_problem(error))
            }
        }
    };
}

/// `GET /api/v1/public/ecommerce/store` — the published store this host
/// serves: name and currency. Unknown hosts, unbound tenants, archived
/// stores and entitlement-off tenants all answer the same 404-shaped
/// `store-unavailable` problem.
#[utoipa::path(
    get,
    path = "/api/v1/public/ecommerce/store",
    tag = "product_access",
    operation_id = "public_ecommerce_store",
    params(("host" = Option<String>, Query, description = "Host override for server-side callers; defaults to the request Host")),
    responses(
        (status = 200, description = "The published store", body = crate::application::shopper::PublicStoreView),
        (status = 404, description = "No store is available for this host (unknown host, no published store, archived store, or the tenant does not have the product)", body = sanvi_http::ProblemDetail),
        (status = 502, description = "The store service answered unexpectedly", body = sanvi_http::ProblemDetail),
        (status = 503, description = "The store service is not configured or unreachable", body = sanvi_http::ProblemDetail)
    )
)]
pub async fn get_public_store(
    state: web::Data<ProductAccessState>,
    request: HttpRequest,
    query: web::Query<HostQuery>,
) -> HttpResponse {
    let host = shopper_host(&request, &query);
    shopper_call!(state, state.shopper.public_store(&host))
}

/// `GET /api/v1/public/ecommerce/products` — one page of active products
/// with sellable variants, server-priced and server-availability-marked.
#[utoipa::path(
    get,
    path = "/api/v1/public/ecommerce/products",
    tag = "product_access",
    operation_id = "public_ecommerce_products",
    params(ProductsQuery),
    responses(
        (status = 200, description = "One page of sellable products", body = crate::application::shopper::PublicProductPage),
        (status = 404, description = "No store is available for this host", body = sanvi_http::ProblemDetail),
        (status = 502, description = "The store service answered unexpectedly", body = sanvi_http::ProblemDetail),
        (status = 503, description = "The store service is not configured or unreachable", body = sanvi_http::ProblemDetail)
    )
)]
pub async fn get_public_products(
    state: web::Data<ProductAccessState>,
    request: HttpRequest,
    query: web::Query<ProductsQuery>,
) -> HttpResponse {
    let host = shopper_host(
        &request,
        &HostQuery {
            host: query.host.clone(),
        },
    );
    shopper_call!(
        state,
        state
            .shopper
            .public_products(&host, query.limit, query.after.as_deref())
    )
}

/// `GET /api/v1/public/ecommerce/products/{product_id}` — one active
/// product with its sellable variants.
#[utoipa::path(
    get,
    path = "/api/v1/public/ecommerce/products/{product_id}",
    tag = "product_access",
    operation_id = "public_ecommerce_product",
    params(
        ("product_id" = String, Path, description = "Product id (decimal Snowflake)"),
        ("host" = Option<String>, Query, description = "Host override for server-side callers; defaults to the request Host")
    ),
    responses(
        (status = 200, description = "The product with sellable variants", body = crate::application::shopper::PublicProductView),
        (status = 404, description = "Unknown product, or no store for this host", body = sanvi_http::ProblemDetail),
        (status = 502, description = "The store service answered unexpectedly", body = sanvi_http::ProblemDetail),
        (status = 503, description = "The store service is not configured or unreachable", body = sanvi_http::ProblemDetail)
    )
)]
pub async fn get_public_product(
    state: web::Data<ProductAccessState>,
    request: HttpRequest,
    path: web::Path<String>,
    query: web::Query<HostQuery>,
) -> HttpResponse {
    let host = shopper_host(&request, &query);
    shopper_call!(
        state,
        state
            .shopper
            .public_product(&host, path.into_inner().as_str())
    )
}

/// `POST /api/v1/public/ecommerce/checkout` — guest checkout start with an
/// `Idempotency-Key`: validates the guest contact + address, reuses or
/// creates the guest customer and address, prices the lines from the
/// catalog, picks reservation locations from live stock and holds the
/// reservation. The response is the only pricing a checkout page may
/// show; the opaque `checkout` token carries it to placement and dies
/// with the hold. Refusals: 409 `ecommerce/insufficient-stock` (with
/// `lines` indexes; nothing is held), 400 validation.
#[utoipa::path(
    post,
    path = "/api/v1/public/ecommerce/checkout",
    tag = "product_access",
    operation_id = "public_ecommerce_checkout_start",
    params(
        ("Idempotency-Key" = String, Header, description = "One key per checkout intent; reused across retries of the same intent, re-minted after a payload edit"),
        ("host" = Option<String>, Query, description = "Host override for server-side callers; defaults to the request Host")
    ),
    request_body = crate::application::shopper::ShopperStartCheckoutRequest,
    responses(
        (status = 200, description = "The held, priced checkout summary", body = crate::application::shopper::ShopperCheckoutView),
        (status = 400, description = "Validation refusal (bounds, ids, duplicate lines)", body = sanvi_http::ProblemDetail),
        (status = 404, description = "No store is available for this host", body = sanvi_http::ProblemDetail),
        (status = 409, description = "Stock unavailable on one or more lines (`lines` extension; nothing held)", body = sanvi_http::ProblemDetail),
        (status = 502, description = "The store service answered unexpectedly", body = sanvi_http::ProblemDetail),
        (status = 503, description = "The store service is not configured or unreachable", body = sanvi_http::ProblemDetail)
    )
)]
pub async fn post_public_checkout(
    state: web::Data<ProductAccessState>,
    request: HttpRequest,
    body: web::Json<ShopperStartCheckoutRequest>,
    query: web::Query<HostQuery>,
) -> HttpResponse {
    let host = shopper_host(&request, &query);
    let key = request
        .headers()
        .get("idempotency-key")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    let checkout = body.into_inner();
    shopper_call!(state, state.shopper.start_checkout(&host, &key, &checkout))
}

/// `POST /api/v1/public/ecommerce/checkout/release` — releases the hold a
/// checkout token names (abandonment path). Idempotent; accepts tokens
/// whose hold already expired.
#[utoipa::path(
    post,
    path = "/api/v1/public/ecommerce/checkout/release",
    tag = "product_access",
    operation_id = "public_ecommerce_checkout_release",
    params(("host" = Option<String>, Query, description = "Host override for server-side callers; defaults to the request Host")),
    request_body = crate::application::shopper::ShopperReleaseCheckoutRequest,
    responses(
        (status = 204, description = "Released (or already released, committed, or expired)"),
        (status = 400, description = "The checkout reference is not valid", body = sanvi_http::ProblemDetail),
        (status = 502, description = "The store service answered unexpectedly", body = sanvi_http::ProblemDetail),
        (status = 503, description = "The store service is not configured or unreachable", body = sanvi_http::ProblemDetail)
    )
)]
pub async fn post_public_checkout_release(
    state: web::Data<ProductAccessState>,
    request: HttpRequest,
    body: web::Json<ShopperReleaseCheckoutRequest>,
    query: web::Query<HostQuery>,
) -> HttpResponse {
    let host = shopper_host(&request, &query);
    match state
        .shopper
        .release_checkout(&host, body.into_inner().checkout.as_str())
        .await
    {
        Ok(()) => no_store(HttpResponse::NoContent().finish()),
        Err(error) => no_store(shopper_problem(error)),
    }
}

/// `POST /api/v1/public/ecommerce/orders` — place the order from the held
/// reservation the checkout token names, with an `Idempotency-Key`. The
/// service snapshots lines, prices and tax; a price drift against the
/// checkout summary is refused (`ecommerce/price-changed`) and the fresh
/// order cancelled. Success returns the order summary of record plus the
/// purchase credential.
#[utoipa::path(
    post,
    path = "/api/v1/public/ecommerce/orders",
    tag = "product_access",
    operation_id = "public_ecommerce_order_place",
    params(
        ("Idempotency-Key" = String, Header, description = "One key per placement intent; replayed intents return the original order"),
        ("host" = Option<String>, Query, description = "Host override for server-side callers; defaults to the request Host")
    ),
    request_body = crate::application::shopper::ShopperPlaceOrderRequest,
    responses(
        (status = 200, description = "The placed order (a replayed intent returns the original)", body = crate::application::shopper::ShopperOrderConfirmationView),
        (status = 400, description = "The checkout reference is not valid", body = sanvi_http::ProblemDetail),
        (status = 404, description = "No store for this host", body = sanvi_http::ProblemDetail),
        (status = 409, description = "Checkout expired, prices changed, reservation expired or already ordered (typed problem)", body = sanvi_http::ProblemDetail),
        (status = 422, description = "Upstream refusal (total too large, destination not served)", body = sanvi_http::ProblemDetail),
        (status = 502, description = "The store service answered unexpectedly", body = sanvi_http::ProblemDetail),
        (status = 503, description = "The store service is not configured or unreachable", body = sanvi_http::ProblemDetail)
    )
)]
pub async fn post_public_order(
    state: web::Data<ProductAccessState>,
    request: HttpRequest,
    body: web::Json<ShopperPlaceOrderRequest>,
    query: web::Query<HostQuery>,
) -> HttpResponse {
    let host = shopper_host(&request, &query);
    let key = request
        .headers()
        .get("idempotency-key")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    shopper_call!(
        state,
        state
            .shopper
            .place_order(&host, &key, body.into_inner().checkout.as_str())
    )
}

/// `GET /api/v1/public/ecommerce/orders/{order_id}` — the credential-
/// scoped order summary. The credential travels in the `credential` query
/// parameter (the status link is a plain URL); an absent, invalid or
/// expired credential renders 401 and never reveals whether the order
/// exists.
#[utoipa::path(
    get,
    path = "/api/v1/public/ecommerce/orders/{order_id}",
    tag = "product_access",
    operation_id = "public_ecommerce_order_get",
    params(
        ("order_id" = String, Path, description = "Order id (decimal Snowflake); authorizes nothing alone"),
        ("credential" = String, Query, description = "The purchase credential minted at placement"),
        ("host" = Option<String>, Query, description = "Host override for server-side callers; defaults to the request Host")
    ),
    responses(
        (status = 200, description = "The order summary of record", body = crate::application::shopper::ShopperOrderView),
        (status = 401, description = "The credential is absent, invalid or expired (distinguished by problem type)", body = sanvi_http::ProblemDetail),
        (status = 502, description = "The store service answered unexpectedly", body = sanvi_http::ProblemDetail),
        (status = 503, description = "The store service is not configured or unreachable", body = sanvi_http::ProblemDetail)
    )
)]
pub async fn get_public_order(
    state: web::Data<ProductAccessState>,
    request: HttpRequest,
    path: web::Path<String>,
    query: web::Query<CredentialQuery>,
) -> HttpResponse {
    let host = shopper_host_with_credential(&request, &query);
    let order_id = path.into_inner();
    shopper_call!(
        state,
        state
            .shopper
            .order_status(&host, &order_id, query.credential.as_deref())
    )
}

/// `POST /api/v1/public/ecommerce/orders/{order_id}/cancel` — cancel
/// while unpaid, with the purchase credential. The orchestration cancels
/// a pending payment attempt first; upstream refusals (payment settled,
/// fulfillment exists, provider cannot cancel yet) pass through typed.
#[utoipa::path(
    post,
    path = "/api/v1/public/ecommerce/orders/{order_id}/cancel",
    tag = "product_access",
    operation_id = "public_ecommerce_order_cancel",
    params(
        ("order_id" = String, Path, description = "Order id (decimal Snowflake)"),
        ("credential" = String, Query, description = "The purchase credential minted at placement"),
        ("host" = Option<String>, Query, description = "Host override for server-side callers; defaults to the request Host")
    ),
    responses(
        (status = 200, description = "The cancelled order (cancelling a cancelled order is idempotent)", body = crate::application::shopper::ShopperOrderView),
        (status = 401, description = "The credential is absent, invalid or expired", body = sanvi_http::ProblemDetail),
        (status = 409, description = "The order cannot be cancelled (typed upstream problem)", body = sanvi_http::ProblemDetail),
        (status = 502, description = "The store service answered unexpectedly", body = sanvi_http::ProblemDetail),
        (status = 503, description = "The store service is not configured or unreachable", body = sanvi_http::ProblemDetail)
    )
)]
pub async fn post_public_order_cancel(
    state: web::Data<ProductAccessState>,
    request: HttpRequest,
    path: web::Path<String>,
    query: web::Query<CredentialQuery>,
) -> HttpResponse {
    let host = shopper_host_with_credential(&request, &query);
    let order_id = path.into_inner();
    shopper_call!(
        state,
        state
            .shopper
            .cancel_order(&host, &order_id, query.credential.as_deref())
    )
}

/// `POST /api/v1/public/ecommerce/orders/{order_id}/payment` — start (or
/// replay) the hosted payment attempt with an `Idempotency-Key`. The
/// amount is always the server's due amount; the response carries the
/// hosted `redirect_url` while pending. A second start while one is
/// pending answers 409 `ecommerce/payment-in-progress`.
#[utoipa::path(
    post,
    path = "/api/v1/public/ecommerce/orders/{order_id}/payment",
    tag = "product_access",
    operation_id = "public_ecommerce_payment_start",
    params(
        ("order_id" = String, Path, description = "Order id (decimal Snowflake)"),
        ("credential" = String, Query, description = "The purchase credential minted at placement"),
        ("Idempotency-Key" = String, Header, description = "One key per payment intent; replayed intents return the original attempt"),
        ("host" = Option<String>, Query, description = "Host override for server-side callers; defaults to the request Host")
    ),
    responses(
        (status = 200, description = "The attempt (replayed intents return the original, with a fresh redirect_url while pending)", body = crate::application::shopper::ShopperPaymentStartView),
        (status = 401, description = "The credential is absent, invalid or expired", body = sanvi_http::ProblemDetail),
        (status = 409, description = "A payment attempt is already pending (`attempt_id` extension)", body = sanvi_http::ProblemDetail),
        (status = 502, description = "The payment provider failed (`ecommerce/payment-provider-error`)", body = sanvi_http::ProblemDetail),
        (status = 503, description = "Provider payments are not configured (`ecommerce/payments-unavailable`)", body = sanvi_http::ProblemDetail)
    )
)]
pub async fn post_public_payment(
    state: web::Data<ProductAccessState>,
    request: HttpRequest,
    path: web::Path<String>,
    query: web::Query<CredentialQuery>,
) -> HttpResponse {
    let host = shopper_host_with_credential(&request, &query);
    let order_id = path.into_inner();
    let key = request
        .headers()
        .get("idempotency-key")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    shopper_call!(
        state,
        state
            .shopper
            .start_payment(&host, &order_id, query.credential.as_deref(), &key)
    )
}

/// `GET /api/v1/public/ecommerce/orders/{order_id}/payment` — the latest
/// payment attempt's state, for the return route's polling to webhook
/// truth. `pending` means processing; a timeout here is never a verdict.
#[utoipa::path(
    get,
    path = "/api/v1/public/ecommerce/orders/{order_id}/payment",
    tag = "product_access",
    operation_id = "public_ecommerce_payment_get",
    params(
        ("order_id" = String, Path, description = "Order id (decimal Snowflake)"),
        ("credential" = String, Query, description = "The purchase credential minted at placement"),
        ("host" = Option<String>, Query, description = "Host override for server-side callers; defaults to the request Host")
    ),
    responses(
        (status = 200, description = "The latest attempt, or `attempt: null` before the first one", body = crate::application::shopper::ShopperPaymentStatusView),
        (status = 401, description = "The credential is absent, invalid or expired", body = sanvi_http::ProblemDetail),
        (status = 502, description = "The store service answered unexpectedly", body = sanvi_http::ProblemDetail),
        (status = 503, description = "The store service is not configured or unreachable", body = sanvi_http::ProblemDetail)
    )
)]
pub async fn get_public_payment(
    state: web::Data<ProductAccessState>,
    request: HttpRequest,
    path: web::Path<String>,
    query: web::Query<CredentialQuery>,
) -> HttpResponse {
    let host = shopper_host_with_credential(&request, &query);
    let order_id = path.into_inner();
    shopper_call!(
        state,
        state
            .shopper
            .payment_status(&host, &order_id, query.credential.as_deref())
    )
}

/// `POST /api/v1/public/ecommerce/orders/{order_id}/payment/cancel` —
/// cancel the pending attempt (the shopper walked away from the provider
/// page). Settled attempts are returned unchanged; a provider that cannot
/// cancel yet answers 409 `ecommerce/payment-in-progress`.
#[utoipa::path(
    post,
    path = "/api/v1/public/ecommerce/orders/{order_id}/payment/cancel",
    tag = "product_access",
    operation_id = "public_ecommerce_payment_cancel",
    params(
        ("order_id" = String, Path, description = "Order id (decimal Snowflake)"),
        ("credential" = String, Query, description = "The purchase credential minted at placement"),
        ("host" = Option<String>, Query, description = "Host override for server-side callers; defaults to the request Host")
    ),
    responses(
        (status = 200, description = "The latest attempt after the cancel", body = crate::application::shopper::ShopperPaymentStatusView),
        (status = 401, description = "The credential is absent, invalid or expired", body = sanvi_http::ProblemDetail),
        (status = 409, description = "The provider cannot cancel the attempt yet", body = sanvi_http::ProblemDetail),
        (status = 502, description = "The payment provider failed", body = sanvi_http::ProblemDetail),
        (status = 503, description = "The store service is not configured or unreachable", body = sanvi_http::ProblemDetail)
    )
)]
pub async fn post_public_payment_cancel(
    state: web::Data<ProductAccessState>,
    request: HttpRequest,
    path: web::Path<String>,
    query: web::Query<CredentialQuery>,
) -> HttpResponse {
    let host = shopper_host_with_credential(&request, &query);
    let order_id = path.into_inner();
    shopper_call!(
        state,
        state
            .shopper
            .cancel_payment(&host, &order_id, query.credential.as_deref())
    )
}

// ----------------------------------------------------------------------
// Tenant-side published-store administration
// ----------------------------------------------------------------------

/// `GET /api/v1/tenant/ecommerce-public-store` — the tenant's published
/// store, if any. Staff-surface for the host → store decision the public
/// scope serves (its console ships in a later phase).
#[utoipa::path(
    get,
    path = "/api/v1/tenant/ecommerce-public-store",
    tag = "product_access",
    operation_id = "get_published_store",
    responses(
        (status = 200, description = "The published store id, or null", body = crate::application::shopper::PublishedStoreView),
        (status = 403, description = "Missing ecommerce.store.read", body = sanvi_http::ProblemDetail),
        (status = 503, description = "Storage unavailable", body = sanvi_http::ProblemDetail)
    )
)]
pub async fn get_published_store(
    state: web::Data<ProductAccessState>,
    tenant: CurrentTenant,
    _permission: RequiredPermission,
) -> HttpResponse {
    match state.shopper.published_store(tenant.0.tenant_id).await {
        Ok(view) => HttpResponse::Ok().json(view),
        Err(error) => shopper_problem(error),
    }
}

/// `PUT /api/v1/tenant/ecommerce-public-store` — publish or withdraw the
/// tenant's store. The store is validated through the caller's own
/// delegation (it must exist, be the tenant's, and be active) before the
/// binding is written.
#[utoipa::path(
    put,
    path = "/api/v1/tenant/ecommerce-public-store",
    tag = "product_access",
    operation_id = "put_published_store",
    request_body = crate::application::shopper::PublishStoreRequest,
    responses(
        (status = 200, description = "The binding after the change", body = crate::application::shopper::PublishedStoreView),
        (status = 400, description = "Unknown store, archived store, or a malformed id", body = sanvi_http::ProblemDetail),
        (status = 403, description = "Missing ecommerce.store.manage, or the tenant lacks the product", body = sanvi_http::ProblemDetail),
        (status = 503, description = "The store service or storage is unavailable", body = sanvi_http::ProblemDetail)
    )
)]
pub async fn put_published_store(
    state: web::Data<ProductAccessState>,
    tenant: CurrentTenant,
    subject: CurrentSubject,
    _permission: RequiredPermission,
    body: web::Json<PublishStoreRequest>,
) -> HttpResponse {
    let requested = body.into_inner().store_id.as_deref().and_then(|value| {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_owned())
        }
    });
    let store_id = match requested {
        None => None,
        Some(value) => match value.parse::<i64>() {
            Ok(number) if number > 0 && number.to_string() == value => Some(number),
            _ => {
                return shopper_problem(ShopperError::CheckoutInvalid {
                    detail: "store_id must be a canonical positive decimal".to_owned(),
                });
            }
        },
    };

    // Validate through the caller's delegation: entitlements, the caller's
    // own ecommerce permissions, then the service's answer.
    let validate = async |tenant_id: sanvi_kernel::TenantId, store: i64| {
        let forward = ForwardRequest::new(
            "GET",
            ProductPath::parse(&format!("stores/{store}")).map_err(ShopperError::from)?,
            None,
            Vec::<(String, String)>::new(),
            Vec::new(),
            None,
        )
        .map_err(ShopperError::from)?;
        let caller = Caller {
            tenant_id,
            user_id: subject.0.user_id,
            role_ids: &subject.0.role_ids,
        };
        let response = state.ecommerce.execute(caller, &forward).await?;
        if response.status != 200 {
            return Err(ShopperError::NotFound { resource: "store" });
        }
        serde_json::from_slice::<crate::application::shopper::StoreDoc>(&response.body).map_err(
            |_| ShopperError::BadGateway("the store service answered unexpectedly".to_owned()),
        )
    };

    match state
        .shopper
        .publish_store(tenant.0.tenant_id, store_id, validate)
        .await
    {
        Ok(view) => HttpResponse::Ok().json(view),
        Err(error) => shopper_problem(error),
    }
}
