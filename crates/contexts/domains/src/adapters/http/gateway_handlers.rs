//! Platform gateway handlers for the Domains bounded context.
//! Enforces entitlements (connect & purchase) and permission checks before delegating.
//! Embedded state is preserved for explicit regression fixtures.

use actix_web::{FromRequest, HttpRequest, HttpResponse, web};
use sanvi_http::{
    ApiError, CurrentLocale, CurrentSubject, CurrentTenant, ProblemDetail, RequiredPermission,
};

use super::handlers::{
    AutoRenewCommand, DomainListQuery, InstructionsQuery, PlatformDomainsQuery,
    ReleaseDomainCommand, SearchQuery, TlsAuthorizeQuery, TlsAuthorizeResponse, api_error,
    claim_custom_domain_embedded, get_domain_instructions_embedded, list_custom_domains_embedded,
    list_domain_orders_embedded, list_platform_domains_embedded, place_domain_order_embedded,
    promote_primary_domain_embedded, release_platform_domain_embedded,
    remove_custom_domain_embedded, request_domain_verification_embedded, search_domains_embedded,
    set_order_auto_renew_embedded, tls_authorize_embedded,
};
use crate::{
    adapters::remote::{GatewayState, proxy, proxy_platform},
    application::{ClaimCustomDomainCommand, InstructionsView, OrderView, PlaceDomainOrderCommand},
    domain::{CustomDomainId, DomainOrderId},
};

#[utoipa::path(
    post,
    path = "/api/v1/tenant/domains",
    tag = "domains",
    operation_id = "claim_custom_domain",
    request_body = ClaimCustomDomainCommand,
    responses(
        (status = 201, description = "The claim and its verification challenge", body = crate::application::CustomDomainView),
        (status = 400, description = "Invalid hostname", body = ProblemDetail),
        (status = 403, description = "Permission denied, or entitlement missing (reason: domain_entitlement_required)", body = ProblemDetail),
        (status = 409, description = "Hostname already claimed", body = ProblemDetail),
        (status = 502, description = "Domains service rejected platform credentials or returned an invalid response", body = ProblemDetail),
        (status = 503, description = "Domains service temporarily unavailable", body = ProblemDetail)
    )
)]
pub async fn claim_custom_domain(
    state: Option<web::Data<crate::adapters::http::DomainsState>>,
    tenant: CurrentTenant,
    subject: CurrentSubject,
    req: HttpRequest,
    guard: RequiredPermission,
    payload: web::Bytes,
) -> Result<HttpResponse, ApiError> {
    let Some(state) = state else {
        let gateway = req
            .app_data::<web::Data<GatewayState>>()
            .cloned()
            .ok_or_else(|| ApiError::service_unavailable("Domains service is not configured"))?;
        if !gateway
            .feature_gate
            .is_enabled(tenant.0.tenant_id, &gateway.connect_entitlement_key)
            .await
            .map_err(api_error)?
        {
            return Err(ApiError::ForbiddenReasoned {
                reason: "domain_entitlement_required".to_string(),
                detail: format!(
                    "this requires the {} entitlement",
                    gateway.connect_entitlement_key
                ),
            });
        }
        return proxy(gateway, tenant, subject, guard, req, payload).await;
    };
    let mut raw: actix_web::dev::Payload = payload.into();
    let body = web::Json::<ClaimCustomDomainCommand>::from_request(&req, &mut raw)
        .await
        .map_err(|error| ApiError::BadRequest {
            detail: error.to_string(),
        })?;
    claim_custom_domain_embedded(state, tenant, subject, body, guard).await
}

/// `GET /api/v1/tenant/domains`.
/// Ordered by id DESC.
#[utoipa::path(
    get,
    path = "/api/v1/tenant/domains",
    tag = "domains",
    operation_id = "list_custom_domains",
    params(DomainListQuery),
    responses(
        (status = 200, description = "The tenant's domains", body = Vec<crate::application::CustomDomainView>),
        (status = 400, description = "Malformed cursor", body = ProblemDetail),
        (status = 502, description = "Domains service rejected platform credentials or returned an invalid response", body = ProblemDetail),
        (status = 503, description = "Domains service temporarily unavailable", body = ProblemDetail),
        (status = 504, description = "Statement timeout - the request exceeded its budget", body = ProblemDetail)
    )
)]
pub async fn list_custom_domains(
    state: Option<web::Data<crate::adapters::http::DomainsState>>,
    tenant: CurrentTenant,
    subject: CurrentSubject,
    query: web::Query<DomainListQuery>,
    guard: RequiredPermission,
    req: HttpRequest,
    payload: web::Bytes,
) -> Result<HttpResponse, ApiError> {
    let Some(state) = state else {
        let gateway = req
            .app_data::<web::Data<GatewayState>>()
            .cloned()
            .ok_or_else(|| ApiError::service_unavailable("Domains service is not configured"))?;
        return proxy(gateway, tenant, subject, guard, req, payload).await;
    };
    list_custom_domains_embedded(state, tenant, query, guard).await
}

