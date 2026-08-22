//! CSV export — server-side, streamed, tenant-scoped, audited.
//!
//! The column set is fixed and documented: it will be pasted into
//! accounting software, and changing columns later breaks tenants'
//! workflows. Any column change is a breaking API change.
//!
//! Columns (in order, header row first):
//! `payment_id,external_payment_id,checkout_reference,amount_minor,currency,status,fee_minor,fee_currency,captured_at,created_at,refunded_minor,dispute_status`
//!
//! The response is streamed: rows are fetched page by page through the
//! keyset cursor and flushed as they are encoded, so memory stays bounded
//! no matter how long the tenant's history is.

use std::net::IpAddr;
use std::sync::Arc;

use actix_web::web::Bytes;
use chrono::Utc;
use futures_util::stream::unfold;
use uuid::Uuid;

use sanvi_audit::domain::{ActorType, AuditEntryData, AuditWriter};
use sanvi_persistence::TxManager;

use crate::domain::error::PaymentsError;
use crate::domain::ports::{DisputeRepository, PaymentFilter, PaymentRepository, RefundRepository};

/// The fixed, documented column set. The header row is part of the
/// contract; adding, removing or reordering columns is a breaking
/// change (accounting software pastes it).
pub const CSV_COLUMNS: &[&str] = &[
    "payment_id",
    "external_payment_id",
    "checkout_reference",
    "amount_minor",
    "currency",
    "status",
    "fee_minor",
    "fee_currency",
    "captured_at",
    "created_at",
    "refunded_minor",
    "dispute_status",
];

/// Rows per streamed page — the memory bound of one chunk.
const PAGE_SIZE: u32 = 200;

pub struct ExportPayments {
    tx_manager: TxManager,
    payments: Arc<dyn PaymentRepository>,
    refunds: Arc<dyn RefundRepository>,
    disputes: Arc<dyn DisputeRepository>,
    checkouts: Arc<dyn crate::domain::CheckoutRepository>,
    audit: Arc<dyn AuditWriter>,
}

#[derive(Debug, Clone, Default)]
pub struct ExportCommand {
    pub tenant_id: Uuid,
    pub actor: Uuid,
    pub ip: Option<IpAddr>,
    pub status: Option<String>,
    pub currency: Option<String>,
    pub customer: Option<String>,
    pub date_from: Option<chrono::DateTime<Utc>>,
    pub date_to: Option<chrono::DateTime<Utc>>,
}

impl ExportPayments {
    pub fn new(
        tx_manager: TxManager,
        payments: Arc<dyn PaymentRepository>,
        refunds: Arc<dyn RefundRepository>,
        disputes: Arc<dyn DisputeRepository>,
        checkouts: Arc<dyn crate::domain::CheckoutRepository>,
        audit: Arc<dyn AuditWriter>,
    ) -> Self {
        Self {
            tx_manager,
            payments,
            refunds,
            disputes,
            checkouts,
            audit,
        }
    }

    /// Validate the filters, audit the export attempt, and return a
    /// stream of CSV chunks (header row first). Tenant-scoped via RLS on
    /// every page query.
    ///
    /// The audit fires before the first byte — an aborted download is
    /// still an export attempt against tenant data, which is exactly what
    /// the audit trail exists for.
    pub async fn execute_stream(
        &self,
        command: ExportCommand,
    ) -> Result<
        impl futures_util::Stream<Item = Result<Bytes, PaymentsError>> + 'static,
        PaymentsError,
    > {
        let filter = export_filter(&command)?;
        self.audit_export(&command).await;

        let state = StreamState {
            tx_manager: self.tx_manager.clone(),
            payments: self.payments.clone(),
            refunds: self.refunds.clone(),
            disputes: self.disputes.clone(),
            checkouts: self.checkouts.clone(),
            tenant_id: command.tenant_id,
            filter,
            cursor: None,
            header_pending: true,
            finished: false,
        };
        Ok(unfold(state, |mut state| async move {
            if state.header_pending {
                state.header_pending = false;
                let mut wtr = csv::Writer::from_writer(Vec::new());
                if let Err(err) = wtr.write_record(CSV_COLUMNS) {
                    return Some((
                        Err(PaymentsError::Storage(format!("csv header write: {err}"))),
                        state,
                    ));
                }
                match wtr.into_inner() {
                    Ok(bytes) => {
                        return Some((Ok(Bytes::from(bytes)), state));
                    }
                    Err(err) => {
                        return Some((
                            Err(PaymentsError::Storage(format!(
                                "csv header finalize: {err}"
                            ))),
                            state,
                        ));
                    }
                }
            }
            if state.finished {
                return None;
            }
            let cursor = state.cursor.take();
            // The page work borrows `state` and returns owned bytes, so
            // the borrow is over before `state` moves into the next step.
            match stream_page(&state, cursor).await {
                Ok((chunk, next_cursor)) => {
                    // A page without a next_cursor is the last one: emit
                    // its chunk and mark the stream done.
                    state.finished = next_cursor.is_none();
                    state.cursor = next_cursor;
                    if chunk.is_empty() && state.cursor.is_none() {
                        None
                    } else {
                        Some((Ok(Bytes::from(chunk)), state))
                    }
                }
                Err(err) => Some((Err(err), state)),
            }
        }))
    }

