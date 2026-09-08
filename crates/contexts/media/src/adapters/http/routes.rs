use actix_web::{http::Method, web};
use sanvi_http::{PermissionMap, require_permission};

use super::handlers::{
    complete_media_asset_upload, create_media_asset, create_media_asset_delivery,
    delete_media_asset, get_media_asset, presign_media_asset_parts,
};

pub fn media_tenant_scope_routes(cfg: &mut web::ServiceConfig) {
    let manage = require_permission!("media.assets.manage", tenant);

    cfg.service(
        web::resource("/v1/assets")
            .app_data(PermissionMap::new(vec![(Method::POST, manage)]))
            .route(web::post().to(create_media_asset)),
    );
    cfg.service(
        web::resource("/v1/assets/{asset_id}")
            .app_data(PermissionMap::new(vec![
                (Method::GET, manage),
                (Method::DELETE, manage),
            ]))
            .route(web::get().to(get_media_asset))
            .route(web::delete().to(delete_media_asset)),
    );
    cfg.service(
        web::resource("/v1/assets/{asset_id}/parts")
            .app_data(PermissionMap::new(vec![(Method::POST, manage)]))
            .route(web::post().to(presign_media_asset_parts)),
    );
    cfg.service(
        web::resource("/v1/assets/{asset_id}/complete")
            .app_data(PermissionMap::new(vec![(Method::POST, manage)]))
            .route(web::post().to(complete_media_asset_upload)),
    );
    cfg.service(
        web::resource("/v1/assets/{asset_id}/delivery")
            .app_data(PermissionMap::new(vec![(Method::POST, manage)]))
            .route(web::post().to(create_media_asset_delivery)),
    );
}
