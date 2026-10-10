//! The sqlx offboarding repository over `tenancy.offboarding_requests`
//! (TASK-028, FR-1131).
//!
//! The transition table is enforced here — the last line of defence in
//! front of the state machine the use cases already check: an
//! `advance` that skips states (or reaches `purging` without the
//! export evidence) is rejected by the WHERE clauses, not by trust.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sanvi_persistence::PlatformScope;
use sqlx::FromRow;

use crate::domain::offboarding::{OffboardingRequest, OffboardingStatus, VerifiedExport};
use crate::domain::tenant::TenantId;
use crate::domain::{OffboardingRepository, TenancyError};

#[derive(Debug, FromRow)]
struct Row {
    id: i64,
    tenant_id: i64,
    idempotency_key: String,
    status: String,
    requested_by: Option<i64>,
    export_path: Option<String>,
    export_checksum: Option<String>,
    purge_report: Option<serde_json::Value>,
    error: Option<String>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    completed_at: Option<DateTime<Utc>>,
}

fn to_domain(row: Row) -> Result<OffboardingRequest, TenancyError> {
    let status = OffboardingStatus::parse(&row.status).ok_or_else(|| {
        TenancyError::Storage(format!("invalid offboarding status: {}", row.status))
    })?;
    let tenant_id = TenantId::try_from(row.tenant_id)
        .map_err(|err| TenancyError::Storage(format!("invalid tenant id: {err}")))?;
    let export = match (row.export_path, row.export_checksum) {
        (Some(path), Some(checksum)) => Some(VerifiedExport { path, checksum }),
        (None, None) => None,
        _ => {
            return Err(TenancyError::Storage(
                "offboarding export evidence is half-written".to_string(),
            ));
        }
    };
    Ok(OffboardingRequest {
        id: row.id,
        tenant_id,
        idempotency_key: row.idempotency_key,
        status,
        requested_by: row.requested_by,
        export,
        purge_report: row.purge_report,
        error: row.error,
        created_at: row.created_at,
        updated_at: row.updated_at,
        completed_at: row.completed_at,
    })
}

const SELECT: &str = "SELECT id, tenant_id, idempotency_key, status, requested_by,
       export_path, export_checksum, purge_report, error,
       created_at, updated_at, completed_at
FROM tenancy.offboarding_requests";

#[derive(Debug, Clone, Copy, Default)]
pub struct SqlxOffboardingRepository;

impl SqlxOffboardingRepository {
    pub const fn new() -> Self {
        Self
    }
}

