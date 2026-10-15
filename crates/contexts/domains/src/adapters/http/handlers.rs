use std::net::IpAddr;

use actix_web::{HttpRequest, HttpResponse, web};
use chrono::Utc;
use serde::Deserialize;
use utoipa::ToSchema;

use sanvi_http::{
    ApiError, CurrentLocale, CurrentSubject, CurrentTenant, ProblemDetail, RequiredPermission,
};
use sanvi_kernel::TenantId;
use sanvi_persistence::ScopedTx;

use crate::application::{
    ClaimCustomDomainCommand, InstructionsView, OrderView, PlaceDomainOrderCommand,
};
use crate::domain::{CustomDomainId, DomainOrderId, DomainsError};

use super::validation;

/// The domains context's domain-error → HTTP mapping.
pub fn api_error(err: DomainsError) -> ApiError {
    match err {
        // TASK-027 (FR-1127): invalid-input rejections are field-addressed
        // — the flat message is unchanged, the offending input is named on
        // top. `validation::validation_issue` owns the field paths.
        DomainsError::InvalidHostname(_)
        | DomainsError::HostnameReserved(_)
        | DomainsError::HostnameOnPlatformDomain(_)
        | DomainsError::UnsupportedTld(_)
        | DomainsError::DomainUnavailable(_) => {
            let (detail, issues) = validation::validation_issue(&err)
                .expect("hostname rejections are field-addressed");
            ApiError::validation(detail, issues)
        }
        DomainsError::OrderNotModifiable(detail) => ApiError::BadRequest { detail },
        DomainsError::OrderNonRefundable => ApiError::BadRequest {
            detail: "registrations are non-refundable once the registrar accepts them".to_string(),
        },
        DomainsError::HostnameAlreadyClaimed(detail) => ApiError::Conflict {
            detail: format!("hostname is already claimed elsewhere: {detail}"),
        },
        DomainsError::HostnameAlreadyRegistered(detail) => ApiError::Conflict {
            detail: format!("hostname is already registered to this tenant: {detail}"),
        },
        DomainsError::UnknownDomain(id) => ApiError::NotFound {
            resource: format!("domain {id}"),
        },
        DomainsError::UnknownOrder(id) => ApiError::NotFound {
            resource: format!("domain order {id}"),
        },
        DomainsError::DomainNotOwned(id) => ApiError::NotFound {
            resource: format!("domain {id}"),
        },
        DomainsError::IllegalTransition { from, to } => ApiError::Conflict {
            detail: format!("cannot move a domain from {from} to {to}"),
        },
        DomainsError::DomainNotLive(hostname) => ApiError::Conflict {
            detail: format!("{hostname} is not live yet"),
        },
        DomainsError::PrimaryAlreadyExists => ApiError::Conflict {
            detail: "this tenant already has a primary domain".to_string(),
        },
        DomainsError::DomainRemoved => ApiError::Gone {
            detail: "the domain was removed".to_string(),
        },
        DomainsError::VerificationThrottled { next } => ApiError::TooManyRequests {
            retry_after_secs: next.signed_duration_since(Utc::now()).num_seconds().max(0) as u64,
        },
        DomainsError::VerificationUnavailable(detail) => ApiError::ServiceUnavailable { detail },
        DomainsError::NotEntitled { feature } => ApiError::ForbiddenReasoned {
            reason: "domain_entitlement_required".to_string(),
            detail: format!("this requires the {feature} entitlement"),
        },
        DomainsError::Registrar(detail) | DomainsError::DnsProvider(detail) => {
            ApiError::BadGateway { detail }
        }
        DomainsError::CertificateAuthority(detail) => ApiError::BadGateway { detail },
        DomainsError::NoCertificate => ApiError::Conflict {
            detail: "no certificate exists for this domain yet".to_string(),
        },
        DomainsError::Charge(detail) => ApiError::BadGateway { detail },
        DomainsError::Refund(detail) => ApiError::BadGateway { detail },
        DomainsError::TlsDenied(detail) => ApiError::Forbidden { detail },
        DomainsError::TlsNotConfigured => ApiError::ServiceUnavailable {
            detail: "tls authorization is not configured on this deployment".to_string(),
        },
        DomainsError::Storage(detail) => {
            tracing::error!(error = %detail, "domains storage failure");
            ApiError::from_storage_detail(&detail)
        }
        DomainsError::Cache(detail) => {
            tracing::error!(error = %detail, "domains cache failure");
            ApiError::internal("cache failure")
        }
        DomainsError::Outbox(detail) => {
            tracing::error!(error = %detail, "domains outbox failure");
            ApiError::internal("event publishing failure")
        }
        DomainsError::Probe(detail) => {
            tracing::warn!(error = %detail, "domains probe failure");
            ApiError::internal("probe failure")
        }
        DomainsError::Notification(detail) => {
            tracing::error!(error = %detail, "domains notification failure");
            ApiError::internal("notification failure")
        }
        DomainsError::IdGeneration(detail) => {
            tracing::error!(error = %detail, "domains id generation failure");
            ApiError::internal("id generation failure")
        }
    }
}

