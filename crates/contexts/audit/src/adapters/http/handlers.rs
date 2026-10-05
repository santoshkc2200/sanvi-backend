//! Handlers: thin actix shims over the application layer.

use std::sync::Arc;

use actix_web::{HttpResponse, Responder, web};
use sanvi_http::{ApiError, CurrentSubject, RequiredPermission};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::application::{AuditPage, ListAuditLog};
use crate::domain::{AuditCursor, AuditEntry, AuditError, AuditFilters};

/// Application state visible to this context's handlers.
#[derive(Clone)]
pub struct AuditState {
    pub list: Arc<ListAuditLog>,
}

/// The audit context's domain-error → HTTP mapping (single place, this
/// boundary).
pub fn api_error(err: AuditError) -> ApiError {
    match err {
        AuditError::InvalidActorType(detail)
        | AuditError::InvalidAction(detail)
        | AuditError::InvalidCursor(detail) => ApiError::BadRequest { detail },
        AuditError::ChainMismatch { .. } => ApiError::Internal {
            detail: "audit chain integrity failure".to_string(),
        },
        AuditError::Storage(detail) => {
            tracing::error!(error = %detail, "audit storage failure");
            ApiError::from_storage_detail(&detail)
        }
    }
}

/// Audit query parameters (all optional filters + cursor pagination).
#[derive(Debug, Clone, Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct AuditQuery {
    /// Restrict to one tenant (decimal Snowflake).
    #[param(value_type = Option<String>)]
    pub tenant_id: Option<sanvi_kernel::TenantId>,
    /// Restrict to one actor (decimal Snowflake).
    #[param(value_type = Option<String>)]
    pub actor_id: Option<sanvi_kernel::UserId>,
    /// Exact action name, e.g. tenancy.suspend.
    pub action: Option<String>,
    /// Inclusive lower bound.
    pub since: Option<chrono::DateTime<chrono::Utc>>,
    /// Exclusive upper bound.
    pub until: Option<chrono::DateTime<chrono::Utc>>,
    /// Cursor: seq of the last entry of the previous page.
    pub after: Option<i64>,
    /// Page size, clamped to 1..=200.
    #[param(schema_with = audit_limit_schema)]
    pub limit: Option<u32>,
}

fn audit_limit_schema() -> utoipa::openapi::schema::Object {
    sanvi_http::openapi::page_limit_schema(200, 50).build()
}

/// One page of audit entries.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct AuditPageView {
    pub entries: Vec<AuditEntry>,
    #[schema(value_type = Option<i64>)]
    pub next_cursor: Option<i64>,
}

impl From<AuditPage> for AuditPageView {
    fn from(page: AuditPage) -> Self {
        Self {
            entries: page.entries,
            next_cursor: page.next_cursor.map(|cursor| cursor.seq),
        }
    }
}

/// List the platform audit trail (operator console).
///
/// Supports filters (`tenant_id`, `actor_id`, `action`, `since`, `until`)
/// and cursor pagination (`after` = the `seq` of the last entry of the
/// previous page).
#[utoipa::path(
    get,
    path = "/api/v1/platform/audit",
    tag = "audit",
    params(AuditQuery),
    responses(
        (status = 200, description = "One page of audit entries, newest first", body = AuditPageView),
        (status = 400, description = "Invalid filter value", body = sanvi_http::ProblemDetail),
        (status = 403, description = "Missing platform.audit.read", body = sanvi_http::ProblemDetail),
        (status = 504, description = "Statement timeout — the request exceeded its budget", body = sanvi_http::ProblemDetail)
    )
)]
pub async fn list_audit(
    state: web::Data<AuditState>,
    query: web::Query<AuditQuery>,
    _subject: CurrentSubject,
    _guard: RequiredPermission,
) -> Result<impl Responder, ApiError> {
    let filters = AuditFilters {
        tenant_id: query.tenant_id,
        actor_id: query.actor_id,
        action: query.action.clone(),
        since: query.since,
        until: query.until,
    };
    let after = query.after.map(|seq| AuditCursor { seq });
    let page = state
        .list
        .execute(filters, after, query.limit)
        .await
        .map_err(api_error)?;
    Ok(HttpResponse::Ok().json(AuditPageView::from(page)))
}
