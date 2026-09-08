//! The media context's `PersonalDataProvider`.
//!
//! This context is a stateless gateway: it owns no tables, and every asset
//! it mints lives in `hitox-media-service` keyed by `(namespace, assetID)`.
//! That service exposes no route that enumerates a namespace's or an
//! owner's assets — every `/v1/assets` route addresses one `assetID` — so
//! this provider cannot list what a subject holds, and therefore cannot
//! erase it either.
//!
//! Rather than report a silent success, which the port's contract calls a
//! lie, it declares the media data class and returns the assets as
//! **retained**, naming the missing capability. Closing this properly needs
//! a list-by-owner route on `hitox-media-service` and a `delete_asset` fan
//! out over it; TASK-032 explicitly ruled out changing that service.

use async_trait::async_trait;

use sanvi_privacy::domain::{
    DataBundle, DataClass, DataClassDef, EraseMode, EraseReport, LegalBasis, PersonalDataProvider,
    PrivacyError, RectifyPatch, RetainedItem, RetentionAction, RetentionReason, SensitivityClass,
    SubjectRef,
};

const CONTEXT: &str = "media";

/// Why an erasure request cannot delete a subject's uploads today. Kept as
/// one constant so the report and its test cannot drift apart.
const RETENTION_DETAIL: &str = "media assets retained: hitox-media-service addresses assets only by \
     id and exposes no list-by-owner route, so this gateway cannot enumerate \
     a subject's uploads; erasure needs the asset ids from the context that \
     stored them";

#[derive(Debug, Clone, Copy, Default)]
pub struct MediaPrivacyProvider;

impl MediaPrivacyProvider {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl PersonalDataProvider for MediaPrivacyProvider {
    fn context_name(&self) -> &'static str {
        CONTEXT
    }

    fn data_classes(&self) -> Vec<DataClassDef> {
        vec![DataClassDef {
            class: DataClass::MediaAsset,
            category: "Media",
            sensitivity: SensitivityClass::Standard,
            basis: LegalBasis::Contract,
            period_days: 1095,
            action: RetentionAction::Delete,
        }]
    }

    /// Empty, and honestly so: with no way to enumerate the subject's
    /// assets there is nothing this context can hand back. The entries a
    /// subject would expect here are disclosed by whichever context stored
    /// the asset id that references them.
    async fn collect(&self, _subject: &SubjectRef) -> Result<DataBundle, PrivacyError> {
        Ok(DataBundle {
            context: CONTEXT.to_string(),
            entries: Vec::new(),
        })
    }

    async fn erase(
        &self,
        _subject: &SubjectRef,
        _mode: EraseMode,
    ) -> Result<EraseReport, PrivacyError> {
        Ok(EraseReport {
            context: CONTEXT.to_string(),
            erased: 0,
            anonymised: 0,
            retained: vec![RetainedItem {
                data_class: DataClass::MediaAsset,
                reason: RetentionReason::Operational,
                detail: RETENTION_DETAIL.to_string(),
            }],
        })
    }

    /// Nothing to correct: the gateway stores no subject-owned field, it
    /// only forwards the owner reference its caller supplies.
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

    fn subject() -> SubjectRef {
        SubjectRef::user(sanvi_kernel::UserId::try_from(1_i64).unwrap())
    }

    #[test]
    fn declares_the_media_data_class_the_data_map_discloses() {
        let classes = MediaPrivacyProvider::new().data_classes();
        assert_eq!(classes.len(), 1);
        assert_eq!(classes[0].class, DataClass::MediaAsset);
    }

    #[tokio::test]
    async fn erase_reports_the_assets_as_retained_rather_than_claiming_success() {
        let report = MediaPrivacyProvider::new()
            .erase(&subject(), EraseMode::Full)
            .await
            .expect("erase reports");

        assert_eq!(report.context, "media");
        assert_eq!(report.erased, 0);
        assert_eq!(report.anonymised, 0);
        // A silent success here would tell a data subject their uploads
        // were deleted when nothing was touched.
        assert_eq!(report.retained.len(), 1);
        assert_eq!(report.retained[0].data_class, DataClass::MediaAsset);
        assert!(report.retained[0].detail.contains("list-by-owner"));
    }

    #[tokio::test]
    async fn collect_returns_an_empty_bundle_for_this_context() {
        let bundle = MediaPrivacyProvider::new()
            .collect(&subject())
            .await
            .expect("collect succeeds");
        assert_eq!(bundle.context, "media");
        assert!(bundle.entries.is_empty());
    }
}
