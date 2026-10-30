//! Platform gateway handlers for the Accounting bounded context.
//! The platform gateway forwards /api/v1/tenant/accounting/* to the standalone
//! Accounting service under a request-bound HMAC delegation. The contract types
//! in [`super::contract`] exist for OpenAPI schema publication; bodies are relayed
//! byte for byte.

use super::contract::{
    ActivateBookRequest, BookView, CompleteAuthorizationRequest, CreateBookRequest,
    RegisterConnectionRequest, ScheduleSwitchRequest, StartAuthorizationResponse,
};
use crate::{
    adapters::remote::{GatewayState, proxy},
    domain::{BookId, ConnectionId},
};
use actix_web::{HttpRequest, HttpResponse, web};
use sanvi_http::{
    ApiError, CurrentSubject, CurrentTenant, FreshAal2Required, ProblemDetail, RequiredAnyFeature,
    RequiredPermission,
};

/// The gateway the composition root registers; without it the service is
/// not configured and every Accounting route answers 503.
fn gateway(req: &HttpRequest) -> Result<web::Data<GatewayState>, ApiError> {
    req.app_data::<web::Data<GatewayState>>()
        .cloned()
        .ok_or_else(|| ApiError::service_unavailable("Accounting service is not configured"))
}

#[utoipa::path(
    get,
    path = "/api/v1/tenant/accounting/books",
    tag = "accounting",
    operation_id = "list_accounting_books",
    responses(
        (status = 200, description = "Every accounting book in the tenant, ordered by ID", body = Vec<BookView>),
        (status = 403, description = "Missing tenant.accounting.read permission or accounting.freee entitlement", body = ProblemDetail),
        (status = 502, description = "Accounting service rejected platform credentials or returned an invalid response", body = ProblemDetail),
        (status = 503, description = "Accounting service or provider temporarily unavailable", body = ProblemDetail)
    )
)]
pub async fn list_accounting_books(
    tenant: CurrentTenant,
    subject: CurrentSubject,
    _permission: RequiredPermission,
    _entitlement: RequiredAnyFeature,
    req: HttpRequest,
    payload: web::Bytes,
) -> Result<HttpResponse, ApiError> {
    proxy(
        gateway(&req)?,
        tenant,
        subject,
        _permission,
        _entitlement,
        req,
        payload,
    )
    .await
}

#[utoipa::path(
    post,
    path = "/api/v1/tenant/accounting/books",
    tag = "accounting",
    operation_id = "create_accounting_book",
    request_body = CreateBookRequest,
    responses(
        (status = 201, description = "Book created", body = BookView, headers(("ETag" = String, description = "Entity tag carrying the book's initial revision"))),
        (status = 400, description = "Invalid book parameters or unknown body fields", body = ProblemDetail),
        (status = 403, description = "Missing tenant.accounting.manage permission or accounting.freee entitlement", body = ProblemDetail),
        (status = 409, description = "Legal entity key already exists for this tenant", body = ProblemDetail),
        (status = 502, description = "Accounting service rejected platform credentials or returned an invalid response", body = ProblemDetail),
        (status = 503, description = "Accounting service or provider temporarily unavailable", body = ProblemDetail)
    )
)]
pub async fn create_accounting_book(
    tenant: CurrentTenant,
    subject: CurrentSubject,
    req: HttpRequest,
    _permission: RequiredPermission,
    _entitlement: RequiredAnyFeature,
    payload: web::Bytes,
) -> Result<HttpResponse, ApiError> {
    proxy(
        gateway(&req)?,
        tenant,
        subject,
        _permission,
        _entitlement,
        req,
        payload,
    )
    .await
}