/// Tenant-scoped transaction helpers (the same shape theming uses).
async fn begin_tenant_scope<'a>(
    tx_manager: &'a sanvi_persistence::TxManager,
    tenant_id: TenantId,
) -> Result<ScopedTx<'a>, DomainsError> {
    tx_manager
        .begin_for(tenant_id.as_i64())
        .await
        .map_err(|err| DomainsError::Storage(format!("failed to open tenant scope: {err}")))
}

async fn commit_scope(scope: ScopedTx<'_>) -> Result<(), ApiError> {
    scope.commit().await.map_err(|err| {
        tracing::error!(error = %err, "failed to commit domains transaction");
        ApiError::from_storage_detail(&err.to_string())
    })
}

// ---------------------------------------------------------------------------
// Tenant: claim / list / delete / promote / verify / instructions.
// ---------------------------------------------------------------------------

/// `POST /api/v1/tenant/domains`.
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
        (status = 409, description = "Hostname already claimed", body = ProblemDetail)
    )
)]
pub async fn claim_custom_domain(
    state: web::Data<crate::adapters::http::DomainsState>,
    tenant: CurrentTenant,
    _subject: CurrentSubject,
    body: web::Json<ClaimCustomDomainCommand>,
    _guard: RequiredPermission,
) -> Result<HttpResponse, ApiError> {
    let tenant_id = tenant.0.tenant_id;
    let mut scope = begin_tenant_scope(&state.tx, tenant_id)
        .await
        .map_err(api_error)?;
    let result = state
        .claim
        .execute(&mut scope, tenant_id, &body.into_inner())
        .await
        .map_err(api_error);
    match result {
        Ok(view) => {
            commit_scope(scope).await?;
            Ok(HttpResponse::Created().json(view))
        }
        Err(err) => {
            let _ = scope.rollback().await;
            Err(err)
        }
    }
}

#[derive(Debug, Clone, Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct DomainListQuery {
    /// Page size, clamped to 1..=200.
    #[param(schema_with = domain_limit_schema)]
    pub limit: Option<u32>,
    /// Cursor: the id of the last domain of the previous page. Absent for
    /// the first page; the page is ordered newest first.
    pub after: Option<String>,
}

fn domain_limit_schema() -> utoipa::openapi::schema::Object {
    sanvi_http::openapi::page_limit_schema(200, 200).build()
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
        (status = 504, description = "Statement timeout - the request exceeded its budget", body = ProblemDetail)
    )
)]
pub async fn list_custom_domains(
    state: web::Data<crate::adapters::http::DomainsState>,
    tenant: CurrentTenant,
    query: web::Query<DomainListQuery>,
    _guard: RequiredPermission,
) -> Result<HttpResponse, ApiError> {
    let tenant_id = tenant.0.tenant_id;
    let after_id = query
        .after
        .as_deref()
        .map(|raw| raw.parse::<i64>())
        .transpose()
        .map_err(|_| ApiError::bad_request("invalid cursor"))?;
    let mut scope = begin_tenant_scope(&state.tx, tenant_id)
        .await
        .map_err(api_error)?;
    let view = state
        .list_domains
        .execute(&mut scope, tenant_id, query.limit, after_id)
        .await
        .map_err(api_error)?;
    commit_scope(scope).await?;
    Ok(HttpResponse::Ok().json(view))
}

