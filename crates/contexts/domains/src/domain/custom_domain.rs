//! The custom-domain aggregate: the claim lifecycle and its state machine.
//!
//! `pending_setup → verifying → verified → issuing_cert → live → degraded`,
//! with `removed` reachable from anywhere. Transitions are methods on the
//! aggregate; a transition from the wrong state is an error, never a silent
//! jump. Invariants enforced by the repository's partial unique indexes:
//! one live hostname per platform (verified claim wins; losers are removed
//! with `verified_by_other_tenant`) and one `primary` per tenant.

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use sanvi_kernel::TenantId;

use crate::domain::{DomainsError, Hostname};

pub enum CustomDomainTag {}
pub type CustomDomainId = sanvi_kernel::Id<CustomDomainTag>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum DomainKind {
    /// The tenant already owns the domain; we verify and serve it.
    Connected,
    /// Sanvi purchased and manages the domain.
    Purchased,
}

impl DomainKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Connected => "connected",
            Self::Purchased => "purchased",
        }
    }

    pub fn parse(raw: &str) -> Result<Self, DomainsError> {
        match raw {
            "connected" => Ok(Self::Connected),
            "purchased" => Ok(Self::Purchased),
            other => Err(DomainsError::InvalidHostname(format!(
                "invalid domain kind: {other}"
            ))),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum DomainRole {
    /// The tenant's canonical hostname; the one the edge redirects aliases to.
    Primary,
    /// An additional hostname resolving to the same storefront.
    Alias,
    /// A hostname that 301s to the primary (commonly `www`).
    Redirect,
}

impl DomainRole {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Primary => "primary",
            Self::Alias => "alias",
            Self::Redirect => "redirect",
        }
    }

    pub fn parse(raw: &str) -> Result<Self, DomainsError> {
        match raw {
            "primary" => Ok(Self::Primary),
            "alias" => Ok(Self::Alias),
            "redirect" => Ok(Self::Redirect),
            other => Err(DomainsError::InvalidHostname(format!(
                "invalid domain role: {other}"
            ))),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum DomainStatus {
    /// Claimed; instructions shown; nothing checked yet.
    PendingSetup,
    /// Actively being polled (backoff).
    Verifying,
    /// Ownership and routing confirmed.
    Verified,
    /// Certificate issuance in flight.
    IssuingCert,
    /// Verified, TLS-terminated, serving.
    Live,
    /// Live, but the health monitor found drift (DNS changed, cert stale).
    Degraded,
    /// Gone: removed by the tenant, expired unverified, force-released, or
    /// superseded by another tenant's verified claim.
    Removed,
}

impl DomainStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PendingSetup => "pending_setup",
            Self::Verifying => "verifying",
            Self::Verified => "verified",
            Self::IssuingCert => "issuing_cert",
            Self::Live => "live",
            Self::Degraded => "degraded",
            Self::Removed => "removed",
        }
    }

    pub fn parse(raw: &str) -> Result<Self, DomainsError> {
        match raw {
            "pending_setup" => Ok(Self::PendingSetup),
            "verifying" => Ok(Self::Verifying),
            "verified" => Ok(Self::Verified),
            "issuing_cert" => Ok(Self::IssuingCert),
            "live" => Ok(Self::Live),
            "degraded" => Ok(Self::Degraded),
            "removed" => Ok(Self::Removed),
            other => Err(DomainsError::InvalidHostname(format!(
                "invalid domain status: {other}"
            ))),
        }
    }

    /// Hostnames in these states hold the global "live claim" lock.
    pub const fn is_live_claim(self) -> bool {
        matches!(
            self,
            Self::Verified | Self::IssuingCert | Self::Live | Self::Degraded
        )
    }

    /// The edge may issue certificates for hostnames in these states.
    pub const fn is_authorized(self) -> bool {
        matches!(
            self,
            Self::Verified | Self::IssuingCert | Self::Live | Self::Degraded
        )
    }
}