    /// Best-effort, unscoped: a lost audit entry is logged loudly but
    /// never fails the export.
    async fn audit_export(&self, command: &ExportCommand) {
        let Ok(mut scope) = self
            .tx_manager
            .begin_unscoped("audit_payments_export", "payments")
            .await
        else {
            tracing::error!("failed to open audit scope for export");
            return;
        };
        if let Err(err) = self
            .audit
            .append(
                &mut scope,
                &AuditEntryData {
                    occurred_at: Utc::now(),
                    actor_type: ActorType::User,
                    actor_id: Some(command.actor),
                    on_behalf_of: None,
                    tenant_id: Some(command.tenant_id),
                    action: "payments.export".to_string(),
                    resource_type: Some("payments.payments".to_string()),
                    resource_id: None,
                    before: None,
                    after: Some(serde_json::json!({
                        // Streamed: the row count is not known until the
                        // client has consumed the body.
                        "streamed": true,
                        "filters": {
                            "status": command.status,
                            "currency": command.currency,
                            "customer": command.customer,
                        }
                    })),
                    request_id: None,
                    ip: command.ip,
                    user_agent: None,
                },
            )
            .await
        {
            tracing::error!(error = %err, "failed to audit payments export");
        }
        if let Err(err) = scope.commit().await {
            tracing::error!(error = %err, "failed to commit payments export audit");
        }
    }
}

/// One page of work: query, enrich, encode. Everything it returns is
/// owned, so the borrow of `state` ends when this future resolves —
/// which is what lets the unfold step hand `state` to the next step.
async fn stream_page(
    state: &StreamState,
    cursor: Option<String>,
) -> Result<(Vec<u8>, Option<String>), PaymentsError> {
    let tenant_id = state.tenant_id;
    let mut scope = state
        .tx_manager
        .begin_for(tenant_id)
        .await
        .map_err(|err| PaymentsError::Storage(err.to_string()))?;
    let result = stream_page_inner(state, &mut scope, cursor).await;
    let _ = scope.rollback().await;
    result
}

/// Records encode one at a time so quoting never crosses a chunk boundary.
async fn stream_page_inner(
    state: &StreamState,
    scope: &mut sanvi_persistence::ScopedTx<'_>,
    cursor: Option<String>,
) -> Result<(Vec<u8>, Option<String>), PaymentsError> {
    let mut page_filter = state.filter.clone();
    page_filter.cursor = cursor;
    page_filter.limit = Some(PAGE_SIZE);
    let page = state
        .payments
        .list(scope, state.tenant_id, &page_filter)
        .await?;
    let next_cursor = page.next_cursor.clone();
    let mut chunk: Vec<u8> = Vec::new();
    for payment in &page.items {
        let refunded_minor = state
            .refunds
            .sum_succeeded_for_payment(scope, state.tenant_id, payment.id)
            .await?;
        // The current dispute is the most recently updated one — a
        // reopened or superseded dispute must win over an old row.
        let dispute_status = state
            .disputes
            .list_for_payment(scope, state.tenant_id, payment.id)
            .await?
            .iter()
            .max_by_key(|d| d.updated_at)
            .map(|d| d.status.as_str().to_string())
            .unwrap_or_default();
        let checkout_ref = match payment.checkout_id {
            Some(checkout_id) => state
                .checkouts
                .find_by_id(scope, state.tenant_id, checkout_id)
                .await?
                .map(|c| c.reference.as_str().to_string())
                .unwrap_or_default(),
            None => String::new(),
        };

        let mut wtr = csv::Writer::from_writer(Vec::new());
        let row = payment_row(payment, refunded_minor, dispute_status, checkout_ref);
        if let Err(err) = wtr.write_record(row.iter().map(String::as_str)) {
            return Err(PaymentsError::Storage(format!("csv row write: {err}")));
        }
        match wtr.into_inner() {
            Ok(bytes) => chunk.extend_from_slice(&bytes),
            Err(err) => {
                return Err(PaymentsError::Storage(format!("csv row write: {err}")));
            }
        }
    }
    Ok((chunk, next_cursor))
}

