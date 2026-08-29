//! The platform's per-tenant application-fee configuration (09.6).
//!
//! A tenant cannot set the platform's own fee — this configuration lives
//! in the **platform-admin** context and its API, never the tenant API.
//! It is data: disabling per tenant needs no deploy, and 0.10.0 ships
//! with every tenant disabled (the platform monetises via subscription).
//!
//! The shape mirrors what checkout computes against: basis points and/or
//! a fixed amount in one named currency, with a minimum interpreted in
//! the charge currency's minor units (currency-aware by construction —
//! JPY minor units are whole yen). Rounding lives with the computation,
//! in the payments context; nothing here duplicates it.

use sanvi_kernel::TenantId;

use crate::domain::error::PlatformAdminError;

/// One tenant's stored fee configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplicationFeeConfig {
    pub tenant_id: TenantId,
    /// Basis points of the charge total (100 = 1%). Absent = no
    /// percentage component.
    pub basis_points: Option<u32>,
    /// Flat component, minor units of `fixed_currency`.
    pub fixed_amount_minor: Option<i64>,
    pub fixed_currency: Option<String>,
    /// Floor on the computed fee, minor units of the charge currency.
    pub minimum_minor: i64,
    /// The master switch. `false` rows are retained so re-enabling keeps
    /// the last-reviewed numbers (still an audited change).
    pub enabled: bool,
}

impl ApplicationFeeConfig {
    /// Validate the money shapes: at least one fee component when
    /// enabled, positive bounds everywhere, ISO-4217-shaped currency.
    pub fn validate(&self) -> Result<(), PlatformAdminError> {
        if let Some(bps) = self.basis_points
            && (bps == 0 || bps > 10_000)
        {
            return Err(PlatformAdminError::InvalidFeeConfig(
                "basis_points must be 1..=10000".to_string(),
            ));
        }
        if self.fixed_amount_minor.is_some() != self.fixed_currency.is_some() {
            return Err(PlatformAdminError::InvalidFeeConfig(
                "fixed_amount_minor and fixed_currency must be set together".to_string(),
            ));
        }
        if let Some(minor) = self.fixed_amount_minor
            && minor < 0
        {
            return Err(PlatformAdminError::InvalidFeeConfig(
                "fixed_amount_minor must be non-negative".to_string(),
            ));
        }
        if let Some(currency) = &self.fixed_currency
            && (currency.len() != 3 || !currency.chars().all(|c| c.is_ascii_uppercase()))
        {
            return Err(PlatformAdminError::InvalidFeeConfig(
                "fixed_currency must be an uppercase ISO-4217 code".to_string(),
            ));
        }
        if self.minimum_minor < 0 {
            return Err(PlatformAdminError::InvalidFeeConfig(
                "minimum_minor must be non-negative".to_string(),
            ));
        }
        if self.enabled && self.basis_points.is_none() && self.fixed_amount_minor.is_none() {
            return Err(PlatformAdminError::InvalidFeeConfig(
                "an enabled fee needs a basis-points or fixed component".to_string(),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid() -> ApplicationFeeConfig {
        ApplicationFeeConfig {
            tenant_id: TenantId::try_from(101_i64).unwrap(),
            basis_points: Some(250),
            fixed_amount_minor: None,
            fixed_currency: None,
            minimum_minor: 0,
            enabled: true,
        }
    }

    #[test]
    fn valid_configurations_pass() {
        assert!(valid().validate().is_ok());
        // Disabled rows may be empty placeholders.
        assert!(
            ApplicationFeeConfig {
                enabled: false,
                ..valid()
            }
            .validate()
            .is_ok()
        );
    }

    #[test]
    fn invalid_configurations_name_the_problem() {
        let mut config = valid();
        config.basis_points = Some(0);
        assert!(config.validate().is_err());

        let mut config = valid();
        config.basis_points = Some(10_001);
        assert!(config.validate().is_err());

        // Fixed amount without its currency is meaningless.
        let mut config = valid();
        config.fixed_amount_minor = Some(20);
        assert!(config.validate().is_err());

        // Enabled but component-less would silently charge nothing.
        let mut config = valid();
        config.basis_points = None;
        assert!(config.validate().is_err());
    }
}
