//! The archive mover: moves a range of a table to sealed cold storage,
//! records it in `platform.archives`, and restores it back with a
//! checksum proof.
//!
//! Every archive is one transaction: serialize the range (ordered by the
//! range key, `to_jsonb` row shapes), seal it, write the cold object,
//! insert the registry row, delete the moved rows — commit. A crash at
//! any point leaves either the table intact or a complete archive; the
//! unique index on active archives refuses a re-run over the same range.
//!
//! Append-only guards (`audit.entries`'s no-UPDATE/DELETE trigger) are
//! lifted for the delete and put back in the same transaction: archival
//! *is* the documented removal path for those tables, and only the
//! guard's owner (the migrator role the CLI connects as) can lift it.

use std::sync::Arc;
use std::time::Instant;

use chrono::{DateTime, Utc};
use sanvi_persistence::TxManager;
use sha2::{Digest, Sha256};
use sqlx::PgConnection;

use crate::storage::SealedStorage;
use crate::{ArchivalError, ranges};

/// One archival target: a table, a half-open time range on its key
/// column, and whose rows move.
#[derive(Debug, Clone)]
pub struct ArchiveTarget {
    pub schema_name: String,
    pub table_name: String,
    /// The range key column the mover orders and slices on.
    pub key_column: String,
    /// `Some` archives that tenant's rows only — a neighbour's rows in
    /// the same range stay in the table (and in their own archives).
    /// `None` on a tenant-keyed table archives the NULL-tenant
    /// (platform-scope) rows; on a platform table it archives everything
    /// in the range.
    pub tenant_id: Option<i64>,
    pub range_start: DateTime<Utc>,
    pub range_end: DateTime<Utc>,
}

impl ArchiveTarget {
    fn qualified(&self) -> String {
        format!("{}.{}", self.schema_name, self.table_name)
    }
}

/// What one archive pass did, for the operator report.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ArchiveOutcome {
    /// 0 for an empty range (nothing was moved or recorded).
    pub archive_id: i64,
    pub table: String,
    pub tenant_id: Option<i64>,
    pub range_start: DateTime<Utc>,
    pub range_end: DateTime<Utc>,
    pub row_count: i64,
    pub checksum: String,
    pub bytes: u64,
    pub duration_ms: u128,
}

/// What one restore pass proved.
#[derive(Debug, Clone, serde::Serialize)]
pub struct RestoreOutcome {
    pub archive_id: i64,
    pub table: String,
    pub row_count: i64,
    pub checksum: String,
    pub duration_ms: u128,
}

/// The mover. `ids` mints archive ids (snowflakes, like every other
/// first-party entity id).
pub struct ArchiveMover {
    tx: TxManager,
    ids: Arc<dyn sanvi_kernel::RawIdGenerator>,
    storage: Arc<SealedStorage>,
}

/// Tables whose append-only trigger must be lifted for the move's delete.
/// Registry-minted names only, same discipline as the partition
/// functions.
fn append_only_guard(schema: &str, table: &str) -> Option<&'static str> {
    match (schema, table) {
        ("audit", "entries") => Some("entries_no_update"),
        _ => None,
    }
}

pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

/// Identifier quoting for registry-minted schema/table/column names.
pub(crate) fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// The canonical tenant filter of one archive scope. `None` on a
/// tenant-keyed table means the NULL-tenant rows; on a platform table it
/// means no filter at all.
async fn tenant_filter(
    conn: &mut PgConnection,
    schema: &str,
    table: &str,
    tenant_id: Option<i64>,
) -> Result<String, ArchivalError> {
    match tenant_id {
        Some(tenant) => Ok(format!(" AND tenant_id = {tenant}")),
        None => {
            let tenant_keyed: bool = sqlx::query_scalar(
                "SELECT EXISTS (
                   SELECT 1 FROM information_schema.columns
                   WHERE table_schema = $1 AND table_name = $2
                     AND column_name = 'tenant_id')",
            )
            .bind(schema)
            .bind(table)
            .fetch_one(&mut *conn)
            .await
            .map_err(ArchivalError::storage)?;
            Ok(if tenant_keyed {
                " AND tenant_id IS NULL".to_string()
            } else {
                String::new()
            })
        }
    }
}

