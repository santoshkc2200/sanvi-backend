//! The SSR cache-key negative tests (TASK-022 11.3b, step 2 key half).
//!
//! A caching bug does not look like a bug — it looks like another tenant's
//! storefront. These tests pin the partitions before any cache exists to
//! put entries in: two tenants can never share an entry, and locale, theme
//! revision and device class each split the key space. The fourth
//! constraint is type-level rather than runtime: `SsrCacheKey::new` takes
//! the `TenantId` by value, the field is private, and the type has no
//! `Default`, no setter and no `Deserialize` impl — a tenant-less key fails
//! to construct, not a code-review habit.

use sanvi_cache::{DeviceClass, SsrCacheKey, ssr_invalidation_pattern};
use sanvi_kernel::{SnowflakeId, TenantId};

fn tenant(raw: i64) -> TenantId {
    TenantId::from_snowflake(SnowflakeId::try_from(raw).unwrap())
}

fn key_for(tenant: TenantId) -> SsrCacheKey {
    SsrCacheKey::new(
        tenant,
        "en",
        3,
        DeviceClass::Desktop,
        "/api/v1/public/theme",
    )
    .unwrap()
}

/// Two tenants with identical locale, theme revision, device class and
/// route produce different keys — the entry — and therefore the response —
/// cannot cross the tenant boundary.
#[test]
fn two_tenants_cannot_share_an_entry() {
    let a = key_for(tenant(0x1101));
    let b = key_for(tenant(0x1102));
    assert_ne!(a, b);
    assert_ne!(a.to_cache_key(), b.to_cache_key());
    assert!(a.to_cache_key().starts_with("sanvi:"));
    assert!(
        a.to_cache_key()
            .contains(&format!("sanvi:{}:ssr:", a.tenant_id()))
    );
    assert!(
        b.to_cache_key()
            .contains(&format!("sanvi:{}:ssr:", b.tenant_id()))
    );
    assert_ne!(
        ssr_invalidation_pattern(a.tenant_id()),
        ssr_invalidation_pattern(b.tenant_id()),
        "invalidating one tenant must never scan another's entries"
    );
}

#[test]
fn locale_partitions_the_key() {
    let base = key_for(tenant(0x1103));
    let other_locale = SsrCacheKey::new(
        base.tenant_id(),
        "ja",
        3,
        DeviceClass::Desktop,
        "/api/v1/public/theme",
    )
    .unwrap();
    assert_ne!(base, other_locale);
    assert_ne!(base.to_cache_key(), other_locale.to_cache_key());
}

#[test]
fn theme_revision_partitions_the_key() {
    let base = key_for(tenant(0x1104));
    let next_revision = SsrCacheKey::new(
        base.tenant_id(),
        "en",
        4,
        DeviceClass::Desktop,
        "/api/v1/public/theme",
    )
    .unwrap();
    assert_ne!(base, next_revision);
    assert_ne!(base.to_cache_key(), next_revision.to_cache_key());
}

#[test]
fn device_class_partitions_the_key() {
    let base = key_for(tenant(0x1105));
    let mobile = SsrCacheKey::new(
        base.tenant_id(),
        "en",
        3,
        DeviceClass::Mobile,
        "/api/v1/public/theme",
    )
    .unwrap();
    assert_ne!(base, mobile);
    assert_ne!(base.to_cache_key(), mobile.to_cache_key());
    assert_ne!(base.device_class(), mobile.device_class());
}

/// The complement of the partitions: the same dimensions in the same order
/// must render the same canonical key, or a cache hit could never happen
/// and the partition tests would be vacuously true.
#[test]
fn identical_dimensions_render_the_identical_key() {
    let a = key_for(tenant(0x1106));
    let b = key_for(tenant(0x1106));
    assert_eq!(a, b);
    assert_eq!(a.to_cache_key(), b.to_cache_key());
}

/// The route (path + non-host query) is part of the key too: the storefront
/// read surface is several operations, and `/api/v1/public/messages/catalog`
/// must never be served for `/api/v1/public/locales`.
#[test]
fn route_partitions_the_key() {
    let tenant = tenant(0x1107);
    let theme = SsrCacheKey::new(
        tenant,
        "en",
        3,
        DeviceClass::Desktop,
        "/api/v1/public/theme",
    )
    .unwrap();
    let messages = SsrCacheKey::new(
        tenant,
        "en",
        3,
        DeviceClass::Desktop,
        "/api/v1/public/messages/catalog",
    )
    .unwrap();
    assert_ne!(theme.to_cache_key(), messages.to_cache_key());
    // ...and the same route twice is one entry, not two.
    assert_eq!(theme.to_cache_key(), {
        SsrCacheKey::new(
            tenant,
            "en",
            3,
            DeviceClass::Desktop,
            "/api/v1/public/theme",
        )
        .unwrap()
        .to_cache_key()
    });
}
