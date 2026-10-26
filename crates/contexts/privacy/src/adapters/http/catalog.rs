//! The privacy context's route catalog: every route it mounts, with its
//! authentication/permission requirement. The "no route without a
//! permission" CI test enumerates this and proves every permission key is
//! declared in the access context's central registry; `Authenticated` and
//! `Public` entries are the explicitly-declared exemptions.
//!
//! Public here means "the operation is its own credential": opt-outs and
//! self-service submissions are deliberately not gated on authentication
//! (an opt-out must never be blocked by identity verification), and status
//! reads are gated by high-entropy challenge tokens.

use actix_web::http::Method;
use sanvi_http::ResourceSpec;

/// The requirement a route declares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Requirement {
    /// Enforced via the permission gate (key + resource).
    Permission(&'static str, ResourceSpec),
    /// Any authenticated subject; the operation is its own credential.
    Authenticated,
    /// No authentication: the operation is safe pre-authentication
    /// (opt-outs, token-bound status reads, public notices).
    Public,
}

pub struct RouteDef {
    pub method: Method,
    pub path: &'static str,
    pub requirement: Requirement,
}

pub const ROUTES: &[RouteDef] = &[
    // --- self-service (subject-scoped, no tenant resolution) ---------------
    RouteDef {
        method: Method::POST,
        path: "/api/v1/privacy/requests",
        requirement: Requirement::Public,
    },
    RouteDef {
        method: Method::POST,
        path: "/api/v1/privacy/requests/{id}/verify",
        requirement: Requirement::Public,
    },
    RouteDef {
        method: Method::GET,
        path: "/api/v1/privacy/requests/{id}",
        requirement: Requirement::Public,
    },
    RouteDef {
        method: Method::GET,
        path: "/api/v1/privacy/requests/{id}/download",
        requirement: Requirement::Public,
    },
    RouteDef {
        method: Method::POST,
        path: "/api/v1/privacy/opt-out",
        requirement: Requirement::Public,
    },
    RouteDef {
        method: Method::POST,
        path: "/api/v1/privacy/limit-sensitive",
        requirement: Requirement::Public,
    },
    RouteDef {
        method: Method::GET,
        path: "/api/v1/privacy/consents",
        requirement: Requirement::Permission("privacy.consent.read", ResourceSpec::User),
    },
    RouteDef {
        method: Method::PUT,
        path: "/api/v1/privacy/consents",
        requirement: Requirement::Permission("privacy.consent.update", ResourceSpec::User),
    },
    RouteDef {
        method: Method::GET,
        path: "/api/v1/privacy/directives",
        requirement: Requirement::Permission("privacy.directive.read", ResourceSpec::User),
    },
    RouteDef {
        method: Method::POST,
        path: "/api/v1/privacy/requests/{id}/appeal",
        requirement: Requirement::Authenticated,
    },
    RouteDef {
        method: Method::PATCH,
        path: "/api/v1/privacy/requests/{id}/rectify",
        requirement: Requirement::Authenticated,
    },
    RouteDef {
        method: Method::POST,
        path: "/api/v1/platform/privacy/requests/{id}/release",
        requirement: Requirement::Permission(
            "platform.privacy.request.write",
            ResourceSpec::Platform,
        ),
    },
    RouteDef {
        method: Method::POST,
        path: "/api/v1/platform/privacy/requests/{id}/fulfill",
        requirement: Requirement::Permission(
            "platform.privacy.request.write",
            ResourceSpec::Platform,
        ),
    },
    // --- public notices & disclosures ---------------------------------------
    RouteDef {
        method: Method::GET,
        path: "/api/v1/public/privacy/notice",
        requirement: Requirement::Public,
    },
    RouteDef {
        method: Method::GET,
        path: "/api/v1/public/privacy/notice-at-collection",
        requirement: Requirement::Public,
    },
    RouteDef {
        method: Method::GET,
        path: "/api/v1/public/privacy/metrics",
        requirement: Requirement::Public,
    },
    RouteDef {
        method: Method::GET,
        path: "/api/v1/public/subprocessors",
        requirement: Requirement::Public,
    },
    // --- platform operator control plane ------------------------------------
    RouteDef {
        method: Method::GET,
        path: "/api/v1/platform/privacy/requests",
        requirement: Requirement::Permission(
            "platform.privacy.request.read",
            ResourceSpec::Platform,
        ),
    },
    RouteDef {
        method: Method::POST,
        path: "/api/v1/platform/privacy/requests/{id}/extend",
        requirement: Requirement::Permission(
            "platform.privacy.request.write",
            ResourceSpec::Platform,
        ),
    },
    RouteDef {
        method: Method::POST,
        path: "/api/v1/platform/privacy/requests/{id}/reject",
        requirement: Requirement::Permission(
            "platform.privacy.request.write",
            ResourceSpec::Platform,
        ),
    },
    RouteDef {
        method: Method::GET,
        path: "/api/v1/platform/privacy/appeals",
        requirement: Requirement::Permission(
            "platform.privacy.appeal.read",
            ResourceSpec::Platform,
        ),
    },
    RouteDef {
        method: Method::POST,
        path: "/api/v1/platform/privacy/appeals/{id}/decision",
        requirement: Requirement::Permission(
            "platform.privacy.appeal.write",
            ResourceSpec::Platform,
        ),
    },
    RouteDef {
        method: Method::GET,
        path: "/api/v1/platform/privacy/jurisdictions",
        requirement: Requirement::Permission(
            "platform.privacy.jurisdiction.read",
            ResourceSpec::Platform,
        ),
    },
    RouteDef {
        method: Method::PUT,
        path: "/api/v1/platform/privacy/jurisdictions",
        requirement: Requirement::Permission(
            "platform.privacy.jurisdiction.write",
            ResourceSpec::Platform,
        ),
    },
    RouteDef {
        method: Method::GET,
        path: "/api/v1/platform/privacy/holds",
        requirement: Requirement::Permission("platform.privacy.hold.read", ResourceSpec::Platform),
    },
    RouteDef {
        method: Method::POST,
        path: "/api/v1/platform/privacy/holds",
        requirement: Requirement::Permission("platform.privacy.hold.write", ResourceSpec::Platform),
    },
    RouteDef {
        method: Method::POST,
        path: "/api/v1/platform/privacy/holds/{id}/release",
        requirement: Requirement::Permission("platform.privacy.hold.write", ResourceSpec::Platform),
    },
    RouteDef {
        method: Method::GET,
        path: "/api/v1/platform/privacy/incidents",
        requirement: Requirement::Permission(
            "platform.privacy.incident.read",
            ResourceSpec::Platform,
        ),
    },
    RouteDef {
        method: Method::POST,
        path: "/api/v1/platform/privacy/incidents",
        requirement: Requirement::Permission(
            "platform.privacy.incident.write",
            ResourceSpec::Platform,
        ),
    },
    RouteDef {
        method: Method::GET,
        path: "/api/v1/platform/privacy/incidents/{id}/obligations",
        requirement: Requirement::Permission(
            "platform.privacy.incident.read",
            ResourceSpec::Platform,
        ),
    },
    RouteDef {
        method: Method::POST,
        path: "/api/v1/platform/privacy/incidents/{id}/notify",
        requirement: Requirement::Permission(
            "platform.privacy.incident.write",
            ResourceSpec::Platform,
        ),
    },
    RouteDef {
        method: Method::GET,
        path: "/api/v1/platform/privacy/subprocessors",
        requirement: Requirement::Permission(
            "platform.privacy.governance.read",
            ResourceSpec::Platform,
        ),
    },
    RouteDef {
        method: Method::POST,
        path: "/api/v1/platform/privacy/subprocessors",
        requirement: Requirement::Permission(
            "platform.privacy.governance.write",
            ResourceSpec::Platform,
        ),
    },
    RouteDef {
        method: Method::DELETE,
        path: "/api/v1/platform/privacy/subprocessors/{name}",
        requirement: Requirement::Permission(
            "platform.privacy.governance.write",
            ResourceSpec::Platform,
        ),
    },
    RouteDef {
        method: Method::GET,
        path: "/api/v1/platform/privacy/activities",
        requirement: Requirement::Permission(
            "platform.privacy.governance.read",
            ResourceSpec::Platform,
        ),
    },
    RouteDef {
        method: Method::POST,
        path: "/api/v1/platform/privacy/activities",
        requirement: Requirement::Permission(
            "platform.privacy.governance.write",
            ResourceSpec::Platform,
        ),
    },
    RouteDef {
        method: Method::GET,
        path: "/api/v1/platform/privacy/assessments",
        requirement: Requirement::Permission(
            "platform.privacy.governance.read",
            ResourceSpec::Platform,
        ),
    },
    RouteDef {
        method: Method::POST,
        path: "/api/v1/platform/privacy/assessments",
        requirement: Requirement::Permission(
            "platform.privacy.governance.write",
            ResourceSpec::Platform,
        ),
    },
    RouteDef {
        method: Method::GET,
        path: "/api/v1/platform/privacy/retention-rules",
        requirement: Requirement::Permission(
            "platform.privacy.governance.read",
            ResourceSpec::Platform,
        ),
    },
    RouteDef {
        method: Method::PUT,
        path: "/api/v1/platform/privacy/retention-rules",
        requirement: Requirement::Permission(
            "platform.privacy.governance.write",
            ResourceSpec::Platform,
        ),
    },
    // --- tenant processor mode ----------------------------------------------
    RouteDef {
        method: Method::POST,
        path: "/api/v1/tenant/privacy/requests",
        requirement: Requirement::Permission("tenant.privacy.request.write", ResourceSpec::Tenant),
    },
    RouteDef {
        method: Method::GET,
        path: "/api/v1/tenant/privacy/requests",
        requirement: Requirement::Permission("tenant.privacy.request.read", ResourceSpec::Tenant),
    },
];