#[utoipa::path(
    get,
    path = "/api/v1/tenant/accounting/books/{book_id}",
    tag = "accounting",
    operation_id = "get_accounting_book",
    params(("book_id" = String, Path, description = "Decimal Snowflake book ID")),
    responses(
        (status = 200, description = "Book details", body = BookView, headers(("ETag" = String, description = "Entity tag carrying the book's current revision"))),
        (status = 403, description = "Missing tenant.accounting.read permission or accounting.freee entitlement", body = ProblemDetail),
        (status = 404, description = "Book not found", body = ProblemDetail),
        (status = 502, description = "Accounting service rejected platform credentials or returned an invalid response", body = ProblemDetail),
        (status = 503, description = "Accounting service or provider temporarily unavailable", body = ProblemDetail)
    )
)]
pub async fn get_accounting_book(
    tenant: CurrentTenant,
    subject: CurrentSubject,
    path: web::Path<BookId>,
    _permission: RequiredPermission,
    _entitlement: RequiredAnyFeature,
    req: HttpRequest,
    payload: web::Bytes,
) -> Result<HttpResponse, ApiError> {
    let _ = path;
    proxy(
        gateway(&req)?,
        tenant,
        subject,
        _permission,
        _entitlement,
        req,
        payload,
    )
    .await
}

#[utoipa::path(
    post,
    path = "/api/v1/tenant/accounting/books/{book_id}/connections",
    tag = "accounting",
    operation_id = "register_accounting_connection",
    params(
        ("book_id" = String, Path, description = "Decimal Snowflake book ID"),
        ("If-Match" = String, Header, description = "Required. The book's current revision entity tag (e.g. \"1\").")
    ),
    request_body = RegisterConnectionRequest,
    responses(
        (status = 200, description = "Connection registered; book returned with updated revision", body = BookView, headers(("ETag" = String, description = "Updated revision entity tag"))),
        (status = 400, description = "Missing or malformed If-Match header, or invalid connection payload", body = ProblemDetail),
        (status = 403, description = "Missing tenant.accounting.manage permission or accounting.freee entitlement", body = ProblemDetail),
        (status = 404, description = "Book not found", body = ProblemDetail),
        (status = 409, description = "Stale If-Match revision, or conflicting provider connection", body = ProblemDetail),
        (status = 502, description = "Accounting service rejected platform credentials or returned an invalid response", body = ProblemDetail),
        (status = 503, description = "Accounting service or provider temporarily unavailable", body = ProblemDetail)
    )
)]
pub async fn register_accounting_connection(
    tenant: CurrentTenant,
    subject: CurrentSubject,
    path: web::Path<BookId>,
    req: HttpRequest,
    _permission: RequiredPermission,
    _entitlement: RequiredAnyFeature,
    payload: web::Bytes,
) -> Result<HttpResponse, ApiError> {
    let _ = path;
    proxy(
        gateway(&req)?,
        tenant,
        subject,
        _permission,
        _entitlement,
        req,
        payload,
    )
    .await
}

#[utoipa::path(
    post,
    path = "/api/v1/tenant/accounting/books/{book_id}/connections/{connection_id}/authorization",
    tag = "accounting",
    operation_id = "start_accounting_authorization",
    params(
        ("book_id" = String, Path, description = "Decimal Snowflake book ID"),
        ("connection_id" = String, Path, description = "Decimal Snowflake connection ID")
    ),
    responses(
        (status = 200, description = "Provider authorization URL issued", body = StartAuthorizationResponse),
        (status = 403, description = "Missing tenant.accounting.manage permission, missing accounting.freee entitlement, or fresh MFA missing", body = ProblemDetail),
        (status = 404, description = "Book or connection not found", body = ProblemDetail),
        (status = 503, description = "freee is not configured", body = ProblemDetail),
        (status = 502, description = "Accounting service rejected platform credentials or returned an invalid response", body = ProblemDetail)
    )
)]
#[allow(clippy::too_many_arguments)]
pub async fn start_accounting_authorization(
    tenant: CurrentTenant,
    subject: CurrentSubject,
    path: web::Path<(BookId, ConnectionId)>,
    req: HttpRequest,
    _fresh_mfa: FreshAal2Required,
    _permission: RequiredPermission,
    _entitlement: RequiredAnyFeature,
    payload: web::Bytes,
) -> Result<HttpResponse, ApiError> {
    let _ = path;
    proxy(
        gateway(&req)?,
        tenant,
        subject,
        _permission,
        _entitlement,
        req,
        payload,
    )
    .await
}

