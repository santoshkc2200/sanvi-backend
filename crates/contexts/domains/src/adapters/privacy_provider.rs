//! The domains context's `PersonalDataProvider`.
//!
//! The personal data this context holds is the registrant contact on
//! purchase orders (name, email, phone — the registrar is a sub-processor
//! per the phase-05 data map) plus the actor identifiers on orders
//! (`ordered_by`). Erasure anonymises the actor id and masks the contact
//! fields, with the ICANN retention override reported honestly:
//! registrant data cannot be fully deleted while the registration exists.

use async_trait::async_trait;

use sanvi_kernel::UserId;
use sanvi_persistence::TxManager;
use sanvi_privacy::domain::{
    DATA_CLASSES, DataBundle, DataClass, DataClassDef, DataEntry, EraseMode, EraseReport,
    PersonalDataProvider, PrivacyError, RectifyPatch, RetainedItem, RetentionReason, SubjectRef,
};

pub struct DomainsPrivacyProvider {
    tx_manager: TxManager,
}

impl DomainsPrivacyProvider {
    pub fn new(tx_manager: TxManager) -> Self {
        Self { tx_manager }
    }
}

#[async_trait]
impl PersonalDataProvider for DomainsPrivacyProvider {
    fn context_name(&self) -> &'static str {
        "domains"
    }

    fn data_classes(&self) -> Vec<DataClassDef> {
        DATA_CLASSES
            .iter()
            .filter(|def| matches!(def.class, DataClass::Contact | DataClass::AuditLog))
            .copied()
            .collect()
    }

    async fn collect(&self, subject: &SubjectRef) -> Result<DataBundle, PrivacyError> {
        let user_id = subject.identifiers.iter().find_map(|id| {
            (id.id_type == "user_id")
                .then(|| id.value.parse::<UserId>().ok())
                .flatten()
        });
        let email = subject
            .identifiers
            .iter()
            .find_map(|id| (id.id_type == "email").then(|| id.value.clone()));
        if user_id.is_none() && email.is_none() {
            return Ok(DataBundle {
                context: "domains".to_string(),
                entries: Vec::new(),
            });
        }

        let user_id_i64 = user_id.map(|id| id.as_i64());
        let mut scope = self
            .tx_manager
            .begin_unscoped("domains_privacy_provider", "domains")
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;
        let rows = sqlx::query!(
            r#"SELECT id, hostname, registrant_contact, ordered_by, created_at
               FROM domains.orders
               WHERE ordered_by = $1 OR registrant_contact->>'email' = $2"#,
            user_id_i64,
            email,
        )
        .fetch_all(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(|err| PrivacyError::Storage(err.to_string()))?;
        scope
            .commit()
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;

        let mut entries = Vec::new();
        for row in rows {
            if let Some(contact) = row.registrant_contact.as_object() {
                for field in ["name", "email", "phone", "organization"] {
                    if let Some(value) = contact.get(field).and_then(|value| value.as_str()) {
                        entries.push(DataEntry {
                            data_class: DataClass::Contact,
                            category: "Registrant".to_string(),
                            label: format!("domain order {} {field}", row.hostname),
                            value: serde_json::json!(value),
                        });
                    }
                }
            }
            if let Some(ordered_by) = row.ordered_by {
                entries.push(DataEntry {
                    data_class: DataClass::AuditLog,
                    category: "Ordering".to_string(),
                    label: format!("domain order {} placed", row.hostname),
                    value: serde_json::json!({ "ordered_by": ordered_by, "at": row.created_at }),
                });
            }
        }
        Ok(DataBundle {
            context: "domains".to_string(),
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
                .then(|| id.value.parse::<UserId>().ok())
                .flatten()
        });
        let email = subject
            .identifiers
            .iter()
            .find_map(|id| (id.id_type == "email").then(|| id.value.clone()));
        if user_id.is_none() && email.is_none() {
            return Ok(EraseReport {
                context: "domains".to_string(),
                erased: 0,
                anonymised: 0,
                retained: Vec::new(),
            });
        }

        let user_id_i64 = user_id.map(|id| id.as_i64());
        let mut scope = self
            .tx_manager
            .begin_unscoped("domains_privacy_provider", "domains")
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;
        let anonymised = sqlx::query!(
            "UPDATE domains.orders SET ordered_by = NULL WHERE ordered_by = $1",
            user_id_i64
        )
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(|err| PrivacyError::Storage(err.to_string()))?;
        let masked = sqlx::query!(
            r#"UPDATE domains.orders
               SET registrant_contact = jsonb_set(
                     jsonb_set(
                       jsonb_set(registrant_contact, '{email}', '"erased@invalid"'),
                       '{name}', '"erased"'),
                     '{phone}', '"erased"')
               WHERE registrant_contact->>'email' = $1"#,
            email
        )
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(|err| PrivacyError::Storage(err.to_string()))?;
        scope
            .commit()
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;

        Ok(EraseReport {
            context: "domains".to_string(),
            erased: 0,
            anonymised: (anonymised.rows_affected() + masked.rows_affected()) as usize,
            retained: vec![RetainedItem {
                data_class: DataClass::Contact,
                reason: RetentionReason::StatutoryRetention,
                detail: "registrant contact is retained in masked form while the ICANN registration exists; full deletion requires the registration to lapse or transfer".to_string(),
            }],
        })
    }

    async fn rectify(
        &self,
        subject: &SubjectRef,
        patch: &RectifyPatch,
    ) -> Result<(), PrivacyError> {
        let email = subject
            .identifiers
            .iter()
            .find_map(|id| (id.id_type == "email").then(|| id.value.clone()));
        let Some(email) = email else {
            return Ok(());
        };
        let contact_corrections: Vec<_> = patch
            .corrections
            .iter()
            .filter(|correction| correction.context == "domains")
            .filter(|correction| {
                matches!(
                    correction.field.as_str(),
                    "registrant.name" | "registrant.email" | "registrant.phone"
                )
            })
            .collect();
        if contact_corrections.is_empty() {
            return Ok(());
        }

        let mut scope = self
            .tx_manager
            .begin_unscoped("domains_privacy_provider", "domains")
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;
        for correction in contact_corrections {
            // Only the three whitelisted fields reach this point (see the
            // filter above); a statically-written statement per field keeps
            // the JSON path injection-free.
            let statement = match correction.field.as_str() {
                "registrant.name" => {
                    "UPDATE domains.orders \
                    SET registrant_contact = jsonb_set(registrant_contact, '{name}', $1::jsonb) \
                    WHERE registrant_contact->>'email' = $2"
                }
                "registrant.email" => {
                    "UPDATE domains.orders \
                    SET registrant_contact = jsonb_set(registrant_contact, '{email}', $1::jsonb) \
                    WHERE registrant_contact->>'email' = $2"
                }
                _ => {
                    "UPDATE domains.orders \
                    SET registrant_contact = jsonb_set(registrant_contact, '{phone}', $1::jsonb) \
                    WHERE registrant_contact->>'email' = $2"
                }
            };
            sqlx::query(statement)
                .bind(serde_json::json!(correction.value).to_string())
                .bind(&email)
                .execute(&mut **scope.tx_mut().as_inner())
                .await
                .map_err(|err| PrivacyError::Storage(err.to_string()))?;
        }
        scope
            .commit()
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;
        Ok(())
    }
}