#[async_trait]
impl OffboardingRepository for SqlxOffboardingRepository {
    async fn create_or_get(
        &self,
        scope: &mut PlatformScope<'_>,
        request: &OffboardingRequest,
    ) -> Result<OffboardingRequest, TenancyError> {
        let conn = &mut **scope.tx_mut().as_inner();
        // Idempotent insert: a racing duplicate lands on the unique key
        // and the winner's row is returned either way.
        let row: Row = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "WITH ins AS (
               INSERT INTO tenancy.offboarding_requests
                 (id, tenant_id, idempotency_key, status, requested_by)
               VALUES ($1, $2, $3, $4, $5)
               ON CONFLICT (tenant_id, idempotency_key) DO NOTHING
               RETURNING id, tenant_id, idempotency_key, status, requested_by,
                         export_path, export_checksum, purge_report, error,
                         created_at, updated_at, completed_at
             )
             SELECT * FROM ins
             UNION ALL
             {SELECT}
             WHERE tenant_id = $2 AND idempotency_key = $3
               AND NOT EXISTS (SELECT 1 FROM ins)
             LIMIT 1",
        )))
        .bind(request.id)
        .bind(request.tenant_id.as_i64())
        .bind(&request.idempotency_key)
        .bind(request.status.as_str())
        .bind(request.requested_by)
        .fetch_one(&mut *conn)
        .await
        .map_err(|err| TenancyError::Storage(err.to_string()))?;
        to_domain(row)
    }

    async fn find_by_id(
        &self,
        scope: &mut PlatformScope<'_>,
        request_id: i64,
    ) -> Result<Option<OffboardingRequest>, TenancyError> {
        let conn = &mut **scope.tx_mut().as_inner();
        let row: Option<Row> =
            sqlx::query_as(sqlx::AssertSqlSafe(format!("{SELECT} WHERE id = $1")))
                .bind(request_id)
                .fetch_optional(&mut *conn)
                .await
                .map_err(|err| TenancyError::Storage(err.to_string()))?;
        row.map(to_domain).transpose()
    }

    async fn find_by_tenant_and_key(
        &self,
        scope: &mut PlatformScope<'_>,
        tenant_id: TenantId,
        key: &str,
    ) -> Result<Option<OffboardingRequest>, TenancyError> {
        let conn = &mut **scope.tx_mut().as_inner();
        let row: Option<Row> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "{SELECT} WHERE tenant_id = $1 AND idempotency_key = $2"
        )))
        .bind(tenant_id.as_i64())
        .bind(key)
        .fetch_optional(&mut *conn)
        .await
        .map_err(|err| TenancyError::Storage(err.to_string()))?;
        row.map(to_domain).transpose()
    }

    async fn latest_for_tenant(
        &self,
        scope: &mut PlatformScope<'_>,
        tenant_id: TenantId,
    ) -> Result<Option<OffboardingRequest>, TenancyError> {
        let conn = &mut **scope.tx_mut().as_inner();
        let row: Option<Row> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "{SELECT} WHERE tenant_id = $1 ORDER BY created_at DESC, id DESC LIMIT 1"
        )))
        .bind(tenant_id.as_i64())
        .fetch_optional(&mut *conn)
        .await
        .map_err(|err| TenancyError::Storage(err.to_string()))?;
        row.map(to_domain).transpose()
    }

    async fn mark_exported(
        &self,
        scope: &mut PlatformScope<'_>,
        request_id: i64,
        export: &VerifiedExport,
    ) -> Result<(), TenancyError> {
        let conn = &mut **scope.tx_mut().as_inner();
        let updated = sqlx::query(
            "UPDATE tenancy.offboarding_requests
             SET export_path = $2, export_checksum = $3,
                 status = 'exported', updated_at = now()
             WHERE id = $1 AND status = 'exporting'
               AND export_path IS NULL AND export_checksum IS NULL",
        )
        .bind(request_id)
        .bind(&export.path)
        .bind(&export.checksum)
        .execute(&mut *conn)
        .await
        .map_err(|err| TenancyError::Storage(err.to_string()))?
        .rows_affected();
        if updated == 0 {
            return Err(TenancyError::OffboardingTransition {
                request_id,
                from: "exporting".to_string(),
                to: "exported".to_string(),
            });
        }
        Ok(())
    }

    async fn advance(
        &self,
        scope: &mut PlatformScope<'_>,
        request_id: i64,
        next: OffboardingStatus,
        purge_report: Option<serde_json::Value>,
        error: Option<String>,
    ) -> Result<(), TenancyError> {
        let conn = &mut **scope.tx_mut().as_inner();
        let terminal = matches!(next, OffboardingStatus::Purged | OffboardingStatus::Failed);
        // The verified-export gate lives in the WHERE clause: purging
        // without the evidence updates zero rows and is refused as an
        // illegal transition.
        let gate = if next == OffboardingStatus::Purging {
            " AND export_path IS NOT NULL AND export_checksum IS NOT NULL"
        } else {
            ""
        };
        let updated = sqlx::query(sqlx::AssertSqlSafe(format!(
            "UPDATE tenancy.offboarding_requests
             SET status = $2, purge_report = COALESCE($3, purge_report),
                 error = $4, updated_at = now(),
                 completed_at = CASE WHEN {terminal} THEN now() ELSE completed_at END
             WHERE id = $1{gate}",
            terminal = terminal,
            gate = gate,
        )))
        .bind(request_id)
        .bind(next.as_str())
        .bind(&purge_report)
        .bind(&error)
        .execute(&mut *conn)
        .await
        .map_err(|err| TenancyError::Storage(err.to_string()))?
        .rows_affected();
        if updated == 0 {
            let current: Option<String> =
                sqlx::query_scalar("SELECT status FROM tenancy.offboarding_requests WHERE id = $1")
                    .bind(request_id)
                    .fetch_optional(&mut *conn)
                    .await
                    .map_err(|err| TenancyError::Storage(err.to_string()))?;
            return Err(match current {
                Some(from) => TenancyError::OffboardingTransition {
                    request_id,
                    from,
                    to: next.as_str().to_string(),
                },
                None => TenancyError::TenantNotFound {
                    id: request_id.to_string(),
                },
            });
        }
        Ok(())
    }
}