#[utoipa::path(
    delete,
    path = "/api/v1/tenant/accounting/books/{book_id}/connections/{connection_id}/authorization",
    tag = "accounting",
    operation_id = "revoke_accounting_authorization",
    params(
        ("book_id" = String, Path, description = "Decimal Snowflake book ID"),
        ("connection_id" = String, Path, description = "Decimal Snowflake connection ID")
    ),
    responses(
        (status = 204, description = "Provider authorization revoked locally"),
        (status = 403, description = "Missing tenant.accounting.manage permission, missing accounting.freee entitlement, or fresh MFA missing", body = ProblemDetail),
        (status = 404, description = "Book or connection not found", body = ProblemDetail),
        (status = 409, description = "The connection is an active, scheduled or draining destination", body = ProblemDetail),
        (status = 503, description = "freee is not configured", body = ProblemDetail),
        (status = 502, description = "Accounting service rejected platform credentials or returned an invalid response", body = ProblemDetail)
    )
)]
#[allow(clippy::too_many_arguments)]
pub async fn revoke_accounting_authorization(
    tenant: CurrentTenant,
    subject: CurrentSubject,
    path: web::Path<(BookId, ConnectionId)>,
    req: HttpRequest,
    _fresh_mfa: FreshAal2Required,
    _permission: RequiredPermission,
    _entitlement: RequiredAnyFeature,
    payload: web::Bytes,
) -> Result<HttpResponse, ApiError> {
    let _ = path;
    proxy(
        gateway(&req)?,
        tenant,
        subject,
        _permission,
        _entitlement,
        req,
        payload,
    )
    .await
}

#[utoipa::path(
    post,
    path = "/api/v1/tenant/accounting/authorization/callback",
    tag = "accounting",
    operation_id = "complete_accounting_authorization",
    request_body = CompleteAuthorizationRequest,
    responses(
        (status = 200, description = "Authorization completed; updated book returned", body = BookView, headers(("ETag" = String, description = "Entity tag carrying the book's current revision"))),
        (status = 400, description = "Authorization rejected (state invalid, expired, already consumed, or organization mismatch)", body = ProblemDetail),
        (status = 403, description = "Missing tenant.accounting.manage permission or accounting.freee entitlement", body = ProblemDetail),
        (status = 503, description = "freee is not configured", body = ProblemDetail),
        (status = 502, description = "Accounting service rejected platform credentials or returned an invalid response", body = ProblemDetail)
    )
)]
pub async fn complete_accounting_authorization(
    tenant: CurrentTenant,
    subject: CurrentSubject,
    req: HttpRequest,
    _permission: RequiredPermission,
    _entitlement: RequiredAnyFeature,
    payload: web::Bytes,
) -> Result<HttpResponse, ApiError> {
    proxy(
        gateway(&req)?,
        tenant,
        subject,
        _permission,
        _entitlement,
        req,
        payload,
    )
    .await
}

#[utoipa::path(
    post,
    path = "/api/v1/tenant/accounting/books/{book_id}/activation",
    tag = "accounting",
    operation_id = "activate_accounting_book",
    params(
        ("book_id" = String, Path, description = "Decimal Snowflake book ID"),
        ("If-Match" = String, Header, description = "Required. The book's current revision entity tag (e.g. \"1\").")
    ),
    request_body = ActivateBookRequest,
    responses(
        (status = 200, description = "Book destination activated; updated book returned", body = BookView, headers(("ETag" = String, description = "Updated revision entity tag"))),
        (status = 400, description = "Missing or malformed If-Match header, or invalid start time", body = ProblemDetail),
        (status = 403, description = "Missing tenant.accounting.manage permission, missing accounting.freee entitlement, or fresh MFA missing", body = ProblemDetail),
        (status = 404, description = "Book or connection not found", body = ProblemDetail),
        (status = 409, description = "Stale If-Match revision, or book already activated", body = ProblemDetail),
        (status = 422, description = "provider verification failed", body = ProblemDetail),
        (status = 503, description = "freee is not configured", body = ProblemDetail),
        (status = 502, description = "Accounting service rejected platform credentials or returned an invalid response", body = ProblemDetail)
    )
)]
#[allow(clippy::too_many_arguments)]
pub async fn activate_accounting_book(
    tenant: CurrentTenant,
    subject: CurrentSubject,
    path: web::Path<BookId>,
    req: HttpRequest,
    _fresh_mfa: FreshAal2Required,
    _permission: RequiredPermission,
    _entitlement: RequiredAnyFeature,
    payload: web::Bytes,
) -> Result<HttpResponse, ApiError> {
    let _ = path;
    proxy(
        gateway(&req)?,
        tenant,
        subject,
        _permission,
        _entitlement,
        req,
        payload,
    )
    .await
}