#[utoipa::path(
    get,
    path = "/api/v1/tenant/domains/{id}/instructions",
    tag = "domains",
    operation_id = "get_domain_instructions",
    params(
        ("id" = String, Path, description = "The domain claim id"),
        ("locale" = Option<String>, Query, description = "Requested locale (defaults to the negotiated one)")
    ),
    responses(
        (status = 200, description = "The exact records and the registrar walkthrough", body = InstructionsView),
        (status = 404, description = "Unknown domain", body = ProblemDetail),
        (status = 502, description = "Domains service rejected platform credentials or returned an invalid response", body = ProblemDetail),
        (status = 503, description = "Domains service temporarily unavailable", body = ProblemDetail)
    )
)]
pub async fn get_domain_instructions(
    state: Option<web::Data<crate::adapters::http::DomainsState>>,
    tenant: CurrentTenant,
    subject: CurrentSubject,
    locale: CurrentLocale,
    (path, query): (web::Path<CustomDomainId>, web::Query<InstructionsQuery>),
    guard: RequiredPermission,
    req: HttpRequest,
    payload: web::Bytes,
) -> Result<HttpResponse, ApiError> {
    let Some(state) = state else {
        let gateway = req
            .app_data::<web::Data<GatewayState>>()
            .cloned()
            .ok_or_else(|| ApiError::service_unavailable("Domains service is not configured"))?;
        return proxy(gateway, tenant, subject, guard, req, payload).await;
    };
    get_domain_instructions_embedded(state, tenant, locale, path, query, guard).await
}

#[utoipa::path(
    post,
    path = "/api/v1/tenant/domains/{id}/verify",
    tag = "domains",
    operation_id = "request_domain_verification",
    params(("id" = String, Path, description = "The domain claim id")),
    responses(
        (status = 200, description = "The domain, now queued for verification", body = crate::application::CustomDomainView),
        (status = 404, description = "Unknown domain", body = ProblemDetail),
        (status = 429, description = "Throttled by the backoff schedule", body = ProblemDetail),
        (status = 502, description = "Domains service rejected platform credentials or returned an invalid response", body = ProblemDetail),
        (status = 503, description = "Domains service temporarily unavailable", body = ProblemDetail)
    )
)]
pub async fn request_domain_verification(
    state: Option<web::Data<crate::adapters::http::DomainsState>>,
    tenant: CurrentTenant,
    subject: CurrentSubject,
    path: web::Path<CustomDomainId>,
    guard: RequiredPermission,
    req: HttpRequest,
    payload: web::Bytes,
) -> Result<HttpResponse, ApiError> {
    let Some(state) = state else {
        let gateway = req
            .app_data::<web::Data<GatewayState>>()
            .cloned()
            .ok_or_else(|| ApiError::service_unavailable("Domains service is not configured"))?;
        return proxy(gateway, tenant, subject, guard, req, payload).await;
    };
    request_domain_verification_embedded(state, tenant, path, guard).await
}

#[utoipa::path(
    post,
    path = "/api/v1/tenant/domains/{id}/promote",
    tag = "domains",
    operation_id = "promote_primary_domain",
    params(("id" = String, Path, description = "The domain claim id")),
    responses(
        (status = 200, description = "The domain is now primary", body = crate::application::CustomDomainView),
        (status = 404, description = "Unknown domain", body = ProblemDetail),
        (status = 502, description = "Domains service rejected platform credentials or returned an invalid response", body = ProblemDetail),
        (status = 503, description = "Domains service temporarily unavailable", body = ProblemDetail)
    )
)]
pub async fn promote_primary_domain(
    state: Option<web::Data<crate::adapters::http::DomainsState>>,
    tenant: CurrentTenant,
    subject: CurrentSubject,
    path: web::Path<CustomDomainId>,
    guard: RequiredPermission,
    req: HttpRequest,
    payload: web::Bytes,
) -> Result<HttpResponse, ApiError> {
    let Some(state) = state else {
        let gateway = req
            .app_data::<web::Data<GatewayState>>()
            .cloned()
            .ok_or_else(|| ApiError::service_unavailable("Domains service is not configured"))?;
        return proxy(gateway, tenant, subject, guard, req, payload).await;
    };
    promote_primary_domain_embedded(state, tenant, path, guard).await
}

