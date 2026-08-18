//! Certificate metadata: what the app tracks about edge-issued certs.
//!
//! The app never holds private keys — those live in the edge proxy's own
//! storage. This aggregate records issuance/renewal facts so the renewal
//! and expiry-warning jobs can run independently of whatever the edge
//! believes.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::domain::{DomainsError, Hostname};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum CertificateStatus {
    Active,
    Revoked,
    Failed,
}

impl CertificateStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Revoked => "revoked",
            Self::Failed => "failed",
        }
    }

    pub fn parse(raw: &str) -> Result<Self, DomainsError> {
        match raw {
            "active" => Ok(Self::Active),
            "revoked" => Ok(Self::Revoked),
            "failed" => Ok(Self::Failed),
            other => Err(DomainsError::CertificateAuthority(format!(
                "invalid certificate status: {other}"
            ))),
        }
    }
}

/// A certificate row for one hostname.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct Certificate {
    pub id: Uuid,
    pub domain_id: Uuid,
    pub tenant_id: Uuid,
    pub hostname: Hostname,
    pub issuer: String,
    pub serial: String,
    pub not_before: DateTime<Utc>,
    pub not_after: DateTime<Utc>,
    pub renewed_at: Option<DateTime<Utc>>,
    /// The last expiry warning threshold already emitted (days-left value),
    /// so each warning fires exactly once per certificate.
    pub expiry_warned_days: Option<u32>,
    pub status: CertificateStatus,
    pub last_error: Option<String>,
}

impl Certificate {
    #[allow(clippy::too_many_arguments)]
    pub fn issued(
        id: Uuid,
        domain_id: Uuid,
        tenant_id: Uuid,
        hostname: Hostname,
        issuer: String,
        serial: String,
        not_before: DateTime<Utc>,
        not_after: DateTime<Utc>,
    ) -> Self {
        Self {
            id,
            domain_id,
            tenant_id,
            hostname,
            issuer,
            serial,
            not_before,
            not_after,
            renewed_at: None,
            expiry_warned_days: None,
            status: CertificateStatus::Active,
            last_error: None,
        }
    }

    /// Renewal succeeded: not_after moves, `renewed_at` records when.
    pub fn renew(&mut self, not_after: DateTime<Utc>, now: DateTime<Utc>) {
        self.not_after = not_after;
        self.renewed_at = Some(now);
        self.expiry_warned_days = None;
        self.status = CertificateStatus::Active;
        self.last_error = None;
    }

    /// The renewal attempt failed; keep the current cert and record why.
    pub fn mark_renewal_error(&mut self, detail: String) {
        self.last_error = Some(detail);
    }

    /// The certificate was revoked (domain removed / force-released).
    pub fn revoke(&mut self) {
        self.status = CertificateStatus::Revoked;
    }

    /// Whole days until expiry (0 when expired).
    pub fn days_to_expiry(&self, now: DateTime<Utc>) -> u32 {
        let delta = self.not_after - now;
        if delta.num_seconds() <= 0 {
            return 0;
        }
        delta.num_days() as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cert() -> Certificate {
        let now = Utc::now();
        Certificate::issued(
            Uuid::now_v7(),
            Uuid::now_v7(),
            Uuid::now_v7(),
            Hostname::parse("example.com").unwrap(),
            "Sandbox CA".to_string(),
            "00:01".to_string(),
            now,
            now + chrono::Duration::days(90),
        )
    }

    #[test]
    fn renewal_moves_expiry_and_clears_warnings() {
        let mut cert = cert();
        cert.expiry_warned_days = Some(7);
        cert.renew(Utc::now() + chrono::Duration::days(120), Utc::now());
        assert_eq!(cert.expiry_warned_days, None);
        assert!(cert.renewed_at.is_some());
    }

    #[test]
    fn days_to_expiry_counts_down() {
        let cert = cert();
        assert!(cert.days_to_expiry(Utc::now()) >= 89);
        assert_eq!(
            cert.days_to_expiry(cert.not_after + chrono::Duration::days(1)),
            0
        );
    }
}
