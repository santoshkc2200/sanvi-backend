use actix_web::http::StatusCode;
use actix_web::{HttpMessage, HttpRequest, HttpResponse, web};
use sanvi_http::{ApiError, CurrentSubject, CurrentTenant, RequestId, RequiredPermission};

use super::ProductAccessState;
use crate::application::Caller;
use crate::domain::{
    DestinationStoreId, ForwardRequest, ForwardResponse, ProductAccessError, ProductPath,
};

fn problem(error: ProductAccessError) -> ApiError {
    match error {
        // Same detail as the platform's feature gate, so the admin renders
        // the same upgrade prompt.
        ProductAccessError::NotEntitled { feature } => ApiError::Forbidden {
            detail: format!("feature not enabled: {feature}"),
        },
        ProductAccessError::NoPermissions { product } => ApiError::Forbidden {
            detail: format!("no {product} permissions in this tenant"),
        },
        ProductAccessError::BadRequest(detail) => ApiError::BadRequest { detail },
        ProductAccessError::Unavailable(_) => ApiError::ServiceUnavailable {
            detail: "the product service is temporarily unavailable".to_owned(),
        },
        ProductAccessError::BadGateway(_) => ApiError::BadGateway {
            detail: "the product service answered unexpectedly".to_owned(),
        },
        ProductAccessError::Storage(detail) => ApiError::internal(detail),
    }
}

/// Read through to ecommerce-service: `GET /api/v1/tenant/ecommerce/{tail}`
/// becomes `GET /v1/{tail}`, e.g. `stores` or `stores/123/products`.
///
/// The service's own contract (`ecommerce-service/api/openapi.yaml`)
/// documents each route; responses, including its problem documents, pass
/// through unchanged apart from header filtering.
#[utoipa::path(
    get,
    path = "/api/v1/tenant/ecommerce/{tail}",
    tag = "product_access",
    operation_id = "proxy_ecommerce_get",
    params(("tail" = String, Path, description = "Path below ecommerce-service's /v1/")),
    responses(
        (status = 200, description = "Whatever ecommerce-service answered (any status it returns passes through)"),
        (status = 400, description = "Malformed path or oversized body", body = sanvi_http::ProblemDetail),
        (status = 403, description = "Tenant lacks products.ecommerce.enabled, or the user holds no ecommerce.* permission", body = sanvi_http::ProblemDetail),
        (status = 502, description = "ecommerce-service rejected the platform's credentials or answered unusably", body = sanvi_http::ProblemDetail),
        (status = 503, description = "ecommerce-service is not configured or unreachable", body = sanvi_http::ProblemDetail)
    )
)]
pub async fn proxy_ecommerce_get(
    state: web::Data<ProductAccessState>,
    tenant: CurrentTenant,
    subject: CurrentSubject,
    request: HttpRequest,
    tail: web::Path<String>,
    body: web::Bytes,
) -> Result<HttpResponse, ApiError> {
    proxy_ecommerce(state, tenant, subject, request, tail, body).await
}

/// Write through to ecommerce-service: `POST /api/v1/tenant/ecommerce/{tail}`
/// becomes `POST /v1/{tail}`. `Idempotency-Key` is forwarded; the service
/// requires it on every create.
#[utoipa::path(
    post,
    path = "/api/v1/tenant/ecommerce/{tail}",
    tag = "product_access",
    operation_id = "proxy_ecommerce_post",
    params(
        ("tail" = String, Path, description = "Path below ecommerce-service's /v1/"),
        ("Idempotency-Key" = String, Header, description = "Forwarded unchanged")
    ),
    responses(
        (status = 200, description = "Whatever ecommerce-service answered (any status it returns passes through)"),
        (status = 400, description = "Malformed path or oversized body", body = sanvi_http::ProblemDetail),
        (status = 403, description = "Tenant lacks products.ecommerce.enabled, or the user holds no ecommerce.* permission", body = sanvi_http::ProblemDetail),
        (status = 502, description = "ecommerce-service rejected the platform's credentials or answered unusably", body = sanvi_http::ProblemDetail),
        (status = 503, description = "ecommerce-service is not configured or unreachable", body = sanvi_http::ProblemDetail)
    )
)]
pub async fn proxy_ecommerce_post(
    state: web::Data<ProductAccessState>,
    tenant: CurrentTenant,
    subject: CurrentSubject,
    request: HttpRequest,
    tail: web::Path<String>,
    body: web::Bytes,
) -> Result<HttpResponse, ApiError> {
    proxy_ecommerce(state, tenant, subject, request, tail, body).await
}