/// `GET /api/v1/tenant/domains/{id}/instructions`.
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
        (status = 404, description = "Unknown domain", body = ProblemDetail)
    )
)]
pub async fn get_domain_instructions(
    state: web::Data<crate::adapters::http::DomainsState>,
    tenant: CurrentTenant,
    locale: CurrentLocale,
    path: web::Path<CustomDomainId>,
    query: web::Query<InstructionsQuery>,
    _guard: RequiredPermission,
) -> Result<HttpResponse, ApiError> {
    let tenant_id = tenant.0.tenant_id;
    let domain_id = path.into_inner();
    let requested_locale = sanitize_locale(query.locale.as_deref().unwrap_or(&locale.0.locale))?;
    let mut scope = begin_tenant_scope(&state.tx, tenant_id)
        .await
        .map_err(api_error)?;
    let view = state
        .instructions
        .execute(&mut scope, tenant_id, domain_id, &requested_locale)
        .await
        .map_err(api_error)?;
    commit_scope(scope).await?;
    Ok(HttpResponse::Ok()
        .insert_header(("cache-control", "private, max-age=60"))
        .json(view))
}

#[derive(Debug, Clone, Deserialize)]
pub struct InstructionsQuery {
    pub locale: Option<String>,
}

/// `POST /api/v1/tenant/domains/{id}/verify`.
#[utoipa::path(
    post,
    path = "/api/v1/tenant/domains/{id}/verify",
    tag = "domains",
    operation_id = "request_domain_verification",
    params(("id" = String, Path, description = "The domain claim id")),
    responses(
        (status = 200, description = "The domain, now queued for verification", body = crate::application::CustomDomainView),
        (status = 404, description = "Unknown domain", body = ProblemDetail),
        (status = 429, description = "Throttled by the backoff schedule", body = ProblemDetail)
    )
)]
pub async fn request_domain_verification(
    state: web::Data<crate::adapters::http::DomainsState>,
    tenant: CurrentTenant,
    path: web::Path<CustomDomainId>,
    _guard: RequiredPermission,
) -> Result<HttpResponse, ApiError> {
    let tenant_id = tenant.0.tenant_id;
    let domain_id = path.into_inner();
    let mut scope = begin_tenant_scope(&state.tx, tenant_id)
        .await
        .map_err(api_error)?;
    let view = state
        .verify
        .execute(&mut scope, tenant_id, domain_id)
        .await
        .map_err(api_error)?;
    commit_scope(scope).await?;
    Ok(HttpResponse::Ok().json(view))
}

/// `POST /api/v1/tenant/domains/{id}/promote`.
#[utoipa::path(
    post,
    path = "/api/v1/tenant/domains/{id}/promote",
    tag = "domains",
    operation_id = "promote_primary_domain",
    params(("id" = String, Path, description = "The domain claim id")),
    responses(
        (status = 200, description = "The domain is now primary", body = crate::application::CustomDomainView),
        (status = 404, description = "Unknown domain", body = ProblemDetail)
    )
)]
pub async fn promote_primary_domain(
    state: web::Data<crate::adapters::http::DomainsState>,
    tenant: CurrentTenant,
    path: web::Path<CustomDomainId>,
    _guard: RequiredPermission,
) -> Result<HttpResponse, ApiError> {
    let tenant_id = tenant.0.tenant_id;
    let domain_id = path.into_inner();
    let mut scope = begin_tenant_scope(&state.tx, tenant_id)
        .await
        .map_err(api_error)?;
    let view = state
        .promote
        .execute(&mut scope, tenant_id, domain_id)
        .await
        .map_err(api_error)?;
    commit_scope(scope).await?;
    Ok(HttpResponse::Ok().json(view))
}

/// `DELETE /api/v1/tenant/domains/{id}`.
#[utoipa::path(
    delete,
    path = "/api/v1/tenant/domains/{id}",
    tag = "domains",
    operation_id = "remove_custom_domain",
    params(("id" = String, Path, description = "The domain claim id")),
    responses(
        (status = 200, description = "The removed domain", body = crate::application::CustomDomainView),
        (status = 404, description = "Unknown domain", body = ProblemDetail)
    )
)]
pub async fn remove_custom_domain(
    state: web::Data<crate::adapters::http::DomainsState>,
    tenant: CurrentTenant,
    path: web::Path<CustomDomainId>,
    _guard: RequiredPermission,
) -> Result<HttpResponse, ApiError> {
    let tenant_id = tenant.0.tenant_id;
    let domain_id = path.into_inner();
    let mut scope = begin_tenant_scope(&state.tx, tenant_id)
        .await
        .map_err(api_error)?;
    let view = state
        .remove
        .execute(&mut scope, tenant_id, domain_id)
        .await
        .map_err(api_error)?;
    commit_scope(scope).await?;
    state
        .lookup
        .invalidate(view.hostname.as_str())
        .await
        .map_err(api_error)?;
    Ok(HttpResponse::Ok().json(view))
}