/// The canonical serialization of a range: full rows, ordered by the
/// range key then id, one JSON array. Archive and restore both build
/// this, so the checksums are comparable facts.
async fn serialize_range(
    conn: &mut PgConnection,
    schema: &str,
    table: &str,
    key_column: &str,
    tenant_id: Option<i64>,
    range_start: DateTime<Utc>,
    range_end: DateTime<Utc>,
) -> Result<serde_json::Value, ArchivalError> {
    let tenant_clause = tenant_filter(&mut *conn, schema, table, tenant_id).await?;
    let key = quote_ident(key_column);
    let statement = format!(
        r#"SELECT COALESCE(jsonb_agg(row), '[]'::jsonb)
           FROM (
             SELECT * FROM {}.{}
             WHERE {key} >= $1 AND {key} < $2{tenant_clause}
             ORDER BY {key}, id
           ) row"#,
        quote_ident(schema),
        quote_ident(table),
    );
    sqlx::query_scalar(sqlx::AssertSqlSafe(statement))
        .bind(range_start)
        .bind(range_end)
        .fetch_one(&mut *conn)
        .await
        .map_err(ArchivalError::storage)
}

impl ArchiveMover {
    pub fn new(
        tx: TxManager,
        ids: Arc<dyn sanvi_kernel::RawIdGenerator>,
        storage: Arc<SealedStorage>,
    ) -> Self {
        Self { tx, ids, storage }
    }

