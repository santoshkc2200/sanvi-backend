//! The notifications context's `PersonalDataProvider`.
//!
//! This context is a stateless gateway: it owns no tables. The personal data
//! it touches — a recipient's email address, and the subject line and body
//! rendered for them — is held by the standalone notification service, in the
//! `notification` schema that service owns and Sanvi migrations must not
//! touch.
//!
//! Two properties of that service's v1 API decide what this provider can
//! honestly report:
//!
//! * Its read routes return **delivery metadata only** — status, attempt
//!   counts, timestamps — and never message content. There is also no route
//!   that looks a notification up by recipient: `GET /v1/notifications` pages
//!   a tenant scope, and `GET /v1/notifications/{id}` needs an id Sanvi only
//!   holds for Lane B sends. So a subject's mail cannot be enumerated.
//! * It exposes **no erasure endpoint**. Message payloads leave on their own
//!   under `RETENTION_PERIOD` (30 days by default) and nothing can pull them
//!   sooner.
//!
//! So this provider declares the data class and reports the subject's mail as
//! **retained**, naming the missing capability, rather than returning a silent
//! success — which the port's contract calls a lie. Closing this properly
//! needs a tenant-scoped erasure endpoint on the notification service and an
//! adapter over it; its own integration guide says not to claim synchronous
//! erasure from the first API.

use async_trait::async_trait;

use sanvi_privacy::domain::{
    DataBundle, DataClass, DataClassDef, EraseMode, EraseReport, LegalBasis, PersonalDataProvider,
    PrivacyError, RectifyPatch, RetainedItem, RetentionAction, RetentionReason, SensitivityClass,
    SubjectRef,
};

const CONTEXT: &str = "notifications";

/// The notification service's own default `RETENTION_PERIOD` (720h), which is
/// what actually removes a delivered message's content.
const RETENTION_DAYS: i64 = 30;

/// Why an erasure request cannot delete a subject's delivered mail today.
/// Kept as one constant so the report and its test cannot drift apart.
const RETENTION_DETAIL: &str = "delivered notifications retained: the notification service exposes \
     no erasure endpoint and no lookup by recipient, so this gateway can \
     neither enumerate nor delete a subject's mail; message payloads age out \
     under the service's own retention period";

#[derive(Debug, Clone, Copy, Default)]
pub struct NotificationsPrivacyProvider;

impl NotificationsPrivacyProvider {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl PersonalDataProvider for NotificationsPrivacyProvider {
    fn context_name(&self) -> &'static str {
        CONTEXT
    }

    fn data_classes(&self) -> Vec<DataClassDef> {
        vec![DataClassDef {
            class: DataClass::Communication,
            category: "Communications",
            sensitivity: SensitivityClass::Standard,
            basis: LegalBasis::Contract,
            period_days: RETENTION_DAYS,
            action: RetentionAction::Delete,
        }]
    }

    /// Empty, and honestly so. The service's read API is redacted by design
    /// and cannot be queried by recipient, so there is nothing this context
    /// can hand back. What a subject would expect to see here — that they
    /// were invited, billed, or warned — is disclosed by the context that
    /// raised the event.
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
                data_class: DataClass::Communication,
                reason: RetentionReason::Operational,
                detail: RETENTION_DETAIL.to_string(),
            }],
        })
    }

    /// Nothing to correct: the gateway stores no subject-owned field. A
    /// corrected address changes where the *next* notification goes, which is
    /// decided by whichever context resolves the recipient.
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
    fn declares_the_communication_data_class_the_data_map_discloses() {
        let classes = NotificationsPrivacyProvider::new().data_classes();
        assert_eq!(classes.len(), 1);
        assert_eq!(classes[0].class, DataClass::Communication);
        assert_eq!(classes[0].period_days, RETENTION_DAYS);
    }

    #[tokio::test]
    async fn erase_reports_the_mail_as_retained_rather_than_claiming_success() {
        let report = NotificationsPrivacyProvider::new()
            .erase(&subject(), EraseMode::Full)
            .await
            .expect("erase reports");

        assert_eq!(report.context, "notifications");
        assert_eq!(report.erased, 0);
        assert_eq!(report.anonymised, 0);
        // A silent success here would tell a data subject their mail was
        // deleted when the service was never asked and cannot comply.
        assert_eq!(report.retained.len(), 1);
        assert_eq!(report.retained[0].data_class, DataClass::Communication);
        assert!(report.retained[0].detail.contains("no erasure endpoint"));
    }

    #[tokio::test]
    async fn collect_returns_an_empty_bundle_for_this_context() {
        let bundle = NotificationsPrivacyProvider::new()
            .collect(&subject())
            .await
            .expect("collect succeeds");
        assert_eq!(bundle.context, "notifications");
        assert!(bundle.entries.is_empty());
    }
}
