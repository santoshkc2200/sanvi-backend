//! The provider registry: `ProviderKind → adapter`, assembled at startup.
//!
//! This is the seam the whole phase hinges on. The catalog endpoint reads
//! the registry, so **a provider appears in the settings page by being
//! registered, never by being hardcoded in a handler** — adding a provider
//! is one `register` call in the composition root plus an adapter, and
//! removing one is one deleted line. The 09.1 acceptance test registers
//! the fake provider and watches it appear; a production environment
//! cannot list it at all.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::domain::error::PaymentsError;
use crate::domain::ports::PaymentProviderPort;
use crate::domain::provider::ProviderKind;

/// The deployment environment, as configured process-wide
/// (`telemetry.environment`). Only the production/non-production
/// distinction matters here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeEnvironment {
    Production,
    NonProduction,
}

impl RuntimeEnvironment {
    pub fn from_config(raw: &str) -> Self {
        if raw.trim().eq_ignore_ascii_case("production") {
            Self::Production
        } else {
            Self::NonProduction
        }
    }
}

/// The registry. `Send + Sync` by construction (every adapter is), shared
/// as an `Arc` between the composition root, the catalog and the use
/// cases.
#[derive(Default)]
pub struct ProviderRegistry {
    providers: BTreeMap<ProviderKind, Arc<dyn PaymentProviderPort>>,
}

impl ProviderRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register one adapter. Registering the same kind twice is a
    /// wiring bug and fails loudly at startup, not silently at request
    /// time. Registration itself does **not** consult the environment —
    /// the composition root decides what to register; the catalog
    /// independently refuses to list non-production descriptors in
    /// production, so the two checks cannot both be forgotten.
    pub fn register(&mut self, adapter: Arc<dyn PaymentProviderPort>) -> Result<(), PaymentsError> {
        let kind = adapter.kind();
        if self.providers.insert(kind, adapter).is_some() {
            return Err(PaymentsError::InvalidConnection(format!(
                "provider {kind:?} is already registered"
            )));
        }
        Ok(())
    }

    pub fn get(&self, kind: ProviderKind) -> Option<Arc<dyn PaymentProviderPort>> {
        self.providers.get(&kind).cloned()
    }

    /// Every registered adapter, in stable (kind) order.
    pub fn adapters(&self) -> Vec<Arc<dyn PaymentProviderPort>> {
        self.providers.values().cloned().collect()
    }

    pub fn kinds(&self) -> Vec<ProviderKind> {
        self.providers.keys().copied().collect()
    }

    pub fn is_empty(&self) -> bool {
        self.providers.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // A minimal second adapter — the "adding a provider is one adapter"
    // proof, distinct from the fake provider so the test is honest about
    // the seam rather than reusing the deliverable.
    struct StubProvider {
        kind: ProviderKind,
    }

    #[async_trait::async_trait]
    impl PaymentProviderPort for StubProvider {
        fn kind(&self) -> ProviderKind {
            self.kind
        }
        fn descriptor(&self) -> crate::domain::ports::ProviderDescriptor {
            crate::domain::ports::ProviderDescriptor {
                kind: self.kind,
                display_name: "Stub".to_string(),
                supported_countries: vec![
                    crate::domain::provider::CountryCode::parse("US").unwrap(),
                ],
                requires_onboarding: false,
                production: true,
            }
        }
        async fn create_account(
            &self,
            _request: &crate::domain::ports::CreateAccountRequest,
        ) -> Result<crate::domain::ports::ProviderAccount, PaymentsError> {
            Err(PaymentsError::ProviderUnavailable("stub".into()))
        }
        async fn create_account_session(
            &self,
            _external_account_id: &str,
        ) -> Result<crate::domain::ports::ProviderAccountSession, PaymentsError> {
            Err(PaymentsError::ProviderUnavailable("stub".into()))
        }
        async fn fetch_status(
            &self,
            _external_account_id: &str,
        ) -> Result<crate::domain::ports::ProviderAccountStatus, PaymentsError> {
            Err(PaymentsError::ProviderUnavailable("stub".into()))
        }
        async fn sync(
            &self,
            _external_account_id: &str,
        ) -> Result<crate::domain::ports::ProviderAccountStatus, PaymentsError> {
            Err(PaymentsError::ProviderUnavailable("stub".into()))
        }
        async fn create_checkout(
            &self,
            _command: &crate::domain::ports::CreateCheckoutCommand,
        ) -> Result<crate::domain::ports::ProviderCheckoutSession, PaymentsError> {
            Err(PaymentsError::ProviderUnavailable("stub".into()))
        }
        async fn refund(
            &self,
            _command: &crate::domain::ports::ProviderRefundCommand,
        ) -> Result<crate::domain::ports::ProviderRefund, PaymentsError> {
            Err(PaymentsError::ProviderUnavailable("stub".into()))
        }
        async fn list_payouts(
            &self,
            _external_account_id: &str,
            _since: Option<chrono::DateTime<chrono::Utc>>,
        ) -> Result<Vec<crate::domain::ports::ProviderPayout>, PaymentsError> {
            Err(PaymentsError::ProviderUnavailable("stub".into()))
        }
        fn verify_webhook(
            &self,
            _payload: &[u8],
            _signature_header: &str,
        ) -> Result<(), PaymentsError> {
            Err(PaymentsError::WebhookRejected("stub".into()))
        }
    }

    #[test]
    fn registering_a_provider_makes_it_resolvable() {
        let mut registry = ProviderRegistry::new();
        assert!(registry.is_empty());
        assert_eq!(registry.kinds(), Vec::<ProviderKind>::new());

        registry
            .register(Arc::new(StubProvider {
                kind: ProviderKind::StripeConnect,
            }))
            .unwrap();
        assert_eq!(registry.kinds(), vec![ProviderKind::StripeConnect]);
        assert!(registry.get(ProviderKind::StripeConnect).is_some());
        assert!(registry.get(ProviderKind::Fake).is_none());

        // Duplicate registration is a wiring error.
        assert!(
            registry
                .register(Arc::new(StubProvider {
                    kind: ProviderKind::StripeConnect
                }))
                .is_err()
        );
    }

    #[test]
    fn the_environment_maps_from_config_strings() {
        assert_eq!(
            RuntimeEnvironment::from_config("production"),
            RuntimeEnvironment::Production
        );
        assert_eq!(
            RuntimeEnvironment::from_config("Production "),
            RuntimeEnvironment::Production
        );
        for other in ["local", "staging", "dev", ""] {
            assert_eq!(
                RuntimeEnvironment::from_config(other),
                RuntimeEnvironment::NonProduction
            );
        }
    }
}
