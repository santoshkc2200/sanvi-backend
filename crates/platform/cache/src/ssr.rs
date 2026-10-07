//! The SSR cache key (TASK-022, FR-1111): the type that makes a wrong-tenant
//! response unrepresentable.
//!
//! Every dimension the storefront response depends on is a partition of the
//! key space: two tenants can never share an entry, and locale, theme
//! revision and device class each split it further. The tenant is a
//! constructor argument of type [`TenantId`] — there is no `Default`, no
//! setter and no `Deserialize` impl, so a tenant-less key fails to
//! construct at the type level, not by review.
//!
//! The canonical Redis key nests under the tenant-namespaced shape the
//! key-audit gate enforces: `sanvi:{tenant_id}:ssr:{…}` — which also makes
//! per-tenant invalidation a single `SCAN` prefix (`ssr_invalidation_pattern`).

use sanvi_kernel::TenantId;
use serde::{Deserialize, Serialize};

/// The device partition of the key (FR-1111). Introduced because the
/// storefront serves different markup to mobile; if that ever stops being
/// true, remove the dimension rather than leave one that only multiplies
/// entries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceClass {
    Desktop,
    Mobile,
}

impl DeviceClass {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Desktop => "desktop",
            Self::Mobile => "mobile",
        }
    }
}

/// A construction input carried characters the canonical key cannot hold.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("invalid SSR cache key component `{name}`: {reason}")]
pub struct SsrKeyError {
    pub name: &'static str,
    pub reason: &'static str,
}

/// The SSR cache key: `(tenant, locale, theme revision, device class)` plus
/// the public route (path with its non-host query) the entry was rendered
/// for. Equality is exact per dimension — the partition tests in
/// `tests/ssr_cache_negative.rs` pin that two keys differing in any single
/// dimension never share an entry.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SsrCacheKey {
    tenant_id: TenantId,
    locale: String,
    theme_revision: i32,
    device_class: DeviceClass,
    route: String,
}

/// Locale labels are lowercased BCP-47-ish tags (`en`, `ja`, `pt-br`).
/// Anything outside this alphabet would smuggle `:` or whitespace into the
/// canonical key, so construction rejects it.
fn validate_locale(locale: &str) -> Result<(), SsrKeyError> {
    let ok = !locale.is_empty()
        && locale.len() <= 35
        && locale
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if ok {
        Ok(())
    } else {
        Err(SsrKeyError {
            name: "locale",
            reason: "expected a lowercase BCP-47 tag of at most 35 characters (a-z, 0-9, '-')",
        })
    }
}

/// The route part is the request path plus its query, minus the `host`
/// parameter (the host *is* the tenant dimension; keeping it would multiply
/// entries per tenant without partitioning anything). Percent-encoded wire
/// bytes are already safe; the length cap keeps a pathological query from
/// bloating Redis keys.
fn validate_route(route: &str) -> Result<(), SsrKeyError> {
    let ok = route.starts_with('/')
        && route.len() <= 512
        && !route.chars().any(|c| c.is_control() || c == ' ');
    if ok {
        Ok(())
    } else {
        Err(SsrKeyError {
            name: "route",
            reason: "expected a request path (+query) of at most 512 printable characters",
        })
    }
}

impl SsrCacheKey {
    /// The only constructor. A key without a tenant is unrepresentable:
    /// `TenantId` is taken by value and stored privately.
    pub fn new(
        tenant_id: TenantId,
        locale: impl Into<String>,
        theme_revision: i32,
        device_class: DeviceClass,
        route: impl Into<String>,
    ) -> Result<Self, SsrKeyError> {
        let locale = locale.into();
        let route = route.into();
        validate_locale(&locale)?;
        validate_route(&route)?;
        Ok(Self {
            tenant_id,
            locale,
            theme_revision,
            device_class,
            route,
        })
    }

    pub fn tenant_id(&self) -> TenantId {
        self.tenant_id
    }

    pub fn locale(&self) -> &str {
        &self.locale
    }

