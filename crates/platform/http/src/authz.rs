//! Authorization: the permission gate port and the route-level extractor.
//!
//! Every non-public route declares the permission it requires with
//! [`require_permission!`] in the route catalog; registration attaches the
//! declaration to the actix `Resource` (`app_data`), and the
//! [`RequiredPermission`] extractor enforces it per request: resolve the
//! subject (inserted by the authentication middleware) and the resource
//! (current tenant, current user, or platform), then ask the gate —
//! implemented by the access context (Keto + role catalog). The gate is
//! absent only when the identity feature flag is off, in which case every
//! route behaves public (the same phase-gate semantics as tenancy's
//! middleware).

use std::sync::Arc;

use actix_web::HttpMessage;
use serde::{Deserialize, Serialize};
use sanvi_kernel::{TenantId, UserId};

use crate::authn::Subject;
use crate::error::ApiError;

/// The resource a permission is checked against.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceSpec {
    /// The tenant resolved for this request (`TenantContext`).
    Tenant,
    /// The signed-in user themselves (`me`/session endpoints).
    User,
    /// Platform-wide (no resource id).
    Platform,
}

/// The gate's answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Allowed,
    Denied,
}

/// Port for permission checks, implemented by the access context.
pub trait PermissionGate: Send + Sync {
    fn check<'a>(
        &'a self,
        subject: &'a Subject,
        permission: &'a str,
        resource: &'a ResourceRef,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<Decision, ApiError>> + Send + 'a>>;
}

/// A concrete resource reference resolved at request time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceRef {
    Tenant(TenantId),
    User(UserId),
    Platform,
}

/// The permission requirement a route declares (attached to the resource by
/// the route catalog; read back by the extractor).
#[derive(Debug, Clone, Copy)]
pub struct RequiredPermission {
    pub key: &'static str,
    pub resource: ResourceSpec,
}

impl RequiredPermission {
    pub fn declared(key: &'static str, resource: ResourceSpec) -> Self {
        Self { key, resource }
    }
}

/// Per-method permissions for a resource that serves several methods (e.g.
/// GET and POST on the same path with different requirements).
#[derive(Clone)]
pub struct PermissionMap {
    entries: Arc<std::collections::HashMap<actix_web::http::Method, RequiredPermission>>,
}

impl PermissionMap {
    pub fn new(entries: Vec<(actix_web::http::Method, RequiredPermission)>) -> Self {
        Self {
            entries: Arc::new(entries.into_iter().collect()),
        }
    }

    pub fn get(&self, method: &actix_web::http::Method) -> Option<RequiredPermission> {
        self.entries.get(method).copied()
    }
}

impl actix_web::FromRequest for RequiredPermission {
    type Error = ApiError;
    type Future = std::pin::Pin<Box<dyn Future<Output = Result<Self, Self::Error>> + 'static>>;

    fn from_request(req: &actix_web::HttpRequest, _: &mut actix_web::dev::Payload) -> Self::Future {
        let subject = req.extensions().get::<Subject>().cloned();
        let requirement = req
            .app_data::<RequiredPermission>()
            .copied()
            .or_else(|| {
                req.app_data::<PermissionMap>()
                    .and_then(|map| map.get(req.method()))
            })
            .unwrap_or_else(|| RequiredPermission::declared("undeclared", ResourceSpec::Platform));
        let tenant_id = req
            .extensions()
            .get::<crate::tenancy::TenantContext>()
            .map(|context| context.tenant_id);
        // Registered as `web::Data::new(Arc<dyn PermissionGate>)` (the
        // `Arc` keeps `T` sized, unlike `Data::from(Arc<dyn T>)`).
        let gate = req
            .app_data::<actix_web::web::Data<Arc<dyn PermissionGate>>>()
            .map(|data| data.get_ref().clone());

        Box::pin(async move {
            // The identity feature flag is off: no gate registered, routes
            // behave public (documented phase-gate semantics).
            let Some(gate) = gate else {
                return Ok(requirement);
            };

            if requirement.key == "undeclared" {
                // Fail closed: a route without a declared permission is a
                // defect the catalog CI test must catch before this ever
                // serves traffic.
                return Err(ApiError::Forbidden {
                    detail: "route has no declared permission".to_string(),
                });
            }

            let Some(subject) = subject else {
                return Err(ApiError::Unauthorized);
            };

            let resource = match requirement.resource {
                ResourceSpec::Tenant => {
                    let Some(tenant_id) = tenant_id else {
                        return Err(ApiError::NotFound {
                            resource: "tenant".to_string(),
                        });
                    };
                    ResourceRef::Tenant(tenant_id)
                }
                ResourceSpec::User => ResourceRef::User(subject.user_id),
                ResourceSpec::Platform => ResourceRef::Platform,
            };

            match gate.check(&subject, requirement.key, &resource).await? {
                Decision::Allowed => Ok(requirement),
                Decision::Denied => Err(ApiError::Forbidden {
                    detail: format!("permission required: {}", requirement.key),
                }),
            }
        })
    }
}

/// Declare the permission a route requires. Used by route catalogs:
///
/// ```ignore
/// use sanvi_http::{require_permission, ResourceSpec};
/// RouteDef::new(Method::GET, "/api/v1/me", require_permission!("identity.profile.read", user), get_me)
/// ```
#[macro_export]
macro_rules! require_permission {
    ($key:literal, tenant) => {
        $crate::authz::RequiredPermission::declared($key, $crate::authz::ResourceSpec::Tenant)
    };
    ($key:literal, user) => {
        $crate::authz::RequiredPermission::declared($key, $crate::authz::ResourceSpec::User)
    };
    ($key:literal, platform) => {
        $crate::authz::RequiredPermission::declared($key, $crate::authz::ResourceSpec::Platform)
    };
}
