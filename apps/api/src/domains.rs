//! Composition root wiring and signed internal delivery endpoints for Domains.

use std::sync::Arc;

use actix_web::{HttpRequest, HttpResponse, web};
use sanvi_access::domain::{Authorizer, Decision, Permission as AccessPermission, ResourceRef};
use sanvi_domains::{
    adapters::remote::{AUDIT_AUDIENCE, AuditCommand, Delegation, DomainsAuthorizer},
    domain::DomainsError,
};
use sanvi_http::ApiError;
use sanvi_kernel::{TenantId, UserId};
use sanvi_persistence::TxManager;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use utoipa::ToSchema;

pub struct AuthorizerDomainsAccess(pub Arc<dyn Authorizer>);

#[async_trait::async_trait]
impl DomainsAuthorizer for AuthorizerDomainsAccess {
    async fn check(
        &self,
        user_id: UserId,
        tenant_id: Option<TenantId>,
        permission: &str,
    ) -> Result<bool, DomainsError> {
        let perm = AccessPermission::parse(permission)
            .map_err(|e| DomainsError::Storage(e.to_string()))?;
        let resource = match tenant_id {
            Some(tid) => ResourceRef::Tenant(tid),
            None => ResourceRef::Platform,
        };
        match self.0.check(user_id, &perm, &resource).await {
            Ok(Decision::Allowed) => Ok(true),
            Ok(Decision::Denied) => Ok(false),
            Err(e) => {
                tracing::warn!(error = %e, "authorizer check failed for domains");
                Err(DomainsError::Storage(e.to_string()))
            }
        }
    }
}

/// Internal service delivery endpoints share the platform's HTTP middleware.
pub fn internal_routes(config: &mut actix_web::web::ServiceConfig) {
    use actix_web::web;
    config.service(
        web::resource("/internal/domains/audit").route(web::post().to(ingest_domains_audit)),
    );
    config.service(
        web::resource("/internal/domains/events").route(web::post().to(ingest_domains_event)),
    );
    config.service(
        web::resource("/internal/domains/tenant-state").route(web::post().to(tenant_state)),
    );
}

/// State for internal Domains delivery and tenant status check.
pub struct DomainsIngestState {
    /// Verifies Domains deliveries (`domains_service.inbound_secret`).
    pub secret: sanvi_telemetry::Redacted<String>,
    pub transactions: TxManager,
    pub writer: Arc<dyn sanvi_audit::AuditWriter>,
    pub resolve_tenant: Arc<sanvi_tenancy::application::ResolveTenant>,
}

