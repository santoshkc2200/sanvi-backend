//! Platform gateway handlers. Embedded state is an explicit regression fixture.
use super::{handlers::*, state::AccountingState};
use crate::{
    adapters::remote::{GatewayState, proxy},
    domain::{BookId, ConnectionId},
};
use actix_web::{FromRequest, HttpRequest, HttpResponse, web};
use sanvi_http::{
    ApiError, CurrentSubject, CurrentTenant, FreshAal2Required, ProblemDetail, RequiredAnyFeature,
    RequiredPermission,
};

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
    state: Option<web::Data<AccountingState>>,
    tenant: CurrentTenant,
    subject: CurrentSubject,
    _permission: RequiredPermission,
    _entitlement: RequiredAnyFeature,
    req: HttpRequest,
    payload: web::Bytes,
) -> Result<HttpResponse, ApiError> {
    let Some(state) = state else {
        let gateway = req
            .app_data::<web::Data<GatewayState>>()
            .cloned()
            .ok_or_else(|| ApiError::service_unavailable("Accounting service is not configured"))?;
        return proxy(
            gateway,
            tenant,
            subject,
            _permission,
            _entitlement,
            req,
            payload,
        )
        .await;
    };
    list_accounting_books_embedded(state, tenant, subject, _permission, _entitlement).await
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
    state: Option<web::Data<AccountingState>>,
    tenant: CurrentTenant,
    subject: CurrentSubject,
    req: HttpRequest,
    _permission: RequiredPermission,
    _entitlement: RequiredAnyFeature,
    payload: web::Bytes,
) -> Result<HttpResponse, ApiError> {
    let Some(state) = state else {
        let gateway = req
            .app_data::<web::Data<GatewayState>>()
            .cloned()
            .ok_or_else(|| ApiError::service_unavailable("Accounting service is not configured"))?;
        return proxy(
            gateway,
            tenant,
            subject,
            _permission,
            _entitlement,
            req,
            payload,
        )
        .await;
    };
    let mut raw: actix_web::dev::Payload = payload.into();
    let body = web::Json::<CreateBookRequest>::from_request(&req, &mut raw)
        .await
        .map_err(|error| ApiError::BadRequest {
            detail: error.to_string(),
        })?;
    create_accounting_book_embedded(state, tenant, subject, req, body, _permission, _entitlement)
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
    state: Option<web::Data<AccountingState>>,
    tenant: CurrentTenant,
    subject: CurrentSubject,
    path: web::Path<BookId>,
    _permission: RequiredPermission,
    _entitlement: RequiredAnyFeature,
    req: HttpRequest,
    payload: web::Bytes,
) -> Result<HttpResponse, ApiError> {
    let Some(state) = state else {
        let gateway = req
            .app_data::<web::Data<GatewayState>>()
            .cloned()
            .ok_or_else(|| ApiError::service_unavailable("Accounting service is not configured"))?;
        return proxy(
            gateway,
            tenant,
            subject,
            _permission,
            _entitlement,
            req,
            payload,
        )
        .await;
    };
    get_accounting_book_embedded(state, tenant, subject, path, _permission, _entitlement).await
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
    state: Option<web::Data<AccountingState>>,
    tenant: CurrentTenant,
    subject: CurrentSubject,
    path: web::Path<BookId>,
    req: HttpRequest,
    _permission: RequiredPermission,
    _entitlement: RequiredAnyFeature,
    payload: web::Bytes,
) -> Result<HttpResponse, ApiError> {
    let Some(state) = state else {
        let gateway = req
            .app_data::<web::Data<GatewayState>>()
            .cloned()
            .ok_or_else(|| ApiError::service_unavailable("Accounting service is not configured"))?;
        return proxy(
            gateway,
            tenant,
            subject,
            _permission,
            _entitlement,
            req,
            payload,
        )
        .await;
    };
    let mut raw: actix_web::dev::Payload = payload.into();
    let body = web::Json::<RegisterConnectionRequest>::from_request(&req, &mut raw)
        .await
        .map_err(|error| ApiError::BadRequest {
            detail: error.to_string(),
        })?;
    register_accounting_connection_embedded(
        state,
        tenant,
        subject,
        path,
        req,
        body,
        _permission,
        _entitlement,
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
    state: Option<web::Data<AccountingState>>,
    tenant: CurrentTenant,
    subject: CurrentSubject,
    path: web::Path<(BookId, ConnectionId)>,
    req: HttpRequest,
    _fresh_mfa: FreshAal2Required,
    _permission: RequiredPermission,
    _entitlement: RequiredAnyFeature,
    payload: web::Bytes,
) -> Result<HttpResponse, ApiError> {
    let Some(state) = state else {
        let gateway = req
            .app_data::<web::Data<GatewayState>>()
            .cloned()
            .ok_or_else(|| ApiError::service_unavailable("Accounting service is not configured"))?;
        return proxy(
            gateway,
            tenant,
            subject,
            _permission,
            _entitlement,
            req,
            payload,
        )
        .await;
    };
    start_accounting_authorization_embedded(
        state,
        tenant,
        subject,
        path,
        req,
        _fresh_mfa,
        _permission,
        _entitlement,
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
    state: Option<web::Data<AccountingState>>,
    tenant: CurrentTenant,
    subject: CurrentSubject,
    path: web::Path<(BookId, ConnectionId)>,
    req: HttpRequest,
    _fresh_mfa: FreshAal2Required,
    _permission: RequiredPermission,
    _entitlement: RequiredAnyFeature,
    payload: web::Bytes,
) -> Result<HttpResponse, ApiError> {
    let Some(state) = state else {
        let gateway = req
            .app_data::<web::Data<GatewayState>>()
            .cloned()
            .ok_or_else(|| ApiError::service_unavailable("Accounting service is not configured"))?;
        return proxy(
            gateway,
            tenant,
            subject,
            _permission,
            _entitlement,
            req,
            payload,
        )
        .await;
    };
    revoke_accounting_authorization_embedded(
        state,
        tenant,
        subject,
        path,
        req,
        _fresh_mfa,
        _permission,
        _entitlement,
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
    state: Option<web::Data<AccountingState>>,
    tenant: CurrentTenant,
    subject: CurrentSubject,
    req: HttpRequest,
    _permission: RequiredPermission,
    _entitlement: RequiredAnyFeature,
    payload: web::Bytes,
) -> Result<HttpResponse, ApiError> {
    let Some(state) = state else {
        let gateway = req
            .app_data::<web::Data<GatewayState>>()
            .cloned()
            .ok_or_else(|| ApiError::service_unavailable("Accounting service is not configured"))?;
        return proxy(
            gateway,
            tenant,
            subject,
            _permission,
            _entitlement,
            req,
            payload,
        )
        .await;
    };
    let mut raw: actix_web::dev::Payload = payload.into();
    let body = web::Json::<CompleteAuthorizationRequest>::from_request(&req, &mut raw)
        .await
        .map_err(|error| ApiError::BadRequest {
            detail: error.to_string(),
        })?;
    complete_accounting_authorization_embedded(
        state,
        tenant,
        subject,
        req,
        body,
        _permission,
        _entitlement,
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
// One argument per extractor: each guard runs before the body.
#[allow(clippy::too_many_arguments)]
pub async fn activate_accounting_book(
    state: Option<web::Data<AccountingState>>,
    tenant: CurrentTenant,
    subject: CurrentSubject,
    path: web::Path<BookId>,
    req: HttpRequest,
    _fresh_mfa: FreshAal2Required,
    _permission: RequiredPermission,
    _entitlement: RequiredAnyFeature,
    payload: web::Bytes,
) -> Result<HttpResponse, ApiError> {
    let Some(state) = state else {
        let gateway = req
            .app_data::<web::Data<GatewayState>>()
            .cloned()
            .ok_or_else(|| ApiError::service_unavailable("Accounting service is not configured"))?;
        return proxy(
            gateway,
            tenant,
            subject,
            _permission,
            _entitlement,
            req,
            payload,
        )
        .await;
    };
    let mut raw: actix_web::dev::Payload = payload.into();
    let body = web::Json::<ActivateBookRequest>::from_request(&req, &mut raw)
        .await
        .map_err(|error| ApiError::BadRequest {
            detail: error.to_string(),
        })?;
    activate_accounting_book_embedded(
        state,
        tenant,
        subject,
        path,
        req,
        body,
        _fresh_mfa,
        _permission,
        _entitlement,
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
// One argument per extractor: each guard runs before the body.
#[allow(clippy::too_many_arguments)]
pub async fn schedule_accounting_switch(
    state: Option<web::Data<AccountingState>>,
    tenant: CurrentTenant,
    subject: CurrentSubject,
    path: web::Path<BookId>,
    req: HttpRequest,
    _fresh_mfa: FreshAal2Required,
    _permission: RequiredPermission,
    _entitlement: RequiredAnyFeature,
    payload: web::Bytes,
) -> Result<HttpResponse, ApiError> {
    let Some(state) = state else {
        let gateway = req
            .app_data::<web::Data<GatewayState>>()
            .cloned()
            .ok_or_else(|| ApiError::service_unavailable("Accounting service is not configured"))?;
        return proxy(
            gateway,
            tenant,
            subject,
            _permission,
            _entitlement,
            req,
            payload,
        )
        .await;
    };
    let mut raw: actix_web::dev::Payload = payload.into();
    let body = web::Json::<ScheduleSwitchRequest>::from_request(&req, &mut raw)
        .await
        .map_err(|error| ApiError::BadRequest {
            detail: error.to_string(),
        })?;
    schedule_accounting_switch_embedded(
        state,
        tenant,
        subject,
        path,
        req,
        body,
        _fresh_mfa,
        _permission,
        _entitlement,
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
    state: Option<web::Data<AccountingState>>,
    tenant: CurrentTenant,
    subject: CurrentSubject,
    path: web::Path<BookId>,
    req: HttpRequest,
    _permission: RequiredPermission,
    _entitlement: RequiredAnyFeature,
    payload: web::Bytes,
) -> Result<HttpResponse, ApiError> {
    let Some(state) = state else {
        let gateway = req
            .app_data::<web::Data<GatewayState>>()
            .cloned()
            .ok_or_else(|| ApiError::service_unavailable("Accounting service is not configured"))?;
        return proxy(
            gateway,
            tenant,
            subject,
            _permission,
            _entitlement,
            req,
            payload,
        )
        .await;
    };
    cancel_accounting_switch_embedded(state, tenant, subject, path, req, _permission, _entitlement)
        .await
}