#[utoipa::path(
    delete,
    path = "/api/v1/tenant/domains/{id}",
    tag = "domains",
    operation_id = "remove_custom_domain",
    params(("id" = String, Path, description = "The domain claim id")),
    responses(
        (status = 200, description = "The removed domain", body = crate::application::CustomDomainView),
        (status = 404, description = "Unknown domain", body = ProblemDetail),
        (status = 502, description = "Domains service rejected platform credentials or returned an invalid response", body = ProblemDetail),
        (status = 503, description = "Domains service temporarily unavailable", body = ProblemDetail)
    )
)]
pub async fn remove_custom_domain(
    state: Option<web::Data<crate::adapters::http::DomainsState>>,
    tenant: CurrentTenant,
    subject: CurrentSubject,
    path: web::Path<CustomDomainId>,
    guard: RequiredPermission,
    req: HttpRequest,
    payload: web::Bytes,
) -> Result<HttpResponse, ApiError> {
    let Some(state) = state else {
        let gateway = req
            .app_data::<web::Data<GatewayState>>()
            .cloned()
            .ok_or_else(|| ApiError::service_unavailable("Domains service is not configured"))?;
        return proxy(gateway, tenant, subject, guard, req, payload).await;
    };
    remove_custom_domain_embedded(state, tenant, path, guard).await
}

#[utoipa::path(
    get,
    path = "/api/v1/tenant/domains/search",
    tag = "domains",
    operation_id = "search_domains",
    params(("q" = String, Query, description = "Bare term (`acme`) or hostname (`acme.com`)")),
    responses(
        (status = 200, description = "Availability and normalized pricing", body = crate::application::DomainSearchView),
        (status = 403, description = "Permission denied, or entitlement missing (reason: domain_entitlement_required)", body = ProblemDetail),
        (status = 502, description = "Domains service rejected platform credentials or returned an invalid response", body = ProblemDetail),
        (status = 503, description = "Domains service temporarily unavailable", body = ProblemDetail)
    )
)]
pub async fn search_domains(
    state: Option<web::Data<crate::adapters::http::DomainsState>>,
    tenant: CurrentTenant,
    subject: CurrentSubject,
    query: web::Query<SearchQuery>,
    guard: RequiredPermission,
    req: HttpRequest,
    payload: web::Bytes,
) -> Result<HttpResponse, ApiError> {
    let Some(state) = state else {
        let gateway = req
            .app_data::<web::Data<GatewayState>>()
            .cloned()
            .ok_or_else(|| ApiError::service_unavailable("Domains service is not configured"))?;
        if !gateway
            .feature_gate
            .is_enabled(tenant.0.tenant_id, &gateway.purchase_entitlement_key)
            .await
            .map_err(api_error)?
        {
            return Err(ApiError::ForbiddenReasoned {
                reason: "domain_entitlement_required".to_string(),
                detail: format!(
                    "this requires the {} entitlement",
                    gateway.purchase_entitlement_key
                ),
            });
        }
        return proxy(gateway, tenant, subject, guard, req, payload).await;
    };
    search_domains_embedded(state, tenant, query, guard).await
}

#[utoipa::path(
    post,
    path = "/api/v1/tenant/domains/orders",
    tag = "domains",
    operation_id = "place_domain_order",
    request_body = PlaceDomainOrderCommand,
    responses(
        (status = 201, description = "The order (the saga may still be in flight; see status)", body = OrderView),
        (status = 400, description = "Invalid hostname or unsupported TLD", body = ProblemDetail),
        (status = 403, description = "Permission denied, or entitlement missing (reason: domain_entitlement_required)", body = ProblemDetail),
        (status = 502, description = "Domains service rejected platform credentials or returned an invalid response", body = ProblemDetail),
        (status = 503, description = "Domains service temporarily unavailable", body = ProblemDetail)
    )
)]
pub async fn place_domain_order(
    state: Option<web::Data<crate::adapters::http::DomainsState>>,
    tenant: CurrentTenant,
    subject: CurrentSubject,
    req: HttpRequest,
    guard: RequiredPermission,
    payload: web::Bytes,
) -> Result<HttpResponse, ApiError> {
    let Some(state) = state else {
        let gateway = req
            .app_data::<web::Data<GatewayState>>()
            .cloned()
            .ok_or_else(|| ApiError::service_unavailable("Domains service is not configured"))?;
        if !gateway
            .feature_gate
            .is_enabled(tenant.0.tenant_id, &gateway.purchase_entitlement_key)
            .await
            .map_err(api_error)?
        {
            return Err(ApiError::ForbiddenReasoned {
                reason: "domain_entitlement_required".to_string(),
                detail: format!(
                    "this requires the {} entitlement",
                    gateway.purchase_entitlement_key
                ),
            });
        }
        return proxy(gateway, tenant, subject, guard, req, payload).await;
    };
    let mut raw: actix_web::dev::Payload = payload.into();
    let body = web::Json::<PlaceDomainOrderCommand>::from_request(&req, &mut raw)
        .await
        .map_err(|error| ApiError::BadRequest {
            detail: error.to_string(),
        })?;
    place_domain_order_embedded(state, tenant, subject, body, guard).await
}