async fn proxy_ecommerce(
    state: web::Data<ProductAccessState>,
    tenant: CurrentTenant,
    subject: CurrentSubject,
    request: HttpRequest,
    tail: web::Path<String>,
    body: web::Bytes,
) -> Result<HttpResponse, ApiError> {
    let path = ProductPath::parse(&tail).map_err(problem)?;
    let headers = request.headers().iter().filter_map(|(name, value)| {
        value
            .to_str()
            .ok()
            .map(|value| (name.as_str().to_owned(), value.to_owned()))
    });
    let request_id = request
        .extensions()
        .get::<RequestId>()
        .map(ToString::to_string);
    let forward = ForwardRequest::new(
        request.method().as_str(),
        path,
        Some(request.query_string()),
        headers,
        body.to_vec(),
        request_id,
    )
    .map_err(problem)?;

    let caller = Caller {
        tenant_id: tenant.0.tenant_id,
        user_id: subject.0.user_id,
        role_ids: &subject.0.role_ids,
    };
    let response = state
        .ecommerce
        .execute(caller, &forward)
        .await
        .map_err(problem)?;

    respond(response)
}

/// Read through to ecommerce-connectors: `GET /api/v1/tenant/ecommerce-imports/stores/{store_id}/{tail}`
/// becomes `GET /v1/{tail}`.
#[utoipa::path(
    get,
    path = "/api/v1/tenant/ecommerce-imports/stores/{store_id}/{tail}",
    tag = "product_access",
    operation_id = "proxy_import_get",
    params(
        ("store_id" = String, Path, description = "Destination ecommerce store id (canonical positive decimal)"),
        ("tail" = String, Path, description = "Path below ecommerce-connectors' /v1/"),
    ),
    responses(
        (status = 200, description = "Whatever ecommerce-connectors answered (any status it returns passes through)"),
        (status = 400, description = "Malformed path or oversized body", body = sanvi_http::ProblemDetail),
        (status = 403, description = "Tenant lacks products.ecommerce.enabled, or the user lacks the ecommerce.import.* permission", body = sanvi_http::ProblemDetail),
        (status = 502, description = "ecommerce-connectors rejected the platform's credentials or answered unusably", body = sanvi_http::ProblemDetail),
        (status = 503, description = "ecommerce-connectors is not configured or unreachable", body = sanvi_http::ProblemDetail)
    )
)]
pub async fn proxy_import_get(
    state: web::Data<ProductAccessState>,
    tenant: CurrentTenant,
    subject: CurrentSubject,
    _permission: RequiredPermission,
    request: HttpRequest,
    path: web::Path<(String, String)>,
    body: web::Bytes,
) -> Result<HttpResponse, ApiError> {
    let (store_id, tail) = path.into_inner();
    proxy_import(state, tenant, subject, request, store_id, tail, body).await
}

/// Write through to ecommerce-connectors: `POST /api/v1/tenant/ecommerce-imports/stores/{store_id}/{tail}`
/// becomes `POST /v1/{tail}`.
#[utoipa::path(
    post,
    path = "/api/v1/tenant/ecommerce-imports/stores/{store_id}/{tail}",
    tag = "product_access",
    operation_id = "proxy_import_post",
    params(
        ("store_id" = String, Path, description = "Destination ecommerce store id (canonical positive decimal)"),
        ("tail" = String, Path, description = "Path below ecommerce-connectors' /v1/"),
    ),
    responses(
        (status = 200, description = "Whatever ecommerce-connectors answered (any status it returns passes through)"),
        (status = 400, description = "Malformed path or oversized body", body = sanvi_http::ProblemDetail),
        (status = 403, description = "Tenant lacks products.ecommerce.enabled, or the user lacks the ecommerce.import.* permission", body = sanvi_http::ProblemDetail),
        (status = 502, description = "ecommerce-connectors rejected the platform's credentials or answered unusably", body = sanvi_http::ProblemDetail),
        (status = 503, description = "ecommerce-connectors is not configured or unreachable", body = sanvi_http::ProblemDetail)
    )
)]
pub async fn proxy_import_post(
    state: web::Data<ProductAccessState>,
    tenant: CurrentTenant,
    subject: CurrentSubject,
    _permission: RequiredPermission,
    request: HttpRequest,
    path: web::Path<(String, String)>,
    body: web::Bytes,
) -> Result<HttpResponse, ApiError> {
    let (store_id, tail) = path.into_inner();
    proxy_import(state, tenant, subject, request, store_id, tail, body).await
}

