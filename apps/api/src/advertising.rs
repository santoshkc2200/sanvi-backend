//! Composition root wiring and signed internal delivery endpoints for Advertising.

use std::sync::Arc;

use actix_web::{HttpRequest, HttpResponse, web};
use sanvi_access::domain::{Authorizer, Decision, Permission as AccessPermission, ResourceRef};
use sanvi_advertising::adapters::remote::{AUDIT_AUDIENCE, AdvertisingAuthorizer, Delegation};
use sanvi_http::ApiError;
use sanvi_kernel::{TenantId, UserId};
use sanvi_persistence::TxManager;
use sha2::{Digest, Sha256};

pub struct AuthorizerAdvertisingAccess(pub Arc<dyn Authorizer>);

#[async_trait::async_trait]
impl AdvertisingAuthorizer for AuthorizerAdvertisingAccess {
    async fn check(
        &self,
        user_id: UserId,
        tenant_id: Option<TenantId>,
        permission: &str,
    ) -> anyhow::Result<bool> {
        let perm = AccessPermission::parse(permission)?;
        let resource = match tenant_id {
            Some(tid) => ResourceRef::Tenant(tid),
            None => ResourceRef::Platform,
        };
        match self.0.check(user_id, &perm, &resource).await {
            Ok(Decision::Allowed) => Ok(true),
            Ok(Decision::Denied) => Ok(false),
            Err(e) => {
                tracing::warn!(error = %e, "authorizer check failed for advertising");
                Err(anyhow::anyhow!(e.to_string()))
            }
        }
    }
}

/// Internal service delivery endpoints share the platform's HTTP middleware.
pub fn internal_routes(config: &mut actix_web::web::ServiceConfig) {
    use actix_web::web;
    config.service(
        web::resource("/internal/advertising/platform").route(web::post().to(platform_operation)),
    );
    config.service(
        web::resource("/internal/advertising/events")
            .route(web::post().to(ingest_advertising_event)),
    );
}

/// State for internal Advertising delivery.
pub struct AdvertisingPlatformState {
    /// Verifies Advertising deliveries (`advertising_service.inbound_secret`).
    pub secret: sanvi_telemetry::Redacted<String>,
    pub transactions: TxManager,
    pub directives: Option<Arc<dyn sanvi_privacy::domain::DirectiveResolver>>,
    pub feature_gate: Arc<dyn sanvi_http::FeatureGate>,
    pub enabled: bool,
}

