//! Route registration: every `/api/v1` route here comes from the catalog
//! (which also feeds the CI permission test), so registration and the
//! permission declarations can never drift apart.
//!
//! Tenant-scoped routes mount *inside* the shared `/api/v1/tenant` scope
//! (together with the tenancy context's routes — two scopes with the same
//! path would shadow each other in actix's router); everything else
//! registers on the app.

use std::collections::BTreeMap;

use actix_web::web;
use sanvi_http::PermissionMap;

use super::catalog::{ROUTES, Requirement, RouteDef};
use super::handlers::{
    accept_invitation, complete_link_challenge, get_me, grant_member, invite_member,
    list_invitations, list_members, list_sessions, remove_member, resend_invitation,
    revoke_invitation, revoke_session, start_link_challenge, update_member_roles,
};
use super::webhooks::{kratos_login, kratos_logout, kratos_registration, kratos_settings};

/// Non-tenant identity routes (`/api/v1/me`, `/api/v1/auth/*`).
pub fn identity_routes(cfg: &mut web::ServiceConfig) {
    let mut by_path: BTreeMap<&'static str, Vec<&RouteDef>> = BTreeMap::new();
    for route in ROUTES {
        if route.path.starts_with("/api/v1/tenant/") {
            continue; // registered via tenant_scope_routes
        }
        by_path.entry(route.path).or_default().push(route);
    }

    for (path, defs) in by_path {
        let permission_entries: Vec<(actix_web::http::Method, sanvi_http::RequiredPermission)> =
            defs.iter()
                .filter_map(|def| match def.requirement {
                    Requirement::Permission(key, resource_spec) => Some((
                        def.method.clone(),
                        sanvi_http::RequiredPermission::declared(key, resource_spec),
                    )),
                    Requirement::Authenticated | Requirement::Public => None,
                })
                .collect();
        let map = if permission_entries.is_empty() {
            None
        } else {
            Some(PermissionMap::new(permission_entries))
        };
        register(cfg, path, map);
    }
}

/// Tenant-scoped identity routes, mounted inside the shared
/// `/api/v1/tenant` scope (paths relative to it).
pub fn tenant_scope_routes(cfg: &mut web::ServiceConfig) {
    let mut by_path: BTreeMap<&'static str, Vec<&RouteDef>> = BTreeMap::new();
    for route in ROUTES {
        if let Some(relative) = route.path.strip_prefix("/api/v1/tenant") {
            by_path.entry(relative).or_default().push(route);
        }
    }

    for (path, defs) in by_path {
        let permission_entries: Vec<(actix_web::http::Method, sanvi_http::RequiredPermission)> =
            defs.iter()
                .filter_map(|def| match def.requirement {
                    Requirement::Permission(key, resource_spec) => Some((
                        def.method.clone(),
                        sanvi_http::RequiredPermission::declared(key, resource_spec),
                    )),
                    Requirement::Authenticated | Requirement::Public => None,
                })
                .collect();
        let map = if permission_entries.is_empty() {
            None
        } else {
            Some(PermissionMap::new(permission_entries))
        };
        register(cfg, path, map);
    }
}

fn register(cfg: &mut web::ServiceConfig, path: &str, map: Option<PermissionMap>) {
    let build = |m: Option<PermissionMap>| {
        let resource = web::resource(path);
        match m {
            Some(map) => resource.app_data(map),
            None => resource,
        }
    };
    match path {
        "/api/v1/me" => {
            cfg.service(build(map).route(web::get().to(get_me)));
        }
        "/api/v1/me/sessions" => {
            cfg.service(build(map).route(web::get().to(list_sessions)));
        }
        "/api/v1/me/sessions/{session_id}" => {
            cfg.service(build(map).route(web::delete().to(revoke_session)));
        }
        "/api/v1/auth/link/challenge" => {
            cfg.service(build(map).route(web::post().to(start_link_challenge)));
        }
        "/api/v1/auth/link/complete" => {
            cfg.service(build(map).route(web::post().to(complete_link_challenge)));
        }
        "/members" => {
            cfg.service(
                build(map)
                    .route(web::get().to(list_members))
                    .route(web::post().to(grant_member)),
            );
        }
        "/members/{user_id}/roles" => {
            cfg.service(build(map).route(web::patch().to(update_member_roles)));
        }
        "/members/{user_id}" => {
            cfg.service(build(map).route(web::delete().to(remove_member)));
        }
        "/invitations" => {
            cfg.service(
                build(map)
                    .route(web::get().to(list_invitations))
                    .route(web::post().to(invite_member)),
            );
        }
        "/invitations/accept" => {
            cfg.service(build(map).route(web::post().to(accept_invitation)));
        }
        "/invitations/{invitation_id}/resend" => {
            cfg.service(build(map).route(web::post().to(resend_invitation)));
        }
        "/invitations/{invitation_id}" => {
            cfg.service(build(map).route(web::delete().to(revoke_invitation)));
        }
        other => {
            unreachable!("catalog route not registered: {other}")
        }
    }
}

/// Internal webhook receivers (not public API; shared-secret + allow-listed).
pub fn internal_routes(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::scope("/internal/kratos")
            .route("/registration", web::post().to(kratos_registration))
            .route("/login", web::post().to(kratos_login))
            .route("/settings", web::post().to(kratos_settings))
            .route("/logout", web::post().to(kratos_logout)),
    );
}
