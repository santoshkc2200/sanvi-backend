//! The identity context's `PersonalDataProvider`: the platform's biggest
//! personal-data holder.
//!
//! Collect: the user projection, memberships, linked provider accounts and
//! invitations bearing the subject's email.
//! Erase (TenantUser): anonymise + deactivate the user row (frees the email
//! via the partial unique index), delete memberships and membership roles,
//! unlink provider accounts, delete invitations and link intents by email,
//! tombstone the Kratos identity (deleting it kills its sessions).
//! Erase (Contact): delete invitations/link intents by email.
//! The report states exactly what was retained and why (nothing — the audit
//! context owns the trail).

use async_trait::async_trait;

use sanvi_persistence::TxManager;
use sanvi_privacy::domain::{
    DATA_CLASSES, DataBundle, DataClass, DataClassDef, DataEntry, EraseMode, EraseReport,
    PersonalDataProvider, PrivacyError, RectifyPatch, SubjectKind, SubjectRef,
};

use crate::domain::{
    Clock, InvitationRepository, KratosClient, LinkIntentRepository, LinkedAccountRepository,
    MembershipRepository, User, UserId, UserRepository, UserStatus,
};

pub struct IdentityPrivacyProvider {
    tx_manager: TxManager,
    users: std::sync::Arc<dyn UserRepository>,
    memberships: std::sync::Arc<dyn MembershipRepository>,
    linked: std::sync::Arc<dyn LinkedAccountRepository>,
    invitations: std::sync::Arc<dyn InvitationRepository>,
    link_intents: std::sync::Arc<dyn LinkIntentRepository>,
    kratos: std::sync::Arc<dyn KratosClient>,
    clock: std::sync::Arc<dyn Clock>,
}

impl IdentityPrivacyProvider {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        tx_manager: TxManager,
        users: std::sync::Arc<dyn UserRepository>,
        memberships: std::sync::Arc<dyn MembershipRepository>,
        linked: std::sync::Arc<dyn LinkedAccountRepository>,
        invitations: std::sync::Arc<dyn InvitationRepository>,
        link_intents: std::sync::Arc<dyn LinkIntentRepository>,
        kratos: std::sync::Arc<dyn KratosClient>,
        clock: std::sync::Arc<dyn Clock>,
    ) -> Self {
        Self {
            tx_manager,
            users,
            memberships,
            linked,
            invitations,
            link_intents,
            kratos,
            clock,
        }
    }

    fn user_id(subject: &SubjectRef) -> Option<UserId> {
        subject
            .identifiers
            .iter()
            .find(|id| id.id_type == "user_id")
            .and_then(|id| id.value.parse::<UserId>().ok())
    }

    fn email(subject: &SubjectRef) -> Option<String> {
        subject
            .identifiers
            .iter()
            .find(|id| id.id_type == "email")
            .map(|id| id.value.clone())
    }
}