/// The ownership challenge: a high-entropy token the tenant publishes as a
/// TXT record at `_sanvi-challenge.<hostname>`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct VerificationChallenge {
    pub token: String,
    /// Always `txt` today; the type drives the checker.
    pub challenge_type: String,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}

impl VerificationChallenge {
    pub fn new(token: String, now: DateTime<Utc>, ttl_hours: u64) -> Self {
        Self {
            token,
            challenge_type: "txt".to_string(),
            created_at: now,
            expires_at: now + Duration::hours(ttl_hours as i64),
        }
    }

    /// The TXT record value the tenant must publish.
    pub fn txt_value(&self) -> String {
        format!(
            "{}{}",
            crate::domain::records::VERIFICATION_TXT_PREFIX,
            self.token
        )
    }

    /// The TXT record name (`_sanvi-challenge.<hostname>`).
    pub fn txt_name(&self, hostname: &Hostname) -> String {
        format!("_sanvi-challenge.{hostname}")
    }
}

/// Why a domain failed or degraded, as a stable code plus free detail.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct DomainFailure {
    pub code: String,
    pub detail: String,
}

/// Failure codes — stable identifiers the UI maps to localized messages.
pub struct DomainErrorCode;

impl DomainErrorCode {
    pub const CHALLENGE_EXPIRED: &'static str = "challenge_expired";
    pub const TXT_MISSING: &'static str = "txt_missing";
    pub const ROUTING_MISSING: &'static str = "routing_missing";
    pub const VERIFIED_BY_OTHER_TENANT: &'static str = "verified_by_other_tenant";
    pub const DNS_DRIFT: &'static str = "dns_drift";
    pub const CERT_INVALID: &'static str = "cert_invalid";
    pub const PROBE_FAILED: &'static str = "probe_failed";
    pub const REMOVED_BY_TENANT: &'static str = "removed_by_tenant";
    pub const PLATFORM_RELEASE: &'static str = "platform_release";
    pub const SUPERSEDED_BY_PURCHASE: &'static str = "superseded_by_purchase";
}

