use actix_web::{HttpRequest, HttpResponse, error::InternalError, web};
use sanvi_http::{ApiError, CurrentTenant, RequiredPermission};
use serde::Deserialize;
use utoipa::ToSchema;

use super::state::MediaState;
use crate::domain::{AssetDeliveryView, CompletedPartInput, CreateAssetInput, MediaError};

use super::validation;

/// Maps a `MediaError` into the standard `ApiError` status vocabulary while
/// threading the upstream `code` through as an RFC 9457 extension member —
/// the same `.problem()` + `extensions` + `InternalError::from_response`
/// pattern `advertising`'s `campaign_validation_problem` already uses
/// (`crates/contexts/advertising/src/adapters/http/handlers.rs:355-374`).
/// `course-media`'s `CourseApiError` reads `problem.code` directly, so this
/// is load-bearing, not decoration — see TASK-032's "Error mapping" section.
fn media_problem(err: MediaError) -> actix_web::Error {
    // TASK-027 (FR-1127): input-shaped rejections whose message names its
    // field are field-addressed (`validation::validation_issue` owns the
    // field table).
    let (code, api) = match &err {
        MediaError::BadRequest { code, detail } => {
            let api = match validation::validation_issue(&err) {
                Some((detail, issues)) => ApiError::validation(detail, issues),
                None => ApiError::BadRequest {
                    detail: detail.clone(),
                },
            };
            (code.clone(), api)
        }
        MediaError::NotFound { code, detail } => (
            code.clone(),
            ApiError::NotFound {
                resource: detail.clone(),
            },
        ),
        MediaError::Conflict { code, detail } => (
            code.clone(),
            ApiError::Conflict {
                detail: detail.clone(),
            },
        ),
        MediaError::Unavailable { detail } => (
            "media_unavailable".to_string(),
            ApiError::ServiceUnavailable {
                detail: detail.clone(),
            },
        ),
    };
    let mut problem = api.problem();
    problem.extensions = Some(
        [("code".to_string(), serde_json::Value::String(code))]
            .into_iter()
            .collect(),
    );
    InternalError::from_response(api.clone(), HttpResponse::build(api.status()).json(problem))
        .into()
}

fn idempotency_key(request: &HttpRequest) -> Result<String, ApiError> {
    let raw = request
        .headers()
        .get("Idempotency-Key")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| ApiError::BadRequest {
            detail: "Idempotency-Key header is required".to_string(),
        })?;
    Ok(raw.to_string())
}

/// Wire-only shape for presigning multipart upload parts. This has no
/// counterpart in `domain::dto` because it is not a value the domain layer
/// needs to model — the use case takes the bare `Vec<i64>`.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct PresignPartsRequest {
    pub part_numbers: Vec<i64>,
}

/// Wire-only shape for completing a multipart upload. Same treatment as
/// [`PresignPartsRequest`]: the use case takes the bare parts vec.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct CompleteUploadRequest {
    pub parts: Vec<CompletedPartInput>,
}