/// `GET /api/v1/tenant/domains/orders`.
/// Ordered by id DESC.
#[utoipa::path(
    get,
    path = "/api/v1/tenant/domains/orders",
    tag = "domains",
    operation_id = "list_domain_orders",
    params(DomainListQuery),
    responses(
        (status = 200, description = "The tenant's domain orders", body = Vec<OrderView>),
        (status = 400, description = "Malformed cursor", body = ProblemDetail),
        (status = 403, description = "Permission denied, or entitlement missing (reason: domain_entitlement_required)", body = ProblemDetail),
        (status = 502, description = "Domains service rejected platform credentials or returned an invalid response", body = ProblemDetail),
        (status = 503, description = "Domains service temporarily unavailable", body = ProblemDetail),
        (status = 504, description = "Statement timeout - the request exceeded its budget", body = ProblemDetail)
    )
)]
pub async fn list_domain_orders(
    state: Option<web::Data<crate::adapters::http::DomainsState>>,
    tenant: CurrentTenant,
    subject: CurrentSubject,
    query: web::Query<DomainListQuery>,
    guard: RequiredPermission,
    req: HttpRequest,
    payload: web::Bytes,
) -> Result<HttpResponse, ApiError> {
    let Some(state) = state else {
        let gateway = req
            .app_data::<web::Data<GatewayState>>()
            .cloned()
            .ok_or_else(|| ApiError::service_unavailable("Domains service is not configured"))?;
        if !gateway
            .feature_gate
            .is_enabled(tenant.0.tenant_id, &gateway.purchase_entitlement_key)
            .await
            .map_err(api_error)?
        {
            return Err(ApiError::ForbiddenReasoned {
                reason: "domain_entitlement_required".to_string(),
                detail: format!(
                    "this requires the {} entitlement",
                    gateway.purchase_entitlement_key
                ),
            });
        }
        return proxy(gateway, tenant, subject, guard, req, payload).await;
    };
    list_domain_orders_embedded(state, tenant, query, guard).await
}

#[utoipa::path(
    post,
    path = "/api/v1/tenant/domains/orders/{id}/auto-renew",
    tag = "domains",
    operation_id = "set_order_auto_renew",
    params(("id" = String, Path, description = "The order id")),
    request_body = AutoRenewCommand,
    responses(
        (status = 200, description = "The updated order", body = OrderView),
        (status = 403, description = "Permission denied, or entitlement missing (reason: domain_entitlement_required)", body = ProblemDetail),
        (status = 404, description = "Unknown order", body = ProblemDetail),
        (status = 502, description = "Domains service rejected platform credentials or returned an invalid response", body = ProblemDetail),
        (status = 503, description = "Domains service temporarily unavailable", body = ProblemDetail)
    )
)]
pub async fn set_order_auto_renew(
    state: Option<web::Data<crate::adapters::http::DomainsState>>,
    tenant: CurrentTenant,
    subject: CurrentSubject,
    path: web::Path<DomainOrderId>,
    req: HttpRequest,
    guard: RequiredPermission,
    payload: web::Bytes,
) -> Result<HttpResponse, ApiError> {
    let Some(state) = state else {
        let gateway = req
            .app_data::<web::Data<GatewayState>>()
            .cloned()
            .ok_or_else(|| ApiError::service_unavailable("Domains service is not configured"))?;
        if !gateway
            .feature_gate
            .is_enabled(tenant.0.tenant_id, &gateway.purchase_entitlement_key)
            .await
            .map_err(api_error)?
        {
            return Err(ApiError::ForbiddenReasoned {
                reason: "domain_entitlement_required".to_string(),
                detail: format!(
                    "this requires the {} entitlement",
                    gateway.purchase_entitlement_key
                ),
            });
        }
        return proxy(gateway, tenant, subject, guard, req, payload).await;
    };
    let mut raw: actix_web::dev::Payload = payload.into();
    let body = web::Json::<AutoRenewCommand>::from_request(&req, &mut raw)
        .await
        .map_err(|error| ApiError::BadRequest {
            detail: error.to_string(),
        })?;
    set_order_auto_renew_embedded(state, tenant, path, body, guard).await
}