    /// Move one range to cold storage. Refuses an overlapping active
    /// archive before touching anything.
    pub async fn archive(&self, target: &ArchiveTarget) -> Result<ArchiveOutcome, ArchivalError> {
        let started = Instant::now();

        let mut scope = self
            .tx
            .begin_unscoped("archival_archive", "platform-archival")
            .await
            .map_err(|err| ArchivalError::storage(err.to_string()))?;
        {
            // The mover runs as the table owner (the migrator role the
            // CLI connects as), but FORCE ROW LEVEL SECURITY binds the
            // owner too: the audited platform-scope escape is the
            // documented way platform maintenance crosses tenant
            // policies. Transaction-local, exactly like
            // advertising.enforce_retention.
            let conn = &mut **scope.tx_mut().as_inner();
            sqlx::query("SELECT set_config('sanvi.platform_scope', 'on', true)")
                .execute(&mut *conn)
                .await
                .map_err(ArchivalError::storage)?;
        }

        // Precondition, own borrow: the range must not overlap an active
        // archive of the same table and tenant scope. Overlap, not
        // equality — a partial re-archive would fork the cold copy of the
        // range.
        let overlap = {
            let conn = &mut **scope.tx_mut().as_inner();
            ranges::overlapping_active(&mut *conn, target)
                .await
                .map_err(ArchivalError::storage)?
        };
        if let Some(archive_id) = overlap {
            return Err(ArchivalError::RangeArchived {
                range_start: target.range_start,
                range_end: target.range_end,
                schema_name: target.schema_name.clone(),
                table_name: target.table_name.clone(),
                archive_id,
            });
        }

        let payload = {
            let conn = &mut **scope.tx_mut().as_inner();
            serialize_range(
                conn,
                &target.schema_name,
                &target.table_name,
                &target.key_column,
                target.tenant_id,
                target.range_start,
                target.range_end,
            )
            .await?
        };

        let row_count = payload
            .as_array()
            .map(|rows| rows.len() as i64)
            .unwrap_or(0);
        if row_count == 0 {
            // An empty range is a no-op wearing a procedure's clothes:
            // nothing moves, nothing is recorded.
            scope.rollback().await.map_err(ArchivalError::storage)?;
            return Ok(ArchiveOutcome {
                archive_id: 0,
                table: target.qualified(),
                tenant_id: target.tenant_id,
                range_start: target.range_start,
                range_end: target.range_end,
                row_count: 0,
                checksum: sha256_hex(b"[]"),
                bytes: 0,
                duration_ms: started.elapsed().as_millis(),
            });
        }

        let plaintext = serde_json::to_vec(&payload)
            .map_err(|err| ArchivalError::storage(format!("serialize archive: {err}")))?;
        let checksum = sha256_hex(&plaintext);

        let archive_id = self
            .ids
            .next_id()
            .map_err(|err| ArchivalError::storage(format!("id generation: {err}")))?
            .as_i64();
        let storage_path = format!(
            "{}/{}/{}/{}.enc",
            target.schema_name,
            target.table_name,
            match target.tenant_id {
                Some(tenant) => tenant.to_string(),
                None => "platform".to_string(),
            },
            archive_id
        );

        // Seal and write the cold object before any row is deleted: a
        // failure here leaves the table untouched.
        let bytes = self.storage.put(&storage_path, &plaintext)?;

        // Registry row, then the delete, in the same transaction: a
        // committed archive always has its evidence row and its rows
        // gone; anything else rolls back together.
        let outcome = {
            let conn = &mut **scope.tx_mut().as_inner();

            sqlx::query(
                "INSERT INTO platform.archives
                   (id, schema_name, table_name, key_column, tenant_id,
                    range_start, range_end, row_count, checksum, storage_path, bytes, status)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, 'archived')",
            )
            .bind(archive_id)
            .bind(&target.schema_name)
            .bind(&target.table_name)
            .bind(&target.key_column)
            .bind(target.tenant_id)
            .bind(target.range_start)
            .bind(target.range_end)
            .bind(row_count)
            .bind(&checksum)
            .bind(&storage_path)
            .bind(bytes as i64)
            .execute(&mut *conn)
            .await
            .map_err(ArchivalError::storage)?;

            let tenant_clause = tenant_filter(
                conn,
                &target.schema_name,
                &target.table_name,
                target.tenant_id,
            )
            .await?;
            let key = quote_ident(&target.key_column);
            if let Some(guard) = append_only_guard(&target.schema_name, &target.table_name) {
                // Archival is the documented removal path for append-only
                // tables (the audit chain's own runbook says so); the
                // guard comes off for this delete and back on inside the
                // same transaction.
                sqlx::query(sqlx::AssertSqlSafe(format!(
                    "ALTER TABLE {}.{} DISABLE TRIGGER {}",
                    quote_ident(&target.schema_name),
                    quote_ident(&target.table_name),
                    quote_ident(guard)
                )))
                .execute(&mut *conn)
                .await
                .map_err(ArchivalError::storage)?;
            }
            let deleted = sqlx::query(sqlx::AssertSqlSafe(format!(
                "DELETE FROM {}.{} WHERE {key} >= $1 AND {key} < $2{tenant_clause}",
                quote_ident(&target.schema_name),
                quote_ident(&target.table_name),
            )))
            .bind(target.range_start)
            .bind(target.range_end)
            .execute(&mut *conn)
            .await
            .map_err(ArchivalError::storage)?
            .rows_affected() as i64;
            if let Some(guard) = append_only_guard(&target.schema_name, &target.table_name) {
                sqlx::query(sqlx::AssertSqlSafe(format!(
                    "ALTER TABLE {}.{} ENABLE TRIGGER {}",
                    quote_ident(&target.schema_name),
                    quote_ident(&target.table_name),
                    quote_ident(guard)
                )))
                .execute(&mut *conn)
                .await
                .map_err(ArchivalError::storage)?;
            }
            deleted
        };

        if outcome != row_count {
            // Never commit a partial move: rows changed under the
            // serialization. Roll everything back, remove the orphaned
            // cold object, and say so loudly.
            scope.rollback().await.map_err(ArchivalError::storage)?;
            let _ = self.storage.delete(&storage_path);
            return Err(ArchivalError::RowCountMismatch {
                archive_id,
                archived: row_count,
                restored: outcome,
            });
        }

        scope.commit().await.map_err(ArchivalError::storage)?;

