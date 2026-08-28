//! Platform-scoped tenant storage (`tenancy.tenants`).

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sanvi_persistence::PlatformScope;
use sqlx::FromRow;

use crate::domain::tenant::{Tenant, TenantId, TenantStatus, TenantTimestamps};
use crate::domain::values::{DataRegion, Locale, SuspensionReason, TenantName, TenantSlug};
use crate::domain::{TenancyError, TenantRepository, TenantSearch, TenantSearchCursor};

/// Row shape of `tenancy.tenants` (sqlx maps citext to String).
#[derive(Debug, Clone, FromRow)]
struct TenantRow {
    id: i64,
    slug: String,
    display_name: String,
    status: String,
    region: String,
    default_locale: String,
    suspended_reason: Option<String>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    archived_at: Option<DateTime<Utc>>,
    purge_requested_at: Option<DateTime<Utc>>,
}

impl TenantRow {
    fn into_tenant(self) -> Result<Tenant, TenancyError> {
        let suspended_reason = match self.suspended_reason {
            Some(reason) => Some(SuspensionReason::parse(&reason)?),
            None => None,
        };
        Tenant::from_parts(
            TenantId::try_from(self.id)
                .map_err(|err| TenancyError::Storage(format!("invalid tenant id: {err}")))?,
            TenantSlug::parse(&self.slug)?,
            TenantName::parse(&self.display_name)?,
            TenantStatus::parse(&self.status)?,
            DataRegion::parse(&self.region)?,
            Locale::parse(&self.default_locale)?,
            TenantTimestamps {
                created_at: self.created_at,
                updated_at: self.updated_at,
                archived_at: self.archived_at,
                purge_requested_at: self.purge_requested_at,
            },
            suspended_reason,
        )
    }
}

/// Platform-scoped repository. Every method requires a `&mut PlatformScope`:
/// calling tenant storage without the audited, unscoped handle is a type
/// error, not a review habit.
#[derive(Debug, Clone, Copy, Default)]
pub struct SqlxTenantRepository;

