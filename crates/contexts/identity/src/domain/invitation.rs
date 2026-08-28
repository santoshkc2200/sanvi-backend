//! Invitations: email-targeted, single-use, expiring tokens that turn into
//! memberships on acceptance.

use chrono::{DateTime, Utc};
use rand::RngCore;
use rand::rngs::OsRng;
use sanvi_kernel::{Id, RoleId, TenantId};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::domain::IdentityError;
use crate::domain::user::UserId;
use crate::domain::values::Email;

pub enum InvitationTag {}
pub type InvitationId = Id<InvitationTag>;

/// The raw secret handed to the invitee. Only its SHA-256 hash is stored;
/// the raw form exists in exactly two places: the invitation email and the
/// accepting request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvitationToken(String);

impl InvitationToken {
    /// Generate a fresh token: 32 random bytes, hex-encoded.
    pub fn generate() -> Self {
        let mut bytes = [0u8; 32];
        OsRng.fill_bytes(&mut bytes);
        Self(bytes.iter().map(|b| format!("{b:02x}")).collect())
    }

    pub fn parse(raw: &str) -> Result<Self, IdentityError> {
        let token = raw.trim();
        if token.len() != 64 || !token.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(IdentityError::MalformedWebhook(
                "invalid token shape".to_string(),
            ));
        }
        Ok(Self(token.to_ascii_lowercase()))
    }

    pub fn token_hash(&self) -> String {
        let digest = Sha256::digest(self.0.as_bytes());
        digest.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// Exposed only to the notification adapter (and tests), which needs the
    /// raw secret to put it in the email.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A pending invitation. The token hash is the lookup key; acceptance marks
/// `accepted_at` and grants the membership in the same transaction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Invitation {
    id: InvitationId,
    tenant_id: TenantId,
    email: Email,
    role_ids: Vec<RoleId>,
    token_hash: String,
    invited_by: Option<UserId>,
    created_at: DateTime<Utc>,
    expires_at: DateTime<Utc>,
    accepted_at: Option<DateTime<Utc>>,
}

impl Invitation {
    pub fn new(
        id: InvitationId,
        tenant_id: TenantId,
        email: Email,
        role_ids: Vec<RoleId>,
        token: &InvitationToken,
        invited_by: Option<UserId>,
        now: DateTime<Utc>,
        expires_at: DateTime<Utc>,
    ) -> Result<Self, IdentityError> {
        if role_ids.is_empty() {
            return Err(IdentityError::NoRoles);
        }
        if expires_at <= now {
            return Err(IdentityError::InvitationExpired);
        }
        Ok(Self {
            id,
            tenant_id,
            email,
            role_ids,
            token_hash: token.token_hash(),
            invited_by,
            created_at: now,
            expires_at,
            accepted_at: None,
        })
    }

    /// Rehydrate a stored row (adapter use only).
    #[allow(clippy::too_many_arguments)]
    pub fn from_parts(
        id: InvitationId,
        tenant_id: TenantId,
        email: Email,
        role_ids: Vec<RoleId>,
        token_hash: String,
        invited_by: Option<UserId>,
        created_at: DateTime<Utc>,
        expires_at: DateTime<Utc>,
        accepted_at: Option<DateTime<Utc>>,
    ) -> Self {
        Self {
            id,
            tenant_id,
            email,
            role_ids,
            token_hash,
            invited_by,
            created_at,
            expires_at,
            accepted_at,
        }
    }

    /// Reissue with a fresh token (resend) — invalidates the previous one.
    pub fn reissue(
        &mut self,
        token: &InvitationToken,
        now: DateTime<Utc>,
        expires_at: DateTime<Utc>,
    ) -> Result<(), IdentityError> {
        if self.accepted_at.is_some() {
            return Err(IdentityError::InvitationAlreadyAccepted);
        }
        if expires_at <= now {
            return Err(IdentityError::InvitationExpired);
        }
        self.token_hash = token.token_hash();
        self.expires_at = expires_at;
        Ok(())
    }

