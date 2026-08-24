use actix_web::{http::Method, web};
use sanvi_http::{PermissionMap, ResourceSpec, require_permission};

use super::handlers::{list_ad_platforms, resolve_campaign_drift};

/// A catalog is available when at least one platform can be used. Individual
/// platform entries remain gated by their own entitlement in later slices.
pub const ADVERTISING_ENTITLEMENTS: &[&str] = &["advertising.google_ads", "advertising.meta_ads"];

pub fn advertising_tenant_scope_routes(cfg: &mut web::ServiceConfig) {
    let read = require_permission!("advertising.read", tenant);
    let write = require_permission!("advertising.campaign.write", tenant);
    cfg.service(
        web::resource("/ads/platforms")
            .app_data(PermissionMap::new(vec![(Method::GET, read)]))
            .app_data(sanvi_http::RequiredAnyFeature::declared(
                ADVERTISING_ENTITLEMENTS,
            ))
            .route(web::get().to(list_ad_platforms)),
    );
    cfg.service(
        web::resource("/ads/campaigns/{campaign_id}/drift")
            .app_data(PermissionMap::new(vec![(Method::POST, write)]))
            .app_data(sanvi_http::RequiredAnyFeature::declared(
                ADVERTISING_ENTITLEMENTS,
            ))
            .route(web::post().to(resolve_campaign_drift)),
    );
}

pub const PERMISSIONED_ROUTES: &[(&str, Method, &str, ResourceSpec)] = &[
    (
        "/api/v1/tenant/ads/platforms",
        Method::GET,
        "advertising.read",
        ResourceSpec::Tenant,
    ),
    (
        "/api/v1/tenant/ads/campaigns/{campaign_id}/drift",
        Method::POST,
        "advertising.campaign.write",
        ResourceSpec::Tenant,
    ),
];

pub const FEATURE_GATED_ROUTES: &[(&str, Method, &[&str])] = &[
    (
        "/api/v1/tenant/ads/platforms",
        Method::GET,
        ADVERTISING_ENTITLEMENTS,
    ),
    (
        "/api/v1/tenant/ads/campaigns/{campaign_id}/drift",
        Method::POST,
        ADVERTISING_ENTITLEMENTS,
    ),
];
