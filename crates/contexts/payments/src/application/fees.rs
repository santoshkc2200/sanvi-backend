//! The configured application-fee policy (09.6).
//!
//! Bridges the platform-admin fee configuration (data — disable per
//! tenant with no deploy) into checkout creation: one lookup per session
//! create, computed through [`crate::domain::fees`] so the rounding rule
//! ("half up on the fee, never on the total") is defined in exactly one
//! place.
//!
//! Disclosure is part of this slice's contract: a fee the merchant cannot
//! see is not acceptable, so the same configuration is surfaced to the
//! tenant through their tax-settings view and lands on each affected
//! payment via the provider-reported `application_fee_amount`.

use std::sync::Arc;

use uuid::Uuid;

use crate::domain::error::PaymentsError;
use crate::domain::fees::FeeConfiguration;
use crate::domain::money::Money;
use crate::domain::ports::ApplicationFeeStore;

/// Reads the platform-admin configuration per tenant at call time — no
/// caching layer sits between a fee change and its effect.
pub struct ConfiguredApplicationFeePolicy {
    store: Arc<dyn ApplicationFeeStore>,
}

impl ConfiguredApplicationFeePolicy {
    pub fn new(store: Arc<dyn ApplicationFeeStore>) -> Self {
        Self { store }
    }

    /// The pure computation, shared with tests: configuration + charge →
    /// fee money (or none).
    pub fn compute(config: &FeeConfiguration, amount: &Money) -> Option<Money> {
        config
            .compute(amount)
            .and_then(|minor| Money::new(minor, amount.currency.as_str()).ok())
    }
}

#[async_trait::async_trait]
impl super::ApplicationFeePolicy for ConfiguredApplicationFeePolicy {
    async fn application_fee(&self, tenant_id: Uuid, amount: &Money) -> Option<Money> {
        match self.store.configuration_for(tenant_id).await {
            // Absent row == disabled == the release default; that path is
            // indistinguishable from an explicitly disabled configuration
            // downstream.
            Ok(config) => config
                .as_ref()
                .and_then(|config| Self::compute(config, amount)),
            // A storage failure must never silently invent a fee. Fees are
            // off in this release, so degrading to "no fee" keeps the
            // release-default guarantee; the error is logged for support.
            Err(err) => {
                tracing::error!(
                    tenant_id = %tenant_id,
                    error = %err,
                    "application-fee lookup failed; proceeding with no fee"
                );
                None
            }
        }
    }
}

/// The read-side projection of a tenant's fee configuration for their own
/// settings page (the disclosure). Never includes who set it or why —
/// that is platform-admin audit data, not tenant-facing copy.
#[derive(Debug, Clone, PartialEq)]
pub struct TenantFeeDisclosure {
    pub enabled: bool,
    pub basis_points: Option<u32>,
    pub fixed_minor: Option<i64>,
    pub fixed_currency: Option<String>,
    pub minimum_minor: i64,
}

impl From<&FeeConfiguration> for TenantFeeDisclosure {
    fn from(config: &FeeConfiguration) -> Self {
        Self {
            enabled: true,
            basis_points: config.basis_points,
            fixed_minor: config.fixed_minor,
            fixed_currency: config.fixed_currency.clone(),
            minimum_minor: config.minimum_minor,
        }
    }
}

/// Look up a tenant's disclosure (`None` = fees off for them).
pub async fn disclose_fee_for(
    store: &dyn ApplicationFeeStore,
    tenant_id: Uuid,
) -> Result<Option<TenantFeeDisclosure>, PaymentsError> {
    Ok(store
        .configuration_for(tenant_id)
        .await?
        .as_ref()
        .map(TenantFeeDisclosure::from))
}
