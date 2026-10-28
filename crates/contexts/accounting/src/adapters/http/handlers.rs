use std::collections::HashMap;

use actix_web::{
    HttpMessage, HttpRequest, HttpResponse, HttpResponseBuilder,
    http::{StatusCode, header::ETAG},
    web,
};
use chrono::{DateTime, Utc};
use sanvi_http::{
    ApiError, CurrentSubject, CurrentTenant, FreshAal2Required, RequiredAnyFeature,
    RequiredPermission,
};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::state::{AccountingState, Consent};
use crate::{
    application::{Actor, BookSummary, RegisterConnectionCommand, RequestContext},
    domain::{
        AccountMappings, AccountingError, BookId, ConnectionId, CredentialStatus,
        DestinationPeriod, LATE_EVENT_GRACE, Provider,
    },
};

/// RFC-9457 problem type slugs emitted by accounting endpoints.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub enum AccountingProblemType {
    #[serde(rename = "accounting/authorization-rejected")]
    AuthorizationRejected,
    #[serde(rename = "accounting/reauthorization-required")]
    ReauthorizationRequired,
    #[serde(rename = "accounting/verification-failed")]
    VerificationFailed,
    #[serde(rename = "accounting/provider-unavailable")]
    ProviderUnavailable,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateBookRequest {
    pub legal_entity_key: String,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RegisterConnectionRequest {
    pub provider: Provider,
    pub organization_id: String,
    pub mappings: AccountMappings,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CompleteAuthorizationRequest {
    pub state: String,
    pub code: String,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ActivateBookRequest {
    #[schema(value_type = String)]
    pub connection_id: ConnectionId,
    pub starts_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ScheduleSwitchRequest {
    #[schema(value_type = String)]
    pub connection_id: ConnectionId,
    pub cutover_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct BookView {
    #[schema(value_type = String)]
    pub id: BookId,
    pub legal_entity_key: String,
    pub revision: i64,
    pub connections: Vec<ConnectionView>,
    pub periods: Vec<PeriodView>,
    pub scheduled_switch: Option<PeriodView>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
#[schema(as = AccountingConnectionView)]
pub struct ConnectionView {
    #[schema(value_type = String)]
    pub id: ConnectionId,
    pub provider: Provider,
    pub organization_id: String,
    pub mappings: AccountMappings,
    pub credential_status: CredentialStatusView,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum CredentialStatusView {
    None,
    Active,
    ReauthorizationRequired,
    Revoked,
}

impl CredentialStatusView {
    pub fn from_optional(status: Option<CredentialStatus>) -> Self {
        match status {
            None => Self::None,
            Some(CredentialStatus::Active) => Self::Active,
            Some(CredentialStatus::ReauthorizationRequired) => Self::ReauthorizationRequired,
            Some(CredentialStatus::Revoked) => Self::Revoked,
        }
    }
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct PeriodView {
    #[schema(value_type = String)]
    pub connection_id: ConnectionId,
    pub starts_at: DateTime<Utc>,
    pub ends_at: Option<DateTime<Utc>>,
    pub state: PeriodStateView,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum PeriodStateView {
    Active,
    Scheduled,
    Draining,
    Closed,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct StartAuthorizationResponse {
    pub authorization_url: String,
}

pub fn api_error(error: AccountingError) -> ApiError {
    match error {
        AccountingError::Invalid(detail) => ApiError::BadRequest {
            detail: detail.to_string(),
        },
        AccountingError::NotFound => ApiError::NotFound {
            resource: "accounting book or connection".to_string(),
        },
        AccountingError::Forbidden => ApiError::Forbidden {
            detail: "forbidden".to_string(),
        },
        AccountingError::StaleRevision => ApiError::Conflict {
            detail: "the book revision is stale; re-read and retry".to_string(),
        },
        AccountingError::Conflict(detail) => ApiError::Conflict {
            detail: detail.to_string(),
        },
        AccountingError::IdempotencyConflict => ApiError::Conflict {
            detail: "source event was already assigned with different content".to_string(),
        },
        AccountingError::ReauthorizationRequired => ApiError::problem_type(
            StatusCode::CONFLICT,
            "accounting/reauthorization-required",
            "Reauthorization Required",
            "provider authorization must be granted again",
        ),
        AccountingError::AuthorizationRejected(detail) => ApiError::problem_type(
            StatusCode::BAD_REQUEST,
            "accounting/authorization-rejected",
            "Authorization Rejected",
            detail,
        ),
        AccountingError::VerificationFailed(detail) => ApiError::problem_type(
            StatusCode::UNPROCESSABLE_ENTITY,
            "accounting/verification-failed",
            "Verification Failed",
            detail,
        ),
        AccountingError::HistoricalImport => ApiError::problem_type(
            StatusCode::UNPROCESSABLE_ENTITY,
            "unprocessable-entity",
            "Unprocessable Entity",
            "historical imports require a separate reviewed migration",
        ),
        AccountingError::VerificationUnavailable => ApiError::problem_type(
            StatusCode::SERVICE_UNAVAILABLE,
            "accounting/provider-unavailable",
            "Provider Unavailable",
            "provider verification is unavailable",
        ),
        AccountingError::ProviderUnavailable => ApiError::problem_type(
            StatusCode::SERVICE_UNAVAILABLE,
            "accounting/provider-unavailable",
            "Provider Unavailable",
            "accounting provider is unavailable",
        ),
        AccountingError::Storage(detail) => ApiError::from_storage_detail(&detail),
    }
}

/// Audit context from the request itself, never from the body.
pub fn request_context(req: &HttpRequest) -> RequestContext {
    let request_id = req
        .extensions()
        .get::<sanvi_http::RequestId>()
        .map(ToString::to_string);
    let user_agent = req
        .headers()
        .get(actix_web::http::header::USER_AGENT)
        .and_then(|value| value.to_str().ok())
        .map(|ua| ua[..ua.floor_char_boundary(512)].to_string());
    RequestContext {
        request_id,
        ip: sanvi_http::client_ip(req),
        user_agent,
    }
}

fn user_actor(tenant: &CurrentTenant, subject: &CurrentSubject) -> Actor {
    Actor::User {
        tenant_id: tenant.0.tenant_id,
        user_id: subject.0.user_id,
    }
}

fn consent(state: &AccountingState) -> Result<&Consent, ApiError> {
    state.consent.as_ref().ok_or_else(|| {
        ApiError::problem_type(
            StatusCode::SERVICE_UNAVAILABLE,
            "accounting/provider-unavailable",
            "Provider Unavailable",
            "freee is not configured",
        )
    })
}

fn book_response(response: &mut HttpResponseBuilder, view: BookView) -> HttpResponse {
    response
        .insert_header((ETAG, format!("\"{}\"", view.revision)))
        .json(view)
}

fn if_match_revision(req: &HttpRequest) -> Result<i64, ApiError> {
    let raw = req
        .headers()
        .get("If-Match")
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| {
            ApiError::bad_request("If-Match header with the book revision is required")
        })?;
    let raw = raw.trim();
    if raw == "*" {
        return Err(ApiError::bad_request(
            "If-Match: * is not supported; send the revision as a quoted entity tag (e.g. \"1\")",
        ));
    }
    let unquoted = if raw.starts_with('"') && raw.ends_with('"') && raw.len() >= 2 {
        &raw[1..raw.len() - 1]
    } else {
        raw
    };
    unquoted
        .parse::<i64>()
        .map_err(|_| ApiError::bad_request("If-Match must be the book's integer revision"))
}

fn period_state(period: &DestinationPeriod, now: DateTime<Utc>) -> PeriodStateView {
    match period.ends_at {
        _ if period.starts_at > now => PeriodStateView::Scheduled,
        Some(ends_at) if now >= ends_at + LATE_EVENT_GRACE => PeriodStateView::Closed,
        Some(ends_at) if now >= ends_at => PeriodStateView::Draining,
        _ => PeriodStateView::Active,
    }
}

fn book_view(summary: &BookSummary, now: DateTime<Utc>) -> BookView {
    let credentials_map: HashMap<_, _> = summary.credentials.iter().copied().collect();
    let connections = summary
        .book
        .connections()
        .iter()
        .map(|conn| ConnectionView {
            id: conn.id(),
            provider: conn.provider(),
            organization_id: conn.organization_id().to_string(),
            mappings: conn.mappings().clone(),
            credential_status: CredentialStatusView::from_optional(
                credentials_map.get(&conn.id()).copied(),
            ),
        })
        .collect();

    let periods = summary
        .book
        .periods()
        .iter()
        .map(|period| PeriodView {
            connection_id: period.connection_id,
            starts_at: period.starts_at,
            ends_at: period.ends_at,
            state: period_state(period, now),
        })
        .collect();

    let scheduled_switch = summary.book.scheduled_switch(now).map(|switch| PeriodView {
        connection_id: switch.connection_id,
        starts_at: switch.starts_at,
        ends_at: switch.ends_at,
        state: PeriodStateView::Scheduled,
    });

    BookView {
        id: summary.book.id(),
        legal_entity_key: summary.book.legal_entity_key().to_string(),
        revision: summary.book.revision(),
        connections,
        periods,
        scheduled_switch,
    }
}

pub async fn list_accounting_books_embedded(
    state: web::Data<AccountingState>,
    tenant: CurrentTenant,
    subject: CurrentSubject,
    _permission: RequiredPermission,
    _entitlement: RequiredAnyFeature,
) -> Result<HttpResponse, ApiError> {
    let actor = user_actor(&tenant, &subject);
    let summaries = state.list_books.execute(actor).await.map_err(api_error)?;
    let now = state.list_books.0.clock.now();
    let views: Vec<BookView> = summaries.iter().map(|s| book_view(s, now)).collect();
    Ok(HttpResponse::Ok().json(views))
}

pub async fn create_accounting_book_embedded(
    state: web::Data<AccountingState>,
    tenant: CurrentTenant,
    subject: CurrentSubject,
    req: HttpRequest,
    body: web::Json<CreateBookRequest>,
    _permission: RequiredPermission,
    _entitlement: RequiredAnyFeature,
) -> Result<HttpResponse, ApiError> {
    let actor = user_actor(&tenant, &subject);
    let request_ctx = request_context(&req);
    let book = state
        .create_book
        .execute(actor, request_ctx, body.into_inner().legal_entity_key)
        .await
        .map_err(api_error)?;
    let now = state.create_book.0.clock.now();
    let summary = BookSummary {
        book,
        credentials: Vec::new(),
    };
    let view = book_view(&summary, now);
    Ok(book_response(&mut HttpResponse::Created(), view))
}

pub async fn get_accounting_book_embedded(
    state: web::Data<AccountingState>,
    tenant: CurrentTenant,
    subject: CurrentSubject,
    path: web::Path<BookId>,
    _permission: RequiredPermission,
    _entitlement: RequiredAnyFeature,
) -> Result<HttpResponse, ApiError> {
    let book_id = path.into_inner();
    let actor = user_actor(&tenant, &subject);
    let summary = state
        .get_book
        .execute(actor, book_id)
        .await
        .map_err(api_error)?;
    let now = state.get_book.0.clock.now();
    let view = book_view(&summary, now);
    Ok(book_response(&mut HttpResponse::Ok(), view))
}

pub async fn register_accounting_connection_embedded(
    state: web::Data<AccountingState>,
    tenant: CurrentTenant,
    subject: CurrentSubject,
    path: web::Path<BookId>,
    req: HttpRequest,
    body: web::Json<RegisterConnectionRequest>,
    _permission: RequiredPermission,
    _entitlement: RequiredAnyFeature,
) -> Result<HttpResponse, ApiError> {
    let book_id = path.into_inner();
    let revision = if_match_revision(&req)?;
    let actor = user_actor(&tenant, &subject);
    let request_ctx = request_context(&req);
    let req_body = body.into_inner();
    let command = RegisterConnectionCommand {
        provider: req_body.provider,
        organization_id: req_body.organization_id,
        mappings: req_body.mappings,
    };
    let _book = state
        .register_connection
        .execute(actor, request_ctx, book_id, revision, command)
        .await
        .map_err(api_error)?;
    let summary = state
        .get_book
        .execute(actor, book_id)
        .await
        .map_err(api_error)?;
    let now = state.get_book.0.clock.now();
    let view = book_view(&summary, now);
    Ok(book_response(&mut HttpResponse::Ok(), view))
}

pub async fn start_accounting_authorization_embedded(
    state: web::Data<AccountingState>,
    tenant: CurrentTenant,
    subject: CurrentSubject,
    path: web::Path<(BookId, ConnectionId)>,
    req: HttpRequest,
    _fresh_mfa: FreshAal2Required,
    _permission: RequiredPermission,
    _entitlement: RequiredAnyFeature,
) -> Result<HttpResponse, ApiError> {
    let (book_id, connection_id) = path.into_inner();
    let consent = consent(&state)?;
    let actor = user_actor(&tenant, &subject);
    let request_ctx = request_context(&req);
    let authorization_url = consent
        .start
        .execute(actor, request_ctx, book_id, connection_id)
        .await
        .map_err(api_error)?;
    Ok(HttpResponse::Ok().json(StartAuthorizationResponse { authorization_url }))
}

pub async fn revoke_accounting_authorization_embedded(
    state: web::Data<AccountingState>,
    tenant: CurrentTenant,
    subject: CurrentSubject,
    path: web::Path<(BookId, ConnectionId)>,
    req: HttpRequest,
    _fresh_mfa: FreshAal2Required,
    _permission: RequiredPermission,
    _entitlement: RequiredAnyFeature,
) -> Result<HttpResponse, ApiError> {
    let (book_id, connection_id) = path.into_inner();
    let consent = consent(&state)?;
    let actor = user_actor(&tenant, &subject);
    let request_ctx = request_context(&req);
    consent
        .revoke
        .execute(actor, request_ctx, book_id, connection_id)
        .await
        .map_err(api_error)?;
    Ok(HttpResponse::NoContent().finish())
}

pub async fn complete_accounting_authorization_embedded(
    state: web::Data<AccountingState>,
    tenant: CurrentTenant,
    subject: CurrentSubject,
    req: HttpRequest,
    body: web::Json<CompleteAuthorizationRequest>,
    _permission: RequiredPermission,
    _entitlement: RequiredAnyFeature,
) -> Result<HttpResponse, ApiError> {
    let consent = consent(&state)?;
    let actor = user_actor(&tenant, &subject);
    let request_ctx = request_context(&req);
    let req_body = body.into_inner();
    let book_id = consent
        .complete
        .execute(actor, request_ctx, &req_body.state, &req_body.code)
        .await
        .map_err(api_error)?;
    let summary = state
        .get_book
        .execute(actor, book_id)
        .await
        .map_err(api_error)?;
    let now = state.get_book.0.clock.now();
    let view = book_view(&summary, now);
    Ok(book_response(&mut HttpResponse::Ok(), view))
}

#[allow(clippy::too_many_arguments)]
pub async fn activate_accounting_book_embedded(
    state: web::Data<AccountingState>,
    tenant: CurrentTenant,
    subject: CurrentSubject,
    path: web::Path<BookId>,
    req: HttpRequest,
    body: web::Json<ActivateBookRequest>,
    _fresh_mfa: FreshAal2Required,
    _permission: RequiredPermission,
    _entitlement: RequiredAnyFeature,
) -> Result<HttpResponse, ApiError> {
    let book_id = path.into_inner();
    let revision = if_match_revision(&req)?;
    let actor = user_actor(&tenant, &subject);
    let request_ctx = request_context(&req);
    let req_body = body.into_inner();
    let _book = state
        .activate_book
        .execute(
            actor,
            request_ctx,
            book_id,
            revision,
            req_body.connection_id,
            req_body.starts_at,
        )
        .await
        .map_err(api_error)?;
    let summary = state
        .get_book
        .execute(actor, book_id)
        .await
        .map_err(api_error)?;
    let now = state.get_book.0.clock.now();
    let view = book_view(&summary, now);
    Ok(book_response(&mut HttpResponse::Ok(), view))
}

#[allow(clippy::too_many_arguments)]
pub async fn schedule_accounting_switch_embedded(
    state: web::Data<AccountingState>,
    tenant: CurrentTenant,
    subject: CurrentSubject,
    path: web::Path<BookId>,
    req: HttpRequest,
    body: web::Json<ScheduleSwitchRequest>,
    _fresh_mfa: FreshAal2Required,
    _permission: RequiredPermission,
    _entitlement: RequiredAnyFeature,
) -> Result<HttpResponse, ApiError> {
    let book_id = path.into_inner();
    let revision = if_match_revision(&req)?;
    let actor = user_actor(&tenant, &subject);
    let request_ctx = request_context(&req);
    let req_body = body.into_inner();
    let _book = state
        .schedule_switch
        .execute(
            actor,
            request_ctx,
            book_id,
            revision,
            req_body.connection_id,
            req_body.cutover_at,
        )
        .await
        .map_err(api_error)?;
    let summary = state
        .get_book
        .execute(actor, book_id)
        .await
        .map_err(api_error)?;
    let now = state.get_book.0.clock.now();
    let view = book_view(&summary, now);
    Ok(book_response(&mut HttpResponse::Ok(), view))
}

pub async fn cancel_accounting_switch_embedded(
    state: web::Data<AccountingState>,
    tenant: CurrentTenant,
    subject: CurrentSubject,
    path: web::Path<BookId>,
    req: HttpRequest,
    _permission: RequiredPermission,
    _entitlement: RequiredAnyFeature,
) -> Result<HttpResponse, ApiError> {
    let book_id = path.into_inner();
    let revision = if_match_revision(&req)?;
    let actor = user_actor(&tenant, &subject);
    let request_ctx = request_context(&req);
    let _book = state
        .cancel_switch
        .execute(actor, request_ctx, book_id, revision)
        .await
        .map_err(api_error)?;
    let summary = state
        .get_book
        .execute(actor, book_id)
        .await
        .map_err(api_error)?;
    let now = state.get_book.0.clock.now();
    let view = book_view(&summary, now);
    Ok(book_response(&mut HttpResponse::Ok(), view))
}

#[cfg(test)]
mod tests {
    use super::*;
    use actix_web::http::StatusCode;
    use sanvi_http::PROBLEM_TYPE_BASE;

    #[test]
    fn error_mapping_matches_the_spec() {
        let cases = [
            (
                AccountingError::Invalid("bad input"),
                StatusCode::BAD_REQUEST,
                None,
            ),
            (AccountingError::NotFound, StatusCode::NOT_FOUND, None),
            (AccountingError::Forbidden, StatusCode::FORBIDDEN, None),
            (AccountingError::StaleRevision, StatusCode::CONFLICT, None),
            (
                AccountingError::Conflict("conflict"),
                StatusCode::CONFLICT,
                None,
            ),
            (
                AccountingError::IdempotencyConflict,
                StatusCode::CONFLICT,
                None,
            ),
            (
                AccountingError::ReauthorizationRequired,
                StatusCode::CONFLICT,
                Some("accounting/reauthorization-required"),
            ),
            (
                AccountingError::AuthorizationRejected("rejected"),
                StatusCode::BAD_REQUEST,
                Some("accounting/authorization-rejected"),
            ),
            (
                AccountingError::VerificationFailed("verify failed"),
                StatusCode::UNPROCESSABLE_ENTITY,
                Some("accounting/verification-failed"),
            ),
            (
                AccountingError::HistoricalImport,
                StatusCode::UNPROCESSABLE_ENTITY,
                None,
            ),
            (
                AccountingError::VerificationUnavailable,
                StatusCode::SERVICE_UNAVAILABLE,
                Some("accounting/provider-unavailable"),
            ),
            (
                AccountingError::ProviderUnavailable,
                StatusCode::SERVICE_UNAVAILABLE,
                Some("accounting/provider-unavailable"),
            ),
            (
                AccountingError::Storage("secret detail".to_string()),
                StatusCode::INTERNAL_SERVER_ERROR,
                None,
            ),
        ];

        for (err, expected_status, expected_slug) in cases {
            let api_err = api_error(err);
            assert_eq!(api_err.status(), expected_status);
            if let Some(slug) = expected_slug {
                let problem = api_err.problem();
                assert_eq!(problem.r#type, format!("{PROBLEM_TYPE_BASE}/{slug}"));
            }
        }

        let storage_err = api_error(AccountingError::Storage("secret detail".to_string()));
        let problem = storage_err.problem();
        assert!(!problem.detail.contains("secret detail"));
        assert!(!format!("{storage_err:?}").contains("secret detail"));
    }
}