    pub fn theme_revision(&self) -> i32 {
        self.theme_revision
    }

    pub fn device_class(&self) -> DeviceClass {
        self.device_class
    }

    pub fn route(&self) -> &str {
        &self.route
    }

    /// The canonical Redis key under the enforced tenant namespace. The
    /// tenant component is spelled out for the key-audit gate: a key that
    /// forgets it is a bypass of RLS.
    pub fn to_cache_key(&self) -> String {
        format!(
            "sanvi:{tenant_id}:ssr:{device}/{revision}/{locale}?{route}",
            tenant_id = self.tenant_id,
            device = self.device_class.as_str(),
            revision = self.theme_revision,
            locale = self.locale,
            route = percent_encode_route(&self.route),
        )
    }
}

/// Route segments can hold `/` and `?` by nature; only `%` needs escaping so
/// the key stays one unambiguous string.
fn percent_encode_route(route: &str) -> String {
    route.replace('%', "%25")
}

/// The invalidation scan pattern for one tenant's SSR entries (event-driven
/// invalidation on theme publish / content change).
pub fn ssr_invalidation_pattern(tenant_id: TenantId) -> String {
    format!("sanvi:{tenant_id}:ssr:*")
}

#[cfg(test)]
mod tests {
    use super::*;
    use sanvi_kernel::SnowflakeId;

    fn key(tenant: TenantId, locale: &str, revision: i32, device: DeviceClass) -> SsrCacheKey {
        SsrCacheKey::new(tenant, locale, revision, device, "/api/v1/public/theme").unwrap()
    }

    fn make_tenant(raw: i64) -> TenantId {
        TenantId::from_snowflake(SnowflakeId::try_from(raw).unwrap())
    }

    #[test]
    fn canonical_key_sits_under_the_tenant_namespace() {
        let tenant = make_tenant(0x7f01);
        let rendered = key(tenant, "en", 3, DeviceClass::Desktop).to_cache_key();
        assert_eq!(
            rendered,
            format!("sanvi:{tenant}:ssr:desktop/3/en?/api/v1/public/theme"),
            "{rendered}"
        );
        assert!(rendered.starts_with(&format!("sanvi:{tenant}:ssr:")));
    }

    #[test]
    fn an_invalid_locale_or_route_is_rejected_at_construction() {
        let tenant = make_tenant(0x7f02);
        let colon = SsrCacheKey::new(tenant, "en:GB", 1, DeviceClass::Desktop, "/x");
        assert_eq!(
            colon.unwrap_err(),
            SsrKeyError {
                name: "locale",
                reason: "expected a lowercase BCP-47 tag of at most 35 characters (a-z, 0-9, '-')",
            }
        );
        let too_long = SsrCacheKey::new(
            tenant,
            "en",
            1,
            DeviceClass::Desktop,
            format!("/x?{}", "a".repeat(600)),
        );
        assert!(too_long.is_err());
        let not_a_path =
            SsrCacheKey::new(tenant, "en", 1, DeviceClass::Desktop, "api/v1/public/theme");
        assert!(not_a_path.is_err());
    }

    #[test]
    fn percent_signs_do_not_ambiguate_the_route() {
        let tenant = make_tenant(0x7f03);
        let a = SsrCacheKey::new(tenant, "en", 1, DeviceClass::Desktop, "/x%2Fy").unwrap();
        let b = SsrCacheKey::new(tenant, "en", 1, DeviceClass::Desktop, "/x%252Fy").unwrap();
        assert_ne!(a.to_cache_key(), b.to_cache_key());
    }

    #[test]
    fn invalidation_pattern_covers_only_the_tenant_ssr_namespace() {
        let tenant = make_tenant(0x7f04);
        assert_eq!(
            ssr_invalidation_pattern(tenant),
            format!("sanvi:{tenant}:ssr:*")
        );
        assert_ne!(
            ssr_invalidation_pattern(tenant),
            ssr_invalidation_pattern(make_tenant(0x7f05))
        );
    }
}
