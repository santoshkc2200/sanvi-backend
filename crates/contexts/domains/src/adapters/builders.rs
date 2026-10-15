//! Shared builders for domains context adapters (API and worker).

use std::sync::Arc;

use sanvi_kernel::{CapabilityMode, RawIdGenerator};

use crate::adapters::edge_ca::EdgeCertificateAuthority;
use crate::adapters::stubs::{
    DisabledCertificateAuthority, DisabledPurchaseCharger, DisabledRegistrar,
};
use crate::adapters::{SandboxCertificateAuthority, SandboxPurchaseCharger, SandboxRegistrar};
use crate::config::DomainsConfig;
use crate::domain::{CertificateAuthorityPort, DomainRegistrarPort, DomainsError, PurchaseCharger};

/// Build the domain registrar adapter according to configured capability mode.
///
/// In `live` mode, a missing or unconstructable adapter is a hard startup failure.
pub fn build_domain_registrar(
    config: &DomainsConfig,
) -> Result<(Arc<dyn DomainRegistrarPort>, CapabilityMode), DomainsError> {
    let mode = CapabilityMode::parse(&config.registrar.mode).map_err(DomainsError::Registrar)?;
    match mode {
        CapabilityMode::Disabled => Ok((Arc::new(DisabledRegistrar), CapabilityMode::Disabled)),
        CapabilityMode::Sandbox => Ok((
            Arc::new(SandboxRegistrar::new(
                config.registrar.registrar_id.clone(),
                &config.registrar.tlds,
            )),
            CapabilityMode::Sandbox,
        )),
        CapabilityMode::Live => Err(DomainsError::Registrar(
            "live domain registrar adapter is not implemented; configure 'disabled' or 'sandbox'"
                .to_string(),
        )),
    }
}

/// Build the certificate authority adapter according to configured capability mode.
///
/// In `live` mode, a missing or unconstructable adapter is a hard startup failure.
pub fn build_certificate_authority(
    config: &DomainsConfig,
    ids: Arc<dyn RawIdGenerator>,
) -> Result<(Arc<dyn CertificateAuthorityPort>, CapabilityMode), DomainsError> {
    let mode =
        CapabilityMode::parse(&config.ca_mode).map_err(DomainsError::CertificateAuthority)?;
    match mode {
        CapabilityMode::Disabled => Ok((
            Arc::new(DisabledCertificateAuthority),
            CapabilityMode::Disabled,
        )),
        CapabilityMode::Sandbox => Ok((
            Arc::new(SandboxCertificateAuthority::new(
                config.cert_lifetime_days,
                ids,
            )),
            CapabilityMode::Sandbox,
        )),
        CapabilityMode::Live => Ok((
            Arc::new(EdgeCertificateAuthority::new(config, ids)?),
            CapabilityMode::Live,
        )),
    }
}

/// Build the purchase charger adapter according to configured capability mode.
///
/// In `live` mode, a missing or unconstructable adapter is a hard startup failure.
pub fn build_purchase_charger(
    config: &DomainsConfig,
) -> Result<(Arc<dyn PurchaseCharger>, CapabilityMode), DomainsError> {
    let mode = CapabilityMode::parse(&config.charger_mode).map_err(DomainsError::Charge)?;
    match mode {
        CapabilityMode::Disabled => {
            Ok((Arc::new(DisabledPurchaseCharger), CapabilityMode::Disabled))
        }
        CapabilityMode::Sandbox => Ok((Arc::new(SandboxPurchaseCharger), CapabilityMode::Sandbox)),
        CapabilityMode::Live => Err(DomainsError::Charge(
            "live purchase charger adapter is not implemented; configure 'disabled' or 'sandbox'"
                .to_string(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registrar_builder_modes() {
        let mut config = DomainsConfig::default();
        config.registrar.mode = "sandbox".to_string();
        let (registrar, mode) = build_domain_registrar(&config).unwrap();
        assert_eq!(mode, CapabilityMode::Sandbox);
        assert_eq!(registrar.registrar_id(), "sandbox");

        config.registrar.mode = "disabled".to_string();
        let (registrar, mode) = build_domain_registrar(&config).unwrap();
        assert_eq!(mode, CapabilityMode::Disabled);
        assert_eq!(registrar.registrar_id(), "disabled");

        config.registrar.mode = "live".to_string();
        assert!(build_domain_registrar(&config).is_err());
    }

    #[test]
    fn ca_builder_modes() {
        let ids = sanvi_testsupport::test_ids();
        let mut config = DomainsConfig {
            ca_mode: "sandbox".to_string(),
            ..Default::default()
        };
        let (_ca, mode) = build_certificate_authority(&config, ids.clone()).unwrap();
        assert_eq!(mode, CapabilityMode::Sandbox);

        config.ca_mode = "disabled".to_string();
        let (_ca, mode) = build_certificate_authority(&config, ids.clone()).unwrap();
        assert_eq!(mode, CapabilityMode::Disabled);

        config.ca_mode = "live".to_string();
        config.anycast_ips = vec!["8.8.8.8".to_string()];
        let (_, mode) = build_certificate_authority(&config, ids).unwrap();
        assert_eq!(mode, CapabilityMode::Live);
    }

    #[test]
    fn charger_builder_modes() {
        let mut config = DomainsConfig {
            charger_mode: "sandbox".to_string(),
            ..Default::default()
        };
        let (_charger, mode) = build_purchase_charger(&config).unwrap();
        assert_eq!(mode, CapabilityMode::Sandbox);

        config.charger_mode = "disabled".to_string();
        let (_charger, mode) = build_purchase_charger(&config).unwrap();
        assert_eq!(mode, CapabilityMode::Disabled);

        config.charger_mode = "live".to_string();
        assert!(build_purchase_charger(&config).is_err());
    }
}
