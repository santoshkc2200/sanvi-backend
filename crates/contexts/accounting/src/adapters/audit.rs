use sanvi_audit::{ActorType, AuditEntryData};
use sanvi_kernel::RawIdGenerator;
use sanvi_persistence::ScopedTx;

use crate::{application::ports::AuditRecord, domain::AccountingError};

fn storage(error: impl std::fmt::Display) -> AccountingError {
    AccountingError::Storage(error.to_string())
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