/// Delete through to ecommerce-connectors: `DELETE /api/v1/tenant/ecommerce-imports/stores/{store_id}/{tail}`
/// becomes `DELETE /v1/{tail}`.
#[utoipa::path(
    delete,
    path = "/api/v1/tenant/ecommerce-imports/stores/{store_id}/{tail}",
    tag = "product_access",
    operation_id = "proxy_import_delete",
    params(
        ("store_id" = String, Path, description = "Destination ecommerce store id (canonical positive decimal)"),
        ("tail" = String, Path, description = "Path below ecommerce-connectors' /v1/"),
    ),
    responses(
        (status = 200, description = "Whatever ecommerce-connectors answered (any status it returns passes through)"),
        (status = 400, description = "Malformed path or oversized body", body = sanvi_http::ProblemDetail),
        (status = 403, description = "Tenant lacks products.ecommerce.enabled, or the user lacks the ecommerce.import.* permission", body = sanvi_http::ProblemDetail),
        (status = 502, description = "ecommerce-connectors rejected the platform's credentials or answered unusably", body = sanvi_http::ProblemDetail),
        (status = 503, description = "ecommerce-connectors is not configured or unreachable", body = sanvi_http::ProblemDetail)
    )
)]
pub async fn proxy_import_delete(
    state: web::Data<ProductAccessState>,
    tenant: CurrentTenant,
    subject: CurrentSubject,
    _permission: RequiredPermission,
    request: HttpRequest,
    path: web::Path<(String, String)>,
    body: web::Bytes,
) -> Result<HttpResponse, ApiError> {
    let (store_id, tail) = path.into_inner();
    proxy_import(state, tenant, subject, request, store_id, tail, body).await
}

/// OAuth connection callback through to ecommerce-connectors: `GET /api/v1/tenant/ecommerce-imports/stores/{store_id}/connections/{provider}/callback`
/// becomes `GET /v1/connections/{provider}/callback`.
#[utoipa::path(
    get,
    path = "/api/v1/tenant/ecommerce-imports/stores/{store_id}/connections/{provider}/callback",
    tag = "product_access",
    operation_id = "proxy_import_callback",
    params(
        ("store_id" = String, Path, description = "Destination ecommerce store id (canonical positive decimal)"),
        ("provider" = String, Path, description = "Connection provider (e.g. shopify or amazon)"),
    ),
    responses(
        (status = 200, description = "Whatever ecommerce-connectors answered (any status it returns passes through)"),
        (status = 400, description = "Malformed path or oversized body", body = sanvi_http::ProblemDetail),
        (status = 403, description = "Tenant lacks products.ecommerce.enabled, or the user lacks the ecommerce.import.* permission", body = sanvi_http::ProblemDetail),
        (status = 502, description = "ecommerce-connectors rejected the platform's credentials or answered unusably", body = sanvi_http::ProblemDetail),
        (status = 503, description = "ecommerce-connectors is not configured or unreachable", body = sanvi_http::ProblemDetail)
    )
)]
pub async fn proxy_import_callback(
    state: web::Data<ProductAccessState>,
    tenant: CurrentTenant,
    subject: CurrentSubject,
    _permission: RequiredPermission,
    request: HttpRequest,
    path: web::Path<(String, String)>,
    body: web::Bytes,
) -> Result<HttpResponse, ApiError> {
    let (store_id, provider) = path.into_inner();
    let tail = format!("connections/{provider}/callback");
    proxy_import(state, tenant, subject, request, store_id, tail, body).await
}

async fn proxy_import(
    state: web::Data<ProductAccessState>,
    tenant: CurrentTenant,
    subject: CurrentSubject,
    request: HttpRequest,
    store_id: String,
    tail: String,
    body: web::Bytes,
) -> Result<HttpResponse, ApiError> {
    let store = DestinationStoreId::parse(&store_id).map_err(problem)?;
    let path = ProductPath::parse(&tail).map_err(problem)?;
    let headers = request.headers().iter().filter_map(|(name, value)| {
        value
            .to_str()
            .ok()
            .map(|value| (name.as_str().to_owned(), value.to_owned()))
    });
    let request_id = request
        .extensions()
        .get::<RequestId>()
        .map(ToString::to_string);
    let forward = ForwardRequest::new(
        request.method().as_str(),
        path,
        Some(request.query_string()),
        headers,
        body.to_vec(),
        request_id,
    )
    .map_err(problem)?;

    let caller = Caller {
        tenant_id: tenant.0.tenant_id,
        user_id: subject.0.user_id,
        role_ids: &subject.0.role_ids,
    };
    let response = state
        .imports
        .execute(caller, store, &forward)
        .await
        .map_err(problem)?;

    respond(response)
}

fn respond(response: ForwardResponse) -> Result<HttpResponse, ApiError> {
    let status = StatusCode::from_u16(response.status).map_err(|_| ApiError::BadGateway {
        detail: "the product service answered unexpectedly".to_owned(),
    })?;
    let mut builder = HttpResponse::build(status);
    for (name, value) in response.headers {
        builder.insert_header((name, value));
    }
    Ok(builder.body(response.body))
}
