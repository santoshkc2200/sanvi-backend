use actix_web::{http::Method, web};
use sanvi_http::{PermissionMap, ResourceSpec, require_permission};

use super::handlers::{
    acknowledge_budget_alert, add_ad, add_ad_group, create_campaign, create_connection,
    create_creative, delete_connection, delete_creative, get_ads_metrics, get_ads_metrics_export,
    get_ads_metrics_freshness, get_ads_metrics_summary, get_budget_alerts, get_budget_caps,
    get_campaign, get_campaign_budget_cap, get_creative, get_creative_previews,
    get_platform_ads_health, get_spend_status, list_ad_platforms, list_campaign_changes,
    list_campaigns, list_connections, list_creatives, oauth_callback, patch_campaign,
    pause_campaign, publish_campaign, put_budget_cap, put_campaign_budget_cap,
    resolve_campaign_drift, resume_campaign, start_oauth, update_ad, update_ad_group,
    validate_campaign,
};

/// A catalog is available when at least one platform can be used. Individual
/// platform entries remain gated by their own entitlement in later slices.
pub const ADVERTISING_ENTITLEMENTS: &[&str] = &["advertising.google_ads", "advertising.meta_ads"];

pub fn advertising_tenant_scope_routes(cfg: &mut web::ServiceConfig) {
    let read = require_permission!("advertising.read", tenant);
    let write = require_permission!("advertising.campaign.write", tenant);
    let connect = require_permission!("advertising.connect", tenant);
    let metrics_read = require_permission!("advertising.metrics.read", tenant);
    let feature_gate = || sanvi_http::RequiredAnyFeature::declared(ADVERTISING_ENTITLEMENTS);

    cfg.service(
        web::resource("/ads/platforms")
            .app_data(PermissionMap::new(vec![(Method::GET, read)]))
            .app_data(feature_gate())
            .route(web::get().to(list_ad_platforms)),
    );
    cfg.service(
        web::resource("/ads/connections/{platform}/oauth/start")
            .app_data(PermissionMap::new(vec![(Method::POST, connect)]))
            .route(web::post().to(start_oauth)),
    );
    cfg.service(
        web::resource("/ads/connections/{platform}/oauth/callback")
            .app_data(PermissionMap::new(vec![(Method::GET, connect)]))
            .route(web::get().to(oauth_callback)),
    );
    cfg.service(
        web::resource("/ads/connections")
            .app_data(PermissionMap::new(vec![
                (Method::GET, read),
                (Method::POST, connect),
            ]))
            .route(web::get().to(list_connections))
            .route(web::post().to(create_connection)),
    );
    cfg.service(
        web::resource("/ads/connections/{connection_id}")
            .app_data(PermissionMap::new(vec![(Method::DELETE, connect)]))
            .route(web::delete().to(delete_connection)),
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
    cfg.service(
        web::resource("/ads/creatives")
            .app_data(PermissionMap::new(vec![
                (Method::GET, read),
                (Method::POST, write),
            ]))
            .app_data(feature_gate())
            .route(web::get().to(list_creatives))
            .route(web::post().to(create_creative)),
    );
    cfg.service(
        web::resource("/ads/creatives/{creative_id}")
            .app_data(PermissionMap::new(vec![
                (Method::GET, read),
                (Method::DELETE, write),
            ]))
            .app_data(feature_gate())
            .route(web::get().to(get_creative))
            .route(web::delete().to(delete_creative)),
    );
    cfg.service(
        web::resource("/ads/creatives/{creative_id}/previews")
            .app_data(PermissionMap::new(vec![(Method::GET, read)]))
            .app_data(feature_gate())
            .route(web::get().to(get_creative_previews)),
    );
    cfg.service(
        web::resource("/ads/metrics")
            .app_data(PermissionMap::new(vec![(Method::GET, metrics_read)]))
            .route(web::get().to(get_ads_metrics)),
    );
    cfg.service(
        web::resource("/ads/metrics/summary")
            .app_data(PermissionMap::new(vec![(Method::GET, metrics_read)]))
            .route(web::get().to(get_ads_metrics_summary)),
    );
    cfg.service(
        web::resource("/ads/metrics/export")
            .app_data(PermissionMap::new(vec![(Method::GET, metrics_read)]))
            .route(web::get().to(get_ads_metrics_export)),
    );
    cfg.service(
        web::resource("/ads/metrics/freshness")
            .app_data(PermissionMap::new(vec![(Method::GET, metrics_read)]))
            .route(web::get().to(get_ads_metrics_freshness)),
    );
    let budget_manage = require_permission!("advertising.budget.manage", tenant);

    cfg.service(
        web::resource("/ads/budget-caps")
            .app_data(PermissionMap::new(vec![
                (Method::GET, read),
                (Method::PUT, budget_manage),
            ]))
            .route(web::get().to(get_budget_caps))
            .route(web::put().to(put_budget_cap)),
    );
    cfg.service(
        web::resource("/ads/campaigns/{campaign_id}/budget-cap")
            .app_data(PermissionMap::new(vec![
                (Method::GET, read),
                (Method::PUT, budget_manage),
            ]))
            .route(web::get().to(get_campaign_budget_cap))
            .route(web::put().to(put_campaign_budget_cap)),
    );
    cfg.service(
        web::resource("/ads/budget-alerts")
            .app_data(PermissionMap::new(vec![(Method::GET, read)]))
            .route(web::get().to(get_budget_alerts)),
    );
    cfg.service(
        web::resource("/ads/budget-alerts/{alert_id}/acknowledge")
            .app_data(PermissionMap::new(vec![(Method::POST, budget_manage)]))
            .route(web::post().to(acknowledge_budget_alert)),
    );
    cfg.service(
        web::resource("/ads/spend-status")
            .app_data(PermissionMap::new(vec![(Method::GET, read)]))
            .route(web::get().to(get_spend_status)),
    );
}

pub fn advertising_platform_routes(cfg: &mut web::ServiceConfig) {
    let read = require_permission!("platform.tenant.read", platform);
    cfg.service(
        web::resource("/ads/health")
            .app_data(PermissionMap::new(vec![(Method::GET, read)]))
            .route(web::get().to(get_platform_ads_health)),
    );
}

pub const PERMISSIONED_ROUTES: &[(&str, Method, &str, ResourceSpec)] = &[
    (
        "/api/v1/platform/ads/health",
        Method::GET,
        "platform.tenant.read",
        ResourceSpec::Platform,
    ),
    (
        "/api/v1/tenant/ads/platforms",
        Method::GET,
        "advertising.read",
        ResourceSpec::Tenant,
    ),
    (
        "/api/v1/tenant/ads/connections/{platform}/oauth/start",
        Method::POST,
        "advertising.connect",
        ResourceSpec::Tenant,
    ),
    (
        "/api/v1/tenant/ads/connections/{platform}/oauth/callback",
        Method::GET,
        "advertising.connect",
        ResourceSpec::Tenant,
    ),
    (
        "/api/v1/tenant/ads/connections",
        Method::GET,
        "advertising.read",
        ResourceSpec::Tenant,
    ),
    (
        "/api/v1/tenant/ads/connections",
        Method::POST,
        "advertising.connect",
        ResourceSpec::Tenant,
    ),
    (
        "/api/v1/tenant/ads/connections/{connection_id}",
        Method::DELETE,
        "advertising.connect",
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
    (
        "/api/v1/tenant/ads/creatives",
        Method::GET,
        "advertising.read",
        ResourceSpec::Tenant,
    ),
    (
        "/api/v1/tenant/ads/creatives",
        Method::POST,
        "advertising.campaign.write",
        ResourceSpec::Tenant,
    ),
    (
        "/api/v1/tenant/ads/creatives/{creative_id}",
        Method::GET,
        "advertising.read",
        ResourceSpec::Tenant,
    ),
    (
        "/api/v1/tenant/ads/creatives/{creative_id}",
        Method::DELETE,
        "advertising.campaign.write",
        ResourceSpec::Tenant,
    ),
    (
        "/api/v1/tenant/ads/creatives/{creative_id}/previews",
        Method::GET,
        "advertising.read",
        ResourceSpec::Tenant,
    ),
    (
        "/api/v1/tenant/ads/budget-caps",
        Method::GET,
        "advertising.read",
        ResourceSpec::Tenant,
    ),
    (
        "/api/v1/tenant/ads/budget-caps",
        Method::PUT,
        "advertising.budget.manage",
        ResourceSpec::Tenant,
    ),
    (
        "/api/v1/tenant/ads/campaigns/{campaign_id}/budget-cap",
        Method::GET,
        "advertising.read",
        ResourceSpec::Tenant,
    ),
    (
        "/api/v1/tenant/ads/campaigns/{campaign_id}/budget-cap",
        Method::PUT,
        "advertising.budget.manage",
        ResourceSpec::Tenant,
    ),
    (
        "/api/v1/tenant/ads/budget-alerts",
        Method::GET,
        "advertising.read",
        ResourceSpec::Tenant,
    ),
    (
        "/api/v1/tenant/ads/budget-alerts/{alert_id}/acknowledge",
        Method::POST,
        "advertising.budget.manage",
        ResourceSpec::Tenant,
    ),
    (
        "/api/v1/tenant/ads/spend-status",
        Method::GET,
        "advertising.read",
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
        "/api/v1/tenant/ads/connections/{platform}/oauth/start",
        Method::POST,
        ADVERTISING_ENTITLEMENTS,
    ),
    (
        "/api/v1/tenant/ads/connections/{platform}/oauth/callback",
        Method::GET,
        ADVERTISING_ENTITLEMENTS,
    ),
    (
        "/api/v1/tenant/ads/connections",
        Method::GET,
        ADVERTISING_ENTITLEMENTS,
    ),
    (
        "/api/v1/tenant/ads/connections",
        Method::POST,
        ADVERTISING_ENTITLEMENTS,
    ),
    (
        "/api/v1/tenant/ads/connections/{connection_id}",
        Method::DELETE,
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
    (
        "/api/v1/tenant/ads/creatives",
        Method::GET,
        ADVERTISING_ENTITLEMENTS,
    ),
    (
        "/api/v1/tenant/ads/creatives",
        Method::POST,
        ADVERTISING_ENTITLEMENTS,
    ),
    (
        "/api/v1/tenant/ads/creatives/{creative_id}",
        Method::GET,
        ADVERTISING_ENTITLEMENTS,
    ),
    (
        "/api/v1/tenant/ads/creatives/{creative_id}",
        Method::DELETE,
        ADVERTISING_ENTITLEMENTS,
    ),
    (
        "/api/v1/tenant/ads/creatives/{creative_id}/previews",
        Method::GET,
        ADVERTISING_ENTITLEMENTS,
    ),
    (
        "/api/v1/tenant/ads/budget-caps",
        Method::GET,
        ADVERTISING_ENTITLEMENTS,
    ),
    (
        "/api/v1/tenant/ads/budget-caps",
        Method::PUT,
        ADVERTISING_ENTITLEMENTS,
    ),
    (
        "/api/v1/tenant/ads/campaigns/{campaign_id}/budget-cap",
        Method::GET,
        ADVERTISING_ENTITLEMENTS,
    ),
    (
        "/api/v1/tenant/ads/campaigns/{campaign_id}/budget-cap",
        Method::PUT,
        ADVERTISING_ENTITLEMENTS,
    ),
    (
        "/api/v1/tenant/ads/budget-alerts",
        Method::GET,
        ADVERTISING_ENTITLEMENTS,
    ),
    (
        "/api/v1/tenant/ads/budget-alerts/{alert_id}/acknowledge",
        Method::POST,
        ADVERTISING_ENTITLEMENTS,
    ),
    (
        "/api/v1/tenant/ads/spend-status",
        Method::GET,
        ADVERTISING_ENTITLEMENTS,
    ),
];

pub fn tracking_public_routes(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::scope("/api/v1/public").service(
            web::resource("/track")
                .app_data(super::tracking_handlers::tracking_json_config())
                .route(web::post().to(super::tracking_handlers::public_track_conversion)),
        ),
    );
}

pub fn tracking_tenant_routes(cfg: &mut web::ServiceConfig) {
    let connect = require_permission!("advertising.connect", tenant);
    let metrics_read = require_permission!("advertising.metrics.read", tenant);
    let read = require_permission!("advertising.read", tenant);
    let write = require_permission!("advertising.campaign.write", tenant);
    let feature_gate = || sanvi_http::RequiredAnyFeature::declared(ADVERTISING_ENTITLEMENTS);

    cfg.service(
        web::resource("/ads/tracking/settings")
            .app_data(PermissionMap::new(vec![
                (Method::GET, connect),
                (Method::PUT, connect),
            ]))
            .route(web::get().to(super::tracking_handlers::get_tracking_settings))
            .route(web::put().to(super::tracking_handlers::put_tracking_settings)),
    );
    cfg.service(
        web::resource("/ads/tracking/test-event")
            .app_data(PermissionMap::new(vec![(Method::POST, metrics_read)]))
            .route(web::post().to(super::tracking_handlers::test_tracking_event)),
    );
    cfg.service(
        web::resource("/ads/conversions")
            .app_data(PermissionMap::new(vec![(Method::GET, metrics_read)]))
            .route(web::get().to(super::tracking_handlers::list_conversions)),
    );
    cfg.service(
        web::resource("/ads/conversions/{id}/diagnostics")
            .app_data(PermissionMap::new(vec![(Method::GET, metrics_read)]))
            .app_data(feature_gate())
            .route(web::get().to(super::tracking_handlers::get_conversion_diagnostics)),
    );
    cfg.service(
        web::resource("/ads/conversions/{id}/retry")
            .app_data(PermissionMap::new(vec![(Method::POST, write)]))
            .app_data(feature_gate())
            .route(web::post().to(super::tracking_handlers::retry_conversion)),
    );
    cfg.service(
        web::resource("/ads/audiences")
            .app_data(PermissionMap::new(vec![
                (Method::GET, read),
                (Method::POST, write),
            ]))
            .app_data(feature_gate())
            .route(web::get().to(super::tracking_handlers::list_audiences))
            .route(web::post().to(super::tracking_handlers::create_audience)),
    );
    cfg.service(
        web::resource("/ads/audiences/{id}/refresh")
            .app_data(PermissionMap::new(vec![(Method::POST, write)]))
            .app_data(feature_gate())
            .route(web::post().to(super::tracking_handlers::refresh_audience)),
    );
}