pub async fn ingest_advertising_event(
    state: web::Data<AdvertisingPlatformState>,
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
            detail: "invalid advertising event".into(),
        })?;
    if !event.aggregate.starts_with("advertising.") || !event.event_type.starts_with("advertising.")
    {
        return Err(ApiError::Forbidden {
            detail: "invalid advertising event scope".into(),
        });
    }
    let digest = hex::encode(Sha256::digest(&body));
    let storage = |e: sqlx::Error| ApiError::internal(e.to_string());
    let mut scope = state
        .transactions
        .begin_unscoped("ingest_advertising_event", "advertising-service")
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?;
    let inserted = sqlx::query!(
        "INSERT INTO platform.advertising_event_receipts (event_id, digest) VALUES ($1, $2) ON CONFLICT DO NOTHING",
        event.id.as_i64(),
        digest
    )
    .execute(&mut **scope.tx_mut().as_inner())
    .await
    .map_err(storage)?
    .rows_affected()
        == 1;
    let existing: String = sqlx::query_scalar!(
        "SELECT digest FROM platform.advertising_event_receipts WHERE event_id = $1",
        event.id.as_i64()
    )
    .fetch_one(&mut **scope.tx_mut().as_inner())
    .await
    .map_err(storage)?;
    if existing != digest {
        return Err(ApiError::Conflict {
            detail: "advertising event ID reused with different content".into(),
        });
    }
    if inserted && event.event_type == "advertising.retention_executed" {
        let class = event
            .payload
            .get("data_class")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let action = event
            .payload
            .get("action")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let affected = event
            .payload
            .get("partitions_affected")
            .and_then(|v| v.as_i64())
            .filter(|n| *n >= 0)
            .ok_or_else(|| ApiError::BadRequest {
                detail: "invalid retention count".into(),
            })?;
        if !matches!(
            (class, action),
            ("advertising_conversion_event", "anonymise" | "delete")
                | ("advertising_metric", "delete")
        ) {
            return Err(ApiError::Forbidden {
                detail: "invalid Advertising retention scope".into(),
            });
        }
        sqlx::query("INSERT INTO privacy.retention_runs(data_class,action,rows_affected,executor,detail) VALUES($1,$2,$3,'advertising.retention','{\"per_parent_actions\":true}'::jsonb)").bind(class).bind(action).bind(affected).execute(&mut **scope.tx_mut().as_inner()).await.map_err(storage)?;
    }
    if inserted && event.event_type == "advertising.conversion_recorded" {
        let tenant_id = event
            .metadata
            .get("tenant_id")
            .and_then(|value| value.as_str())
            .and_then(|value| value.parse::<i64>().ok())
            .ok_or_else(|| ApiError::BadRequest {
                detail: "conversion event requires tenant metadata".into(),
            })?;
        sqlx::query("SELECT platform.record_quota_counter($1,'tracked_conversions',date_trunc('month',$2::timestamptz AT TIME ZONE 'UTC')::date,1)")
            .bind(tenant_id).bind(event.occurred_at).execute(&mut **scope.tx_mut().as_inner()).await.map_err(storage)?;
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
            detail: "Advertising event conflicts with an existing event".into(),
        });
    }
    scope
        .commit()
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?;
    Ok(HttpResponse::NoContent().finish())
}