// ---------------------------------------------------------------------------
// Tenant: purchase journey.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct SearchQuery {
    pub q: String,
}

/// `GET /api/v1/tenant/domains/search`.
#[utoipa::path(
    get,
    path = "/api/v1/tenant/domains/search",
    tag = "domains",
    operation_id = "search_domains",
    params(("q" = String, Query, description = "Bare term (`acme`) or hostname (`acme.com`)")),
    responses(
        (status = 200, description = "Availability and normalized pricing", body = crate::application::DomainSearchView),
        (status = 403, description = "Permission denied, or entitlement missing (reason: domain_entitlement_required)", body = ProblemDetail)
    )
)]
pub async fn search_domains(
    state: web::Data<crate::adapters::http::DomainsState>,
    tenant: CurrentTenant,
    query: web::Query<SearchQuery>,
    _guard: RequiredPermission,
) -> Result<HttpResponse, ApiError> {
    let view = state
        .search
        .execute(tenant.0.tenant_id, &query.q)
        .await
        .map_err(api_error)?;
    Ok(HttpResponse::Ok().json(view))
}

/// `POST /api/v1/tenant/domains/orders`.
#[utoipa::path(
    post,
    path = "/api/v1/tenant/domains/orders",
    tag = "domains",
    operation_id = "place_domain_order",
    request_body = PlaceDomainOrderCommand,
    responses(
        (status = 201, description = "The order (the saga may still be in flight; see status)", body = OrderView),
        (status = 400, description = "Invalid hostname or unsupported TLD", body = ProblemDetail),
        (status = 403, description = "Permission denied, or entitlement missing (reason: domain_entitlement_required)", body = ProblemDetail)
    )
)]
pub async fn place_domain_order(
    state: web::Data<crate::adapters::http::DomainsState>,
    tenant: CurrentTenant,
    subject: CurrentSubject,
    body: web::Json<PlaceDomainOrderCommand>,
    _guard: RequiredPermission,
) -> Result<HttpResponse, ApiError> {
    let view = state
        .place_order
        .execute(
            tenant.0.tenant_id,
            Some(subject.0.user_id),
            &body.into_inner(),
        )
        .await
        .map_err(api_error)?;
    Ok(HttpResponse::Created().json(view))
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
        (status = 504, description = "Statement timeout - the request exceeded its budget", body = ProblemDetail)
    )
)]
pub async fn list_domain_orders(
    state: web::Data<crate::adapters::http::DomainsState>,
    tenant: CurrentTenant,
    query: web::Query<DomainListQuery>,
    _guard: RequiredPermission,
) -> Result<HttpResponse, ApiError> {
    let tenant_id = tenant.0.tenant_id;
    let after_id = query
        .after
        .as_deref()
        .map(|raw| raw.parse::<i64>())
        .transpose()
        .map_err(|_| ApiError::bad_request("invalid cursor"))?;
    let mut scope = begin_tenant_scope(&state.tx, tenant_id)
        .await
        .map_err(api_error)?;
    let view = state
        .list_orders
        .execute(&mut scope, tenant_id, query.limit, after_id)
        .await
        .map_err(api_error)?;
    commit_scope(scope).await?;
    Ok(HttpResponse::Ok().json(view))
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct AutoRenewCommand {
    pub enabled: bool,
}

/// `POST /api/v1/tenant/domains/orders/{id}/auto-renew`.
#[utoipa::path(
    post,
    path = "/api/v1/tenant/domains/orders/{id}/auto-renew",
    tag = "domains",
    operation_id = "set_order_auto_renew",
    params(("id" = String, Path, description = "The order id")),
    request_body = AutoRenewCommand,
    responses(
        (status = 200, description = "The updated order", body = OrderView),
        (status = 404, description = "Unknown order", body = ProblemDetail)
    )
)]
pub async fn set_order_auto_renew(
    state: web::Data<crate::adapters::http::DomainsState>,
    tenant: CurrentTenant,
    path: web::Path<DomainOrderId>,
    body: web::Json<AutoRenewCommand>,
    _guard: RequiredPermission,
) -> Result<HttpResponse, ApiError> {
    let tenant_id = tenant.0.tenant_id;
    let order_id = path.into_inner();
    let mut scope = begin_tenant_scope(&state.tx, tenant_id)
        .await
        .map_err(api_error)?;
    let use_case = if body.enabled {
        &state.set_auto_renew_on
    } else {
        &state.set_auto_renew_off
    };
    let view = use_case
        .execute(&mut scope, tenant_id, order_id)
        .await
        .map_err(api_error)?;
    commit_scope(scope).await?;
    Ok(HttpResponse::Ok().json(view))
}

