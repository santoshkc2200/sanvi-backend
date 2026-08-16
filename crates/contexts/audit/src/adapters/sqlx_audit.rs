//! The sqlx audit reader/writer over `audit.entries`.
//!
//! Appends are serialized with a transaction-scoped advisory lock (one
//! global chain, so concurrent writers must not fork it): take the lock,
//! read the current tail hash (`ORDER BY seq DESC LIMIT 1`), insert the
//! entry linking to it. Everything runs inside the caller's transaction.

use std::net::IpAddr;

use async_trait::async_trait;
use sanvi_persistence::PlatformScope;
use sqlx::FromRow;

use crate::domain::{
    ActorType, AuditCursor, AuditEntry, AuditEntryData, AuditError, AuditFilters, AuditReader,
    AuditWriter, GENESIS_HASH,
};

/// The single global chain lock (arbitrary constant; only the audit writer
/// uses it).
const CHAIN_LOCK_KEY: i64 = 0x5356_4155_4449_5401;

#[derive(Debug, Clone, FromRow)]
struct EntryRow {
    seq: i64,
    id: uuid::Uuid,
    occurred_at: chrono::DateTime<chrono::Utc>,
    actor_type: String,
    actor_id: Option<uuid::Uuid>,
    on_behalf_of: Option<uuid::Uuid>,
    tenant_id: Option<uuid::Uuid>,
    action: String,
    resource_type: Option<String>,
    resource_id: Option<String>,
    before: Option<serde_json::Value>,
    after: Option<serde_json::Value>,
    request_id: Option<String>,
    ip: Option<IpAddr>,
    user_agent: Option<String>,
    prev_hash: Vec<u8>,
    hash: Vec<u8>,
}

