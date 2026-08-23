//! Privacy inventory adapter for the payments context.
//!
//! Phase 09 stores tenant-level provider connection metadata and payment
//! transaction records, deliberately no payer PII beyond what the tenant
//! chose to put in `customer_ref` (free-form order context). 09.7
//! registers the statutory override explicitly: transaction records
//! survive an erasure request (financial records outlive the person for
//! accounting/tax law), so `erase` reports them as retained rather than
//! silently succeeding — a provider that reports nothing while keeping
//! rows is lying to the privacy centre.

use async_trait::async_trait;

use sanvi_privacy::domain::{
    DATA_CLASSES, DataBundle, DataClass, DataClassDef, EraseMode, EraseReport,
    PersonalDataProvider, PrivacyError, RectifyPatch, RetainedItem, RetentionReason, SubjectRef,
};

#[derive(Debug, Clone, Copy, Default)]
pub struct PaymentsPrivacyProvider;

impl PaymentsPrivacyProvider {
    pub const fn new() -> Self {
        Self
    }
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

    async fn collect(&self, _subject: &SubjectRef) -> Result<DataBundle, PrivacyError> {
        Ok(DataBundle {
            context: "payments".to_string(),
            entries: Vec::new(),
        })
    }

    async fn erase(
        &self,
        _subject: &SubjectRef,
        _mode: EraseMode,
    ) -> Result<EraseReport, PrivacyError> {
        // Transaction records are statutorily retained (the data map's
        // phase-09 override): they are never deleted by an erasure. The
        // report says so, in terms the privacy centre renders verbatim.
        Ok(EraseReport {
            context: "payments".to_string(),
            erased: 0,
            anonymised: 0,
            retained: vec![RetainedItem {
                data_class: DataClass::TransactionRecord,
                reason: RetentionReason::StatutoryRetention,
                detail: "payment transaction records retained: financial records outlive an \
                         erasure request; identifiers are already provider-scoped tokens with \
                         no card data"
                    .to_string(),
            }],
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

    #[test]
    fn the_statutory_override_is_registered_in_the_data_map() {
        let classes = PaymentsPrivacyProvider.data_classes();
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
    async fn erase_reports_retained_transactions_instead_of_silently_succeeding() {
        let subject = SubjectRef::end_user(uuid::Uuid::now_v7(), "cust-1");
        let report = PaymentsPrivacyProvider
            .erase(&subject, EraseMode::Full)
            .await
            .unwrap();
        assert_eq!(report.context, "payments");
        assert_eq!(report.erased, 0);
        assert!(report.retained.iter().any(|item| {
            item.data_class == DataClass::TransactionRecord
                && item.reason == RetentionReason::StatutoryRetention
        }));
    }
}