#[utoipa::path(
    post,
    path = "/api/v1/tenant/media/v1/assets",
    tag = "media",
    operation_id = "create_media_asset",
    request_body = CreateAssetInput,
    params(
        ("Idempotency-Key" = String, Header, description = "Required by hitox-media-service; a retried key returns the original asset.")
    ),
    responses(
        (status = 201, description = "Asset minted, upload presigned", body = crate::domain::AssetUploadView),
        (status = 400, description = "hitox-media-service rejected the request (e.g. missing Idempotency-Key)", body = sanvi_http::ProblemDetail),
        (status = 403, description = "Missing media.assets.manage permission", body = sanvi_http::ProblemDetail)
    )
)]
pub async fn create_media_asset(
    state: web::Data<MediaState>,
    tenant: CurrentTenant,
    _permission: RequiredPermission,
    request: HttpRequest,
    body: web::Json<CreateAssetInput>,
) -> Result<HttpResponse, actix_web::Error> {
    let key = idempotency_key(&request)?;
    let runtime = request.app_data::<web::Data<sanvi_http::quota_middleware::QuotaRuntime>>();
    let reservation = if let Some(runtime) = runtime {
        let payload = serde_json::to_vec(&*body)
            .map_err(|_| ApiError::internal("media payload serialization failed"))?;
        runtime
            .reserve_external_payload(
                tenant.0.tenant_id,
                sanvi_http::quotas::QuotaDimension::StorageBytes,
                &key,
                body.size_bytes,
                &payload,
            )
            .await?
    } else {
        None
    };
    let result = state
        .create_asset_upload
        .execute(tenant.0.tenant_id, key.clone(), body.into_inner())
        .await;
    if let (Some(runtime), Some(ticket)) = (runtime, &reservation) {
        use sanvi_http::external_quotas::ExternalOutcome;
        let outcome = match &result {
            Ok(view) => ExternalOutcome::Accepted {
                asset_id: Some(view.asset_id.clone()),
            },
            Err(MediaError::BadRequest { .. } | MediaError::NotFound { .. }) => {
                ExternalOutcome::Rejected
            }
            Err(_) => ExternalOutcome::Ambiguous,
        };
        runtime.finish_external(ticket, outcome).await?;
    }
    let view = result.map_err(media_problem)?;
    if let Some(runtime) = runtime {
        // Also attempt binding after a shadow-mode accounting outage.
        runtime
            .bind_asset(tenant.0.tenant_id, &key, &view.asset_id)
            .await?;
    }
    Ok(HttpResponse::Created().json(view))
}

#[utoipa::path(
    post,
    path = "/api/v1/tenant/media/v1/assets/{asset_id}/parts",
    tag = "media",
    operation_id = "presign_media_asset_parts",
    request_body = PresignPartsRequest,
    params(("asset_id" = String, Path)),
    responses(
        (status = 200, description = "Presigned URLs for the requested parts, wrapped as {\"part_urls\": [...]} to match course-media's wire.part_urls read", body = serde_json::Value),
        (status = 404, description = "No such asset in this tenant's namespace", body = sanvi_http::ProblemDetail),
        (status = 403, description = "Missing media.assets.manage permission", body = sanvi_http::ProblemDetail)
    )
)]
pub async fn presign_media_asset_parts(
    state: web::Data<MediaState>,
    tenant: CurrentTenant,
    _permission: RequiredPermission,
    asset_id: web::Path<String>,
    body: web::Json<PresignPartsRequest>,
) -> Result<HttpResponse, actix_web::Error> {
    let urls = state
        .presign_asset_parts
        .execute(
            tenant.0.tenant_id,
            asset_id.into_inner(),
            body.into_inner().part_numbers,
        )
        .await
        .map_err(media_problem)?;
    Ok(HttpResponse::Ok().json(serde_json::json!({"part_urls": urls})))
}

#[utoipa::path(
    post,
    path = "/api/v1/tenant/media/v1/assets/{asset_id}/complete",
    tag = "media",
    operation_id = "complete_media_asset_upload",
    request_body = CompleteUploadRequest,
    params(("asset_id" = String, Path)),
    responses(
        (status = 200, description = "Upload completed, asset state after completion", body = crate::domain::AssetView),
        (status = 404, description = "No such asset in this tenant's namespace", body = sanvi_http::ProblemDetail),
        (status = 409, description = "Upload already completed or part etags do not match", body = sanvi_http::ProblemDetail),
        (status = 403, description = "Missing media.assets.manage permission", body = sanvi_http::ProblemDetail)
    )
)]
pub async fn complete_media_asset_upload(
    state: web::Data<MediaState>,
    tenant: CurrentTenant,
    _permission: RequiredPermission,
    asset_id: web::Path<String>,
    body: web::Json<CompleteUploadRequest>,
) -> Result<HttpResponse, actix_web::Error> {
    let view = state
        .complete_asset_upload
        .execute(
            tenant.0.tenant_id,
            asset_id.into_inner(),
            body.into_inner().parts,
        )
        .await
        .map_err(media_problem)?;
    Ok(HttpResponse::Ok().json(view))
}

