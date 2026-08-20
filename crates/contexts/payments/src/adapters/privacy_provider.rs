//! Privacy inventory adapter for the payments context.
//!
//! Phase 09.1 stores only tenant-level provider connection metadata and
//! deliberately stores no payer, customer, or onboarding-person PII. The
//! provider still participates in every DSR saga so future payments tables
//! cannot silently escape the privacy inventory.

use async_trait::async_trait;

use sanvi_privacy::domain::{
    DATA_CLASSES, DataBundle, DataClass, DataClassDef, EraseMode, EraseReport,
    PersonalDataProvider, PrivacyError, RectifyPatch, SubjectRef,
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
            .filter(|def| def.class == DataClass::Billing)
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
        Ok(EraseReport {
            context: "payments".to_string(),
            ..EraseReport::default()
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
