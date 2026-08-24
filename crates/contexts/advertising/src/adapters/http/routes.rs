use actix_web::{http::Method, web};
use sanvi_http::{PermissionMap, ResourceSpec, require_permission};

use super::handlers::{
    add_ad, add_ad_group, create_campaign, get_campaign, list_ad_platforms, list_campaign_changes,
    list_campaigns, patch_campaign, pause_campaign, publish_campaign, resolve_campaign_drift,
    resume_campaign, update_ad, update_ad_group, validate_campaign,
};

/// A catalog is available when at least one platform can be used. Individual
/// platform entries remain gated by their own entitlement in later slices.
pub const ADVERTISING_ENTITLEMENTS: &[&str] = &["advertising.google_ads", "advertising.meta_ads"];

pub fn advertising_tenant_scope_routes(cfg: &mut web::ServiceConfig) {
    let read = require_permission!("advertising.read", tenant);
    let write = require_permission!("advertising.campaign.write", tenant);
    let feature_gate = || sanvi_http::RequiredAnyFeature::declared(ADVERTISING_ENTITLEMENTS);

    cfg.service(
        web::resource("/ads/platforms")
            .app_data(PermissionMap::new(vec![(Method::GET, read)]))
            .app_data(feature_gate())
            .route(web::get().to(list_ad_platforms)),
    );
    cfg.service(
        web::resource("/ads/campaigns")
            .app_data(PermissionMap::new(vec![
                (Method::GET, read),
                (Method::POST, write),
            ]))
            .route(web::get().to(list_campaigns))
            .route(web::post().to(create_campaign)),
    );
    cfg.service(
        web::resource("/ads/campaigns/{campaign_id}")
            .app_data(PermissionMap::new(vec![
                (Method::GET, read),
                (Method::PATCH, write),
            ]))
            .route(web::get().to(get_campaign))
            .route(web::patch().to(patch_campaign)),
    );
    cfg.service(
        web::resource("/ads/campaigns/{campaign_id}/validate")
            .app_data(PermissionMap::new(vec![(Method::POST, write)]))
            .route(web::post().to(validate_campaign)),
    );
    cfg.service(
        web::resource("/ads/campaigns/{campaign_id}/publish")
            .app_data(PermissionMap::new(vec![(Method::POST, write)]))
            .route(web::post().to(publish_campaign)),
    );
    cfg.service(
        web::resource("/ads/campaigns/{campaign_id}/pause")
            .app_data(PermissionMap::new(vec![(Method::POST, write)]))
            .route(web::post().to(pause_campaign)),
    );
    cfg.service(
        web::resource("/ads/campaigns/{campaign_id}/resume")
            .app_data(PermissionMap::new(vec![(Method::POST, write)]))
            .route(web::post().to(resume_campaign)),
    );
    cfg.service(
        web::resource("/ads/campaigns/{campaign_id}/changes")
            .app_data(PermissionMap::new(vec![(Method::GET, read)]))
            .route(web::get().to(list_campaign_changes)),
    );
    cfg.service(
        web::resource("/ads/campaigns/{campaign_id}/drift")
            .app_data(PermissionMap::new(vec![(Method::POST, write)]))
            .route(web::post().to(resolve_campaign_drift)),
    );
    cfg.service(
        web::resource("/ads/campaigns/{campaign_id}/ad-groups")
            .app_data(PermissionMap::new(vec![(Method::POST, write)]))
            .route(web::post().to(add_ad_group)),
    );
    cfg.service(
        web::resource("/ads/campaigns/{campaign_id}/ad-groups/{ad_group_id}")
            .app_data(PermissionMap::new(vec![(Method::PATCH, write)]))
            .route(web::patch().to(update_ad_group)),
    );
    cfg.service(
        web::resource("/ads/campaigns/{campaign_id}/ad-groups/{ad_group_id}/ads")
            .app_data(PermissionMap::new(vec![(Method::POST, write)]))
            .route(web::post().to(add_ad)),
    );
    cfg.service(
        web::resource("/ads/campaigns/{campaign_id}/ad-groups/{ad_group_id}/ads/{ad_id}")
            .app_data(PermissionMap::new(vec![(Method::PATCH, write)]))
            .route(web::patch().to(update_ad)),
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
        "/api/v1/tenant/ads/campaigns",
        Method::GET,
        "advertising.read",
        ResourceSpec::Tenant,
    ),
    (
        "/api/v1/tenant/ads/campaigns",
        Method::POST,
        "advertising.campaign.write",
        ResourceSpec::Tenant,
    ),
    (
        "/api/v1/tenant/ads/campaigns/{campaign_id}",
        Method::GET,
        "advertising.read",
        ResourceSpec::Tenant,
    ),
    (
        "/api/v1/tenant/ads/campaigns/{campaign_id}",
        Method::PATCH,
        "advertising.campaign.write",
        ResourceSpec::Tenant,
    ),
    (
        "/api/v1/tenant/ads/campaigns/{campaign_id}/validate",
        Method::POST,
        "advertising.campaign.write",
        ResourceSpec::Tenant,
    ),
    (
        "/api/v1/tenant/ads/campaigns/{campaign_id}/publish",
        Method::POST,
        "advertising.campaign.write",
        ResourceSpec::Tenant,
    ),
    (
        "/api/v1/tenant/ads/campaigns/{campaign_id}/pause",
        Method::POST,
        "advertising.campaign.write",
        ResourceSpec::Tenant,
    ),
    (
        "/api/v1/tenant/ads/campaigns/{campaign_id}/resume",
        Method::POST,
        "advertising.campaign.write",
        ResourceSpec::Tenant,
    ),
    (
        "/api/v1/tenant/ads/campaigns/{campaign_id}/changes",
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
    (
        "/api/v1/tenant/ads/campaigns/{campaign_id}/ad-groups",
        Method::POST,
        "advertising.campaign.write",
        ResourceSpec::Tenant,
    ),
    (
        "/api/v1/tenant/ads/campaigns/{campaign_id}/ad-groups/{ad_group_id}",
        Method::PATCH,
        "advertising.campaign.write",
        ResourceSpec::Tenant,
    ),
    (
        "/api/v1/tenant/ads/campaigns/{campaign_id}/ad-groups/{ad_group_id}/ads",
        Method::POST,
        "advertising.campaign.write",
        ResourceSpec::Tenant,
    ),
    (
        "/api/v1/tenant/ads/campaigns/{campaign_id}/ad-groups/{ad_group_id}/ads/{ad_id}",
        Method::PATCH,
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
        "/api/v1/tenant/ads/campaigns",
        Method::GET,
        ADVERTISING_ENTITLEMENTS,
    ),
    (
        "/api/v1/tenant/ads/campaigns",
        Method::POST,
        ADVERTISING_ENTITLEMENTS,
    ),
    (
        "/api/v1/tenant/ads/campaigns/{campaign_id}",
        Method::GET,
        ADVERTISING_ENTITLEMENTS,
    ),
    (
        "/api/v1/tenant/ads/campaigns/{campaign_id}",
        Method::PATCH,
        ADVERTISING_ENTITLEMENTS,
    ),
    (
        "/api/v1/tenant/ads/campaigns/{campaign_id}/validate",
        Method::POST,
        ADVERTISING_ENTITLEMENTS,
    ),
    (
        "/api/v1/tenant/ads/campaigns/{campaign_id}/publish",
        Method::POST,
        ADVERTISING_ENTITLEMENTS,
    ),
    (
        "/api/v1/tenant/ads/campaigns/{campaign_id}/pause",
        Method::POST,
        ADVERTISING_ENTITLEMENTS,
    ),
    (
        "/api/v1/tenant/ads/campaigns/{campaign_id}/resume",
        Method::POST,
        ADVERTISING_ENTITLEMENTS,
    ),
    (
        "/api/v1/tenant/ads/campaigns/{campaign_id}/changes",
        Method::GET,
        ADVERTISING_ENTITLEMENTS,
    ),
    (
        "/api/v1/tenant/ads/campaigns/{campaign_id}/drift",
        Method::POST,
        ADVERTISING_ENTITLEMENTS,
    ),
    (
        "/api/v1/tenant/ads/campaigns/{campaign_id}/ad-groups",
        Method::POST,
        ADVERTISING_ENTITLEMENTS,
    ),
    (
        "/api/v1/tenant/ads/campaigns/{campaign_id}/ad-groups/{ad_group_id}",
        Method::PATCH,
        ADVERTISING_ENTITLEMENTS,
    ),
    (
        "/api/v1/tenant/ads/campaigns/{campaign_id}/ad-groups/{ad_group_id}/ads",
        Method::POST,
        ADVERTISING_ENTITLEMENTS,
    ),
    (
        "/api/v1/tenant/ads/campaigns/{campaign_id}/ad-groups/{ad_group_id}/ads/{ad_id}",
        Method::PATCH,
        ADVERTISING_ENTITLEMENTS,
    ),
];