#[utoipa::path(
    get,
    path = "/api/v1/tenant/media/v1/assets/{asset_id}",
    tag = "media",
    operation_id = "get_media_asset",
    params(("asset_id" = String, Path)),
    responses(
        (status = 200, description = "Current asset state", body = crate::domain::AssetView),
        (status = 404, description = "No such asset in this tenant's namespace", body = sanvi_http::ProblemDetail),
        (status = 403, description = "Missing media.assets.manage permission", body = sanvi_http::ProblemDetail)
    )
)]
pub async fn get_media_asset(
    state: web::Data<MediaState>,
    tenant: CurrentTenant,
    _permission: RequiredPermission,
    asset_id: web::Path<String>,
) -> Result<HttpResponse, actix_web::Error> {
    let view = state
        .get_media_asset
        .execute(tenant.0.tenant_id, asset_id.into_inner())
        .await
        .map_err(media_problem)?;
    Ok(HttpResponse::Ok().json(view))
}

#[utoipa::path(
    delete,
    path = "/api/v1/tenant/media/v1/assets/{asset_id}",
    tag = "media",
    operation_id = "delete_media_asset",
    params(("asset_id" = String, Path)),
    responses(
        (status = 204, description = "Asset deleted"),
        (status = 404, description = "No such asset in this tenant's namespace", body = sanvi_http::ProblemDetail),
        (status = 403, description = "Missing media.assets.manage permission", body = sanvi_http::ProblemDetail)
    )
)]
pub async fn delete_media_asset(
    state: web::Data<MediaState>,
    tenant: CurrentTenant,
    _permission: RequiredPermission,
    asset_id: web::Path<String>,
    request: HttpRequest,
) -> Result<HttpResponse, actix_web::Error> {
    let result = state
        .delete_media_asset
        .execute(tenant.0.tenant_id, asset_id.to_string())
        .await;
    if matches!(&result, Ok(()) | Err(MediaError::NotFound { .. }))
        && let Some(runtime) =
            request.app_data::<web::Data<sanvi_http::quota_middleware::QuotaRuntime>>()
        && let Err(error) = runtime.release_asset(tenant.0.tenant_id, &asset_id).await
    {
        if result.is_ok() {
            return Err(error.into());
        }
        // A confirmed upstream 404 heals a delete whose local accounting
        // commit previously failed. Preserve its original HTTP contract.
        tracing::warn!(%error,"missing media asset quota release unavailable; retry reconciliation");
    }
    result.map_err(media_problem)?;
    Ok(HttpResponse::NoContent().finish())
}

