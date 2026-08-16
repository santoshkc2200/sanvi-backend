//! Route registration: every permissioned route here comes from the catalog,
//! which also feeds the CI permission test — the two can never drift apart.
//! The `/api/v1/privacy/*` scope carries the GPC middleware (signals are
//! recorded before authentication).

use actix_web::web;

use sanvi_http::RequiredPermission;

use super::catalog::{ROUTES, Requirement};
use super::handlers::{
    appeal_request, apply_hold, decide_appeal, download_export, extend_dsr, get_consents,
    get_directives, get_dsr_status, get_notice_at_collection, get_privacy_notice,
    get_public_metrics, limit_sensitive_use, list_activities, list_appeals, list_assessments,
    list_dsrs, list_holds, list_incidents, list_jurisdictions, list_obligations,
    list_public_subprocessors, list_retention_rules, list_subprocessors, list_tenant_dsrs,
    mark_obligation_notified, record_incident, record_opt_out, rectify_request, reject_dsr,
    release_hold, remove_subprocessor, submit_dsr, submit_tenant_dsr, update_consents,
    upsert_activity, upsert_assessment, upsert_jurisdiction, upsert_retention_rule,
    upsert_subprocessor, verify_dsr,
};
use crate::adapters::GpcSignalMiddleware;

/// The subject-scoped self-service scope (`/api/v1/privacy/*`): public by
/// design (the tenant middleware's public prefixes), each route declaring
/// its own requirement. GPC middleware wraps the whole scope.
pub fn privacy_scope(cfg: &mut web::ServiceConfig, gpc: GpcSignalMiddleware) {
    cfg.service(
        web::scope("/api/v1/privacy")
            .wrap(gpc)
            .configure(privacy_routes),
    );
}

pub fn privacy_routes(cfg: &mut web::ServiceConfig) {
    for route in ROUTES
        .iter()
        .filter(|route| route.path.starts_with("/api/v1/privacy"))
    {
        let resource = web::resource(route.path).app_data(permission(route));
        match (route.method.as_str(), route.path) {
            ("POST", "/api/v1/privacy/requests") => {
                cfg.service(resource.route(web::post().to(submit_dsr)));
            }
            ("POST", "/api/v1/privacy/requests/{id}/verify") => {
                cfg.service(resource.route(web::post().to(verify_dsr)));
            }
            ("GET", "/api/v1/privacy/requests/{id}") => {
                cfg.service(resource.route(web::get().to(get_dsr_status)));
            }
            ("GET", "/api/v1/privacy/requests/{id}/download") => {
                cfg.service(resource.route(web::get().to(download_export)));
            }
            ("POST", "/api/v1/privacy/opt-out") => {
                cfg.service(resource.route(web::post().to(record_opt_out)));
            }
            ("POST", "/api/v1/privacy/limit-sensitive") => {
                cfg.service(resource.route(web::post().to(limit_sensitive_use)));
            }
            ("GET", "/api/v1/privacy/consents") => {
                cfg.service(resource.route(web::get().to(get_consents)));
            }
            ("PUT", "/api/v1/privacy/consents") => {
                cfg.service(resource.route(web::put().to(update_consents)));
            }
            ("GET", "/api/v1/privacy/directives") => {
                cfg.service(resource.route(web::get().to(get_directives)));
            }
            ("POST", "/api/v1/privacy/requests/{id}/appeal") => {
                cfg.service(resource.route(web::post().to(appeal_request)));
            }
            ("PATCH", "/api/v1/privacy/requests/{id}/rectify") => {
                cfg.service(resource.route(web::patch().to(rectify_request)));
            }
            (method, path) => {
                unreachable!("catalog route not registered: {method} {path}")
            }
        }
    }
}

/// The public disclosure scope.
pub fn public_routes(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::resource("/api/v1/public/privacy/notice").route(web::get().to(get_privacy_notice)),
    );
    cfg.service(
        web::resource("/api/v1/public/privacy/notice-at-collection")
            .route(web::get().to(get_notice_at_collection)),
    );
    cfg.service(
        web::resource("/api/v1/public/privacy/metrics").route(web::get().to(get_public_metrics)),
    );
    cfg.service(
        web::resource("/api/v1/public/subprocessors")
            .route(web::get().to(list_public_subprocessors)),
    );
}

