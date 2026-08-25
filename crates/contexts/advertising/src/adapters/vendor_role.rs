//! Maps an ad platform to its phase-05 sub-processor role
//! (`ServiceProvider` vs `ThirdParty`), which decides whether the
//! directive gate must also ask for `sale_or_share` /
//! `targeted_advertising` before uploading.
//!
//! The registry (`sanvi_privacy::domain::SubProcessorRepository`) is the
//! single source of truth for a vendor's role — advertising does not keep
//! its own copy of that legal classification. It changes rarely, so the
//! snapshot is loaded once at boot rather than queried per beacon hit,
//! which would otherwise put an audited platform-scope transaction on the
//! hot path of an unauthenticated public endpoint.

use std::collections::HashMap;

use async_trait::async_trait;
use sanvi_privacy::domain::SubProcessorRepository;
use sanvi_privacy::domain::SubProcessorRole as PrivacySubProcessorRole;

use crate::domain::{AdvertisingError, PlatformKey, VendorRole, VendorRolePort};

/// Sub-processor registry names for the two platforms this phase ships.
/// A platform with no matching registry entry defaults to `ThirdParty` —
/// the more protective classification — rather than silently skipping the
/// extra consent purposes.
fn registry_name_for(platform: &PlatformKey) -> Option<&'static str> {
    match platform.as_str() {
        "google_ads" => Some("Google Ads"),
        "meta" => Some("Meta"),
        _ => None,
    }
}

fn to_vendor_role(role: PrivacySubProcessorRole) -> VendorRole {
    match role {
        PrivacySubProcessorRole::ServiceProvider | PrivacySubProcessorRole::Processor => {
            VendorRole::ServiceProvider
        }
        PrivacySubProcessorRole::Contractor | PrivacySubProcessorRole::ThirdParty => {
            VendorRole::ThirdParty
        }
    }
}

pub struct SnapshotVendorRoleLookup {
    roles: HashMap<PlatformKey, VendorRole>,
}

impl SnapshotVendorRoleLookup {
    pub fn new(roles: HashMap<PlatformKey, VendorRole>) -> Self {
        Self { roles }
    }

    /// Builds the snapshot from the privacy context's live sub-processor
    /// registry. Call once at composition time.
    pub async fn load(
        subprocessors: &dyn SubProcessorRepository,
        scope: &mut sanvi_persistence::PlatformScope<'_>,
    ) -> Result<Self, AdvertisingError> {
        let entries = subprocessors
            .list_all(scope)
            .await
            .map_err(|err| AdvertisingError::Storage(err.to_string()))?;

        let mut roles = HashMap::new();
        for (key_str, registry_name) in [("google_ads", "Google Ads"), ("meta", "Meta")] {
            let key = PlatformKey::parse(key_str).expect("static platform key");
            if let Some(entry) = entries
                .iter()
                .find(|entry| entry.name == registry_name && entry.removed_at.is_none())
            {
                roles.insert(key, to_vendor_role(entry.role));
            } else {
                // No registry entry yet: default to the more protective
                // classification rather than assuming a service-provider
                // relationship that was never actually recorded.
                roles.insert(key, VendorRole::ThirdParty);
            }
        }
        Ok(Self { roles })
    }
}

#[async_trait]
impl VendorRolePort for SnapshotVendorRoleLookup {
    async fn lookup_role(&self, platform: &PlatformKey) -> Result<VendorRole, AdvertisingError> {
        if registry_name_for(platform).is_none() {
            return Ok(VendorRole::ThirdParty);
        }
        Ok(self
            .roles
            .get(platform)
            .copied()
            .unwrap_or(VendorRole::ThirdParty))
    }
}
