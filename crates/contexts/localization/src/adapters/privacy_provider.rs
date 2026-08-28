//! The localization context's `PersonalDataProvider`.
//!
//! The only personal data the context holds is `localization.translations.
//! updated_by` (the actor id recorded on copy overrides). On erasure the
//! identifier is anonymised (set NULL); the translation itself is tenant
//! copy, not personal data, and survives — which is also the honest
//! explanation the report gives.

use async_trait::async_trait;

use sanvi_persistence::TxManager;
use sanvi_privacy::domain::{
    DATA_CLASSES, DataBundle, DataClass, DataClassDef, DataEntry, EraseMode, EraseReport,
    PersonalDataProvider, PrivacyError, RectifyPatch, RetainedItem, RetentionReason, SubjectRef,
};

use crate::adapters::repos::SqlxTranslationRepository;

pub struct LocalizationPrivacyProvider {
    tx_manager: TxManager,
}

impl LocalizationPrivacyProvider {
    pub fn new(tx_manager: TxManager) -> Self {
        Self { tx_manager }
    }
}

#[async_trait]
impl PersonalDataProvider for LocalizationPrivacyProvider {
    fn context_name(&self) -> &'static str {
        "localization"
    }

    fn data_classes(&self) -> Vec<DataClassDef> {
        DATA_CLASSES
            .iter()
            .filter(|def| matches!(def.class, DataClass::AuditLog))
            .copied()
            .collect()
    }

    async fn collect(&self, subject: &SubjectRef) -> Result<DataBundle, PrivacyError> {
        let user_id = subject.identifiers.iter().find_map(|id| {
            (id.id_type == "user_id")
                .then(|| id.value.parse::<sanvi_kernel::UserId>().ok())
                .flatten()
        });
        let Some(user_id) = user_id else {
            return Ok(DataBundle {
                context: "localization".to_string(),
                entries: Vec::new(),
            });
        };

        let mut scope = self
            .tx_manager
            .begin_unscoped("localization_privacy_provider", "localization")
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;
        let rows = sqlx::query!(
            "SELECT scope, key, locale, value, updated_at
             FROM localization.translations
             WHERE updated_by = $1",
            user_id.as_i64()
        )
        .fetch_all(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(|err| PrivacyError::Storage(err.to_string()))?;
        scope
            .commit()
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;

        let entries = rows
            .into_iter()
            .map(|row| DataEntry {
                data_class: DataClass::AuditLog,
                category: "Configuration".to_string(),
                label: format!("copy override {} ({})", row.key, row.locale),
                value: serde_json::json!({
                    "scope": row.scope,
                    "updated_at": row.updated_at,
                }),
            })
            .collect();
        Ok(DataBundle {
            context: "localization".to_string(),
            entries,
        })
    }

    async fn erase(
        &self,
        subject: &SubjectRef,
        _mode: EraseMode,
    ) -> Result<EraseReport, PrivacyError> {
        let user_id = subject.identifiers.iter().find_map(|id| {
            (id.id_type == "user_id")
                .then(|| id.value.parse::<sanvi_kernel::UserId>().ok())
                .flatten()
        });
        let Some(user_id) = user_id else {
            return Ok(EraseReport {
                context: "localization".to_string(),
                erased: 0,
                anonymised: 0,
                retained: Vec::new(),
            });
        };

        let mut scope = self
            .tx_manager
            .begin_unscoped("localization_privacy_provider", "localization")
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;
        let result = sqlx::query!(
            "UPDATE localization.translations SET updated_by = NULL WHERE updated_by = $1",
            user_id.as_i64()
        )
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(|err| PrivacyError::Storage(err.to_string()))?;
        scope
            .commit()
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;

        Ok(EraseReport {
            context: "localization".to_string(),
            erased: 0,
            anonymised: result.rows_affected() as usize,
            retained: vec![RetainedItem {
                data_class: DataClass::AuditLog,
                reason: RetentionReason::StatutoryRetention,
                detail: "translation content is tenant copy, not personal data; the actor identifier is anonymised".to_string(),
            }],
        })
    }

    async fn rectify(
        &self,
        _subject: &SubjectRef,
        _patch: &RectifyPatch,
    ) -> Result<(), PrivacyError> {
        // No rectifiable personal data beyond the anonymised actor id.
        let _ = SqlxTranslationRepository::new();
        Ok(())
    }
}