        tracing::info!(
            archive_id,
            table = %target.qualified(),
            tenant_id = ?target.tenant_id,
            row_count,
            bytes,
            duration_ms = started.elapsed().as_millis(),
            "archival: range moved to cold storage"
        );
        Ok(ArchiveOutcome {
            archive_id,
            table: target.qualified(),
            tenant_id: target.tenant_id,
            range_start: target.range_start,
            range_end: target.range_end,
            row_count,
            checksum,
            bytes,
            duration_ms: started.elapsed().as_millis(),
        })
    }

    /// Restore an archived range back into its table and prove it: same
    /// row count, same canonical checksum, ordering included.
    pub async fn restore(&self, archive_id: i64) -> Result<RestoreOutcome, ArchivalError> {
        let started = Instant::now();

        let record = ranges::by_id(self.tx.pool(), archive_id)
            .await
            .map_err(ArchivalError::storage)?
            .ok_or(ArchivalError::NotFound(archive_id))?;
        if record.status != "archived" {
            return Err(ArchivalError::WrongStatus {
                archive_id,
                status: record.status,
                expected: "archived".to_string(),
            });
        }

        let plaintext = self.storage.get(&record.storage_path)?;
        let payload: serde_json::Value = serde_json::from_slice(&plaintext)
            .map_err(|err| ArchivalError::storage(format!("archive payload is not JSON: {err}")))?;

        let mut scope = self
            .tx
            .begin_unscoped("archival_restore", "platform-archival")
            .await
            .map_err(|err| ArchivalError::storage(err.to_string()))?;
        {
            // Same audited platform-scope escape as the archive pass: the
            // restored rows include tenant rows the owner could not
            // otherwise write under FORCE RLS.
            let conn = &mut **scope.tx_mut().as_inner();
            sqlx::query("SELECT set_config('sanvi.platform_scope', 'on', true)")
                .execute(&mut *conn)
                .await
                .map_err(ArchivalError::storage)?;
        }

        let (restored, verify_checksum) = {
            let conn = &mut **scope.tx_mut().as_inner();
            let table = format!(
                "{}.{}",
                quote_ident(&record.schema_name),
                quote_ident(&record.table_name)
            );
            // Stage through the table's own row type so jsonb
            // rehydration is exact (the tenant-restore mechanism
            // verbatim), then insert and count.
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "CREATE TEMP TABLE archival_restore_src ON COMMIT DROP AS
                 SELECT * FROM jsonb_populate_recordset(NULL::{table}, $1::jsonb)",
            )))
            .bind(&payload)
            .execute(&mut *conn)
            .await
            .map_err(ArchivalError::storage)?;

            let restored: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                "WITH ins AS (INSERT INTO {table} SELECT * FROM archival_restore_src RETURNING 1)
                     SELECT count(*) FROM ins"
            )))
            .fetch_one(&mut *conn)
            .await
            .map_err(ArchivalError::storage)?;

            let verify = serialize_range(
                conn,
                &record.schema_name,
                &record.table_name,
                &record.key_column,
                record.tenant_id,
                record.range_start,
                record.range_end,
            )
            .await?;
            let verify_bytes = serde_json::to_vec(&verify)
                .map_err(|err| ArchivalError::storage(format!("serialize verification: {err}")))?;
            (restored, sha256_hex(&verify_bytes))
        };

        // The proof: what the table now holds over the restored range
        // checksums to what was archived.
        if verify_checksum != record.checksum {
            scope.rollback().await.map_err(ArchivalError::storage)?;
            return Err(ArchivalError::ChecksumMismatch {
                archive_id,
                archived: record.checksum,
                restored: verify_checksum,
            });
        }
        if restored != record.row_count {
            scope.rollback().await.map_err(ArchivalError::storage)?;
            return Err(ArchivalError::RowCountMismatch {
                archive_id,
                archived: record.row_count,
                restored,
            });
        }

        {
            let conn = &mut **scope.tx_mut().as_inner();
            sqlx::query(
                "UPDATE platform.archives
                 SET status = 'restored', restored_at = now()
                 WHERE id = $1 AND status = 'archived'",
            )
            .bind(archive_id)
            .execute(&mut *conn)
            .await
            .map_err(ArchivalError::storage)?;
        }

        scope.commit().await.map_err(ArchivalError::storage)?;

        tracing::info!(
            archive_id,
            table = %record.schema_table(),
            row_count = restored,
            duration_ms = started.elapsed().as_millis(),
            "archival: range restored with identical checksum"
        );
        Ok(RestoreOutcome {
            archive_id,
            table: record.schema_table(),
            row_count: restored,
            checksum: verify_checksum,
            duration_ms: started.elapsed().as_millis(),
        })
    }
}