    /// Claim the invitation for acceptance. Idempotence of the *row* is the
    /// adapter's job (guarded by the transactional uniqueness of the
    /// membership); this only validates eligibility.
    pub fn validate_acceptance(
        &self,
        email: &Email,
        now: DateTime<Utc>,
    ) -> Result<(), IdentityError> {
        if self.accepted_at.is_some() {
            return Err(IdentityError::InvitationAlreadyAccepted);
        }
        if now >= self.expires_at {
            return Err(IdentityError::InvitationExpired);
        }
        if &self.email != email {
            return Err(IdentityError::InvitationEmailMismatch);
        }
        Ok(())
    }

    pub fn mark_accepted(&mut self, now: DateTime<Utc>) -> Result<(), IdentityError> {
        self.validate_acceptance(&self.email.clone(), now)?;
        self.accepted_at = Some(now);
        Ok(())
    }

    pub fn id(&self) -> InvitationId {
        self.id
    }
    pub fn tenant_id(&self) -> TenantId {
        self.tenant_id
    }
    pub fn email(&self) -> &Email {
        &self.email
    }
    pub fn role_ids(&self) -> &[RoleId] {
        &self.role_ids
    }
    pub fn token_hash(&self) -> &str {
        &self.token_hash
    }
    pub fn invited_by(&self) -> Option<UserId> {
        self.invited_by
    }
    pub fn created_at(&self) -> DateTime<Utc> {
        self.created_at
    }
    pub fn expires_at(&self) -> DateTime<Utc> {
        self.expires_at
    }
    pub fn accepted_at(&self) -> Option<DateTime<Utc>> {
        self.accepted_at
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn invitation() -> (Invitation, InvitationToken) {
        let token = InvitationToken::generate();
        let now = Utc::now();
        let inv = Invitation::new(
            InvitationId::try_from(101_i64).unwrap(),
            TenantId::try_from(102_i64).unwrap(),
            Email::parse("alice@example.com").unwrap(),
            vec![RoleId::try_from(103_i64).unwrap()],
            &token,
            None,
            now,
            now + chrono::Duration::days(7),
        )
        .unwrap();
        (inv, token)
    }

    #[test]
    fn token_is_stable_and_hashes_differ() {
        let (inv, token) = invitation();
        assert_eq!(inv.token_hash(), token.token_hash());
        let other = InvitationToken::generate();
        assert_ne!(inv.token_hash(), other.token_hash());
    }

    #[test]
    fn acceptance_validates_email_expiry_and_reuse() {
        let (mut inv, _) = invitation();
        let now = Utc::now();
        let wrong = Email::parse("bob@example.com").unwrap();
        assert!(matches!(
            inv.validate_acceptance(&wrong, now),
            Err(IdentityError::InvitationEmailMismatch)
        ));
        inv.mark_accepted(now).unwrap();
        assert!(matches!(
            inv.validate_acceptance(inv.email(), now),
            Err(IdentityError::InvitationAlreadyAccepted)
        ));
        assert!(
            inv.reissue(
                &InvitationToken::generate(),
                now,
                now + chrono::Duration::days(7)
            )
            .is_err()
        );
    }

    #[test]
    fn reissue_rotates_the_token() {
        let (mut inv, token) = invitation();
        let now = Utc::now();
        let fresh = InvitationToken::generate();
        inv.reissue(&fresh, now, now + chrono::Duration::days(7))
            .unwrap();
        assert_ne!(inv.token_hash(), token.token_hash());
        assert_eq!(inv.token_hash(), fresh.token_hash());
    }

    #[test]
    fn token_parse_enforces_shape() {
        assert!(InvitationToken::parse("abcd").is_err());
        assert!(InvitationToken::parse(&"f".repeat(64)).is_ok());
        assert!(InvitationToken::parse(&format!("{}x", "f".repeat(63))).is_err());
    }
}