#[utoipa::path(
    post,
    path = "/api/v1/tenant/media/v1/assets/{asset_id}/delivery",
    tag = "media",
    operation_id = "create_media_asset_delivery",
    params(("asset_id" = String, Path)),
    responses(
        (status = 200, description = "Delivery URL for the asset", body = crate::domain::AssetDeliveryView),
        (status = 404, description = "No such asset in this tenant's namespace", body = sanvi_http::ProblemDetail),
        (status = 403, description = "Missing media.assets.manage permission", body = sanvi_http::ProblemDetail)
    )
)]
pub async fn create_media_asset_delivery(
    state: web::Data<MediaState>,
    tenant: CurrentTenant,
    _permission: RequiredPermission,
    asset_id: web::Path<String>,
) -> Result<HttpResponse, actix_web::Error> {
    let view: AssetDeliveryView = state
        .create_asset_delivery
        .execute(tenant.0.tenant_id, asset_id.into_inner())
        .await
        .map_err(media_problem)?;
    Ok(HttpResponse::Ok().json(view))
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use actix_web::{App, HttpMessage, http::StatusCode, test, web};
    use async_trait::async_trait;
    use sanvi_http::{ResolutionSource, TenantContext, TenantRuntimeStatus};
    use sanvi_kernel::TenantId;

    use super::super::routes::media_tenant_scope_routes;
    use super::super::state::MediaState;
    use crate::application::{
        CompleteAssetUpload, CreateAssetDelivery, CreateAssetUpload, DeleteMediaAsset,
        GetMediaAsset, PresignAssetParts,
    };
    use crate::domain::{
        AssetDeliveryView, AssetUploadView, AssetView, CompletedPartInput, CreateAssetInput,
        MediaError, MediaGatewayPort, UploadPartView,
    };

    fn session_tenant_id() -> TenantId {
        TenantId::try_from(1_i64).unwrap()
    }

    fn session_tenant() -> TenantContext {
        TenantContext {
            tenant_id: session_tenant_id(),
            slug: "acme".to_string(),
            display_name: "Acme".to_string(),
            status: TenantRuntimeStatus::Active,
            region: "us".to_string(),
            default_locale: "en".to_string(),
            resolution_source: ResolutionSource::SessionClaim,
        }
    }

    fn test_asset_view(id: &str) -> AssetView {
        AssetView {
            id: id.to_string(),
            owner_id: None,
            external_ref: None,
            kind: "image".to_string(),
            status: "ready".to_string(),
            placeholder: None,
            duration_ms: None,
            width: None,
            height: None,
            progress_percent: 100,
            error_message: None,
            version: 1,
            updated_at: None,
        }
    }

    /// Scriptable port double: records every tenant id it is called with so
    /// tests can prove the handler forwarded the session tenant, and either
    /// answers with canned views or fails every call with `failure`.
    #[derive(Clone)]
    struct FakeGateway {
        seen_tenants: Arc<Mutex<Vec<TenantId>>>,
        failure: Option<MediaError>,
    }

    impl FakeGateway {
        fn succeeding() -> Arc<Self> {
            Arc::new(Self {
                seen_tenants: Arc::new(Mutex::new(Vec::new())),
                failure: None,
            })
        }

        fn failing(err: MediaError) -> Arc<Self> {
            Arc::new(Self {
                seen_tenants: Arc::new(Mutex::new(Vec::new())),
                failure: Some(err),
            })
        }

        fn record(&self, tenant_id: TenantId) -> Result<(), MediaError> {
            self.seen_tenants.lock().unwrap().push(tenant_id);
            match &self.failure {
                Some(err) => Err(err.clone()),
                None => Ok(()),
            }
        }
    }

    #[async_trait]
    impl MediaGatewayPort for FakeGateway {
        async fn create_asset(
            &self,
            tenant_id: TenantId,
            _idempotency_key: &str,
            _input: CreateAssetInput,
        ) -> Result<AssetUploadView, MediaError> {
            self.record(tenant_id)?;
            Ok(AssetUploadView {
                asset_id: "a1".to_string(),
                upload_id: None,
                upload_url: Some("https://s3.example/put".to_string()),
                part_size: None,
                part_urls: None,
                expires_at: "2026-09-08T00:00:00Z".to_string(),
            })
        }

        async fn presign_parts(
            &self,
            tenant_id: TenantId,
            _asset_id: &str,
            _part_numbers: Vec<i64>,
        ) -> Result<Vec<UploadPartView>, MediaError> {
            self.record(tenant_id)?;
            Ok(vec![
                UploadPartView {
                    part_number: 1,
                    url: "https://s3.example/part1".to_string(),
                },
                UploadPartView {
                    part_number: 2,
                    url: "https://s3.example/part2".to_string(),
                },
            ])
        }

        async fn complete_asset(
            &self,
            tenant_id: TenantId,
            asset_id: &str,
            _parts: Vec<CompletedPartInput>,
        ) -> Result<AssetView, MediaError> {
            self.record(tenant_id)?;
            Ok(test_asset_view(asset_id))
        }

        async fn get_asset(
            &self,
            tenant_id: TenantId,
            asset_id: &str,
        ) -> Result<AssetView, MediaError> {
            self.record(tenant_id)?;
            Ok(test_asset_view(asset_id))
        }

        async fn delete_asset(
            &self,
            tenant_id: TenantId,
            _asset_id: &str,
        ) -> Result<(), MediaError> {
            self.record(tenant_id)?;
            Ok(())
        }

        async fn create_delivery(
            &self,
            tenant_id: TenantId,
            _asset_id: &str,
        ) -> Result<AssetDeliveryView, MediaError> {
            self.record(tenant_id)?;
            Ok(AssetDeliveryView {
                url: Some("https://cdn.example/a1".to_string()),
                expires_at: Some("2026-09-08T00:00:00Z".to_string()),
                renditions: vec![],
            })
        }
    }

    fn test_state(gateway: Arc<FakeGateway>) -> MediaState {
        let port: Arc<dyn MediaGatewayPort> = gateway;
        MediaState {
            create_asset_upload: Arc::new(CreateAssetUpload::new(port.clone())),
            presign_asset_parts: Arc::new(PresignAssetParts::new(port.clone())),
            complete_asset_upload: Arc::new(CompleteAssetUpload::new(port.clone())),
            get_media_asset: Arc::new(GetMediaAsset::new(port.clone())),
            delete_media_asset: Arc::new(DeleteMediaAsset::new(port.clone())),
            create_asset_delivery: Arc::new(CreateAssetDelivery::new(port)),
        }
    }

    /// Drives a request through the real route catalog with the
    /// `CurrentTenant`/`RequiredPermission` extractors satisfied exactly the
    /// way production satisfies them: the tenant context is inserted into
    /// request extensions by middleware one layer out (copied from
    /// `payments/tests/payments_integration.rs:687-698`), and no
    /// `PermissionGate` is registered so `RequiredPermission` takes its
    /// documented no-gate public path (`authz.rs:120-122`).
    async fn send(
        method: &str,
        uri: &str,
        body: Option<serde_json::Value>,
        extra_headers: Vec<(String, String)>,
        gateway: Arc<FakeGateway>,
    ) -> (StatusCode, Vec<u8>) {
        let tenant = session_tenant();
        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(test_state(gateway)))
                .wrap(actix_web::middleware::from_fn(
                    move |req: actix_web::dev::ServiceRequest,
                          srv: actix_web::middleware::Next<actix_web::body::BoxBody>| {
                        let tenant = tenant.clone();
                        async move {
                            req.extensions_mut().insert(tenant);
                            srv.call(req).await
                        }
                    },
                ))
                .service(web::scope("/api/v1/tenant").configure(media_tenant_scope_routes)),
        )
        .await;
        let mut builder = match method {
            "GET" => test::TestRequest::get(),
            "POST" => test::TestRequest::post(),
            "DELETE" => test::TestRequest::delete(),
            other => panic!("unsupported test method {other}"),
        }
        .uri(uri);
        for (name, value) in extra_headers {
            builder = builder.insert_header((name, value));
        }
        let req = match body {
            Some(json) => builder.set_json(json).to_request(),
            None => builder.to_request(),
        };
        let res = test::call_service(&app, req).await;
        let status = res.status();
        let bytes = test::read_body(res).await.to_vec();
        (status, bytes)
    }

    fn json_body(bytes: &[u8]) -> serde_json::Value {
        serde_json::from_slice(bytes).expect("handler must return a JSON body")
    }

    fn create_body() -> serde_json::Value {
        serde_json::json!({
            "kind": "image",
            "content_type": "image/png",
            "filename": "a.png",
            "size_bytes": 10
        })
    }

    #[actix_web::test]
    async fn create_returns_201_with_the_gateway_view_for_the_session_tenant() {
        let gateway = FakeGateway::succeeding();
        let (status, bytes) = send(
            "POST",
            "/api/v1/tenant/v1/assets",
            Some(create_body()),
            vec![("Idempotency-Key".to_string(), "idem-1".to_string())],
            gateway.clone(),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        let body = json_body(&bytes);
        assert_eq!(body["asset_id"], "a1");
        assert_eq!(body["upload_url"], "https://s3.example/put");
    }

    #[actix_web::test]
    async fn create_ignores_a_spoofed_namespace_header_and_uses_the_session_tenant() {
        let gateway = FakeGateway::succeeding();
        let (status, _) = send(
            "POST",
            "/api/v1/tenant/v1/assets",
            Some(create_body()),
            vec![
                ("Idempotency-Key".to_string(), "idem-1".to_string()),
                ("X-Namespace-ID".to_string(), "9999".to_string()),
            ],
            gateway.clone(),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        let seen = gateway.seen_tenants.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0], session_tenant_id());
    }

    #[actix_web::test]
    async fn create_maps_bad_request_with_code_in_the_body() {
        let gateway = FakeGateway::failing(MediaError::BadRequest {
            code: "invalid_input".to_string(),
            detail: "unsupported image format".to_string(),
        });
        let (status, bytes) = send(
            "POST",
            "/api/v1/tenant/v1/assets",
            Some(create_body()),
            vec![("Idempotency-Key".to_string(), "idem-fail".to_string())],
            gateway,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let body = json_body(&bytes);
        assert_eq!(body["code"], "invalid_input");
    }

    #[actix_web::test]
    async fn create_returns_400_when_idempotency_key_is_missing_without_calling_gateway() {
        let gateway = FakeGateway::succeeding();
        let (status, bytes) = send(
            "POST",
            "/api/v1/tenant/v1/assets",
            Some(create_body()),
            vec![], // No Idempotency-Key header
            gateway.clone(),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let body = json_body(&bytes);
        assert_eq!(body["detail"], "Idempotency-Key header is required");
        // Ensure gateway was never called
        assert!(gateway.seen_tenants.lock().unwrap().is_empty());

        // Also test empty string header
        let (status_empty, bytes_empty) = send(
            "POST",
            "/api/v1/tenant/v1/assets",
            Some(create_body()),
            vec![("Idempotency-Key".to_string(), "   ".to_string())],
            gateway.clone(),
        )
        .await;
        assert_eq!(status_empty, StatusCode::BAD_REQUEST);
        let body_empty = json_body(&bytes_empty);
        assert_eq!(body_empty["detail"], "Idempotency-Key header is required");
        assert!(gateway.seen_tenants.lock().unwrap().is_empty());
    }

    #[actix_web::test]
    async fn presign_returns_part_urls_wrapped_in_an_object() {
        let gateway = FakeGateway::succeeding();
        let (status, bytes) = send(
            "POST",
            "/api/v1/tenant/v1/assets/a1/parts",
            Some(serde_json::json!({ "part_numbers": [1, 2] })),
            vec![],
            gateway,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let body = json_body(&bytes);
        assert_eq!(body["part_urls"].as_array().unwrap().len(), 2);
        assert_eq!(body["part_urls"][0]["url"], "https://s3.example/part1");
    }

    #[actix_web::test]
    async fn presign_maps_not_found_with_code_in_the_body() {
        let gateway = FakeGateway::failing(MediaError::NotFound {
            code: "not_found".to_string(),
            detail: "asset nope does not exist".to_string(),
        });
        let (status, bytes) = send(
            "POST",
            "/api/v1/tenant/v1/assets/nope/parts",
            Some(serde_json::json!({ "part_numbers": [1] })),
            vec![],
            gateway,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let body = json_body(&bytes);
        assert_eq!(body["code"], "not_found");
    }

    #[actix_web::test]
    async fn complete_returns_the_asset_view() {
        let gateway = FakeGateway::succeeding();
        let (status, bytes) = send(
            "POST",
            "/api/v1/tenant/v1/assets/a1/complete",
            Some(serde_json::json!({ "parts": [{ "part_number": 1, "etag": "etag-1" }] })),
            vec![],
            gateway,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let body = json_body(&bytes);
        assert_eq!(body["id"], "a1");
        assert_eq!(body["status"], "ready");
    }

    #[actix_web::test]
    async fn complete_maps_conflict_with_code_in_the_body() {
        let gateway = FakeGateway::failing(MediaError::Conflict {
            code: "already_completed".to_string(),
            detail: "upload for asset a1 is already completed".to_string(),
        });
        let (status, bytes) = send(
            "POST",
            "/api/v1/tenant/v1/assets/a1/complete",
            Some(serde_json::json!({ "parts": [{ "part_number": 1, "etag": "etag-1" }] })),
            vec![],
            gateway,
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        let body = json_body(&bytes);
        assert_eq!(body["code"], "already_completed");
    }

    #[actix_web::test]
    async fn get_returns_the_current_asset_state() {
        let gateway = FakeGateway::succeeding();
        let (status, bytes) =
            send("GET", "/api/v1/tenant/v1/assets/a1", None, vec![], gateway).await;
        assert_eq!(status, StatusCode::OK);
        let body = json_body(&bytes);
        assert_eq!(body["id"], "a1");
        assert_eq!(body["status"], "ready");
    }

    #[actix_web::test]
    async fn get_maps_not_found_with_code_in_the_body() {
        let gateway = FakeGateway::failing(MediaError::NotFound {
            code: "not_found".to_string(),
            detail: "asset nope does not exist".to_string(),
        });
        let (status, bytes) = send(
            "GET",
            "/api/v1/tenant/v1/assets/nope",
            None,
            vec![],
            gateway,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let body = json_body(&bytes);
        assert_eq!(body["code"], "not_found");
    }

    #[actix_web::test]
    async fn delete_returns_204_on_success() {
        let gateway = FakeGateway::succeeding();
        let (status, bytes) = send(
            "DELETE",
            "/api/v1/tenant/v1/assets/a1",
            None,
            vec![],
            gateway.clone(),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        assert!(bytes.is_empty());
        let seen = gateway.seen_tenants.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0], session_tenant_id());
    }

    #[actix_web::test]
    async fn delete_maps_not_found_with_code_in_the_body() {
        let gateway = FakeGateway::failing(MediaError::NotFound {
            code: "not_found".to_string(),
            detail: "asset nope does not exist".to_string(),
        });
        let (status, bytes) = send(
            "DELETE",
            "/api/v1/tenant/v1/assets/nope",
            None,
            vec![],
            gateway,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let body = json_body(&bytes);
        assert_eq!(body["code"], "not_found");
    }

    #[actix_web::test]
    async fn delivery_returns_the_delivery_view() {
        let gateway = FakeGateway::succeeding();
        let (status, bytes) = send(
            "POST",
            "/api/v1/tenant/v1/assets/a1/delivery",
            None,
            vec![],
            gateway,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let body = json_body(&bytes);
        assert_eq!(body["url"], "https://cdn.example/a1");
    }

    #[actix_web::test]
    async fn delivery_maps_unavailable_to_503_with_code_in_the_body() {
        let gateway = FakeGateway::failing(MediaError::Unavailable {
            detail: "service is temporarily offline".to_string(),
        });
        let (status, bytes) = send(
            "POST",
            "/api/v1/tenant/v1/assets/a1/delivery",
            None,
            vec![],
            gateway,
        )
        .await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        let body = json_body(&bytes);
        assert_eq!(body["code"], "media_unavailable");
    }
}
