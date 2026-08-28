//! The user aggregate: the local projection of a Kratos identity.
//!
//! Kratos owns credentials, traits and sessions; this row exists so joins,
//! RLS and soft-delete have something local to hold on to. Every mutation
//! comes from the Kratos webhooks (the projection) or explicit
//! deactivation — never from a client directly.

use chrono::{DateTime, Utc};
pub use sanvi_kernel::UserId;
use serde::Serialize;
use utoipa::ToSchema;

use crate::domain::IdentityError;
use crate::domain::values::{Email, Locale, ProviderId, ProviderSubject};


/// Lifecycle of a projected user. Deactivated is terminal; the email is
/// released for reuse.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum UserStatus {
    Active,
    Deactivated,
}

impl UserStatus {
    pub fn parse(raw: &str) -> Result<Self, IdentityError> {
        match raw {
            "active" => Ok(Self::Active),
            "deactivated" => Ok(Self::Deactivated),
            other => Err(IdentityError::invalid_user_status(other)),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Deactivated => "deactivated",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct User {
    id: UserId,
    kratos_identity_id: uuid::Uuid,
    primary_email: Email,
    email_verified: bool,
    status: UserStatus,
    locale: Option<Locale>,
    created_at: DateTime<Utc>,
    deactivated_at: Option<DateTime<Utc>>,
}

impl User {
    /// Create the projection for a freshly registered Kratos identity.
    pub fn project_new(
        id: UserId,
        kratos_identity_id: uuid::Uuid,
        primary_email: Email,
        email_verified: bool,
        locale: Option<Locale>,
        now: DateTime<Utc>,
    ) -> Self {
        Self {
            id,
            kratos_identity_id,
            primary_email,
            email_verified,
            status: UserStatus::Active,
            locale,
            created_at: now,
            deactivated_at: None,
        }
    }

    /// Rehydrate a stored row (adapter use only).
    pub fn from_parts(
        id: UserId,
        kratos_identity_id: uuid::Uuid,
        primary_email: Email,
        email_verified: bool,
        status: UserStatus,
        locale: Option<Locale>,
        created_at: DateTime<Utc>,
        deactivated_at: Option<DateTime<Utc>>,
    ) -> Self {
        Self {
            id,
            kratos_identity_id,
            primary_email,
            email_verified,
            status,
            locale,
            created_at,
            deactivated_at,
        }
    }

    /// Apply a Kratos webhook snapshot (registration/login/settings). Returns
    /// true when anything changed. Email moves only when Kratos says the new
    /// address is verified; Kratos itself guarantees uniqueness. Deactivated
    /// users are frozen: their projection never changes again.
    pub fn apply_snapshot(
        &mut self,
        email: Option<Email>,
        email_verified: bool,
        locale: Option<Locale>,
    ) -> bool {
        if self.status != UserStatus::Active {
            return false;
        }
        let mut changed = false;
        if let Some(email) = email
            && email != self.primary_email
        {
            self.primary_email = email;
            changed = true;
        }
        if email_verified && !self.email_verified {
            self.email_verified = true;
            changed = true;
        }
        if locale.is_some() && locale != self.locale {
            self.locale = locale;
            changed = true;
        }
        changed
    }

    pub fn deactivate(&mut self, now: DateTime<Utc>) -> Result<(), IdentityError> {
        if self.status == UserStatus::Deactivated {
            return Err(IdentityError::IllegalUserTransition {
                from: self.status.as_str().to_string(),
                to: UserStatus::Deactivated.as_str().to_string(),
            });
        }
        self.status = UserStatus::Deactivated;
        self.deactivated_at = Some(now);
        Ok(())
    }

    pub fn ensure_active(&self) -> Result<(), IdentityError> {
        if self.status != UserStatus::Active {
            return Err(IdentityError::UserNotActive { id: self.id });
        }
        Ok(())
    }

    pub fn id(&self) -> UserId {
        self.id
    }
    pub fn kratos_identity_id(&self) -> uuid::Uuid {
        self.kratos_identity_id
    }
    pub fn primary_email(&self) -> &Email {
        &self.primary_email
    }
    pub fn email_verified(&self) -> bool {
        self.email_verified
    }
    pub fn status(&self) -> UserStatus {
        self.status
    }
    pub fn locale(&self) -> Option<&Locale> {
        self.locale.as_ref()
    }
    pub fn created_at(&self) -> DateTime<Utc> {
        self.created_at
    }
    pub fn deactivated_at(&self) -> Option<DateTime<Utc>> {
        self.deactivated_at
    }
}

/// How a provider account came to be bound to a user. Every value except
/// `AutoVerified` is recorded by an explicit, authenticated action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum LinkedVia {
    /// Linked during first-time registration with the provider.
    Registration,
    /// Auto-linked because the provider verified the email and the existing
    /// user's email is verified too.
    AutoVerified,
    /// Linked after the account-linking challenge (proof of ownership).
    Challenge,
    /// Linked by completing the Kratos settings flow while signed in.
    SettingsFlow,
}

impl LinkedVia {
    pub fn parse(raw: &str) -> Result<Self, IdentityError> {
        match raw {
            "registration" => Ok(Self::Registration),
            "auto_verified" => Ok(Self::AutoVerified),
            "challenge" => Ok(Self::Challenge),
            "settings_flow" => Ok(Self::SettingsFlow),
            other => Err(IdentityError::InvalidProvider(other.to_string())),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Registration => "registration",
            Self::AutoVerified => "auto_verified",
            Self::Challenge => "challenge",
            Self::SettingsFlow => "settings_flow",
        }
    }
}

/// A bound external account. Uniqueness is global on `(provider, subject)`:
/// one provider account can never belong to two users.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkedAccount {
    user_id: UserId,
    provider: ProviderId,
    subject: ProviderSubject,
    linked_via: LinkedVia,
    linked_at: DateTime<Utc>,
}

impl LinkedAccount {
    pub fn new(
        user_id: UserId,
        provider: ProviderId,
        subject: ProviderSubject,
        linked_via: LinkedVia,
        linked_at: DateTime<Utc>,
    ) -> Self {
        Self {
            user_id,
            provider,
            subject,
            linked_via,
            linked_at,
        }
    }