#[utoipa::path(
    put,
    path = "/api/v1/tenant/accounting/books/{book_id}/switch",
    tag = "accounting",
    operation_id = "schedule_accounting_switch",
    params(
        ("book_id" = String, Path, description = "Decimal Snowflake book ID"),
        ("If-Match" = String, Header, description = "Required. The book's current revision entity tag (e.g. \"1\").")
    ),
    request_body = ScheduleSwitchRequest,
    responses(
        (status = 200, description = "Switch scheduled; updated book returned", body = BookView, headers(("ETag" = String, description = "Updated revision entity tag"))),
        (status = 400, description = "Missing or malformed If-Match header, or invalid cutover time", body = ProblemDetail),
        (status = 403, description = "Missing tenant.accounting.manage permission, missing accounting.freee entitlement, or fresh MFA missing", body = ProblemDetail),
        (status = 404, description = "Book or connection not found", body = ProblemDetail),
        (status = 409, description = "Stale If-Match revision, or switch already pending", body = ProblemDetail),
        (status = 422, description = "provider verification failed", body = ProblemDetail),
        (status = 503, description = "freee is not configured", body = ProblemDetail),
        (status = 502, description = "Accounting service rejected platform credentials or returned an invalid response", body = ProblemDetail)
    )
)]
#[allow(clippy::too_many_arguments)]
pub async fn schedule_accounting_switch(
    tenant: CurrentTenant,
    subject: CurrentSubject,
    path: web::Path<BookId>,
    req: HttpRequest,
    _fresh_mfa: FreshAal2Required,
    _permission: RequiredPermission,
    _entitlement: RequiredAnyFeature,
    payload: web::Bytes,
) -> Result<HttpResponse, ApiError> {
    let _ = path;
    proxy(
        gateway(&req)?,
        tenant,
        subject,
        _permission,
        _entitlement,
        req,
        payload,
    )
    .await
}

#[utoipa::path(
    delete,
    path = "/api/v1/tenant/accounting/books/{book_id}/switch",
    tag = "accounting",
    operation_id = "cancel_accounting_switch",
    params(
        ("book_id" = String, Path, description = "Decimal Snowflake book ID"),
        ("If-Match" = String, Header, description = "Required. The book's current revision entity tag (e.g. \"1\").")
    ),
    responses(
        (status = 200, description = "Scheduled switch canceled; updated book returned", body = BookView, headers(("ETag" = String, description = "Updated revision entity tag"))),
        (status = 400, description = "Missing or malformed If-Match header", body = ProblemDetail),
        (status = 403, description = "Missing tenant.accounting.manage permission or accounting.freee entitlement", body = ProblemDetail),
        (status = 404, description = "Book not found", body = ProblemDetail),
        (status = 409, description = "Stale If-Match revision, or no switch is scheduled", body = ProblemDetail),
        (status = 502, description = "Accounting service rejected platform credentials or returned an invalid response", body = ProblemDetail),
        (status = 503, description = "Accounting service or provider temporarily unavailable", body = ProblemDetail)
    )
)]
pub async fn cancel_accounting_switch(
    tenant: CurrentTenant,
    subject: CurrentSubject,
    path: web::Path<BookId>,
    req: HttpRequest,
    _permission: RequiredPermission,
    _entitlement: RequiredAnyFeature,
    payload: web::Bytes,
) -> Result<HttpResponse, ApiError> {
    let _ = path;
    proxy(
        gateway(&req)?,
        tenant,
        subject,
        _permission,
        _entitlement,
        req,
        payload,
    )
    .await
}
