//! Privacy inventory adapter for payments.
//!
//! Financial transaction fields remain under the statutory retention rule,
//! while the tenant-provided `customer_ref` is personal context and is
//! removed for the matching end-user subject during erasure.

use async_trait::async_trait;
use serde_json::Value;
use uuid::Uuid;

use sanvi_persistence::{PlatformScope, TxManager};
use sanvi_privacy::domain::{
    DATA_CLASSES, DataBundle, DataClass, DataClassDef, DataEntry, EraseMode, EraseReport,
    PersonalDataProvider, PrivacyError, RectifyPatch, RetainedItem, RetentionReason, SubjectRef,
};

pub struct PaymentsPrivacyProvider {
    tx_manager: TxManager,
}

impl PaymentsPrivacyProvider {
    pub fn new(tx_manager: TxManager) -> Self {
        Self { tx_manager }
    }

    async fn matching_checkouts(
        &self,
        scope: &mut PlatformScope<'_>,
        tenant_id: Uuid,
        external_id: &str,
    ) -> Result<Vec<CheckoutPrivacyRow>, PrivacyError> {
        sqlx::query_as(
            "SELECT c.id, c.reference, c.customer_ref, c.status, c.amount_minor, c.currency,
                    EXISTS (SELECT 1 FROM payments.payments p WHERE p.checkout_id = c.id)
                      AS has_transaction
             FROM payments.checkouts c
             WHERE c.tenant_id = $1
               AND c.customer_ref IS NOT NULL
               AND EXISTS (
                   WITH RECURSIVE json_values(value) AS (
                       SELECT c.customer_ref
                       UNION ALL
                       SELECT child.value
                       FROM json_values parent
                       CROSS JOIN LATERAL (
                           SELECT value FROM jsonb_array_elements(
                               CASE jsonb_typeof(parent.value)
                                   WHEN 'array' THEN parent.value ELSE '[]'::jsonb END
                           )
                           UNION ALL
                           SELECT value FROM jsonb_each(
                               CASE jsonb_typeof(parent.value)
                                   WHEN 'object' THEN parent.value ELSE '{}'::jsonb END
                           )
                       ) child
                   )
                   SELECT 1 FROM json_values
                   WHERE jsonb_typeof(value) = 'string'
                     AND value = to_jsonb($2::text)
               )
             ORDER BY created_at, id",
        )
        .bind(tenant_id)
        .bind(external_id)
        .fetch_all(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)
    }
}

#[derive(sqlx::FromRow)]
struct CheckoutPrivacyRow {
    id: Uuid,
    reference: String,
    customer_ref: Option<Value>,
    status: String,
    amount_minor: i64,
    currency: String,
    has_transaction: bool,
}

fn end_user(subject: &SubjectRef) -> Option<(Uuid, &str)> {
    let tenant_id = subject.tenant_id?;
    let external_id = subject
        .identifiers
        .iter()
        .find(|identifier| identifier.id_type == "external_id")?
        .value
        .as_str();
    Some((tenant_id, external_id))
}

fn storage(err: sqlx::Error) -> PrivacyError {
    PrivacyError::Storage(err.to_string())
}

