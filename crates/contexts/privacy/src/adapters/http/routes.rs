//! Route registration: every permissioned route here comes from the catalog,
//! which also feeds the CI permission test — the two can never drift apart.
//! The `/api/v1/privacy/*` scope carries the GPC middleware (signals are
//! recorded before authentication).
//!
//! Resources are grouped **per path** (all methods of one path share one
//! `web::resource`, per-method permissions via `PermissionMap`): actix
//! commits a request to the first matching resource, so registering the
//! same path twice shadows the second registration's methods with 405s.
//! Subject-scope paths are registered relative to the `/api/v1/privacy`
//! prefix they are mounted under — full paths here doubled the prefix and
//! left every subject endpoint 404 in the assembled app.

use std::collections::BTreeMap;

use actix_web::web;

use sanvi_http::{PermissionMap, RequiredPermission};

use super::catalog::{ROUTES, Requirement, RouteDef};
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
    register_grouped(
        cfg,
        ROUTES
            .iter()
            .filter(|route| route.path.starts_with("/api/v1/privacy")),
        "/api/v1/privacy",
    );
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
    register_grouped(
        cfg,
        ROUTES
            .iter()
            .filter(|route| route.path.starts_with("/api/v1/platform/privacy")),
        "",
    );
}

/// Tenant processor-mode routes (mounted inside the shared
/// `/api/v1/tenant` scope; paths registered relative to it).
pub fn tenant_scope_routes(cfg: &mut web::ServiceConfig) {
    register_grouped(
        cfg,
        ROUTES
            .iter()
            .filter(|route| route.path.starts_with("/api/v1/tenant/privacy")),
        "/api/v1/tenant",
    );
}

/// Group the catalog routes by path and register **one** resource per path
/// carrying all of its methods. Paths are registered relative to
/// `mounted_under` (empty for a root mount). Single-method resources keep
/// the direct `RequiredPermission` app-data; multi-method paths carry a
/// `PermissionMap` so each method keeps its own requirement.
fn register_grouped<'a, I>(cfg: &mut web::ServiceConfig, routes: I, mounted_under: &str)
where
    I: Iterator<Item = &'a RouteDef>,
{
    let mut by_path: BTreeMap<&'static str, Vec<&RouteDef>> = BTreeMap::new();
    for route in routes {
        by_path.entry(route.path).or_default().push(route);
    }

    for (path, defs) in by_path {
        let registered = path
            .strip_prefix(mounted_under)
            .expect("catalog path carries the mount prefix");
        let resource = if defs.len() == 1 {
            let def = defs[0];
            web::resource(registered).app_data(permission(def))
        } else {
            let permissions = PermissionMap::new(
                defs.iter()
                    .filter_map(|def| permission(def).map(|p| (def.method.clone(), p)))
                    .collect(),
            );
            web::resource(registered).app_data(permissions)
        };
        let resource = defs
            .iter()
            .fold(resource, |resource, def| resource.route(method_route(def)));
        cfg.service(resource);
    }
}