/// Platform operator control plane.
pub fn privacy_platform_routes(cfg: &mut web::ServiceConfig) {
    for route in ROUTES
        .iter()
        .filter(|route| route.path.starts_with("/api/v1/platform/privacy"))
    {
        let resource = web::resource(route.path).app_data(permission(route));
        match (route.method.as_str(), route.path) {
            ("GET", "/api/v1/platform/privacy/requests") => {
                cfg.service(resource.route(web::get().to(list_dsrs)));
            }
            ("POST", "/api/v1/platform/privacy/requests/{id}/extend") => {
                cfg.service(resource.route(web::post().to(extend_dsr)));
            }
            ("POST", "/api/v1/platform/privacy/requests/{id}/reject") => {
                cfg.service(resource.route(web::post().to(reject_dsr)));
            }
            ("GET", "/api/v1/platform/privacy/appeals") => {
                cfg.service(resource.route(web::get().to(list_appeals)));
            }
            ("POST", "/api/v1/platform/privacy/appeals/{id}/decision") => {
                cfg.service(resource.route(web::post().to(decide_appeal)));
            }
            ("GET", "/api/v1/platform/privacy/jurisdictions") => {
                cfg.service(resource.route(web::get().to(list_jurisdictions)));
            }
            ("PUT", "/api/v1/platform/privacy/jurisdictions") => {
                cfg.service(resource.route(web::put().to(upsert_jurisdiction)));
            }
            ("GET", "/api/v1/platform/privacy/holds") => {
                cfg.service(resource.route(web::get().to(list_holds)));
            }
            ("POST", "/api/v1/platform/privacy/holds") => {
                cfg.service(resource.route(web::post().to(apply_hold)));
            }
            ("POST", "/api/v1/platform/privacy/holds/{id}/release") => {
                cfg.service(resource.route(web::post().to(release_hold)));
            }
            ("GET", "/api/v1/platform/privacy/incidents") => {
                cfg.service(resource.route(web::get().to(list_incidents)));
            }
            ("POST", "/api/v1/platform/privacy/incidents") => {
                cfg.service(resource.route(web::post().to(record_incident)));
            }
            ("GET", "/api/v1/platform/privacy/incidents/{id}/obligations") => {
                cfg.service(resource.route(web::get().to(list_obligations)));
            }
            ("POST", "/api/v1/platform/privacy/incidents/{id}/notify") => {
                cfg.service(resource.route(web::post().to(mark_obligation_notified)));
            }
            ("GET", "/api/v1/platform/privacy/subprocessors") => {
                cfg.service(resource.route(web::get().to(list_subprocessors)));
            }
            ("POST", "/api/v1/platform/privacy/subprocessors") => {
                cfg.service(resource.route(web::post().to(upsert_subprocessor)));
            }
            ("DELETE", "/api/v1/platform/privacy/subprocessors/{name}") => {
                cfg.service(resource.route(web::delete().to(remove_subprocessor)));
            }
            ("GET", "/api/v1/platform/privacy/activities") => {
                cfg.service(resource.route(web::get().to(list_activities)));
            }
            ("POST", "/api/v1/platform/privacy/activities") => {
                cfg.service(resource.route(web::post().to(upsert_activity)));
            }
            ("GET", "/api/v1/platform/privacy/assessments") => {
                cfg.service(resource.route(web::get().to(list_assessments)));
            }
            ("POST", "/api/v1/platform/privacy/assessments") => {
                cfg.service(resource.route(web::post().to(upsert_assessment)));
            }
            ("GET", "/api/v1/platform/privacy/retention-rules") => {
                cfg.service(resource.route(web::get().to(list_retention_rules)));
            }
            ("PUT", "/api/v1/platform/privacy/retention-rules") => {
                cfg.service(resource.route(web::put().to(upsert_retention_rule)));
            }
            (method, path) => {
                unreachable!("catalog route not registered: {method} {path}")
            }
        }
    }
}

/// Tenant processor-mode routes (mounted inside the shared
/// `/api/v1/tenant` scope; paths registered relative to it).
pub fn tenant_scope_routes(cfg: &mut web::ServiceConfig) {
    for route in ROUTES
        .iter()
        .filter(|route| route.path.starts_with("/api/v1/tenant/privacy"))
    {
        let relative = route
            .path
            .strip_prefix("/api/v1/tenant")
            .unwrap_or(route.path);
        let resource = web::resource(relative).app_data(permission(route));
        match (route.method.as_str(), route.path) {
            ("POST", "/api/v1/tenant/privacy/requests") => {
                cfg.service(resource.route(web::post().to(submit_tenant_dsr)));
            }
            ("GET", "/api/v1/tenant/privacy/requests") => {
                cfg.service(resource.route(web::get().to(list_tenant_dsrs)));
            }
            (method, path) => {
                unreachable!("catalog route not registered: {method} {path}")
            }
        }
    }
}

fn permission(route: &super::catalog::RouteDef) -> Option<RequiredPermission> {
    match route.requirement {
        Requirement::Permission(key, resource) => Some(RequiredPermission::declared(key, resource)),
        Requirement::Authenticated | Requirement::Public => None,
    }
}
