//! Phase-05 registry participation for the advertising context.
//!
//! TASK-018: export/erasure of hashed conversion identifiers and platform-suppression work.

use async_trait::async_trait;
use sanvi_kernel::TenantId;
use sanvi_persistence::TxManager;
use sanvi_privacy::domain::{
    DATA_CLASSES, DataBundle, DataClass, DataClassDef, DataEntry, EraseMode, EraseReport,
    PersonalDataProvider, PrivacyError, RectifyPatch, RetainedItem, RetentionReason, SubjectRef,
};

fn end_user(subject: &SubjectRef) -> Option<(TenantId, &str)> {
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

pub struct AdvertisingPrivacyProvider {
    tx_manager: TxManager,
}

impl AdvertisingPrivacyProvider {
    pub fn new(tx_manager: TxManager) -> Self {
        Self { tx_manager }
    }
}

#[async_trait]
impl PersonalDataProvider for AdvertisingPrivacyProvider {
    fn context_name(&self) -> &'static str {
        "advertising"
    }

    fn data_classes(&self) -> Vec<DataClassDef> {
        DATA_CLASSES
            .iter()
            .filter(|def| def.class == DataClass::AdvertisingConversionEvent)
            .copied()
            .collect()
    }

    async fn collect(&self, subject: &SubjectRef) -> Result<DataBundle, PrivacyError> {
        let Some((tenant_id, external_id)) = end_user(subject) else {
            return Ok(DataBundle {
                context: "advertising".to_string(),
                entries: Vec::new(),
            });
        };

        let mut scope = self
            .tx_manager
            .begin_unscoped("advertising_privacy_collect", "privacy")
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;

        // 1. Fetch conversion events
        let events = sqlx::query_as::<
            _,
            (
                i64,
                String,
                serde_json::Value,
                serde_json::Value,
                serde_json::Value,
            ),
        >(
            "SELECT id, event_id, click_ids, hashed_ids, upload_states
             FROM advertising.conversion_events
             WHERE tenant_id = $1
               AND subject_key->>'kind' = 'end_user'
               AND subject_key->>'external_id' = $2",
        )
        .bind(tenant_id.as_i64())
        .bind(external_id)
        .fetch_all(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?;

        let mut hashed_ids: Vec<String> = Vec::new();
        let mut entries = Vec::new();

        for (_id, event_id, click_ids, event_hashed_ids, upload_states) in events {
            entries.push(DataEntry {
                data_class: DataClass::AdvertisingConversionEvent,
                category: "Advertising".to_string(),
                label: "conversion_event".to_string(),
                value: serde_json::json!({
                    "event_id": event_id,
                    "click_ids": click_ids,
                    "hashed_ids": event_hashed_ids,
                    "upload_states": upload_states,
                }),
            });

            if let Some(obj) = event_hashed_ids.as_object() {
                for v in obj.values() {
                    if let Some(s) = v.as_str() {
                        hashed_ids.push(s.to_string());
                    }
                }
            }
        }

        if !hashed_ids.is_empty() {
            let audiences = sqlx::query_as::<_, (i64, String, String)>(
                "SELECT id, platform, name
                 FROM advertising.audiences
                 WHERE tenant_id = $1
                   AND included_identifiers ?| $2",
            )
            .bind(tenant_id.as_i64())
            .bind(&hashed_ids)
            .fetch_all(&mut **scope.tx_mut().as_inner())
            .await
            .map_err(storage)?;

            for (aud_id, platform, name) in audiences {
                entries.push(DataEntry {
                    data_class: DataClass::AdvertisingConversionEvent,
                    category: "Advertising".to_string(),
                    label: "audience_membership".to_string(),
                    value: serde_json::json!({
                        "audience_id": aud_id,
                        "platform": platform,
                        "name": name,
                    }),
                });
            }
        }

        scope
            .commit()
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;

        Ok(DataBundle {
            context: "advertising".to_string(),
            entries,
        })
    }

    async fn erase(
        &self,
        subject: &SubjectRef,
        _mode: EraseMode,
    ) -> Result<EraseReport, PrivacyError> {
        let Some((tenant_id, external_id)) = end_user(subject) else {
            return Ok(EraseReport {
                context: "advertising".to_string(),
                ..EraseReport::default()
            });
        };

        let mut scope = self
            .tx_manager
            .begin_unscoped("advertising_privacy_erase", "privacy")
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;

        let event_count: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM advertising.conversion_events
             WHERE tenant_id = $1
               AND subject_key->>'kind' = 'end_user'
               AND subject_key->>'external_id' = $2",
        )
        .bind(tenant_id.as_i64())
        .bind(external_id)
        .fetch_one(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?;

        if event_count == 0 {
            scope
                .commit()
                .await
                .map_err(|err| PrivacyError::Storage(err.to_string()))?;
            return Ok(EraseReport {
                context: "advertising".to_string(),
                ..EraseReport::default()
            });
        }

        let anonymised = sqlx::query(
            "UPDATE advertising.conversion_events
             SET subject_key = '{\"kind\": \"contact\"}'::jsonb,
                 hashed_ids = '{}'::jsonb,
                 click_ids = '{}'::jsonb
             WHERE tenant_id = $1
               AND subject_key->>'kind' = 'end_user'
               AND subject_key->>'external_id' = $2",
        )
        .bind(tenant_id.as_i64())
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
            context: "advertising".to_string(),
            erased: 0,
            anonymised,
            retained: (event_count > 0)
                .then_some(RetainedItem {
                    data_class: DataClass::AdvertisingConversionEvent,
                    reason: RetentionReason::StatutoryRetention,
                    detail: "conversion events retained (anonymised): proof of consent honoured under phase-05 legitimate interest".to_string(),
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