#[utoipa::path(
    get,
    path = "/internal/tls/authorize",
    tag = "domains",
    operation_id = "tls_authorize",
    params(("host" = String, Query, description = "The hostname the edge wants a certificate for")),
    responses(
        (status = 200, description = "Issuance authorized", body = TlsAuthorizeResponse),
        (status = 403, description = "Issuance denied", body = TlsAuthorizeResponse),
        (status = 502, description = "Domains service rejected platform credentials or returned an invalid response", body = ProblemDetail),
        (status = 503, description = "The gate is not configured", body = ProblemDetail)
    )
)]
pub async fn tls_authorize(
    state: Option<web::Data<crate::adapters::http::DomainsState>>,
    request: HttpRequest,
    query: web::Query<TlsAuthorizeQuery>,
) -> Result<HttpResponse, ApiError> {
    let Some(state) = state else {
        let gateway = request
            .app_data::<web::Data<GatewayState>>()
            .cloned()
            .ok_or_else(|| ApiError::service_unavailable("Domains service is not configured"))?;
        let presented = request
            .headers()
            .get("x-sanvi-internal")
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default();
        return gateway
            .remote
            .proxy_tls_authorize(&query.host, presented)
            .await?
            .into_http();
    };
    tls_authorize_embedded(state, request, query).await
}

/// `GET /api/v1/platform/domains`.
/// Ordered by id DESC.
#[utoipa::path(
    get,
    path = "/api/v1/platform/domains",
    tag = "domains",
    operation_id = "list_platform_domains",
    params(PlatformDomainsQuery),
    responses(
        (status = 200, description = "Domains across tenants", body = Vec<crate::application::CustomDomainView>),
        (status = 400, description = "Malformed cursor", body = ProblemDetail),
        (status = 502, description = "Domains service rejected platform credentials or returned an invalid response", body = ProblemDetail),
        (status = 503, description = "Domains service temporarily unavailable", body = ProblemDetail),
        (status = 504, description = "Statement timeout - the request exceeded its budget", body = ProblemDetail)
    )
)]
pub async fn list_platform_domains(
    state: Option<web::Data<crate::adapters::http::DomainsState>>,
    subject: CurrentSubject,
    query: web::Query<PlatformDomainsQuery>,
    guard: RequiredPermission,
    req: HttpRequest,
    payload: web::Bytes,
) -> Result<HttpResponse, ApiError> {
    let Some(state) = state else {
        let gateway = req
            .app_data::<web::Data<GatewayState>>()
            .cloned()
            .ok_or_else(|| ApiError::service_unavailable("Domains service is not configured"))?;
        return proxy_platform(gateway, subject, guard, req, payload).await;
    };
    list_platform_domains_embedded(state, query, guard).await
}

#[utoipa::path(
    post,
    path = "/api/v1/platform/domains/{id}/release",
    tag = "domains",
    operation_id = "release_platform_domain",
    params(("id" = String, Path, description = "The domain id")),
    request_body = ReleaseDomainCommand,
    responses(
        (status = 200, description = "The released domain", body = crate::application::CustomDomainView),
        (status = 404, description = "Unknown domain", body = ProblemDetail),
        (status = 502, description = "Domains service rejected platform credentials or returned an invalid response", body = ProblemDetail),
        (status = 503, description = "Domains service temporarily unavailable", body = ProblemDetail)
    )
)]
pub async fn release_platform_domain(
    state: Option<web::Data<crate::adapters::http::DomainsState>>,
    subject: CurrentSubject,
    path: web::Path<CustomDomainId>,
    req: HttpRequest,
    guard: RequiredPermission,
    payload: web::Bytes,
) -> Result<HttpResponse, ApiError> {
    let Some(state) = state else {
        let gateway = req
            .app_data::<web::Data<GatewayState>>()
            .cloned()
            .ok_or_else(|| ApiError::service_unavailable("Domains service is not configured"))?;
        return proxy_platform(gateway, subject, guard, req, payload).await;
    };
    let mut raw: actix_web::dev::Payload = payload.into();
    let body = web::Json::<ReleaseDomainCommand>::from_request(&req, &mut raw)
        .await
        .map_err(|error| ApiError::BadRequest {
            detail: error.to_string(),
        })?;
    release_platform_domain_embedded(state, path, body, guard).await
}
