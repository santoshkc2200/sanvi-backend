//! Provider authorization state for one connection binding.
//!
//! A credential exists only after a tenant user completed the provider's OAuth
//! consent for the exact organization registered on the connection. Tokens are
//! held as [`Secret`] so they never reach logs, events or responses.

use chrono::{DateTime, TimeDelta, Utc};
use sanvi_kernel::{TenantId, UserId};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{BookId, ConnectionId, Provider};

/// A consent link is usable for this long after it is issued.
pub const OAUTH_STATE_TTL: TimeDelta = match TimeDelta::try_minutes(10) {
    Some(delta) => delta,
    None => panic!("invalid OAuth state TTL"),
};
/// A cached access token is discarded this long before the provider expires it.
pub const ACCESS_TOKEN_MARGIN: TimeDelta = match TimeDelta::try_seconds(60) {
    Some(delta) => delta,
    None => panic!("invalid access token margin"),
};
/// freee refresh tokens expire 90 days after issue; an older token is not tried.
pub const REFRESH_TOKEN_LIFETIME: TimeDelta = match TimeDelta::try_days(90) {
    Some(delta) => delta,
    None => panic!("invalid refresh token lifetime"),
};

/// Credential material. Never formats or serializes its value.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }
    /// For provider adapters and the encrypting store only.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret([REDACTED])")
    }
}

/// Claims carried through the provider redirect. The initiating user must be
/// the one who completes the callback.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OAuthStateClaims {
    pub tenant_id: TenantId,
    pub user_id: UserId,
    pub book_id: BookId,
    pub connection_id: ConnectionId,
    pub provider: Provider,
    pub nonce: Uuid,
    pub expires_at: DateTime<Utc>,
}

/// Tokens returned by a provider's token endpoint.
#[derive(Debug, Clone)]
pub struct TokenGrant {
    pub access_token: Secret,
    pub refresh_token: Secret,
    pub access_expires_at: DateTime<Utc>,
    /// The organization the grant is limited to, when the provider reports one.
    pub organization_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialStatus {
    Active,
    /// The provider refused the refresh token; a user must consent again.
    ReauthorizationRequired,
    /// Deliberately disconnected; no token material is retained.
    Revoked,
}

impl CredentialStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::ReauthorizationRequired => "reauthorization_required",
            Self::Revoked => "revoked",
        }
    }
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "active" => Some(Self::Active),
            "reauthorization_required" => Some(Self::ReauthorizationRequired),
            "revoked" => Some(Self::Revoked),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tokens {
    pub refresh_token: Secret,
    pub refresh_issued_at: DateTime<Utc>,
    pub access_token: Secret,
    pub access_expires_at: DateTime<Utc>,
}

/// Token material is present only while the credential is active.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Credential {
    pub book_id: BookId,
    pub connection_id: ConnectionId,
    pub status: CredentialStatus,
    pub tokens: Option<Tokens>,
}

impl Credential {
    pub fn authorized(
        book_id: BookId,
        connection_id: ConnectionId,
        grant: TokenGrant,
        now: DateTime<Utc>,
    ) -> Self {
        Self {
            book_id,
            connection_id,
            status: CredentialStatus::Active,
            tokens: Some(Tokens {
                refresh_token: grant.refresh_token,
                refresh_issued_at: now,
                access_token: grant.access_token,
                access_expires_at: grant.access_expires_at,
            }),
        }
    }

    pub fn usable_access_token(&self, now: DateTime<Utc>) -> Option<&Secret> {
        let tokens = self.tokens.as_ref()?;
        (self.status == CredentialStatus::Active
            && now + ACCESS_TOKEN_MARGIN < tokens.access_expires_at)
            .then_some(&tokens.access_token)
    }

    /// The refresh token to trade, or `None` when only a new consent can help.
    pub fn refreshable_token(&self, now: DateTime<Utc>) -> Option<&Secret> {
        let tokens = self.tokens.as_ref()?;
        (self.status == CredentialStatus::Active
            && now < tokens.refresh_issued_at + REFRESH_TOKEN_LIFETIME)
            .then_some(&tokens.refresh_token)
    }

    /// Single-use refresh tokens: the old one is dead once this grant exists.
    pub fn rotated(self, grant: TokenGrant, now: DateTime<Utc>) -> Self {
        Self::authorized(self.book_id, self.connection_id, grant, now)
    }

    pub fn requiring_reauthorization(self) -> Self {
        Self {
            status: CredentialStatus::ReauthorizationRequired,
            tokens: None,
            ..self
        }
    }

    pub fn revoked(self) -> Self {
        Self {
            status: CredentialStatus::Revoked,
            tokens: None,
            ..self
        }
    }
}

/// What the provider says the authorized user may do in one organization.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProviderCatalog {
    pub can_post: bool,
    pub account_ids: std::collections::BTreeSet<String>,
    pub tax_codes: std::collections::BTreeSet<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(value: &str) -> DateTime<Utc> {
        value.parse().unwrap()
    }

    fn credential(now: DateTime<Utc>) -> Credential {
        Credential::authorized(
            BookId::try_from(1).unwrap(),
            ConnectionId::try_from(2).unwrap(),
            TokenGrant {
                access_token: Secret::new("access"),
                refresh_token: Secret::new("refresh"),
                access_expires_at: now + TimeDelta::hours(6),
                organization_id: Some("10".into()),
            },
            now,
        )
    }

    #[test]
    fn secrets_never_format() {
        assert_eq!(format!("{:?}", Secret::new("abc")), "Secret([REDACTED])");
        let debug = format!("{:?}", credential(at("2026-10-26T00:00:00Z")));
        assert!(!debug.contains("\"access\"") && !debug.contains("\"refresh\""));
    }

    #[test]
    fn access_token_is_reused_until_the_margin_before_expiry() {
        let now = at("2026-10-26T00:00:00Z");
        let credential = credential(now);
        assert!(credential.usable_access_token(now).is_some());
        let near_expiry = now + TimeDelta::hours(6) - ACCESS_TOKEN_MARGIN;
        assert!(credential.usable_access_token(near_expiry).is_none());
    }

    #[test]
    fn refresh_token_is_not_tried_after_its_lifetime() {
        let now = at("2026-10-26T00:00:00Z");
        let credential = credential(now);
        assert!(credential.refreshable_token(now).is_some());
        assert!(
            credential
                .refreshable_token(now + REFRESH_TOKEN_LIFETIME)
                .is_none()
        );
    }

    #[test]
    fn inactive_credentials_hold_no_tokens() {
        let now = at("2026-10-26T00:00:00Z");
        for inactive in [
            credential(now).requiring_reauthorization(),
            credential(now).revoked(),
        ] {
            assert!(inactive.tokens.is_none());
            assert!(inactive.usable_access_token(now).is_none());
            assert!(inactive.refreshable_token(now).is_none());
        }
    }

    #[test]
    fn status_names_round_trip() {
        for status in [
            CredentialStatus::Active,
            CredentialStatus::ReauthorizationRequired,
            CredentialStatus::Revoked,
        ] {
            assert_eq!(CredentialStatus::parse(status.as_str()), Some(status));
        }
    }
}