// ---------------------------------------------------------------------------
// Internal: the edge's TLS authorization gate.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct TlsAuthorizeQuery {
    pub host: String,
}

#[derive(Debug, Clone, serde::Serialize, ToSchema)]
pub struct TlsAuthorizeResponse {
    pub allowed: bool,
    pub reason: String,
}

/// `GET /internal/tls/authorize` — the edge asks before issuing a
/// certificate. Self-authenticated: `x-sanvi-internal` shared secret plus
/// an optional client-IP allow-list; `/internal` sits outside the authn
/// middleware.
#[utoipa::path(
    get,
    path = "/internal/tls/authorize",
    tag = "domains",
    operation_id = "tls_authorize",
    params(("host" = String, Query, description = "The hostname the edge wants a certificate for")),
    responses(
        (status = 200, description = "Issuance authorized", body = TlsAuthorizeResponse),
        (status = 403, description = "Issuance denied", body = TlsAuthorizeResponse),
        (status = 503, description = "The gate is not configured", body = ProblemDetail)
    )
)]
pub async fn tls_authorize(
    state: web::Data<crate::adapters::http::DomainsState>,
    request: HttpRequest,
    query: web::Query<TlsAuthorizeQuery>,
) -> Result<HttpResponse, ApiError> {
    if !internal_request_allowed(&state, &request) {
        state.metrics.tls_authorize_denied_total.inc();
        return Ok(HttpResponse::Forbidden().json(TlsAuthorizeResponse {
            allowed: false,
            reason: "unauthorized caller".to_string(),
        }));
    }

    let decision = state
        .authorize_tls
        .execute(&query.host)
        .await
        .map_err(api_error)?;

    if decision.is_allowed() {
        Ok(HttpResponse::Ok().json(TlsAuthorizeResponse {
            allowed: true,
            reason: decision.reason().to_string(),
        }))
    } else {
        state.metrics.tls_authorize_denied_total.inc();
        tracing::warn!(
            host = %query.host,
            reason = decision.reason(),
            "tls authorization denied"
        );
        Ok(HttpResponse::Forbidden().json(TlsAuthorizeResponse {
            allowed: false,
            reason: decision.reason().to_string(),
        }))
    }
}

