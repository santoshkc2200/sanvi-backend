//! Tax-liability state (09.6).
//!
//! With direct charges the **connected account is the merchant of
//! record**, so when a tenant enables automatic tax the liability routes
//! to their account (`automatic_tax.liability = { type: 'account',
//! account: acct_id }`), never to Sanvi.
//!
//! The preflight is the whole point of this slice: Stripe Tax collects
//! nothing where the liable account has no active registration, and it
//! returns **no error** while doing so. An enabled toggle that silently
//! collects zero is worse than a disabled one, so enabling requires the
//! account's tax `status == active` *and* at least one active
//! registration — checked against the provider, not assumed.
//!
//! We never advise: whether and where to register is a legal
//! determination for the tenant's tax advisor. Nothing here suggests
//! jurisdictions; the stored snapshot exists so a lapsed registration can
//! warn instead of silently under-collecting.

use chrono::{DateTime, Utc};
use sanvi_kernel::TenantId;

/// The provider's compliance snapshot for one connected account — what
/// [`crate::domain::PaymentProviderPort::fetch_tax_compliance`] returns
/// and what we store alongside our own toggle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaxComplianceSnapshot {
    /// The provider's tax-settings status (`active`, `pending`, …).
    pub status: String,
    /// How many *active* registrations the account holds.
    pub active_registrations: u32,
}

/// Why the preflight refused automatic tax. The reason is specific by
/// contract — "which step is missing", never a generic failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaxPreflightRejection {
    /// The connected account's tax settings are not `active`.
    StatusNotActive { status: String },
    /// Tax settings are active but no registration exists (or none is
    /// active): with zero registrations Stripe would collect nothing,
    /// silently.
    NoActiveRegistration,
}

impl TaxPreflightRejection {
    /// The stable machine reason the API surfaces; the UI renders it.
    pub fn code(&self) -> &'static str {
        match self {
            Self::StatusNotActive { .. } => "tax_status_not_active",
            Self::NoActiveRegistration => "no_active_registration",
        }
    }
}

impl TaxComplianceSnapshot {
    /// The preflight: both steps must hold before `automatic_tax` may be
    /// enabled on this account's sessions.
    pub fn preflight(&self) -> Result<(), TaxPreflightRejection> {
        if self.status != "active" {
            return Err(TaxPreflightRejection::StatusNotActive {
                status: self.status.clone(),
            });
        }
        if self.active_registrations == 0 {
            return Err(TaxPreflightRejection::NoActiveRegistration);
        }
        Ok(())
    }

    /// True when the tenant has automatic tax on but compliance has since
    /// lapsed — the scheduled re-check's warning condition.
    pub fn lapsed(&self) -> bool {
        self.status != "active" || self.active_registrations == 0
    }
}

/// A stored warning about lapsed compliance, rendered by the tenant UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaxWarning {
    /// Stable machine key (`payments.tax.warning.*` vocabulary).
    pub code: String,
    pub detail: String,
    pub detected_at: DateTime<Utc>,
}

/// The per-tenant tax settings record: our toggle plus the last observed
/// provider snapshot. One row per tenant — liability always routes to the
/// tenant's single live connection's account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaxSettingsRecord {
    pub tenant_id: TenantId,
    /// Whether checkout sessions carry
    /// `automatic_tax[enabled]=true` + the account liability. Flips on
    /// only through a passed preflight; flips off any time (with the
    /// documented caveat that mid-period changes alter what customers are
    /// charged — a tenant-facing change requiring notice, never a silent
    /// flag flip).
    pub enabled: bool,
    /// The connected-account id the liability names. Refreshed whenever
    /// compliance is re-checked.
    pub liability_account: String,
    pub provider_status: String,
    pub active_registrations: u32,
    pub warning: Option<TaxWarning>,
    pub last_checked_at: Option<DateTime<Utc>>,
    pub updated_at: Option<DateTime<Utc>>,
}

impl TaxSettingsRecord {
    /// A fresh, disabled record for a tenant.
    pub fn disabled(tenant_id: TenantId) -> Self {
        Self {
            tenant_id,
            enabled: false,
            liability_account: String::new(),
            provider_status: "unknown".to_string(),
            active_registrations: 0,
            warning: None,
            last_checked_at: None,
            updated_at: None,
        }
    }