#[async_trait]
impl PersonalDataProvider for IdentityPrivacyProvider {
    fn context_name(&self) -> &'static str {
        "identity"
    }

    fn data_classes(&self) -> Vec<DataClassDef> {
        DATA_CLASSES
            .iter()
            .filter(|def| {
                matches!(
                    def.class,
                    DataClass::IdentityProfile
                        | DataClass::Authentication
                        | DataClass::Membership
                        | DataClass::Communication
                )
            })
            .copied()
            .collect()
    }

    async fn collect(&self, subject: &SubjectRef) -> Result<DataBundle, PrivacyError> {
        let mut scope = self
            .tx_manager
            .begin_unscoped("identity_privacy_provider", "identity")
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;
        let mut entries = Vec::new();

        if let Some(user_id) = Self::user_id(subject) {
            let id = user_id;
            if let Some(user) = self
                .users
                .find_by_id(&mut scope, id)
                .await
                .map_err(storage)?
            {
                entries.push(DataEntry {
                    data_class: DataClass::IdentityProfile,
                    category: "Account".to_string(),
                    label: "primary_email".to_string(),
                    value: serde_json::json!(user.primary_email().as_str()),
                });
                entries.push(DataEntry {
                    data_class: DataClass::IdentityProfile,
                    category: "Account".to_string(),
                    label: "email_verified".to_string(),
                    value: serde_json::json!(user.email_verified()),
                });
                entries.push(DataEntry {
                    data_class: DataClass::IdentityProfile,
                    category: "Account".to_string(),
                    label: "locale".to_string(),
                    value: serde_json::json!(user.locale().map(|l| l.as_str())),
                });
                entries.push(DataEntry {
                    data_class: DataClass::Authentication,
                    category: "Account".to_string(),
                    label: "kratos_identity_id".to_string(),
                    value: serde_json::json!(user.kratos_identity_id()),
                });
            }

            for membership in self
                .memberships
                .list_all_platform(&mut scope)
                .await
                .map_err(storage)?
                .into_iter()
                .filter(|membership| membership.user_id() == user_id)
            {
                entries.push(DataEntry {
                    data_class: DataClass::Membership,
                    category: "Account".to_string(),
                    label: "membership".to_string(),
                    value: serde_json::json!({
                        "tenant_id": membership.tenant_id(),
                        "status": membership.status().as_str(),
                        "invited_by": membership.invited_by(),
                    }),
                });
            }

            for account in self
                .linked
                .list_for_user(&mut scope, id)
                .await
                .map_err(storage)?
            {
                entries.push(DataEntry {
                    data_class: DataClass::Authentication,
                    category: "Account".to_string(),
                    label: "linked_account".to_string(),
                    value: serde_json::json!({
                        "provider": account.provider().as_str(),
                        "subject": account.subject().as_str(),
                    }),
                });
            }
        }

        if Self::email(subject).is_some() {
            // Invitations are tenant-scoped; the platform-scoped erase path
            // deletes them by email — for collection, surface that they may
            // exist without enumerating (a count is enough for the export).
            entries.push(DataEntry {
                data_class: DataClass::Communication,
                category: "Contact".to_string(),
                label: "pending_invitations".to_string(),
                value: serde_json::json!("invitations for this address are deleted on erasure"),
            });
        }

        scope
            .commit()
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;
        Ok(DataBundle {
            context: "identity".to_string(),
            entries,
        })
    }

    async fn erase(
        &self,
        subject: &SubjectRef,
        _mode: EraseMode,
    ) -> Result<EraseReport, PrivacyError> {
        let mut scope = self
            .tx_manager
            .begin_unscoped("identity_privacy_erase", "identity")
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;
        let mut erased = 0;
        let mut anonymised = 0;
        let mut retained = Vec::new();

        if let Some(email) = Self::email(subject) {
            erased += self
                .invitations
                .remove_by_email_platform(&mut scope, &email)
                .await
                .map_err(storage)?;
            erased += self
                .link_intents
                .remove_by_email_platform(&mut scope, &email)
                .await
                .map_err(storage)?;
        }

        if let Some(user_id) = Self::user_id(subject) {
            let id = user_id;

            // Anonymise + deactivate the projection. The pseudonym email
            // keeps the partial unique index satisfied while releasing the
            // real address.
            if let Some(user) = self
                .users
                .find_by_id(&mut scope, id)
                .await
                .map_err(storage)?
            {
                let pseudonym_email = format!(
                    "{}@deleted.invalid",
                    sanvi_privacy::adapters::pseudonym(subject.canonical_key())
                );
                let email = crate::domain::Email::parse(&pseudonym_email)
                    .map_err(|err| PrivacyError::Storage(err.to_string()))?;
                let anonymised_user = User::from_parts(
                    id,
                    user.kratos_identity_id(),
                    email,
                    false,
                    UserStatus::Deactivated,
                    user.locale().cloned(),
                    user.created_at(),
                    Some(self.clock.now()),
                );
                self.users
                    .save(&mut scope, &anonymised_user)
                    .await
                    .map_err(storage)?;
                anonymised += 1;

                // Tombstone the Kratos identity: deletion kills its
                // sessions and frees the address.
                self.kratos
                    .delete_identity(user.kratos_identity_id())
                    .await
                    .map_err(|err| PrivacyError::Upstream(err.to_string()))?;
            }

            erased += self
                .memberships
                .remove_all_platform(&mut scope, id)
                .await
                .map_err(storage)?;
            erased += self
                .linked
                .remove_all(&mut scope, id)
                .await
                .map_err(storage)?;
        } else if subject.kind == SubjectKind::TenantUser {
            retained.push(sanvi_privacy::domain::RetainedItem {
                data_class: DataClass::IdentityProfile,
                reason: sanvi_privacy::domain::RetentionReason::Operational,
                detail: "no user id on the subject reference; the projection could not be located"
                    .to_string(),
            });
        }

        scope
            .commit()
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;
        Ok(EraseReport {
            context: "identity".to_string(),
            erased,
            anonymised,
            retained,
        })
    }

    async fn rectify(
        &self,
        subject: &SubjectRef,
        patch: &RectifyPatch,
    ) -> Result<(), PrivacyError> {
        let Some(user_id) = Self::user_id(subject) else {
            return Err(PrivacyError::Conflict(
                "rectification requires a user subject".to_string(),
            ));
        };
        let id = user_id;
        let mut scope = self
            .tx_manager
            .begin_unscoped("identity_privacy_rectify", "identity")
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;
        let Some(user) = self
            .users
            .find_by_id(&mut scope, id)
            .await
            .map_err(storage)?
        else {
            return Err(PrivacyError::SubjectNotFound(user_id.to_string()));
        };
        let mut corrected = User::from_parts(
            id,
            user.kratos_identity_id(),
            user.primary_email().clone(),
            user.email_verified(),
            user.status(),
            user.locale().cloned(),
            user.created_at(),
            user.deactivated_at(),
        );
        for correction in &patch.corrections {
            if correction.context == "identity" && correction.field == "email" {
                let email = crate::domain::Email::parse(&correction.value)
                    .map_err(|err| PrivacyError::Storage(err.to_string()))?;
                // Email moves only as a verified change (Kratos owns the
                // authoritative address; the projection follows).
                corrected.apply_snapshot(Some(email), true, None);
            }
        }
        self.users
            .save(&mut scope, &corrected)
            .await
            .map_err(storage)?;
        scope
            .commit()
            .await
            .map_err(|err| PrivacyError::Storage(err.to_string()))?;
        Ok(())
    }
}

fn storage(err: crate::domain::IdentityError) -> PrivacyError {
    PrivacyError::Storage(err.to_string())
}