    pub fn user_id(&self) -> UserId {
        self.user_id
    }
    pub fn provider(&self) -> &ProviderId {
        &self.provider
    }
    pub fn subject(&self) -> &ProviderSubject {
        &self.subject
    }
    pub fn linked_via(&self) -> LinkedVia {
        self.linked_via
    }
    pub fn linked_at(&self) -> DateTime<Utc> {
        self.linked_at
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(now: DateTime<Utc>) -> User {
        User::project_new(
            UserId::try_from(101_i64).unwrap(),
            uuid::Uuid::now_v7(),
            Email::parse("alice@example.com").unwrap(),
            false,
            None,
            now,
        )
    }

    #[test]
    fn snapshot_updates_email_locale_and_verification() {
        let now = Utc::now();
        let mut user = user(now);
        assert!(!user.apply_snapshot(None, false, None));

        assert!(user.apply_snapshot(
            Some(Email::parse("alice.new@example.com").unwrap()),
            true,
            Some(Locale::parse("ja").unwrap()),
        ));
        assert_eq!(user.primary_email().as_str(), "alice.new@example.com");
        assert!(user.email_verified());
        assert_eq!(user.locale().map(|l| l.as_str()), Some("ja"));
    }

    #[test]
    fn deactivation_is_terminal() {
        let now = Utc::now();
        let mut user = user(now);
        user.deactivate(now).unwrap();
        assert_eq!(user.status(), UserStatus::Deactivated);
        assert!(matches!(
            user.deactivate(now),
            Err(IdentityError::IllegalUserTransition { .. })
        ));
        // A deactivated user's email must not move.
        assert!(
            !user.apply_snapshot(Some(Email::parse("other@example.com").unwrap()), true, None,)
        );
    }

    #[test]
    fn status_parse_roundtrips() {
        assert_eq!(UserStatus::parse("active").unwrap(), UserStatus::Active);
        assert!(UserStatus::parse("bogus").is_err());
        assert_eq!(UserStatus::Active.as_str(), "active");
    }

    #[test]
    fn linked_via_parse_roundtrips() {
        assert_eq!(LinkedVia::parse("challenge").unwrap(), LinkedVia::Challenge);
        assert!(LinkedVia::parse("bogus").is_err());
    }
}
