//! The product-access context's `PersonalDataProvider`. The proxy stores
//! nothing: it forwards requests and signs short-lived delegations. Personal
//! data a product service records (for example ecommerce-service's
//! `created_by` user ids) lives in that service and is not reached through
//! this provider yet; see docs/superpowers/specs/2026-10-27-ecommerce-proxy-design.md.

use async_trait::async_trait;

use sanvi_privacy::domain::{
    DataBundle, DataClassDef, EraseMode, EraseReport, PersonalDataProvider, PrivacyError,
    RectifyPatch, SubjectRef,
};

#[derive(Debug, Clone, Copy, Default)]
pub struct ProductAccessPrivacyProvider;

impl ProductAccessPrivacyProvider {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl PersonalDataProvider for ProductAccessPrivacyProvider {
    fn context_name(&self) -> &'static str {
        "product_access"
    }

    fn data_classes(&self) -> Vec<DataClassDef> {
        Vec::new()
    }

    async fn collect(&self, _subject: &SubjectRef) -> Result<DataBundle, PrivacyError> {
        Ok(DataBundle {
            context: "product_access".to_string(),
            entries: Vec::new(),
        })
    }

    async fn erase(
        &self,
        _subject: &SubjectRef,
        _mode: EraseMode,
    ) -> Result<EraseReport, PrivacyError> {
        Ok(EraseReport {
            context: "product_access".to_string(),
            erased: 0,
            anonymised: 0,
            retained: Vec::new(),
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
