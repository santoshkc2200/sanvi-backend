//! The system context's `PersonalDataProvider`: the context holds no
//! personal data (build info, health probes, liveness/readiness only). The
//! provider exists because the CI registry test requires **every** context
//! to register one — an absent provider is how a data-bearing context gets
//! caught, and system is the honest empty case.

use async_trait::async_trait;

use sanvi_privacy::domain::{
    DataBundle, DataClassDef, EraseMode, EraseReport, PersonalDataProvider, PrivacyError,
    RectifyPatch, SubjectRef,
};

#[derive(Debug, Clone, Copy, Default)]
pub struct SystemPrivacyProvider;

impl SystemPrivacyProvider {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl PersonalDataProvider for SystemPrivacyProvider {
    fn context_name(&self) -> &'static str {
        "system"
    }

    fn data_classes(&self) -> Vec<DataClassDef> {
        Vec::new()
    }

    async fn collect(&self, _subject: &SubjectRef) -> Result<DataBundle, PrivacyError> {
        Ok(DataBundle {
            context: "system".to_string(),
            entries: Vec::new(),
        })
    }

    async fn erase(
        &self,
        _subject: &SubjectRef,
        _mode: EraseMode,
    ) -> Result<EraseReport, PrivacyError> {
        Ok(EraseReport {
            context: "system".to_string(),
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
