//! The theming context's `PersonalDataProvider`.
//!
//! The personal data this context holds is limited to the actor identifiers
//! recorded on tenant theme edits and publish snapshots (`updated_by`,
//! `published_by`). Erasure anonymises those identifiers; the theme
//! configuration itself is tenant data, not personal data, and survives —
//! which is also the honest explanation the report gives.

use async_trait::async_trait;

use sanvi_persistence::TxManager;
use sanvi_privacy::domain::{
    DATA_CLASSES, DataBundle, DataClass, DataClassDef, DataEntry, EraseMode, EraseReport,
    PersonalDataProvider, PrivacyError, RectifyPatch, RetainedItem, RetentionReason, SubjectRef,
};

pub struct ThemingPrivacyProvider {
    tx_manager: TxManager,
}

impl ThemingPrivacyProvider {
    pub fn new(tx_manager: TxManager) -> Self {
        Self { tx_manager }
    }
}

#[async_trait]
impl PersonalDataProvider for ThemingPrivacyProvider {
    fn context_name(&self) -> &'static str {
        "theming"
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
                context: "theming".to_string(),
                entries: Vec::new(),
            });
        };

        let mut scope = self
            .tx_manager
            .begin_unscoped("theming_privacy_provider", "theming")
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;
        let rows = sqlx::query!(
            "SELECT state, updated_at FROM theming.tenant_themes WHERE updated_by = $1",
            user_id.as_i64()
        )
        .fetch_all(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(|err| PrivacyError::Storage(err.to_string()))?;
        let revision_rows = sqlx::query!(
            "SELECT revision, published_at FROM theming.tenant_theme_revisions
             WHERE published_by = $1",
            user_id.as_i64()
        )
        .fetch_all(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(|err| PrivacyError::Storage(err.to_string()))?;
        scope
            .commit()
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;

        let mut entries: Vec<DataEntry> = rows
            .into_iter()
            .map(|row| DataEntry {
                data_class: DataClass::AuditLog,
                category: "Configuration".to_string(),
                label: format!("theme {} edited", row.state),
                value: serde_json::json!({ "updated_at": row.updated_at }),
            })
            .collect();
        entries.extend(revision_rows.into_iter().map(|row| DataEntry {
            data_class: DataClass::AuditLog,
            category: "Configuration".to_string(),
            label: format!("theme publish revision {}", row.revision),
            value: serde_json::json!({ "published_at": row.published_at }),
        }));
        Ok(DataBundle {
            context: "theming".to_string(),
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
                context: "theming".to_string(),
                erased: 0,
                anonymised: 0,
                retained: Vec::new(),
            });
        };

        let mut scope = self
            .tx_manager
            .begin_unscoped("theming_privacy_provider", "theming")
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;
        let themes = sqlx::query!(
            "UPDATE theming.tenant_themes SET updated_by = NULL WHERE updated_by = $1",
            user_id.as_i64()
        )
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(|err| PrivacyError::Storage(err.to_string()))?;
        let revisions = sqlx::query!(
            "UPDATE theming.tenant_theme_revisions SET published_by = NULL
             WHERE published_by = $1",
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
            context: "theming".to_string(),
            erased: 0,
            anonymised: (themes.rows_affected() + revisions.rows_affected()) as usize,
            retained: vec![RetainedItem {
                data_class: DataClass::AuditLog,
                reason: RetentionReason::StatutoryRetention,
                detail: "theme configuration is tenant data, not personal data; the actor identifiers are anonymised".to_string(),
            }],
        })
    }

    async fn rectify(
        &self,
        _subject: &SubjectRef,
        _patch: &RectifyPatch,
    ) -> Result<(), PrivacyError> {
        // No rectifiable personal data beyond the anonymised actor ids.
        Ok(())
    }
}