fn to_entry(row: EntryRow) -> Result<AuditEntry, AuditError> {
    let prev_hash: [u8; 32] = row
        .prev_hash
        .try_into()
        .map_err(|_| AuditError::Storage("prev_hash is not 32 bytes".to_string()))?;
    let hash: [u8; 32] = row
        .hash
        .try_into()
        .map_err(|_| AuditError::Storage("hash is not 32 bytes".to_string()))?;
    Ok(AuditEntry::from_parts(
        row.seq,
        row.id,
        row.occurred_at,
        ActorType::parse(&row.actor_type)?,
        row.actor_id,
        row.on_behalf_of,
        row.tenant_id,
        row.action,
        row.resource_type,
        row.resource_id,
        row.before,
        row.after,
        row.request_id,
        row.ip,
        row.user_agent,
        prev_hash,
        hash,
    ))
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SqlxAuditWriter;

impl SqlxAuditWriter {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl AuditWriter for SqlxAuditWriter {
    async fn append(
        &self,
        scope: &mut PlatformScope<'_>,
        data: &AuditEntryData,
    ) -> Result<i64, AuditError> {
        // Serialize all chain appends (transaction-scoped: released on
        // commit/rollback) so two writers can never link to the same tail.
        sqlx::query("SELECT pg_advisory_xact_lock($1)")
            .bind(CHAIN_LOCK_KEY)
            .execute(&mut **scope.tx_mut().as_inner())
            .await
            .map_err(|err| AuditError::Storage(err.to_string()))?;

        let prev: Option<Vec<u8>> =
            sqlx::query_scalar("SELECT hash FROM audit.entries ORDER BY seq DESC LIMIT 1")
                .fetch_optional(&mut **scope.tx_mut().as_inner())
                .await
                .map_err(|err| AuditError::Storage(err.to_string()))?;

        let prev_hash: [u8; 32] = match prev {
            Some(bytes) => bytes
                .try_into()
                .map_err(|_| AuditError::Storage("tail hash is not 32 bytes".to_string()))?,
            None => GENESIS_HASH,
        };
        let entry = AuditEntry::new(data.clone(), prev_hash)?;

        let seq: i64 = sqlx::query_scalar(
            "INSERT INTO audit.entries
               (id, occurred_at, actor_type, actor_id, on_behalf_of, tenant_id,
                action, resource_type, resource_id, before, after, request_id,
                ip, user_agent, prev_hash, hash)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16)
             RETURNING seq",
        )
        .bind(entry.id)
        .bind(entry.occurred_at)
        .bind(entry.actor_type.as_str())
        .bind(entry.actor_id)
        .bind(entry.on_behalf_of)
        .bind(entry.tenant_id)
        .bind(&entry.action)
        .bind(&entry.resource_type)
        .bind(&entry.resource_id)
        .bind(&entry.before)
        .bind(&entry.after)
        .bind(&entry.request_id)
        .bind(entry.ip)
        .bind(&entry.user_agent)
        .bind(entry.prev_hash.as_slice())
        .bind(entry.hash.as_slice())
        .fetch_one(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(|err| AuditError::Storage(err.to_string()))?;
        Ok(seq)
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SqlxAuditReader;

impl SqlxAuditReader {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl AuditReader for SqlxAuditReader {
    async fn list(
        &self,
        scope: &mut PlatformScope<'_>,
        filters: &AuditFilters,
        after: Option<AuditCursor>,
        limit: u32,
        ascending: bool,
    ) -> Result<(Vec<AuditEntry>, Option<AuditCursor>), AuditError> {
        // Two explicit statements (no dynamic SQL): sqlx audits query
        // strings and the ordering difference is only these two shapes.
        let rows: Vec<EntryRow> = if ascending {
            sqlx::query_as(
                "SELECT seq, id, occurred_at, actor_type, actor_id, on_behalf_of, tenant_id,
                        action, resource_type, resource_id, before, after, request_id,
                        ip, user_agent, prev_hash, hash
                 FROM audit.entries
                 WHERE ($1::uuid IS NULL OR tenant_id = $1)
                   AND ($2::uuid IS NULL OR actor_id = $2)
                   AND ($3::text IS NULL OR action = $3)
                   AND ($4::timestamptz IS NULL OR occurred_at >= $4)
                   AND ($5::timestamptz IS NULL OR occurred_at < $5)
                   AND ($6::bigint IS NULL OR seq > $6)
                 ORDER BY seq ASC
                 LIMIT $7",
            )
            .bind(filters.tenant_id)
            .bind(filters.actor_id)
            .bind(&filters.action)
            .bind(filters.since)
            .bind(filters.until)
            .bind(after.map(|cursor| cursor.seq))
            .bind(i64::from(limit))
            .fetch_all(&mut **scope.tx_mut().as_inner())
            .await
            .map_err(|err| AuditError::Storage(err.to_string()))?
        } else {
            sqlx::query_as(
                "SELECT seq, id, occurred_at, actor_type, actor_id, on_behalf_of, tenant_id,
                        action, resource_type, resource_id, before, after, request_id,
                        ip, user_agent, prev_hash, hash
                 FROM audit.entries
                 WHERE ($1::uuid IS NULL OR tenant_id = $1)
                   AND ($2::uuid IS NULL OR actor_id = $2)
                   AND ($3::text IS NULL OR action = $3)
                   AND ($4::timestamptz IS NULL OR occurred_at >= $4)
                   AND ($5::timestamptz IS NULL OR occurred_at < $5)
                   AND ($6::bigint IS NULL OR seq < $6)
                 ORDER BY seq DESC
                 LIMIT $7",
            )
            .bind(filters.tenant_id)
            .bind(filters.actor_id)
            .bind(&filters.action)
            .bind(filters.since)
            .bind(filters.until)
            .bind(after.map(|cursor| cursor.seq))
            .bind(i64::from(limit))
            .fetch_all(&mut **scope.tx_mut().as_inner())
            .await
            .map_err(|err| AuditError::Storage(err.to_string()))?
        };

        let next_cursor = rows.last().map(|row| AuditCursor { seq: row.seq });
        let mut entries = Vec::with_capacity(rows.len());
        for row in rows {
            entries.push(to_entry(row)?);
        }
        Ok((entries, next_cursor))
    }
}

/// Convenience for tests and callers that build entries directly.
pub fn entry_data(
    occurred_at: chrono::DateTime<chrono::Utc>,
    actor_type: ActorType,
    actor_id: Option<uuid::Uuid>,
    tenant_id: Option<uuid::Uuid>,
    action: impl Into<String>,
) -> AuditEntryData {
    AuditEntryData {
        occurred_at,
        actor_type,
        actor_id,
        on_behalf_of: None,
        tenant_id,
        action: action.into(),
        resource_type: None,
        resource_id: None,
        before: None,
        after: None,
        request_id: None,
        ip: None,
        user_agent: None,
    }
}