    /// Fold a fresh compliance observation into the record. Compliance
    /// alone never flips the toggle: an enabled-but-lapsed record stays
    /// enabled but carries the warning (the tenant turns it off, or fixes
    /// the registration — silently disabling tax under them would change
    /// what customers are charged without notice).
    pub fn observe(&mut self, account: &str, snapshot: &TaxComplianceSnapshot, now: DateTime<Utc>) {
        self.liability_account = account.to_string();
        self.provider_status = snapshot.status.clone();
        self.active_registrations = snapshot.active_registrations;
        self.last_checked_at = Some(now);
        if self.enabled && snapshot.lapsed() {
            let (code, detail) = if snapshot.status != "active" {
                (
                    "payments.tax.warning.status_not_active",
                    format!(
                        "the connected account's tax status is {:?}",
                        snapshot.status
                    ),
                )
            } else {
                (
                    "payments.tax.warning.no_active_registration",
                    "no active tax registration remains on the connected account".to_string(),
                )
            };
            // Keep the earliest detection of an ongoing lapse stable so
            // repeated passes do not look like fresh incidents.
            if self.warning.as_ref().map(|w| w.code.as_str()) != Some(code) {
                self.warning = Some(TaxWarning {
                    code: code.to_string(),
                    detail,
                    detected_at: now,
                });
            }
        } else {
            self.warning = None;
        }
        self.updated_at = Some(now);
    }

    /// Enable after a passed preflight.
    pub fn enable(&mut self, account: &str, snapshot: &TaxComplianceSnapshot) {
        self.observe(account, snapshot, Utc::now());
        self.enabled = true;
    }

    /// Disable (tenant-initiated). Clears any stale warning.
    pub fn disable(&mut self) {
        self.enabled = false;
        self.warning = None;
        self.updated_at = Some(Utc::now());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn compliant() -> TaxComplianceSnapshot {
        TaxComplianceSnapshot {
            status: "active".to_string(),
            active_registrations: 2,
        }
    }

    #[test]
    fn the_preflight_names_the_exact_missing_step() {
        // Both steps present: preflight passes.
        assert_eq!(compliant().preflight(), Ok(()));

        // Settings pending → that exact step is named…
        let pending = TaxComplianceSnapshot {
            status: "pending".to_string(),
            active_registrations: 3,
        };
        assert_eq!(
            pending.preflight().unwrap_err().code(),
            "tax_status_not_active"
        );

        // …and zero registrations is its own distinct reason even when
        // settings are active — the silent-under-collection case.
        let unregistered = TaxComplianceSnapshot {
            active_registrations: 0,
            ..compliant()
        };
        assert_eq!(
            unregistered.preflight().unwrap_err().code(),
            "no_active_registration"
        );
    }

    #[test]
    fn observing_a_lapse_warns_without_flipping_the_toggle() {
        let mut record = TaxSettingsRecord::disabled(TenantId::try_from(100).unwrap());
        record.enable("acct_test_1", &compliant());
        assert!(record.enabled);

        let now = Utc::now();
        let lapsed = TaxComplianceSnapshot {
            active_registrations: 0,
            ..compliant()
        };
        record.observe("acct_test_1", &lapsed, now);

        assert!(record.enabled, "compliance drift never silently disables");
        assert!(record.warning.is_some());
        assert!(lapsed.lapsed());
    }

    #[test]
    fn a_repeated_pass_does_not_reset_the_warning_clock() {
        let mut record = TaxSettingsRecord::disabled(TenantId::try_from(100).unwrap());
        record.enable("acct_test_1", &compliant());
        let first_seen = Utc::now();
        record.observe(
            "acct_test_1",
            &TaxComplianceSnapshot {
                active_registrations: 0,
                ..compliant()
            },
            first_seen,
        );
        let detected = record.warning.as_ref().unwrap().detected_at;

        record.observe(
            "acct_test_1",
            &TaxComplianceSnapshot {
                active_registrations: 0,
                ..compliant()
            },
            first_seen + chrono::Duration::hours(1),
        );
        assert_eq!(
            record.warning.as_ref().unwrap().detected_at,
            detected,
            "an ongoing lapse keeps its original detection time"
        );
    }

    #[test]
    fn recovery_clears_the_warning_and_disable_is_immediate() {
        let mut record = TaxSettingsRecord::disabled(TenantId::try_from(100).unwrap());
        record.enable("acct_test_1", &compliant());
        record.observe(
            "acct_test_1",
            &TaxComplianceSnapshot {
                active_registrations: 0,
                ..compliant()
            },
            Utc::now(),
        );
        assert!(record.warning.is_some());

        record.observe("acct_test_1", &compliant(), Utc::now());
        assert!(record.warning.is_none());

        record.disable();
        assert!(!record.enabled);
        assert!(record.warning.is_none());
    }
}
