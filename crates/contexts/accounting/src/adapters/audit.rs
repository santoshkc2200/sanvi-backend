use std::sync::Arc;

use sanvi_audit::{ActorType, AuditEntryData, domain::AuditWriter};
use sanvi_kernel::RawIdGenerator;
use sanvi_persistence::{ScopedTx, TxManager};

use crate::{application::ports::AuditRecord, domain::AccountingError};

fn storage(error: impl std::fmt::Display) -> AccountingError {
    AccountingError::Storage(error.to_string())
}

pub struct AuditDelivery {
    transactions: TxManager,
    writer: Arc<dyn AuditWriter>,
}

impl AuditDelivery {
    pub fn new(transactions: TxManager, writer: Arc<dyn AuditWriter>) -> Self {
        Self {
            transactions,
            writer,
        }
    }

    pub async fn deliver(&self, limit: i64) -> Result<usize, AccountingError> {
        let mut scope = self
            .transactions
            .begin_unscoped("deliver_accounting_audits", "accounting-worker")
            .await
            .map_err(storage)?;

        struct PendingRow {
            id: i64,
            entry: serde_json::Value,
        }

        let pending = sqlx::query_as!(
            PendingRow,
            "SELECT id, entry FROM accounting.audit_commands WHERE delivered_at IS NULL ORDER BY created_at, id LIMIT $1 FOR UPDATE SKIP LOCKED",
            limit
        )
        .fetch_all(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?;

        let count = pending.len();
        for row in pending {
            let entry: AuditEntryData = serde_json::from_value(row.entry).map_err(storage)?;
            self.writer
                .append(&mut scope, &entry)
                .await
                .map_err(storage)?;
            sqlx::query!(
                "UPDATE accounting.audit_commands SET delivered_at = now() WHERE id = $1",
                row.id
            )
            .execute(&mut **scope.tx_mut().as_inner())
            .await
            .map_err(storage)?;
        }

        scope.commit().await.map_err(storage)?;
        Ok(count)
    }
}

pub async fn record(
    scope: &mut ScopedTx<'_>,
    ids: &dyn RawIdGenerator,
    record: &AuditRecord,
) -> Result<(), AccountingError> {
    let id = ids.next_id().map_err(storage)?;
    let entry = serde_json::to_value(AuditEntryData {
        occurred_at: record.occurred_at,
        actor_type: ActorType::User,
        actor_id: Some(record.user_id),
        on_behalf_of: None,
        tenant_id: Some(record.tenant_id),
        action: record.action.to_string(),
        resource_type: Some("accounting_book".to_string()),
        resource_id: Some(record.book_id.to_string()),
        before: None,
        after: Some(record.after.clone()),
        request_id: record.request.request_id.clone(),
        ip: record.request.ip,
        user_agent: record.request.user_agent.clone(),
    })
    .map_err(storage)?;

    sqlx::query!(
        "INSERT INTO accounting.audit_commands (id, tenant_id, book_id, entry, created_at) VALUES ($1, $2, $3, $4, $5)",
        id.as_i64(),
        record.tenant_id.as_i64(),
        record.book_id.as_i64(),
        entry,
        record.occurred_at,
    )
    .execute(&mut **scope.tx_mut().as_inner())
    .await
    .map_err(storage)?;

    Ok(())
}
