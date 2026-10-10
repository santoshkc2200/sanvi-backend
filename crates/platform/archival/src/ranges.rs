//! The archived-range read path: which ranges of which tables are
//! currently archived.
//!
//! This is the module a read path consults *before* answering a ranged
//! query. A silently-partial result is the failure mode that makes
//! archival untrustworthy, so the contract is binary: if any active
//! archive overlaps the requested window (for the requested tenant
//! scope), the read must say archived-range, never return the surviving
//! rows as if they were the whole answer.

use chrono::{DateTime, Utc};
use sqlx::PgConnection;

use crate::mover::ArchiveTarget;

/// One archive registry row, as the read paths and the mover need it.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ArchiveRecord {
    pub id: i64,
    pub schema_name: String,
    pub table_name: String,
    pub key_column: String,
    pub tenant_id: Option<i64>,
    pub range_start: DateTime<Utc>,
    pub range_end: DateTime<Utc>,
    pub row_count: i64,
    pub checksum: String,
    pub storage_path: String,
    pub bytes: i64,
    pub status: String,
    pub created_at: DateTime<Utc>,
    pub restored_at: Option<DateTime<Utc>>,
    pub purged_at: Option<DateTime<Utc>>,
}

impl ArchiveRecord {
    pub fn schema_table(&self) -> String {
        format!("{}.{}", self.schema_name, self.table_name)
    }
}

fn to_record(row: ArchiveRow) -> ArchiveRecord {
    ArchiveRecord {
        id: row.id,
        schema_name: row.schema_name,
        table_name: row.table_name,
        key_column: row.key_column,
        tenant_id: row.tenant_id,
        range_start: row.range_start,
        range_end: row.range_end,
        row_count: row.row_count,
        checksum: row.checksum,
        storage_path: row.storage_path,
        bytes: row.bytes,
        status: row.status,
        created_at: row.created_at,
        restored_at: row.restored_at,
        purged_at: row.purged_at,
    }
}

#[derive(Debug, Clone, sqlx::FromRow)]
struct ArchiveRow {
    id: i64,
    schema_name: String,
    table_name: String,
    key_column: String,
    tenant_id: Option<i64>,
    range_start: DateTime<Utc>,
    range_end: DateTime<Utc>,
    row_count: i64,
    checksum: String,
    storage_path: String,
    bytes: i64,
    status: String,
    created_at: DateTime<Utc>,
    restored_at: Option<DateTime<Utc>>,
    purged_at: Option<DateTime<Utc>>,
}

/// Does an **active** archive of this table and tenant scope overlap the
/// target range? Returns the archive id when it does — the caller refuses
/// loudly with it.
///
/// Overlap is interval intersection on the half-open ranges: an archived
/// range ending exactly where the target starts (or starting exactly
/// where it ends) does not overlap.
pub async fn overlapping_active(
    conn: &mut PgConnection,
    target: &ArchiveTarget,
) -> Result<Option<i64>, sqlx::Error> {
    sqlx::query_scalar::<_, i64>(
        "SELECT id FROM platform.archives
         WHERE schema_name = $1 AND table_name = $2 AND status = 'archived'
           AND tenant_id IS NOT DISTINCT FROM $3
           AND range_start < $5 AND range_end > $4
         LIMIT 1",
    )
    .bind(&target.schema_name)
    .bind(&target.table_name)
    .bind(target.tenant_id)
    .bind(target.range_start)
    .bind(target.range_end)
    .fetch_optional(conn)
    .await
}

/// Every active archive of one table overlapping a read window.
///
/// `tenant_id`: `Some(t)` asks for that tenant's archives — a
/// platform-scope (NULL-tenant) archive never makes a tenant-scoped read
/// partial, because those rows were never part of its answer. `None`
/// asks for *any* archive of the table: a platform-wide read window must
/// not be answered partially because the missing rows belong to some
/// tenant's cold copy. The audit read path uses exactly this shape.
pub async fn overlapping_reads(
    conn: &mut PgConnection,
    schema_name: &str,
    table_name: &str,
    tenant_id: Option<i64>,
    window_start: Option<DateTime<Utc>>,
    window_end: Option<DateTime<Utc>>,
) -> Result<Vec<ArchiveRecord>, sqlx::Error> {
    let rows: Vec<ArchiveRow> = sqlx::query_as(
        "SELECT id, schema_name, table_name, key_column, tenant_id,
                range_start, range_end, row_count, checksum, storage_path,
                bytes, status, created_at, restored_at, purged_at
         FROM platform.archives
         WHERE schema_name = $1 AND table_name = $2 AND status = 'archived'
           AND ($3::bigint IS NULL OR tenant_id = $3)
           AND ($4::timestamptz IS NULL OR range_end > $4)
           AND ($5::timestamptz IS NULL OR range_start < $5)
         ORDER BY range_start",
    )
    .bind(schema_name)
    .bind(table_name)
    .bind(tenant_id)
    .bind(window_start)
    .bind(window_end)
    .fetch_all(conn)
    .await?;
    Ok(rows.into_iter().map(to_record).collect())
}

/// One archive by id, whatever its status.
pub async fn by_id(
    pool: &sqlx::PgPool,
    archive_id: i64,
) -> Result<Option<ArchiveRecord>, sqlx::Error> {
    let row: Option<ArchiveRow> = sqlx::query_as(
        "SELECT id, schema_name, table_name, key_column, tenant_id,
                range_start, range_end, row_count, checksum, storage_path,
                bytes, status, created_at, restored_at, purged_at
         FROM platform.archives WHERE id = $1",
    )
    .bind(archive_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(to_record))
}

/// Every active archive of one table (operator listing).
pub async fn list_active(
    conn: &mut PgConnection,
    schema_name: &str,
    table_name: &str,
) -> Result<Vec<ArchiveRecord>, sqlx::Error> {
    let rows: Vec<ArchiveRow> = sqlx::query_as(
        "SELECT id, schema_name, table_name, key_column, tenant_id,
                range_start, range_end, row_count, checksum, storage_path,
                bytes, status, created_at, restored_at, purged_at
         FROM platform.archives
         WHERE schema_name = $1 AND table_name = $2 AND status = 'archived'
         ORDER BY range_start",
    )
    .bind(schema_name)
    .bind(table_name)
    .fetch_all(conn)
    .await?;
    Ok(rows.into_iter().map(to_record).collect())
}

/// Every active archive of one tenant, across every table — the
/// offboarding purge's cold-storage sweep list.
pub async fn active_for_tenant(
    conn: &mut PgConnection,
    tenant_id: i64,
) -> Result<Vec<ArchiveRecord>, sqlx::Error> {
    let rows: Vec<ArchiveRow> = sqlx::query_as(
        "SELECT id, schema_name, table_name, key_column, tenant_id,
                range_start, range_end, row_count, checksum, storage_path,
                bytes, status, created_at, restored_at, purged_at
         FROM platform.archives
         WHERE tenant_id = $1 AND status = 'archived'
         ORDER BY schema_name, table_name, range_start",
    )
    .bind(tenant_id)
    .fetch_all(conn)
    .await?;
    Ok(rows.into_iter().map(to_record).collect())
}

/// Mark an archive purged (the cold object is deleted by the caller).
/// Only active archives can be purged — restoring a purged archive is
/// not a thing, and the row stays as the evidence that it happened.
pub async fn mark_purged(conn: &mut PgConnection, archive_id: i64) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE platform.archives
         SET status = 'purged', purged_at = now()
         WHERE id = $1 AND status = 'archived'",
    )
    .bind(archive_id)
    .execute(conn)
    .await?;
    Ok(())
}
