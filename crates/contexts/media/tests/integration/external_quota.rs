use std::sync::Arc;

use actix_web::{App, HttpMessage, http::StatusCode, test, web};
use async_trait::async_trait;
use sanvi_http::{
    quota_middleware::QuotaRuntime,
    quotas::{PgQuotaPlanProvider, QuotaDimension, QuotaService},
    tenancy::{ResolutionSource, TenantContext, TenantRuntimeStatus},
};
use sanvi_kernel::TenantId;
use sanvi_media::{
    adapters::http::MediaState, application::*, domain::*, routes::media_tenant_scope_routes,
};

struct Gateway(MediaError);

#[async_trait]
impl MediaGatewayPort for Gateway {
    async fn create_asset(
        &self,
        _: TenantId,
        _: &str,
        _: CreateAssetInput,
    ) -> Result<AssetUploadView, MediaError> {
        Err(self.0.clone())
    }
    async fn delete_asset(&self, _: TenantId, _: &str) -> Result<(), MediaError> {
        Err(self.0.clone())
    }
    async fn presign_parts(
        &self,
        _: TenantId,
        _: &str,
        _: Vec<i64>,
    ) -> Result<Vec<UploadPartView>, MediaError> {
        unreachable!()
    }
    async fn complete_asset(
        &self,
        _: TenantId,
        _: &str,
        _: Vec<CompletedPartInput>,
    ) -> Result<AssetView, MediaError> {
        unreachable!()
    }
    async fn get_asset(&self, _: TenantId, _: &str) -> Result<AssetView, MediaError> {
        unreachable!()
    }
    async fn create_delivery(&self, _: TenantId, _: &str) -> Result<AssetDeliveryView, MediaError> {
        unreachable!()
    }
}

async fn fixture() -> (sanvi_testsupport::TestDb, QuotaRuntime, TenantContext) {
    let db = sanvi_testsupport::test_db().await;
    let tenant_id = TenantId::try_from(600_i64).unwrap();
    sqlx::query("INSERT INTO tenancy.tenants(id,slug,display_name,status,region) VALUES($1,'media-quota','Media quota','active','jp')")
        .bind(tenant_id.as_i64()).execute(&db.admin_pool().await).await.unwrap();
    let tx = sanvi_persistence::TxManager::new(db.app_pool(), sanvi_testsupport::test_ids());
    let runtime = QuotaRuntime {
        service: Arc::new(QuotaService::new(
            tx.clone(),
            Arc::new(PgQuotaPlanProvider::new(tx)),
            None,
        )),
        enabled: false,
    };
    let tenant = TenantContext {
        tenant_id,
        slug: "media-quota".into(),
        display_name: "Media quota".into(),
        status: TenantRuntimeStatus::Active,
        region: "jp".into(),
        default_locale: "en".into(),
        resolution_source: ResolutionSource::SessionClaim,
    };
    (db, runtime, tenant)
}

async fn request(
    runtime: &QuotaRuntime,
    tenant: TenantContext,
    failure: MediaError,
    delete: bool,
) -> StatusCode {
    let port: Arc<dyn MediaGatewayPort> = Arc::new(Gateway(failure));
    let state = MediaState {
        create_asset_upload: Arc::new(CreateAssetUpload::new(port.clone())),
        presign_asset_parts: Arc::new(PresignAssetParts::new(port.clone())),
        complete_asset_upload: Arc::new(CompleteAssetUpload::new(port.clone())),
        get_media_asset: Arc::new(GetMediaAsset::new(port.clone())),
        delete_media_asset: Arc::new(DeleteMediaAsset::new(port.clone())),
        create_asset_delivery: Arc::new(CreateAssetDelivery::new(port)),
    };
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(state))
            .app_data(web::Data::new(runtime.clone()))
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
    let request = if delete {
        test::TestRequest::delete()
            .uri("/api/v1/tenant/v1/assets/gone")
            .to_request()
    } else {
        test::TestRequest::post().uri("/api/v1/tenant/v1/assets")
            .insert_header(("Idempotency-Key", "upload-key"))
            .set_json(serde_json::json!({"kind":"image","content_type":"image/png","filename":"a.png","size_bytes":100,"owner_id":"user:1"})).to_request()
    };
    test::call_service(&app, request).await.status()
}

async fn used(runtime: &QuotaRuntime, tenant: TenantId) -> u64 {
    runtime.service.usage(tenant).await.unwrap().dimensions[&QuotaDimension::StorageBytes].used
}

#[actix_web::test]
async fn acknowledged_missing_asset_delete_repairs_storage_usage_and_keeps_404() {
    let (_db, runtime, tenant) = fixture().await;
    runtime
        .reserve_external(
            tenant.tenant_id,
            QuotaDimension::StorageBytes,
            "deleted-upload",
            100,
        )
        .await
        .unwrap();
    runtime
        .bind_asset(tenant.tenant_id, "deleted-upload", "gone")
        .await
        .unwrap();
    assert_eq!(
        request(
            &runtime,
            tenant.clone(),
            MediaError::NotFound {
                code: "not_found".into(),
                detail: "asset already deleted".into()
            },
            true
        )
        .await,
        StatusCode::NOT_FOUND
    );
    assert_eq!(used(&runtime, tenant.tenant_id).await, 0);
}

#[actix_web::test]
async fn rejected_media_create_refunds_the_reservation_and_keeps_400() {
    let (_db, runtime, tenant) = fixture().await;
    assert_eq!(
        request(
            &runtime,
            tenant.clone(),
            MediaError::BadRequest {
                code: "invalid_input".into(),
                detail: "unsupported format".into()
            },
            false
        )
        .await,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(used(&runtime, tenant.tenant_id).await, 0);
}

#[actix_web::test]
async fn ambiguous_media_create_does_not_refund_after_a_later_rejection() {
    let (_db, runtime, tenant) = fixture().await;
    assert_eq!(
        request(
            &runtime,
            tenant.clone(),
            MediaError::Unavailable {
                detail: "response lost".into()
            },
            false
        )
        .await,
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(used(&runtime, tenant.tenant_id).await, 100);
    assert_eq!(
        request(
            &runtime,
            tenant.clone(),
            MediaError::BadRequest {
                code: "invalid_input".into(),
                detail: "unsupported format".into()
            },
            false
        )
        .await,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(used(&runtime, tenant.tenant_id).await, 100);
}