/// The custom-domain aggregate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct CustomDomain {
    #[schema(value_type = String)]
    pub id: CustomDomainId,
    #[schema(value_type = String)]
    pub tenant_id: TenantId,
    pub hostname: Hostname,
    pub kind: DomainKind,
    pub role: DomainRole,
    pub status: DomainStatus,
    pub challenge: Option<VerificationChallenge>,
    /// The detected registrar guide id (`cloudflare`, `route53`, …), set by
    /// the verification worker from the zone's NS records.
    pub detected_registrar: Option<String>,
    pub verified_at: Option<DateTime<Utc>>,
    pub last_checked_at: Option<DateTime<Utc>>,
    pub next_check_at: Option<DateTime<Utc>>,
    pub verification_attempts: u32,
    pub failure: Option<DomainFailure>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl CustomDomain {
    #[allow(clippy::too_many_arguments)]
    pub fn claim(
        id: CustomDomainId,
        tenant_id: TenantId,
        hostname: Hostname,
        kind: DomainKind,
        role: DomainRole,
        token: String,
        now: DateTime<Utc>,
        challenge_ttl_hours: u64,
        backoff_base_secs: u64,
    ) -> Self {
        Self {
            id,
            tenant_id,
            hostname,
            kind,
            role,
            status: DomainStatus::PendingSetup,
            challenge: Some(VerificationChallenge::new(token, now, challenge_ttl_hours)),
            detected_registrar: None,
            verified_at: None,
            last_checked_at: None,
            next_check_at: Some(now + Duration::seconds(backoff_base_secs as i64)),
            verification_attempts: 0,
            failure: None,
            created_at: now,
            updated_at: now,
        }
    }

    fn require_status(
        &self,
        expected: &[DomainStatus],
        transition: &str,
    ) -> Result<(), DomainsError> {
        if expected.contains(&self.status) {
            return Ok(());
        }
        Err(DomainsError::IllegalTransition {
            from: self.status.as_str().to_string(),
            to: transition.to_string(),
        })
    }

    /// Manual or automatic verification kick-off.
    pub fn start_verification(&mut self, now: DateTime<Utc>) -> Result<(), DomainsError> {
        if self.status == DomainStatus::Removed {
            return Err(DomainsError::DomainRemoved);
        }
        if self.status != DomainStatus::Verifying {
            self.status = DomainStatus::Verifying;
            self.next_check_at = Some(now);
            self.updated_at = now;
        }
        Ok(())
    }

    /// A polling attempt ran and succeeded.
    pub fn mark_verified(&mut self, now: DateTime<Utc>) -> Result<(), DomainsError> {
        self.require_status(
            &[DomainStatus::PendingSetup, DomainStatus::Verifying],
            "verified",
        )?;
        self.status = DomainStatus::Verified;
        self.verified_at = Some(now);
        self.last_checked_at = Some(now);
        self.next_check_at = None;
        self.failure = None;
        self.updated_at = now;
        Ok(())
    }

    /// A polling attempt ran and failed: stay `verifying`, record why, and
    /// schedule the next attempt with backoff.
    pub fn mark_verification_failed(
        &mut self,
        code: &str,
        detail: String,
        now: DateTime<Utc>,
        backoff_base_secs: u64,
        backoff_max_secs: u64,
    ) -> Result<(), DomainsError> {
        self.require_status(
            &[DomainStatus::PendingSetup, DomainStatus::Verifying],
            "verifying",
        )?;
        self.status = DomainStatus::Verifying;
        self.verification_attempts += 1;
        self.last_checked_at = Some(now);
        self.failure = Some(DomainFailure {
            code: code.to_string(),
            detail,
        });
        self.next_check_at = Some(
            now + Duration::seconds(verification_backoff(
                self.verification_attempts,
                backoff_base_secs,
                backoff_max_secs,
            ) as i64),
        );
        self.updated_at = now;
        Ok(())
    }

    /// Certificate issuance begins.
    pub fn start_cert_issuance(&mut self, now: DateTime<Utc>) -> Result<(), DomainsError> {
        self.require_status(&[DomainStatus::Verified], "issuing_cert")?;
        self.status = DomainStatus::IssuingCert;
        self.updated_at = now;
        Ok(())
    }

    /// A valid certificate exists: serve.
    pub fn mark_live(&mut self, now: DateTime<Utc>) -> Result<(), DomainsError> {
        self.require_status(
            &[
                DomainStatus::Verified,
                DomainStatus::IssuingCert,
                DomainStatus::Degraded,
            ],
            "live",
        )?;
        self.status = DomainStatus::Live;
        self.failure = None;
        self.updated_at = now;
        Ok(())
    }

    /// Health drift detected on a live domain.
    pub fn mark_degraded(
        &mut self,
        code: &str,
        detail: String,
        now: DateTime<Utc>,
    ) -> Result<(), DomainsError> {
        self.require_status(&[DomainStatus::Live, DomainStatus::Degraded], "degraded")?;
        self.status = DomainStatus::Degraded;
        self.failure = Some(DomainFailure {
            code: code.to_string(),
            detail,
        });
        self.updated_at = now;
        Ok(())
    }

    /// Remove, with the reason recorded for the audit trail.
    pub fn remove(
        &mut self,
        code: &str,
        detail: String,
        now: DateTime<Utc>,
    ) -> Result<(), DomainsError> {
        if self.status == DomainStatus::Removed {
            return Ok(());
        }
        self.status = DomainStatus::Removed;
        self.failure = Some(DomainFailure {
            code: code.to_string(),
            detail,
        });
        self.next_check_at = None;
        self.updated_at = now;
        Ok(())
    }

    /// A new primary: demote the current one.
    pub fn demote_primary(&mut self, now: DateTime<Utc>) -> Result<(), DomainsError> {
        if self.role != DomainRole::Primary {
            return Err(DomainsError::IllegalTransition {
                from: self.role.as_str().to_string(),
                to: "alias".to_string(),
            });
        }
        self.role = DomainRole::Alias;
        self.updated_at = now;
        Ok(())
    }

    /// Promote this domain to primary.
    pub fn promote_primary(&mut self, now: DateTime<Utc>) -> Result<(), DomainsError> {
        if self.status == DomainStatus::Removed {
            return Err(DomainsError::DomainRemoved);
        }
        if !self.status.is_live_claim()
            && self.status != DomainStatus::PendingSetup
            && self.status != DomainStatus::Verifying
        {
            return Err(DomainsError::DomainNotLive(
                self.hostname.as_str().to_string(),
            ));
        }
        self.role = DomainRole::Primary;
        self.updated_at = now;
        Ok(())
    }
}

