//! Feature gating: the declarative route-level entitlement check.
//!
//! A route opts in with [`require_feature!`] (attached to the resource as
//! app data, exactly like permissions); the [`RequiredFeature`] extractor
//! resolves the current tenant and asks the [`FeatureGate`] port — a
//! thin HTTP-level slice of the access context's gate. With no gate
//! registered (the feature flag is off), gated routes behave as if
//! enabled, matching the permission gate's phase-gate semantics.

use std::{future::Future, pin::Pin, sync::Arc};

use actix_web::HttpMessage;
use uuid::Uuid;

use crate::error::ApiError;

/// The route-level declaration: "this route requires `<feature>` enabled
/// for the current tenant".
#[derive(Debug, Clone, Copy)]
pub struct RequiredFeature {
    pub key: &'static str,
}

impl RequiredFeature {
    pub fn declared(key: &'static str) -> Self {
        Self { key }
    }
}

/// Port for entitlement checks, implemented by the access context.
pub trait FeatureGate: Send + Sync {
    fn check<'a>(
        &'a self,
        tenant_id: Uuid,
        feature: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<bool, ApiError>> + Send + 'a>>;
}

impl actix_web::FromRequest for RequiredFeature {
    type Error = ApiError;
    type Future = Pin<Box<dyn Future<Output = Result<Self, Self::Error>> + 'static>>;

    fn from_request(req: &actix_web::HttpRequest, _: &mut actix_web::dev::Payload) -> Self::Future {
        let requirement = req.app_data::<RequiredFeature>().copied();
        let tenant_id = req
            .extensions()
            .get::<crate::tenancy::TenantContext>()
            .map(|context| context.tenant_id);
        let gate = req
            .app_data::<actix_web::web::Data<Arc<dyn FeatureGate>>>()
            .map(|data| data.get_ref().clone());

        Box::pin(async move {
            // Not a feature-gated route: nothing to check.
            let Some(requirement) = requirement else {
                return Ok(RequiredFeature::declared("undeclared"));
            };

            // The platform_admin feature flag is off: no gate registered,
            // gated routes behave enabled (documented phase-gate
            // semantics).
            let Some(gate) = gate else {
                return Ok(requirement);
            };

            let Some(tenant_id) = tenant_id else {
                return Err(ApiError::NotFound {
                    resource: "tenant".to_string(),
                });
            };

            match gate.check(tenant_id, requirement.key).await? {
                true => Ok(requirement),
                false => Err(ApiError::Forbidden {
                    detail: format!("feature not enabled: {}", requirement.key),
                }),
            }
        })
    }
}

/// Declare the feature a route is gated on. Used by route catalogs:
///
/// ```ignore
/// use sanvi_http::require_feature;
/// RouteDef { feature: Some(require_feature!("domains.custom").key), .. }
/// ```
#[macro_export]
macro_rules! require_feature {
    ($key:literal) => {
        $crate::features::RequiredFeature::declared($key)
    };
}