/// The internal-route guard: constant-time secret comparison plus the
/// optional IP allow-list. An unconfigured secret fails closed.
fn internal_request_allowed(
    state: &crate::adapters::http::DomainsState,
    request: &HttpRequest,
) -> bool {
    if state.config.internal_secret.is_empty() {
        return false;
    }
    let presented = request
        .headers()
        .get("x-sanvi-internal")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    if !constant_time_eq(
        presented.as_bytes(),
        state.config.internal_secret.as_bytes(),
    ) {
        return false;
    }
    if state.config.internal_allowed_ips.is_empty() {
        return true;
    }
    let peer = request
        .connection_info()
        .realip_remote_addr()
        .and_then(|address| address.parse::<IpAddr>().ok());
    let Some(peer) = peer else {
        return false;
    };
    state.config.internal_allowed_ips.iter().any(|cidr| {
        parse_cidr(cidr)
            .map(|(network, prefix)| ip_in_prefix(peer, network, prefix))
            .unwrap_or(false)
    })
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

fn parse_cidr(cidr: &str) -> Option<(IpAddr, u8)> {
    let (address, prefix) = cidr.split_once('/')?;
    let address: IpAddr = address.parse().ok()?;
    let prefix: u8 = prefix.parse().ok()?;
    let max = if address.is_ipv4() { 32 } else { 128 };
    (prefix <= max).then_some((address, prefix))
}

fn ip_in_prefix(address: IpAddr, network: IpAddr, prefix: u8) -> bool {
    match (address, network) {
        (IpAddr::V4(address), IpAddr::V4(network)) => {
            let mask = if prefix == 0 {
                0u32
            } else {
                u32::MAX << (32 - prefix)
            };
            (u32::from(address) & mask) == (u32::from(network) & mask)
        }
        (IpAddr::V6(address), IpAddr::V6(network)) => {
            let mask = if prefix == 0 {
                0u128
            } else {
                u128::MAX << (128 - prefix)
            };
            (u128::from(address) & mask) == (u128::from(network) & mask)
        }
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Platform.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct PlatformDomainsQuery {
    /// Hostname substring.
    #[serde(default)]
    pub q: String,
    /// Page size, clamped to 1..=100.
    #[serde(default)]
    #[param(schema_with = platform_domain_limit_schema)]
    pub limit: Option<u32>,
    /// Cursor: the id of the last domain of the previous page. Absent for
    /// the first page; the page is ordered newest first.
    #[serde(default)]
    pub after: Option<String>,
}

fn platform_domain_limit_schema() -> utoipa::openapi::schema::Object {
    sanvi_http::openapi::page_limit_schema(100, 50).build()
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
        (status = 504, description = "Statement timeout - the request exceeded its budget", body = ProblemDetail)
    )
)]
pub async fn list_platform_domains(
    state: web::Data<crate::adapters::http::DomainsState>,
    query: web::Query<PlatformDomainsQuery>,
    _guard: RequiredPermission,
) -> Result<HttpResponse, ApiError> {
    let after_id = query
        .after
        .as_deref()
        .map(|raw| raw.parse::<i64>())
        .transpose()
        .map_err(|_| ApiError::bad_request("invalid cursor"))?;
    let view = state
        .platform_list
        .execute(&query.q, query.limit, after_id)
        .await
        .map_err(api_error)?;
    Ok(HttpResponse::Ok().json(view))
}

/// `POST /api/v1/platform/domains/{id}/release`.
#[utoipa::path(
    post,
    path = "/api/v1/platform/domains/{id}/release",
    tag = "domains",
    operation_id = "release_platform_domain",
    params(("id" = String, Path, description = "The domain id")),
    request_body = ReleaseDomainCommand,
    responses(
        (status = 200, description = "The released domain", body = crate::application::CustomDomainView),
        (status = 404, description = "Unknown domain", body = ProblemDetail)
    )
)]
pub async fn release_platform_domain(
    state: web::Data<crate::adapters::http::DomainsState>,
    path: web::Path<CustomDomainId>,
    body: web::Json<ReleaseDomainCommand>,
    _guard: RequiredPermission,
) -> Result<HttpResponse, ApiError> {
    let domain_id = path.into_inner();
    let view = state
        .platform_release
        .execute(domain_id, &body.reason)
        .await
        .map_err(api_error)?;
    state
        .lookup
        .invalidate(view.hostname.as_str())
        .await
        .map_err(api_error)?;
    Ok(HttpResponse::Ok().json(view))
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct ReleaseDomainCommand {
    /// The audited reason for the force-release.
    pub reason: String,
}

// ---------------------------------------------------------------------------

/// Locale guard (same contract as theming's).
fn sanitize_locale(raw: &str) -> Result<String, ApiError> {
    let trimmed = raw.trim().to_ascii_lowercase();
    let valid = (2..=12).contains(&trimmed.len())
        && trimmed.chars().all(|c| c.is_ascii_alphabetic() || c == '-');
    if valid {
        Ok(trimmed)
    } else {
        Err(ApiError::BadRequest {
            detail: format!("invalid locale {raw:?}"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entitlement_failure_has_a_machine_readable_reason() {
        for feature in ["domains.custom", "domains.purchase"] {
            let problem = api_error(DomainsError::NotEntitled {
                feature: feature.into(),
            })
            .problem();
            assert_eq!(problem.status, 403);
            assert_eq!(
                problem.reason.as_deref(),
                Some("domain_entitlement_required")
            );
        }
    }
}