/// The handler for one catalog definition, keyed by its declared method —
/// the runtime form of the match the per-path grouping dispatches through.
fn method_route(def: &RouteDef) -> actix_web::Route {
    match (def.method.as_str(), def.path) {
        // Subject self-service (relative to /api/v1/privacy).
        ("POST", "/api/v1/privacy/requests") => web::post().to(submit_dsr),
        ("POST", "/api/v1/privacy/requests/{id}/verify") => web::post().to(verify_dsr),
        ("GET", "/api/v1/privacy/requests/{id}") => web::get().to(get_dsr_status),
        ("GET", "/api/v1/privacy/requests/{id}/download") => web::get().to(download_export),
        ("POST", "/api/v1/privacy/opt-out") => web::post().to(record_opt_out),
        ("POST", "/api/v1/privacy/limit-sensitive") => web::post().to(limit_sensitive_use),
        ("GET", "/api/v1/privacy/consents") => web::get().to(get_consents),
        ("PUT", "/api/v1/privacy/consents") => web::put().to(update_consents),
        ("GET", "/api/v1/privacy/directives") => web::get().to(get_directives),
        ("POST", "/api/v1/privacy/requests/{id}/appeal") => web::post().to(appeal_request),
        ("PATCH", "/api/v1/privacy/requests/{id}/rectify") => web::patch().to(rectify_request),
        // Platform operator control plane.
        ("GET", "/api/v1/platform/privacy/requests") => web::get().to(list_dsrs),
        ("POST", "/api/v1/platform/privacy/requests/{id}/extend") => web::post().to(extend_dsr),
        ("POST", "/api/v1/platform/privacy/requests/{id}/reject") => web::post().to(reject_dsr),
        ("GET", "/api/v1/platform/privacy/appeals") => web::get().to(list_appeals),
        ("POST", "/api/v1/platform/privacy/appeals/{id}/decision") => web::post().to(decide_appeal),
        ("GET", "/api/v1/platform/privacy/jurisdictions") => web::get().to(list_jurisdictions),
        ("PUT", "/api/v1/platform/privacy/jurisdictions") => web::put().to(upsert_jurisdiction),
        ("GET", "/api/v1/platform/privacy/holds") => web::get().to(list_holds),
        ("POST", "/api/v1/platform/privacy/holds") => web::post().to(apply_hold),
        ("POST", "/api/v1/platform/privacy/holds/{id}/release") => web::post().to(release_hold),
        ("GET", "/api/v1/platform/privacy/incidents") => web::get().to(list_incidents),
        ("POST", "/api/v1/platform/privacy/incidents") => web::post().to(record_incident),
        ("GET", "/api/v1/platform/privacy/incidents/{id}/obligations") => {
            web::get().to(list_obligations)
        }
        ("POST", "/api/v1/platform/privacy/incidents/{id}/notify") => {
            web::post().to(mark_obligation_notified)
        }
        ("GET", "/api/v1/platform/privacy/subprocessors") => web::get().to(list_subprocessors),
        ("POST", "/api/v1/platform/privacy/subprocessors") => web::post().to(upsert_subprocessor),
        ("DELETE", "/api/v1/platform/privacy/subprocessors/{name}") => {
            web::delete().to(remove_subprocessor)
        }
        ("GET", "/api/v1/platform/privacy/activities") => web::get().to(list_activities),
        ("POST", "/api/v1/platform/privacy/activities") => web::post().to(upsert_activity),
        ("GET", "/api/v1/platform/privacy/assessments") => web::get().to(list_assessments),
        ("POST", "/api/v1/platform/privacy/assessments") => web::post().to(upsert_assessment),
        ("GET", "/api/v1/platform/privacy/retention-rules") => web::get().to(list_retention_rules),
        ("PUT", "/api/v1/platform/privacy/retention-rules") => web::put().to(upsert_retention_rule),
        // Tenant processor mode (relative to /api/v1/tenant).
        ("POST", "/api/v1/tenant/privacy/requests") => web::post().to(submit_tenant_dsr),
        ("GET", "/api/v1/tenant/privacy/requests") => web::get().to(list_tenant_dsrs),
        (method, path) => {
            unreachable!("catalog route not registered: {method} {path}")
        }
    }
}

fn permission(route: &RouteDef) -> Option<RequiredPermission> {
    match route.requirement {
        Requirement::Permission(key, resource) => Some(RequiredPermission::declared(key, resource)),
        Requirement::Authenticated | Requirement::Public => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use actix_web::{App, http::StatusCode, test};

    /// Regression test for the assembled-app mounting defect found during
    /// TASK-021 11.2c endpoint profiling: registering one resource per
    /// catalog entry made actix commit every request to the first resource
    /// on a shared path, 405ing every second method (platform writes, the
    /// tenant DSR list), and mounting the subject scope under
    /// `/api/v1/privacy` while registering full paths doubled the prefix,
    /// 404ing the whole subject scope at its contracted paths. Every
    /// catalog route must *match* in the composition root's mounts — the
    /// handlers themselves are covered by the context's use-case tests,
    /// so a matched-but-unwired handler (500, no `PrivacyState`) passes
    /// here while an unmatched one (404/405) fails naming the operation.
    #[actix_web::test]
    async fn every_catalog_route_matches_in_the_composition_mounts() {
        let app = test::init_service(
            App::new()
                .configure(public_routes)
                .configure(privacy_platform_routes)
                .service(web::scope("/api/v1/privacy").configure(privacy_routes))
                .service(web::scope("/api/v1/tenant").configure(tenant_scope_routes)),
        )
        .await;

        for route in ROUTES {
            let req = test::TestRequest::default()
                .method(route.method.clone())
                .uri(route.path)
                .to_request();
            let res = test::call_service(&app, req).await;
            let served = res.status();
            assert_ne!(
                served,
                StatusCode::NOT_FOUND,
                "{} {} is not mounted at its contracted path (assembled-app mount defect)",
                route.method,
                route.path
            );
            assert_ne!(
                served,
                StatusCode::METHOD_NOT_ALLOWED,
                "{} {} is shadowed by another resource on the same path \
                 (paths must register all methods on one resource)",
                route.method,
                route.path
            );
        }
    }
}