impl SqlxTenantRepository {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl TenantRepository for SqlxTenantRepository {
    async fn save(
        &self,
        scope: &mut PlatformScope<'_>,
        tenant: &Tenant,
    ) -> Result<(), TenancyError> {
        let result = sqlx::query!(
            "INSERT INTO tenancy.tenants
               (id, slug, display_name, status, region, default_locale, suspended_reason,
                created_at, updated_at, archived_at, purge_requested_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
             ON CONFLICT (id) DO UPDATE SET
               display_name = EXCLUDED.display_name,
               status = EXCLUDED.status,
               region = EXCLUDED.region,
               default_locale = EXCLUDED.default_locale,
               suspended_reason = EXCLUDED.suspended_reason,
               updated_at = EXCLUDED.updated_at,
               archived_at = EXCLUDED.archived_at,
               purge_requested_at = EXCLUDED.purge_requested_at",
            tenant.id().as_i64(),
            tenant.slug().as_str(),
            tenant.display_name().as_str(),
            tenant.status().as_str(),
            tenant.region().to_string(),
            tenant.default_locale().as_str(),
            tenant.suspended_reason().map(|reason| reason.to_string()),
            tenant.created_at(),
            tenant.updated_at(),
            tenant.archived_at(),
            tenant.purge_requested_at(),
        )
        .execute(&mut **scope.tx_mut().as_inner())
        .await;

        match result {
            Ok(_) => Ok(()),
            Err(sqlx::Error::Database(db)) if db.is_unique_violation() => {
                // The only reachable unique violation here is the slug
                // constraint: id conflicts are the upsert's own path.
                Err(TenancyError::SlugTaken {
                    slug: tenant.slug().to_string(),
                })
            }
            Err(err) => Err(TenancyError::Storage(err.to_string())),
        }
    }

    async fn find_by_id(
        &self,
        scope: &mut PlatformScope<'_>,
        id: TenantId,
    ) -> Result<Option<Tenant>, TenancyError> {
        let row = sqlx::query_as!(
            TenantRow,
            "SELECT id, slug, display_name, status, region, default_locale, suspended_reason,
                    created_at, updated_at, archived_at, purge_requested_at
             FROM tenancy.tenants WHERE id = $1",
            id.as_i64()
        )
        .fetch_optional(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(|err| TenancyError::Storage(err.to_string()))?;
        row.map(TenantRow::into_tenant).transpose()
    }

    async fn find_by_slug(
        &self,
        scope: &mut PlatformScope<'_>,
        slug: &TenantSlug,
    ) -> Result<Option<Tenant>, TenancyError> {
        let row = sqlx::query_as!(
            TenantRow,
            "SELECT id, slug, display_name, status, region, default_locale, suspended_reason,
                    created_at, updated_at, archived_at, purge_requested_at
             FROM tenancy.tenants WHERE slug = $1",
            slug.as_str()
        )
        .fetch_optional(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(|err| TenancyError::Storage(err.to_string()))?;
        row.map(TenantRow::into_tenant).transpose()
    }

    async fn list(
        &self,
        scope: &mut PlatformScope<'_>,
        after: Option<TenantId>,
        limit: u32,
        status: Option<TenantStatus>,
    ) -> Result<(Vec<Tenant>, Option<TenantId>), TenancyError> {
        // Fetch limit + 1: the extra row is the presence probe for the next
        // cursor.
        let rows = sqlx::query_as!(
            TenantRow,
            "SELECT id, slug, display_name, status, region, default_locale, suspended_reason,
                    created_at, updated_at, archived_at, purge_requested_at
             FROM tenancy.tenants
             WHERE ($1::bigint IS NULL OR id > $1)
               AND ($2::text IS NULL OR status = $2)
             ORDER BY id
             LIMIT $3",
            after.map(|id| id.as_i64()),
            status.map(|status| status.as_str()),
            i64::from(limit) + 1
        )
        .fetch_all(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(|err| TenancyError::Storage(err.to_string()))?;

        let has_more = rows.len() > limit as usize;
        let mut rows = rows;
        if has_more {
            rows.truncate(limit as usize);
        }
        let next_cursor = rows
            .last()
            .map(|row| TenantId::try_from(row.id))
            .transpose()
            .map_err(|err| TenancyError::Storage(format!("invalid tenant id: {err}")))?;
        let tenants = rows
            .into_iter()
            .map(TenantRow::into_tenant)
            .collect::<Result<Vec<_>, _>>()?;
        Ok((tenants, if has_more { next_cursor } else { None }))
    }

    async fn search(
        &self,
        scope: &mut PlatformScope<'_>,
        search: &TenantSearch,
    ) -> Result<(Vec<Tenant>, Option<TenantSearchCursor>), TenancyError> {
        let rows = sqlx::query_as!(
            TenantRow,
            "SELECT id, slug, display_name, status, region, default_locale, suspended_reason,
                    created_at, updated_at, archived_at, purge_requested_at
             FROM tenancy.tenants
             WHERE ($1::text IS NULL
                    OR slug ILIKE '%' || $1 || '%'
                    OR display_name ILIKE '%' || $1 || '%')
               AND ($2::text IS NULL OR status = $2)
               AND ($3::timestamptz IS NULL
                    OR created_at < $3
                    OR (created_at = $3 AND id < $4))
             ORDER BY created_at DESC, id DESC
             LIMIT $5",
            search.query.as_deref(),
            search.status.map(|status| status.as_str()),
            search.after.map(|cursor| cursor.created_at),
            search.after.map(|cursor| cursor.id.as_i64()),
            i64::from(search.limit) + 1
        )
        .fetch_all(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(|err| TenancyError::Storage(err.to_string()))?;

        let has_more = rows.len() > search.limit as usize;
        let mut rows = rows;
        if has_more {
            rows.truncate(search.limit as usize);
        }
        let next_cursor = rows.last().map(|row| TenantSearchCursor {
            created_at: row.created_at,
            id: TenantId::try_from(row.id)
                .map_err(|err| TenancyError::Storage(format!("invalid tenant id: {err}")))?,
        });
        let tenants = rows
            .into_iter()
            .map(TenantRow::into_tenant)
            .collect::<Result<Vec<_>, _>>()?;
        Ok((tenants, if has_more { next_cursor } else { None }))
    }

    async fn due_for_purge_request(
        &self,
        scope: &mut PlatformScope<'_>,
        before: DateTime<Utc>,
        limit: u32,
    ) -> Result<Vec<Tenant>, TenancyError> {
        let rows = sqlx::query_as!(
            TenantRow,
            "SELECT id, slug, display_name, status, region, default_locale, suspended_reason,
                    created_at, updated_at, archived_at, purge_requested_at
             FROM tenancy.tenants
             WHERE status = 'archived'
               AND purge_requested_at IS NULL
               AND archived_at < $1
             ORDER BY archived_at
             LIMIT $2",
            before,
            i64::from(limit)
        )
        .fetch_all(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(|err| TenancyError::Storage(err.to_string()))?;
        rows.into_iter().map(TenantRow::into_tenant).collect()
    }
}
