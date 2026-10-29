//! Composition root wiring and signed internal delivery endpoints for Payments.

use std::sync::Arc;

use actix_web::{HttpRequest, HttpResponse, web};
use sanvi_access::domain::{Authorizer, Decision, Permission as AccessPermission, ResourceRef};
use sanvi_http::ApiError;
use sanvi_kernel::{TenantId, UserId};
use sanvi_payments::{
    adapters::remote::{AUDIT_AUDIENCE, AuditCommand, Delegation, PaymentsAuthorizer},
    domain::error::PaymentsError,
};
use sanvi_persistence::TxManager;
use sha2::{Digest, Sha256};

pub struct AuthorizerPaymentsAccess(pub Arc<dyn Authorizer>);

#[async_trait::async_trait]
impl PaymentsAuthorizer for AuthorizerPaymentsAccess {
    async fn check(
        &self,
        user_id: UserId,
        tenant_id: Option<TenantId>,
        permission: &str,
    ) -> Result<bool, PaymentsError> {
        let perm = AccessPermission::parse(permission)
            .map_err(|e| PaymentsError::InvalidConnection(e.to_string()))?;
        let resource = match tenant_id {
            Some(tid) => ResourceRef::Tenant(tid),
            None => ResourceRef::Platform,
        };
        match self.0.check(user_id, &perm, &resource).await {
            Ok(Decision::Allowed) => Ok(true),
            Ok(Decision::Denied) => Ok(false),
            Err(e) => {
                tracing::warn!(error = %e, "authorizer check failed for payments");
                Err(PaymentsError::InvalidConnection(e.to_string()))
            }
        }
    }
}

/// Internal service delivery endpoints share the platform's HTTP middleware.
pub fn internal_routes(config: &mut actix_web::web::ServiceConfig) {
    use actix_web::web;
    config.service(
        web::resource("/internal/payments/audit").route(web::post().to(ingest_payments_audit)),
    );
    config.service(
        web::resource("/internal/payments/events").route(web::post().to(ingest_payments_event)),
    );
}

/// State for internal Payments delivery.
pub struct PaymentsIngestState {
    /// Verifies Payments deliveries (`payments_service.inbound_secret`).
    pub secret: sanvi_telemetry::Redacted<String>,
    pub transactions: TxManager,
    pub writer: Arc<dyn sanvi_audit::AuditWriter>,
}

pub async fn ingest_payments_audit(
    state: web::Data<PaymentsIngestState>,
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
    if command.id <= 0 || !entry.action.starts_with("payments.") || entry.tenant_id.is_none() {
        return Err(ApiError::Forbidden {
            detail: "invalid payments audit scope".into(),
        });
    }
    let digest = hex::encode(Sha256::digest(&body));
    let storage = |e: sqlx::Error| ApiError::internal(e.to_string());
    let mut scope = state
        .transactions
        .begin_unscoped("ingest_payments_audit", "payments-service")
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?;
    let claimed = sqlx::query!(
        "INSERT INTO audit.payments_receipts (command_id, digest) VALUES ($1, $2) ON CONFLICT DO NOTHING",
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
            "SELECT digest FROM audit.payments_receipts WHERE command_id = $1",
            command.id
        )
        .fetch_one(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?;
        if existing != digest {
            return Err(ApiError::Conflict {
                detail: "payments audit command ID reused with different content".into(),
            });
        }
    }
    scope
        .commit()
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?;
    Ok(HttpResponse::NoContent().finish())
}

pub async fn ingest_payments_event(
    state: web::Data<PaymentsIngestState>,
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
            detail: "invalid payments event".into(),
        })?;
    if !event.aggregate.starts_with("payments.") || !event.event_type.starts_with("payments.") {
        return Err(ApiError::Forbidden {
            detail: "invalid payments event scope".into(),
        });
    }
    let digest = hex::encode(Sha256::digest(&body));
    let storage = |e: sqlx::Error| ApiError::internal(e.to_string());
    let mut scope = state
        .transactions
        .begin_unscoped("ingest_payments_event", "payments-service")
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?;
    let inserted = sqlx::query!(
        "INSERT INTO platform.payments_event_receipts (event_id, digest) VALUES ($1, $2) ON CONFLICT DO NOTHING",
        event.id.as_i64(),
        digest
    )
    .execute(&mut **scope.tx_mut().as_inner())
    .await
    .map_err(storage)?
    .rows_affected()
        == 1;
    let existing: String = sqlx::query_scalar!(
        "SELECT digest FROM platform.payments_event_receipts WHERE event_id = $1",
        event.id.as_i64()
    )
    .fetch_one(&mut **scope.tx_mut().as_inner())
    .await
    .map_err(storage)?;
    if existing != digest {
        return Err(ApiError::Conflict {
            detail: "payments event ID reused with different content".into(),
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
            detail: "Payments event conflicts with an existing event".into(),
        });
    }
    scope
        .commit()
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?;
    Ok(HttpResponse::NoContent().finish())
}
