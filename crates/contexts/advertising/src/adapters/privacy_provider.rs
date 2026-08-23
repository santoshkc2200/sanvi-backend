//! Phase-05 registry participation for the advertising context.
//!
//! TASK-009 has no event intake or identifier storage yet, so the provider is
//! intentionally empty. TASK-018 replaces this with export/erasure of hashed
//! conversion identifiers and platform-suppression work; registering now
//! keeps a new context from silently falling outside the DSR fan-out.

use async_trait::async_trait;
use sanvi_privacy::domain::{
    DataBundle, DataClassDef, EraseMode, EraseReport, PersonalDataProvider, PrivacyError,
    RectifyPatch, SubjectRef,
};

pub struct AdvertisingPrivacyProvider;

impl AdvertisingPrivacyProvider {
    pub fn new() -> Self {
        Self
    }
}

impl Default for AdvertisingPrivacyProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl PersonalDataProvider for AdvertisingPrivacyProvider {
    fn context_name(&self) -> &'static str {
        "advertising"
    }

    fn data_classes(&self) -> Vec<DataClassDef> {
        // No rows exist until directive-gated conversion capture (TASK-014).
        Vec::new()
    }

    async fn collect(&self, _subject: &SubjectRef) -> Result<DataBundle, PrivacyError> {
        Ok(DataBundle {
            context: self.context_name().to_string(),
            entries: Vec::new(),
        })
    }

    async fn erase(
        &self,
        _subject: &SubjectRef,
        _mode: EraseMode,
    ) -> Result<EraseReport, PrivacyError> {
        Ok(EraseReport {
            context: self.context_name().to_string(),
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