/// Exponential backoff, capped: 10s → 20s → 40s … → 300s.
pub fn verification_backoff(attempt: u32, base_secs: u64, max_secs: u64) -> u64 {
    let multiplier = 1u64 << attempt.saturating_sub(1).min(10);
    base_secs.saturating_mul(multiplier).min(max_secs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn domain() -> CustomDomain {
        CustomDomain::claim(
            CustomDomainId::try_from(1i64).unwrap(),
            TenantId::try_from(1i64).unwrap(),
            Hostname::parse("example.com").unwrap(),
            DomainKind::Connected,
            DomainRole::Primary,
            "token".to_string(),
            Utc::now(),
            72,
            10,
        )
    }

    #[test]
    fn backoff_grows_and_caps() {
        assert_eq!(verification_backoff(0, 10, 300), 10);
        assert_eq!(verification_backoff(1, 10, 300), 10);
        assert_eq!(verification_backoff(2, 10, 300), 20);
        assert_eq!(verification_backoff(3, 10, 300), 40);
        assert_eq!(verification_backoff(10, 10, 300), 300);
    }

    #[test]
    fn happy_path_transitions() {
        let mut domain = domain();
        let now = Utc::now();
        domain.start_verification(now).unwrap();
        assert_eq!(domain.status, DomainStatus::Verifying);
        domain.mark_verified(now).unwrap();
        assert_eq!(domain.status, DomainStatus::Verified);
        domain.start_cert_issuance(now).unwrap();
        assert_eq!(domain.status, DomainStatus::IssuingCert);
        domain.mark_live(now).unwrap();
        assert_eq!(domain.status, DomainStatus::Live);
    }

    #[test]
    fn illegal_transitions_error() {
        let mut domain = domain();
        let now = Utc::now();
        // Verified → cert issuance only from verified.
        assert!(domain.start_cert_issuance(now).is_err());
        domain.start_verification(now).unwrap();
        assert!(domain.mark_live(now).is_err());
    }

    #[test]
    fn removal_is_idempotent() {
        let mut domain = domain();
        let now = Utc::now();
        domain.remove("test", "test".to_string(), now).unwrap();
        assert_eq!(domain.status, DomainStatus::Removed);
        domain.remove("test", "test".to_string(), now).unwrap();
        assert!(domain.start_verification(now).is_err());
    }

    #[test]
    fn failed_verification_schedules_backoff() {
        let mut domain = domain();
        let now = Utc::now();
        domain.start_verification(now).unwrap();
        domain
            .mark_verification_failed("txt_missing", "no record".to_string(), now, 10, 300)
            .unwrap();
        assert_eq!(domain.verification_attempts, 1);
        assert!(domain.next_check_at.unwrap() > now);
    }

    #[test]
    fn primary_promotion_demotes_current() {
        let mut primary = domain();
        let now = Utc::now();
        primary.start_verification(now).unwrap();
        primary.mark_verified(now).unwrap();
        primary.demote_primary(now).unwrap();
        assert_eq!(primary.role, DomainRole::Alias);
        primary.promote_primary(now).unwrap();
        assert_eq!(primary.role, DomainRole::Primary);
    }
}
