//! Handlers: thin actix shims over the application layer.

use std::sync::Arc;

use actix_web::{HttpResponse, Responder, web};
use sanvi_http::RequiredPermission;

use crate::application::{ListPermissions, PermissionView};

/// Application state visible to this context's handlers.
#[derive(Clone)]
pub struct AccessState {
    pub list_permissions: Arc<ListPermissions>,
}

/// `GET /api/v1/access/permissions` — the central permission registry,
/// for the admin/UI contracts. Readable by any authenticated subject.
#[utoipa::path(
    get,
    path = "/api/v1/access/permissions",
    tag = "access",
    operation_id = "list_permissions",
    responses(
        (status = 200, description = "Permission registry", body = Vec<PermissionView>),
        (status = 401, description = "Not authenticated", body = sanvi_http::ProblemDetail),
        (status = 403, description = "Permission denied", body = sanvi_http::ProblemDetail)
    )
)]
pub async fn list_permissions(
    state: web::Data<AccessState>,
    _guard: RequiredPermission,
) -> impl Responder {
    HttpResponse::Ok().json(state.list_permissions.execute())
}