#[async_trait]
impl PersonalDataProvider for PaymentsPrivacyProvider {
    fn context_name(&self) -> &'static str {
        "payments"
    }

    fn data_classes(&self) -> Vec<DataClassDef> {
        DATA_CLASSES
            .iter()
            .filter(|def| {
                def.class == DataClass::Billing || def.class == DataClass::TransactionRecord
            })
            .copied()
            .collect()
    }

    async fn collect(&self, subject: &SubjectRef) -> Result<DataBundle, PrivacyError> {
        let Some((tenant_id, external_id)) = end_user(subject) else {
            return Ok(DataBundle {
                context: "payments".to_string(),
                entries: Vec::new(),
            });
        };
        let mut scope = self
            .tx_manager
            .begin_unscoped("payments_privacy_collect", "privacy")
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;
        let rows = self
            .matching_checkouts(&mut scope, tenant_id, external_id)
            .await?;
        scope
            .commit()
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;

        Ok(DataBundle {
            context: "payments".to_string(),
            entries: rows
                .into_iter()
                .map(|row| DataEntry {
                    data_class: if row.has_transaction {
                        DataClass::TransactionRecord
                    } else {
                        DataClass::Billing
                    },
                    category: "Financial".to_string(),
                    label: "payment_checkout".to_string(),
                    value: serde_json::json!({
                        "checkout_id": row.id,
                        "reference": row.reference,
                        "customer_ref": row.customer_ref,
                        "status": row.status,
                        "amount_minor": row.amount_minor,
                        "currency": row.currency,
                    }),
                })
                .collect(),
        })
    }

    async fn erase(
        &self,
        subject: &SubjectRef,
        _mode: EraseMode,
    ) -> Result<EraseReport, PrivacyError> {
        let Some((tenant_id, external_id)) = end_user(subject) else {
            return Ok(EraseReport {
                context: "payments".to_string(),
                ..EraseReport::default()
            });
        };
        let mut scope = self
            .tx_manager
            .begin_unscoped("payments_privacy_erase", "privacy")
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;
        let matching = self
            .matching_checkouts(&mut scope, tenant_id, external_id)
            .await?;
        if matching.is_empty() {
            scope
                .commit()
                .await
                .map_err(|err| PrivacyError::Storage(err.to_string()))?;
            return Ok(EraseReport {
                context: "payments".to_string(),
                ..EraseReport::default()
            });
        }

        let payment_count: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM payments.payments p
             JOIN payments.checkouts c ON c.id = p.checkout_id
             WHERE c.tenant_id = $1
               AND c.customer_ref IS NOT NULL
               AND EXISTS (
                   WITH RECURSIVE json_values(value) AS (
                       SELECT c.customer_ref
                       UNION ALL
                       SELECT child.value
                       FROM json_values parent
                       CROSS JOIN LATERAL (
                           SELECT value FROM jsonb_array_elements(
                               CASE jsonb_typeof(parent.value)
                                   WHEN 'array' THEN parent.value ELSE '[]'::jsonb END
                           )
                           UNION ALL
                           SELECT value FROM jsonb_each(
                               CASE jsonb_typeof(parent.value)
                                   WHEN 'object' THEN parent.value ELSE '{}'::jsonb END
                           )
                       ) child
                   )
                   SELECT 1 FROM json_values
                   WHERE jsonb_typeof(value) = 'string'
                     AND value = to_jsonb($2::text)
               )",
        )
        .bind(tenant_id)
        .bind(external_id)
        .fetch_one(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?;
        let anonymised = sqlx::query(
            "UPDATE payments.checkouts
             SET customer_ref = NULL, updated_at = now()
             WHERE tenant_id = $1
               AND customer_ref IS NOT NULL
               AND EXISTS (
                   WITH RECURSIVE json_values(value) AS (
                       SELECT customer_ref
                       UNION ALL
                       SELECT child.value
                       FROM json_values parent
                       CROSS JOIN LATERAL (
                           SELECT value FROM jsonb_array_elements(
                               CASE jsonb_typeof(parent.value)
                                   WHEN 'array' THEN parent.value ELSE '[]'::jsonb END
                           )
                           UNION ALL
                           SELECT value FROM jsonb_each(
                               CASE jsonb_typeof(parent.value)
                                   WHEN 'object' THEN parent.value ELSE '{}'::jsonb END
                           )
                       ) child
                   )
                   SELECT 1 FROM json_values
                   WHERE jsonb_typeof(value) = 'string'
                     AND value = to_jsonb($2::text)
               )",
        )
        .bind(tenant_id)
        .bind(external_id)
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?
        .rows_affected() as usize;

        scope
            .commit()
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;
        Ok(EraseReport {
            context: "payments".to_string(),
            erased: 0,
            anonymised,
            retained: (payment_count > 0)
                .then_some(RetainedItem {
                    data_class: DataClass::TransactionRecord,
                    reason: RetentionReason::StatutoryRetention,
                    detail: "payment transaction records retained: financial records outlive an erasure request; customer reference removed".to_string(),
                })
                .into_iter()
                .collect(),
        })
    }

    async fn rectify(
        &self,
        _subject: &SubjectRef,
        _patch: &RectifyPatch,
    ) -> Result<(), PrivacyError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_statutory_override_is_registered_in_the_data_map() {
        let provider = PaymentsPrivacyProvider::new(TxManager::new(
            sqlx::PgPool::connect_lazy("postgres://sanvi_app:app@127.0.0.1:1/sanvi_test")
                .expect("lazy pool construction is configuration only"),
        ));
        let classes = provider.data_classes();
        assert!(classes.iter().any(|def| def.class == DataClass::Billing));
        let transactions = classes
            .iter()
            .find(|def| def.class == DataClass::TransactionRecord)
            .expect("transaction records must be registered");
        assert_eq!(
            transactions.basis,
            sanvi_privacy::domain::LegalBasis::StatutoryRetention
        );
        assert_eq!(
            transactions.action,
            sanvi_privacy::domain::RetentionAction::Archive
        );
    }

    #[tokio::test]
    async fn non_end_user_subjects_report_no_payment_data() {
        let provider = PaymentsPrivacyProvider::new(TxManager::new(
            sqlx::PgPool::connect_lazy("postgres://sanvi_app:app@127.0.0.1:1/sanvi_test")
                .expect("lazy pool construction is configuration only"),
        ));
        let subject = SubjectRef::contact("payer@example.test");
        let report = provider.erase(&subject, EraseMode::Full).await.unwrap();
        assert!(report.retained.is_empty());
        assert_eq!(report.anonymised, 0);
    }
}