#[utoipa::path(
    post,
    path = "/internal/domains/audit",
    tag = "domains",
    operation_id = "ingest_domains_audit",
    request_body = serde_json::Value,
    params(("x-domains-delegation" = String, Header, description = "Request-bound HMAC delegation; audience sanvi-platform-domains-audit, purpose audit")),
    responses(
        (status = 204, description = "Audit command durably appended or already received"),
        (status = 401, description = "Invalid service delegation", body = sanvi_http::ProblemDetail),
        (status = 403, description = "Invalid Domains audit scope", body = sanvi_http::ProblemDetail),
        (status = 409, description = "Audit command ID reused with different content", body = sanvi_http::ProblemDetail)
    )
)]
pub async fn ingest_domains_audit(
    state: web::Data<DomainsIngestState>,
    request: HttpRequest,
    body: web::Bytes,
) -> Result<HttpResponse, ApiError> {
    Delegation::from_request(
        state.secret.expose_secret(),
        &request,
        &body,
        AUDIT_AUDIENCE,
        "audit",
    )?;
    let command: AuditCommand =
        serde_json::from_slice(&body).map_err(|_| ApiError::BadRequest {
            detail: "invalid audit command".into(),
        })?;
    let entry = command.entry;
    if command.id <= 0 || !entry.action.starts_with("domains.") || entry.tenant_id.is_none() {
        return Err(ApiError::Forbidden {
            detail: "invalid domains audit scope".into(),
        });
    }
    let digest = hex::encode(Sha256::digest(&body));
    let storage = |e: sqlx::Error| ApiError::internal(e.to_string());
    let mut scope = state
        .transactions
        .begin_unscoped("ingest_domains_audit", "domains-service")
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?;
    let claimed = sqlx::query!(
        "INSERT INTO audit.domains_receipts (command_id, digest) VALUES ($1, $2) ON CONFLICT DO NOTHING",
        command.id,
        digest
    )
    .execute(&mut **scope.tx_mut().as_inner())
    .await
    .map_err(storage)?
    .rows_affected()
        == 1;
    if claimed {
        state
            .writer
            .append(&mut scope, &entry)
            .await
            .map_err(|e| ApiError::internal(e.to_string()))?;
    } else {
        let existing: String = sqlx::query_scalar!(
            "SELECT digest FROM audit.domains_receipts WHERE command_id = $1",
            command.id
        )
        .fetch_one(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?;
        if existing != digest {
            return Err(ApiError::Conflict {
                detail: "domains audit command ID reused with different content".into(),
            });
        }
    }
    scope
        .commit()
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?;
    Ok(HttpResponse::NoContent().finish())
}

#[utoipa::path(
    post,
    path = "/internal/domains/events",
    tag = "domains",
    operation_id = "ingest_domains_event",
    request_body = serde_json::Value,
    params(("x-domains-delegation" = String, Header, description = "Request-bound HMAC delegation; audience sanvi-platform-domains-audit, purpose event")),
    responses(
        (status = 204, description = "Event durably accepted or already received"),
        (status = 401, description = "Invalid service delegation", body = sanvi_http::ProblemDetail),
        (status = 403, description = "Invalid Domains event scope", body = sanvi_http::ProblemDetail),
        (status = 409, description = "Event ID conflicts with an existing event", body = sanvi_http::ProblemDetail)
    )
)]
pub async fn ingest_domains_event(
    state: web::Data<DomainsIngestState>,
    request: HttpRequest,
    body: web::Bytes,
) -> Result<HttpResponse, ApiError> {
    Delegation::from_request(
        state.secret.expose_secret(),
        &request,
        &body,
        AUDIT_AUDIENCE,
        "event",
    )?;
    let event: sanvi_messaging::OutboxRecord =
        serde_json::from_slice(&body).map_err(|_| ApiError::BadRequest {
            detail: "invalid domains event".into(),
        })?;
    if !event.aggregate.starts_with("domains.") || !event.event_type.starts_with("domains.") {
        return Err(ApiError::Forbidden {
            detail: "invalid domains event scope".into(),
        });
    }
    let digest = hex::encode(Sha256::digest(&body));
    let storage = |e: sqlx::Error| ApiError::internal(e.to_string());
    let mut scope = state
        .transactions
        .begin_unscoped("ingest_domains_event", "domains-service")
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?;
    let inserted = sqlx::query!(
        "INSERT INTO platform.domains_event_receipts (event_id, digest) VALUES ($1, $2) ON CONFLICT DO NOTHING",
        event.id.as_i64(),
        digest
    )
    .execute(&mut **scope.tx_mut().as_inner())
    .await
    .map_err(storage)?
    .rows_affected()
        == 1;
    let existing: String = sqlx::query_scalar!(
        "SELECT digest FROM platform.domains_event_receipts WHERE event_id = $1",
        event.id.as_i64()
    )
    .fetch_one(&mut **scope.tx_mut().as_inner())
    .await
    .map_err(storage)?;
    if existing != digest {
        return Err(ApiError::Conflict {
            detail: "domains event ID reused with different content".into(),
        });
    }
    if inserted {
        sqlx::query!(
            "INSERT INTO platform.outbox (id, aggregate, aggregate_id, event_type, payload, metadata, occurred_at) SELECT $1,$2,$3,$4,$5,$6,$7 WHERE NOT EXISTS (SELECT FROM platform.outbox WHERE id = $1)",
            event.id.as_i64(),
            event.aggregate,
            event.aggregate_id.as_i64(),
            event.event_type,
            event.payload,
            event.metadata,
            event.occurred_at
        )
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?;
    }
    let matches: bool = sqlx::query_scalar!(
        "SELECT EXISTS (SELECT 1 FROM platform.outbox WHERE id = $1 AND aggregate = $2 AND aggregate_id = $3 AND event_type = $4 AND payload = $5 AND metadata = $6 AND occurred_at = $7) AS \"matches!\"",
        event.id.as_i64(),
        event.aggregate,
        event.aggregate_id.as_i64(),
        event.event_type,
        event.payload,
        event.metadata,
        event.occurred_at
    )
    .fetch_one(&mut **scope.tx_mut().as_inner())
    .await
    .map_err(storage)?;
    if inserted && !matches {
        return Err(ApiError::Conflict {
            detail: "Domains event conflicts with an existing event".into(),
        });
    }
    scope
        .commit()
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?;
    Ok(HttpResponse::NoContent().finish())
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct TenantStateRequest {
    #[schema(value_type = String, example = "100000000000000000")]
    pub tenant_id: TenantId,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct TenantStateResponse {
    pub active: bool,
    pub status: Option<String>,
}

#[utoipa::path(
    post,
    path = "/internal/domains/tenant-state",
    tag = "domains",
    operation_id = "check_domains_tenant_state",
    request_body = TenantStateRequest,
    params(("x-domains-delegation" = String, Header, description = "Request-bound HMAC delegation; audience sanvi-platform-domains-audit, purpose tenant-state")),
    responses(
        (status = 200, description = "Tenancy status for the tenant", body = TenantStateResponse),
        (status = 401, description = "Invalid service delegation", body = sanvi_http::ProblemDetail),
        (status = 400, description = "Invalid request payload", body = sanvi_http::ProblemDetail)
    )
)]
pub async fn tenant_state(
    state: web::Data<DomainsIngestState>,
    request: HttpRequest,
    body: web::Bytes,
) -> Result<HttpResponse, ApiError> {
    // Tolerates purpose "tenant-state" or "tenant_state"
    let verified = Delegation::from_request(
        state.secret.expose_secret(),
        &request,
        &body,
        AUDIT_AUDIENCE,
        "tenant-state",
    )
    .or_else(|_| {
        Delegation::from_request(
            state.secret.expose_secret(),
            &request,
            &body,
            AUDIT_AUDIENCE,
            "tenant_state",
        )
    });
    if verified.is_err() {
        return Err(ApiError::Unauthorized);
    }
    let req: TenantStateRequest =
        serde_json::from_slice(&body).map_err(|_| ApiError::BadRequest {
            detail: "invalid tenant-state payload".into(),
        })?;
    let tenant = state
        .resolve_tenant
        .by_id(req.tenant_id)
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?;
    let active = matches!(
        tenant.as_ref().map(|t| t.status()),
        Some(sanvi_tenancy::domain::TenantStatus::Active)
    );
    let status = tenant.map(|t| t.status().as_str().to_string());
    Ok(HttpResponse::Ok().json(TenantStateResponse { active, status }))
}
