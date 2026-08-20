//! The payments context's HTTP handlers.

use actix_web::{HttpResponse, web};
use sanvi_http::{ApiError, CurrentTenant, ProblemDetail, RequiredFeature, RequiredPermission};

use crate::adapters::http::PaymentsState;
use crate::application::ProvidersView;

/// The payments context's domain-error → HTTP mapping. The stable
/// `payments/*` problem-type slugs live on the error itself
/// (`PaymentsError::type_slug`) so the taxonomy can't drift from the
/// contract; this maps onto the platform's status vocabulary.
pub fn api_error(err: crate::domain::PaymentsError) -> ApiError {
    match err {
        crate::domain::PaymentsError::InvalidMoney(detail)
        | crate::domain::PaymentsError::InvalidConnection(detail) => {
            ApiError::BadRequest { detail }
        }
        crate::domain::PaymentsError::NotEntitled(feature) => ApiError::Forbidden {
            detail: format!("this requires the {feature} entitlement"),
        },
        crate::domain::PaymentsError::ConnectionNotActive(detail)
        | crate::domain::PaymentsError::IdempotencyConflict(detail) => {
            ApiError::Conflict { detail }
        }
        crate::domain::PaymentsError::CapabilityMissing { capability, detail } => {
            ApiError::Conflict {
                detail: format!("capability {capability} is not active: {detail}"),
            }
        }
        crate::domain::PaymentsError::CurrencyUnsupported { currency, detail } => {
            ApiError::BadRequest {
                detail: format!(
                    "currency {currency} is not supported by this connection: {detail}"
                ),
            }
        }
        crate::domain::PaymentsError::ProviderUnavailable(detail) => {
            ApiError::BadGateway { detail }
        }
        crate::domain::PaymentsError::WebhookRejected(detail) => {
            // The detail is logged for observability; it is never echoed
            // to a caller who just failed authentication (09.3's
            // transport keeps the same discipline).
            tracing::warn!(detail = %detail, "payments webhook rejected");
            ApiError::Unauthorized
        }
        crate::domain::PaymentsError::Storage(detail) => {
            tracing::error!(error = %detail, "payments storage failure");
            ApiError::internal("storage failure")
        }
    }
}

/// `GET /api/v1/tenant/payments/providers`.
///
/// Gated three ways: the `payments.enabled` phase flag (route absent when
/// off), `payments.read` (permission extractor), and the
/// `payments.stripe_connect` entitlement (`RequiredFeature` — 403, never
/// 404, so the frontend can render an upgrade prompt instead of a dead
/// end).
///
/// The response is derived from the provider registry — the handler adds
/// nothing provider-specific. The tenant's country (from the tenant
/// context's region, when it parses as ISO-3166-1 alpha-2) decides
/// per-tenant availability; an unparseable region leaves the provider
/// available rather than locking the tenant out.
#[utoipa::path(
    get,
    path = "/api/v1/tenant/payments/providers",
    tag = "payments",
    operation_id = "list_payment_providers",
    responses(
        (status = 200, description = "Connectable providers, derived from the provider registry", body = crate::application::ProvidersView),
        (status = 403, description = "Missing payments.read permission or the payments.stripe_connect entitlement", body = ProblemDetail)
    ),
    params(
        ("Idempotency-Key" = Option<uuid::Uuid>, Header, description = "Required on every *mutating* payments endpoint (none exist yet); documented here because the convention is fixed now, before the first one ships")
    )
)]
pub async fn list_payment_providers(
    state: web::Data<PaymentsState>,
    tenant: CurrentTenant,
    _permission: RequiredPermission,
    _entitlement: RequiredFeature,
) -> Result<HttpResponse, ApiError> {
    let tenant_id = tenant.0.tenant_id;
    let tenant_country = crate::domain::provider::CountryCode::parse(&tenant.0.region).ok();
    let view: ProvidersView = state
        .list_providers
        .execute(tenant_id, tenant_country)
        .await
        .map_err(api_error)?;
    state
        .metrics
        .catalog_requests
        .with_label_values(&["ok"])
        .inc();
    Ok(HttpResponse::Ok()
        .insert_header(("cache-control", "private, max-age=60"))
        .json(view))
}
