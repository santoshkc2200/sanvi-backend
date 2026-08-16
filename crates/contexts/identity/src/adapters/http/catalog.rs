//! The identity context's route catalog: every route it mounts, with its
//! authentication/permission requirement. The "no route without a
//! permission" CI test enumerates this and proves every permission key is
//! declared in the access context's central registry; `Authenticated` and
//! `Public` entries are the explicitly-declared exemptions.

use actix_web::http::Method;
use sanvi_http::ResourceSpec;

/// The requirement a route declares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Requirement {
    /// Enforced via the permission gate (key + resource).
    Permission(&'static str, ResourceSpec),
    /// Any authenticated subject; the operation is its own credential
    /// (a single-use token, the caller's own account).
    Authenticated,
    /// No authentication: the operation is safe pre-authentication
    /// (creating a linking intent whose proof-of-ownership comes later).
    Public,
}

pub struct RouteDef {
    pub method: Method,
    pub path: &'static str,
    pub requirement: Requirement,
}

pub const ROUTES: &[RouteDef] = &[
    // --- me / sessions (user-scoped) ----------------------------------------
    RouteDef {
        method: Method::GET,
        path: "/api/v1/me",
        requirement: Requirement::Permission("identity.profile.read", ResourceSpec::User),
    },
    RouteDef {
        method: Method::GET,
        path: "/api/v1/me/sessions",
        requirement: Requirement::Permission("identity.session.list", ResourceSpec::User),
    },
    RouteDef {
        method: Method::DELETE,
        path: "/api/v1/me/sessions/{session_id}",
        requirement: Requirement::Permission("identity.session.revoke", ResourceSpec::User),
    },
    // --- account linking -----------------------------------------------------
    RouteDef {
        method: Method::POST,
        path: "/api/v1/auth/link/challenge",
        requirement: Requirement::Public,
    },
    RouteDef {
        method: Method::POST,
        path: "/api/v1/auth/link/complete",
        requirement: Requirement::Authenticated,
    },
    // --- members ------------------------------------------------------------
    RouteDef {
        method: Method::GET,
        path: "/api/v1/tenant/members",
        requirement: Requirement::Permission("identity.member.read", ResourceSpec::Tenant),
    },
    RouteDef {
        method: Method::POST,
        path: "/api/v1/tenant/members",
        requirement: Requirement::Permission("identity.member.grant", ResourceSpec::Tenant),
    },
    RouteDef {
        method: Method::PATCH,
        path: "/api/v1/tenant/members/{user_id}/roles",
        requirement: Requirement::Permission("identity.member.grant", ResourceSpec::Tenant),
    },
    RouteDef {
        method: Method::DELETE,
        path: "/api/v1/tenant/members/{user_id}",
        requirement: Requirement::Permission("identity.member.remove", ResourceSpec::Tenant),
    },
    // --- invitations ----------------------------------------------------------
    RouteDef {
        method: Method::GET,
        path: "/api/v1/tenant/invitations",
        requirement: Requirement::Permission("identity.member.read", ResourceSpec::Tenant),
    },
    RouteDef {
        method: Method::POST,
        path: "/api/v1/tenant/invitations",
        requirement: Requirement::Permission("identity.member.invite", ResourceSpec::Tenant),
    },
    RouteDef {
        method: Method::POST,
        path: "/api/v1/tenant/invitations/accept",
        requirement: Requirement::Permission("identity.membership.claim", ResourceSpec::Tenant),
    },
    RouteDef {
        method: Method::POST,
        path: "/api/v1/tenant/invitations/{invitation_id}/resend",
        requirement: Requirement::Permission("identity.invitation.manage", ResourceSpec::Tenant),
    },
    RouteDef {
        method: Method::DELETE,
        path: "/api/v1/tenant/invitations/{invitation_id}",
        requirement: Requirement::Permission("identity.invitation.manage", ResourceSpec::Tenant),
    },
];