/// Authoritative platform reads for the isolated Advertising database.
async fn platform_operation(
    state: web::Data<AdvertisingPlatformState>,
    request: HttpRequest,
    body: web::Bytes,
) -> Result<HttpResponse, ApiError> {
    use sanvi_advertising::adapters::remote::PlatformRequest;
    let claims = Delegation::from_request(
        state.secret.expose_secret(),
        &request,
        &body,
        AUDIT_AUDIENCE,
        "platform",
    )?;
    let operation: PlatformRequest =
        serde_json::from_slice(&body).map_err(|_| ApiError::BadRequest {
            detail: "invalid Advertising platform request".into(),
        })?;
    let storage = |error: sqlx::Error| ApiError::internal(error.to_string());
    let mut scope = state
        .transactions
        .begin_unscoped("advertising_platform_operation", "advertising-service")
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?;
    // Replay protection also makes tracking admission accounting single-use.
    sqlx::query("DELETE FROM platform.advertising_delegation_nonces WHERE expires_at < now() - interval '1 minute'").execute(&mut **scope.tx_mut().as_inner()).await.map_err(storage)?;
    let fresh = sqlx::query("INSERT INTO platform.advertising_delegation_nonces (jti, expires_at) VALUES ($1,to_timestamp($2::bigint)) ON CONFLICT DO NOTHING").bind(&claims.jti).bind(claims.expires_at).execute(&mut **scope.tx_mut().as_inner()).await.map_err(storage)?.rows_affected() == 1;
    if !fresh {
        return Err(ApiError::Unauthorized);
    }
    scope
        .commit()
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?;
    let response = match operation {
        PlatformRequest::Directive { subject, purpose } => {
            let resolver = state
                .directives
                .as_ref()
                .ok_or_else(|| ApiError::service_unavailable("Privacy is unavailable"))?;
            HttpResponse::Ok().json(
                resolver
                    .resolve(&subject, purpose)
                    .await
                    .map_err(|e| ApiError::internal(e.to_string()))?,
            )
        }
        PlatformRequest::Snapshot { subject } => {
            let resolver = state
                .directives
                .as_ref()
                .ok_or_else(|| ApiError::service_unavailable("Privacy is unavailable"))?;
            HttpResponse::Ok().json(
                resolver
                    .snapshot(&subject)
                    .await
                    .map_err(|e| ApiError::internal(e.to_string()))?,
            )
        }
        PlatformRequest::Feature { tenant_id, feature } => {
            if !feature.starts_with("advertising.") {
                return Err(ApiError::Forbidden {
                    detail: "invalid feature scope".into(),
                });
            }
            HttpResponse::Ok()
                .json(state.enabled && state.feature_gate.check(tenant_id, &feature).await?)
        }
        PlatformRequest::VendorRole { platform } => {
            let name = match platform.as_str() {
                "google_ads" => "Google Ads",
                "meta" => "Meta",
                _ => "",
            };
            let role: Option<String> = sqlx::query_scalar(
                "SELECT role FROM privacy.subprocessors WHERE name=$1 AND removed_at IS NULL",
            )
            .bind(name)
            .fetch_optional(state.transactions.pool())
            .await
            .map_err(storage)?;
            HttpResponse::Ok().json(match role.as_deref() {
                Some("service_provider" | "processor") => "ServiceProvider",
                _ => "ThirdParty",
            })
        }
        PlatformRequest::Timezone { tenant_id } => {
            let mut scope = state
                .transactions
                .begin_for(tenant_id.as_i64())
                .await
                .map_err(|e| ApiError::internal(e.to_string()))?;
            use sanvi_localization::domain::TenantLocaleRepository;
            let locale = sanvi_localization::adapters::SqlxTenantLocaleRepository::new()
                .get(&mut scope, tenant_id)
                .await
                .map_err(|e| ApiError::internal(e.to_string()))?;
            scope
                .commit()
                .await
                .map_err(|e| ApiError::internal(e.to_string()))?;
            HttpResponse::Ok().json(locale.map(|s| s.timezone).unwrap_or_else(|| "UTC".into()))
        }
        PlatformRequest::TenantName { tenant_id } => {
            let name: Option<String> =
                sqlx::query_scalar("SELECT display_name FROM tenancy.tenants WHERE id=$1")
                    .bind(tenant_id.as_i64())
                    .fetch_optional(state.transactions.pool())
                    .await
                    .map_err(storage)?;
            HttpResponse::Ok().json(name.unwrap_or_else(|| tenant_id.to_string()))
        }
        PlatformRequest::Retention => {
            let rules: Vec<(String, String, i64)> = sqlx::query_as("SELECT data_class, action, (EXTRACT(EPOCH FROM period)/86400)::bigint FROM privacy.retention_rules WHERE data_class IN ('advertising_conversion_event','advertising_metric')").fetch_all(state.transactions.pool()).await.map_err(storage)?;
            HttpResponse::Ok().json(rules)
        }
        PlatformRequest::TrackingAdmission { tenant_id } => {
            if !state.enabled {
                return Err(ApiError::service_unavailable("Advertising disabled"));
            }
            if let Some(runtime) =
                request.app_data::<web::Data<sanvi_http::quota_middleware::QuotaRuntime>>()
            {
                let accounting = async {
                    runtime
                        .service
                        .configure_enforcement(tenant_id, runtime.enabled)
                        .await?;
                    runtime
                        .service
                        .record(
                            tenant_id,
                            sanvi_http::quotas::QuotaDimension::ApiRequests,
                            1,
                            false,
                        )
                        .await?;
                    if runtime.enabled {
                        runtime
                            .service
                            .enforce(tenant_id, sanvi_http::quotas::QuotaDimension::ApiRequests)
                            .await?;
                    }
                    Ok::<_, sanvi_http::quotas::QuotaError>(())
                }
                .await;
                if let Err(error) = accounting {
                    tracing::warn!(%error, "public tracking quota admission failed");
                    if runtime.enabled {
                        return Err(ApiError::from(error));
                    }
                }
            }
            HttpResponse::Ok().json(true)
        }
    };
    Ok(response)
}
