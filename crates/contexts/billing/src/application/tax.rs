//! The Stripe Tax ops check: the loud warning behind the most common Stripe
//! Tax mistake. Enabling `automatic_tax` without an active registration
//! silently collects nothing — this check surfaces that state at startup,
//! in the scheduler and from the CLI.

use std::sync::Arc;

use tracing::{error, info};

use crate::domain::{BillingError, StripeClient, TaxRegistration};

pub struct CheckTaxRegistrations {
    stripe: Arc<dyn StripeClient>,
    automatic_tax_enabled: bool,
}

impl CheckTaxRegistrations {
    pub fn new(stripe: Arc<dyn StripeClient>, automatic_tax_enabled: bool) -> Self {
        Self {
            stripe,
            automatic_tax_enabled,
        }
    }

    /// Returns the active registrations. When `automatic_tax` is enabled but
    /// no registration is active, fails loudly (and logs at error level, so
    /// the alerting rules fire) — never silently.
    #[tracing::instrument(name = "billing.check_tax_registrations", skip_all)]
    pub async fn execute(&self) -> Result<Vec<TaxRegistration>, BillingError> {
        let registrations = self.stripe.list_active_tax_registrations().await?;
        if self.automatic_tax_enabled && registrations.is_empty() {
            error!(
                "automatic_tax is enabled but no active Stripe Tax registration exists — no tax is being collected"
            );
            return Err(BillingError::TaxRegistrationMissing);
        }
        info!(
            registrations = registrations.len(),
            automatic_tax = self.automatic_tax_enabled,
            "stripe tax registrations checked",
        );
        Ok(registrations)
    }
}
