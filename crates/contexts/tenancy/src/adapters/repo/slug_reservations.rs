//! Slug allocation: reservations arbitrate concurrent provisions before the
//! tenant row exists, the operator blocklist and the tenants table are the
//! availability check.

use async_trait::async_trait;
use sanvi_persistence::PlatformScope;

use crate::domain::values::TenantSlug;
use crate::domain::{SlugReservationPort, TenancyError};

/// Platform-scoped port implementation; methods demand the audited handle.
#[derive(Debug, Clone, Copy, Default)]
pub struct SqlxSlugReservationPort;

impl SqlxSlugReservationPort {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl SlugReservationPort for SqlxSlugReservationPort {
    async fn reserve(
        &self,
        scope: &mut PlatformScope<'_>,
        slug: &TenantSlug,
    ) -> Result<(), TenancyError> {
        // Operator blocklist first: a reserved word is a policy rejection,
        // not a collision.
        let blocked = sqlx::query_scalar!(
            "SELECT EXISTS(SELECT 1 FROM tenancy.reserved_slugs WHERE slug = $1)",
            slug.as_str()
        )
        .fetch_one(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(|err| TenancyError::Storage(err.to_string()))?
        .unwrap_or(false);
        if blocked {
            return Err(TenancyError::SlugReserved {
                slug: slug.to_string(),
            });
        }

        // A live tenant with this slug (defence in depth: the tenants unique
        // index also arbitrates at save time).
        let taken = sqlx::query_scalar!(
            "SELECT EXISTS(SELECT 1 FROM tenancy.tenants WHERE slug = $1)",
            slug.as_str()
        )
        .fetch_one(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(|err| TenancyError::Storage(err.to_string()))?
        .unwrap_or(false);
        if taken {
            return Err(TenancyError::SlugTaken {
                slug: slug.to_string(),
            });
        }

        // The reservation insert is the concurrency arbiter: exactly one of
        // two racing provisions inserts; the loser sees zero affected rows.
        let inserted = sqlx::query!(
            "INSERT INTO tenancy.slug_reservations (slug) VALUES ($1)
             ON CONFLICT (slug) DO NOTHING",
            slug.as_str()
        )
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(|err| TenancyError::Storage(err.to_string()))?
        .rows_affected();

        if inserted == 0 {
            return Err(TenancyError::SlugTaken {
                slug: slug.to_string(),
            });
        }
        Ok(())
    }

    async fn is_available(
        &self,
        scope: &mut PlatformScope<'_>,
        slug: &TenantSlug,
    ) -> Result<bool, TenancyError> {
        let unavailable = sqlx::query_scalar!(
            "SELECT EXISTS(
               SELECT 1 FROM tenancy.reserved_slugs WHERE slug = $1
               UNION ALL
               SELECT 1 FROM tenancy.slug_reservations WHERE slug = $1
               UNION ALL
               SELECT 1 FROM tenancy.tenants WHERE slug = $1
             )",
            slug.as_str()
        )
        .fetch_one(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(|err| TenancyError::Storage(err.to_string()))?
        .unwrap_or(true);
        Ok(!unavailable)
    }
}