/// Everything one unfold step needs: owned clones, so the stream is
/// `'static` and never borrows from the use case.
struct StreamState {
    tx_manager: TxManager,
    payments: Arc<dyn PaymentRepository>,
    refunds: Arc<dyn RefundRepository>,
    disputes: Arc<dyn DisputeRepository>,
    checkouts: Arc<dyn crate::domain::CheckoutRepository>,
    tenant_id: Uuid,
    filter: PaymentFilter,
    cursor: Option<String>,
    header_pending: bool,
    /// The last page carried no next_cursor: after its chunk is flushed,
    /// the stream is done — re-querying would repeat the same rows.
    finished: bool,
}

fn export_filter(command: &ExportCommand) -> Result<PaymentFilter, PaymentsError> {
    let mut filter = PaymentFilter::default();
    filter.customer = command.customer.clone();
    filter.date_from = command.date_from;
    filter.date_to = command.date_to;
    if let Some(raw) = command.status.clone() {
        let trimmed = raw.trim().to_string();
        if !trimmed.is_empty() {
            match crate::domain::PaymentStatus::parse(&trimmed) {
                Some(status) => filter.status = Some(status),
                None => {
                    return Err(PaymentsError::InvalidConnection(format!(
                        "unknown payment status {trimmed:?}"
                    )));
                }
            }
        }
    }
    if let Some(raw) = command.currency.clone() {
        let trimmed = raw.trim();
        if !trimmed.is_empty() {
            filter.currency = Some(crate::domain::Currency::parse(trimmed)?);
        }
    }
    Ok(filter)
}

type Row = [String; CSV_COLUMNS.len()];

fn payment_row(
    payment: &crate::domain::PaymentRecord,
    refunded_minor: i64,
    dispute_status: String,
    checkout_ref: String,
) -> Row {
    [
        payment.id.to_string(),
        payment.external_payment_id.clone(),
        checkout_ref,
        payment.amount.amount_minor.to_string(),
        payment.amount.currency.as_str().to_string(),
        payment.status.as_str().to_string(),
        payment
            .fee
            .as_ref()
            .map(|f| f.amount_minor.to_string())
            .unwrap_or_default(),
        payment
            .fee
            .as_ref()
            .map(|f| f.currency.as_str().to_string())
            .unwrap_or_default(),
        payment
            .captured_at
            .map(|dt| dt.to_rfc3339())
            .unwrap_or_default(),
        // The real row timestamp — NOT captured_at, which pending and
        // failed payments never have.
        payment
            .created_at
            .map(|dt| dt.to_rfc3339())
            .unwrap_or_default(),
        refunded_minor.to_string(),
        dispute_status,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csv_columns_are_fixed_and_documented() {
        // This test is the change detector: adding, removing or reordering
        // columns breaks this assertion, which is intentional — the column
        // set is a breaking API contract.
        assert_eq!(
            CSV_COLUMNS,
            &[
                "payment_id",
                "external_payment_id",
                "checkout_reference",
                "amount_minor",
                "currency",
                "status",
                "fee_minor",
                "fee_currency",
                "captured_at",
                "created_at",
                "refunded_minor",
                "dispute_status"
            ]
        );
    }

    #[test]
    fn created_at_is_the_real_column_even_without_a_capture() {
        let mut payment = crate::domain::PaymentRecord::new(
            Uuid::now_v7(),
            Uuid::now_v7(),
            None,
            "pi_test_1".to_string(),
            crate::domain::Money::new(1000, "JPY").unwrap(),
        )
        .unwrap();
        assert!(payment.captured_at.is_none());
        let created = Utc::now();
        payment.created_at = Some(created);
        let row = payment_row(&payment, 0, String::new(), String::new());
        assert_eq!(row[9], created.to_rfc3339());
        assert_eq!(row[8], "");
    }
}
