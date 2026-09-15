//! The feature catalog: every optional capability the platform can gate
//! behind an entitlement. Feature *keys* follow `<context>.<capability>`
//! (lowercase, `[a-z0-9_]`, 2–4 segments); entitlements attach per tenant
//! on top of these defaults.

use std::fmt;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::domain::AccessError;

/// A feature key, e.g. `advertising.google_ads`, `domains.custom`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
pub struct FeatureKey(String);

impl FeatureKey {
    pub fn parse(raw: &str) -> Result<Self, AccessError> {
        let key = raw.trim().to_ascii_lowercase();
        let mut valid = !key.is_empty();
        let segments = key.split('.');
        let mut count = 0;
        for segment in segments {
            count += 1;
            if segment.is_empty()
                || !segment
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
            {
                valid = false;
            }
        }
        if !(2..=4).contains(&count) {
            valid = false;
        }
        if !valid {
            return Err(AccessError::InvalidFeatureKey(raw.to_string()));
        }
        Ok(Self(key))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for FeatureKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// What an entitlement buys.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum FeatureKind {
    /// On/off capability (defaults may be enabled or disabled).
    Boolean,
    /// A metered capability: enabled with an optional per-period limit.
    Quota,
}

impl FeatureKind {
    pub fn parse(raw: &str) -> Result<Self, AccessError> {
        match raw {
            "boolean" => Ok(Self::Boolean),
            "quota" => Ok(Self::Quota),
            other => Err(AccessError::InvalidFeatureDefinition(format!(
                "unknown feature kind: {other}"
            ))),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Boolean => "boolean",
            Self::Quota => "quota",
        }
    }
}

/// Whether the feature shows up in tenant-facing catalogs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum Visibility {
    Public,
    Hidden,
}

impl Visibility {
    pub fn parse(raw: &str) -> Result<Self, AccessError> {
        match raw {
            "public" => Ok(Self::Public),
            "hidden" => Ok(Self::Hidden),
            other => Err(AccessError::InvalidFeatureDefinition(format!(
                "unknown visibility: {other}"
            ))),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Public => "public",
            Self::Hidden => "hidden",
        }
    }
}

/// One catalog entry: key, kind and the defaults a tenant inherits when no
/// entitlement row overrides them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct Feature {
    key: FeatureKey,
    name: String,
    kind: FeatureKind,
    default_enabled: bool,
    default_limit: Option<i64>,
    visibility: Visibility,
    deprecated_at: Option<DateTime<Utc>>,
}

impl Feature {
    pub fn new(
        key: FeatureKey,
        name: String,
        kind: FeatureKind,
        default_enabled: bool,
        default_limit: Option<i64>,
        visibility: Visibility,
    ) -> Result<Self, AccessError> {
        if name.trim().is_empty() {
            return Err(AccessError::InvalidFeatureDefinition(
                "feature name must not be empty".to_string(),
            ));
        }
        match kind {
            FeatureKind::Boolean if default_limit.is_some() => {
                return Err(AccessError::InvalidFeatureDefinition(
                    "boolean features cannot carry a default limit".to_string(),
                ));
            }
            FeatureKind::Boolean | FeatureKind::Quota => {}
        }
        if let Some(limit) = default_limit
            && limit < 0
        {
            return Err(AccessError::InvalidLimit(limit.to_string()));
        }
        Ok(Self {
            key,
            name,
            kind,
            default_enabled,
            default_limit,
            visibility,
            deprecated_at: None,
        })
    }

    /// Re-hydration constructor (no validation; the verifier re-checks).
    #[allow(clippy::too_many_arguments)]
    pub fn from_parts(
        key: FeatureKey,
        name: String,
        kind: FeatureKind,
        default_enabled: bool,
        default_limit: Option<i64>,
        visibility: Visibility,
        deprecated_at: Option<DateTime<Utc>>,
    ) -> Self {
        Self {
            key,
            name,
            kind,
            default_enabled,
            default_limit,
            visibility,
            deprecated_at,
        }
    }

    pub fn key(&self) -> &FeatureKey {
        &self.key
    }
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn kind(&self) -> FeatureKind {
        self.kind
    }
    pub fn default_enabled(&self) -> bool {
        self.default_enabled
    }
    pub fn default_limit(&self) -> Option<i64> {
        self.default_limit
    }
    pub fn visibility(&self) -> Visibility {
        self.visibility
    }
    pub fn deprecated_at(&self) -> Option<DateTime<Utc>> {
        self.deprecated_at
    }
    pub fn is_deprecated(&self) -> bool {
        self.deprecated_at.is_some()
    }

    /// Change the defaults (name/kind are immutable once defined).
    pub fn update_defaults(
        &mut self,
        name: String,
        default_enabled: bool,
        default_limit: Option<i64>,
        visibility: Visibility,
    ) -> Result<(), AccessError> {
        *self = Self::new(
            self.key.clone(),
            name,
            self.kind,
            default_enabled,
            default_limit,
            visibility,
        )?;
        Ok(())
    }

    /// Mark deprecated (removed from tenant-facing catalogs; resolution
    /// keeps serving existing entitlements).
    pub fn deprecate(&mut self, now: DateTime<Utc>) -> Result<(), AccessError> {
        if self.deprecated_at.is_some() {
            return Err(AccessError::InvalidFeatureDefinition(format!(
                "feature {} is already deprecated",
                self.key
            )));
        }
        self.deprecated_at = Some(now);
        Ok(())
    }
}

/// One seed entry of the compile-time catalog. Seeded idempotently by
/// `apps/cli seed features`; operators extend it at runtime via the API.
pub struct FeatureSeed {
    pub key: &'static str,
    pub name: &'static str,
    pub kind: FeatureKind,
    pub default_enabled: bool,
    pub default_limit: Option<i64>,
    pub visibility: Visibility,
}

/// The initial catalog: the keys phases 06–10 will gate behind
/// entitlements, declared now so contracts and seeds are stable. Keys
/// referenced by code must exist here (CI test), and operators extend the
/// catalog through the API from phase 03 onward.
pub const FEATURE_SEEDS: &[FeatureSeed] = &[
    // --- phase 03 itself ------------------------------------------------------
    FeatureSeed {
        key: "access.tenant_entitlements",
        name: "Tenant Entitlements API",
        kind: FeatureKind::Boolean,
        default_enabled: true,
        default_limit: None,
        visibility: Visibility::Public,
    },
    // --- tenancy quotas ------------------------------------------------------
    FeatureSeed {
        key: "tenancy.member_limit",
        name: "Member Limit",
        kind: FeatureKind::Quota,
        default_enabled: true,
        default_limit: None,
        visibility: Visibility::Public,
    },
    // --- localization (06) ----------------------------------------------------
    FeatureSeed {
        key: "localization.custom_locales",
        name: "Custom Locales",
        kind: FeatureKind::Boolean,
        default_enabled: false,
        default_limit: None,
        visibility: Visibility::Public,
    },
    // --- theming (07) ---------------------------------------------------------
    FeatureSeed {
        key: "theming.custom_css",
        name: "Custom CSS",
        kind: FeatureKind::Boolean,
        default_enabled: false,
        default_limit: None,
        visibility: Visibility::Public,
    },
    FeatureSeed {
        key: "theming.custom_templates",
        name: "Custom Templates",
        kind: FeatureKind::Boolean,
        default_enabled: false,
        default_limit: None,
        visibility: Visibility::Public,
    },
    FeatureSeed {
        key: "theming.premium_themes",
        name: "Premium Themes",
        kind: FeatureKind::Boolean,
        default_enabled: false,
        default_limit: None,
        visibility: Visibility::Public,
    },
    // --- domains (08) ---------------------------------------------------------
    FeatureSeed {
        key: "domains.custom",
        name: "Custom Domains",
        kind: FeatureKind::Boolean,
        default_enabled: false,
        default_limit: None,
        visibility: Visibility::Public,
    },
    FeatureSeed {
        key: "domains.purchase",
        name: "Domain Purchase",
        kind: FeatureKind::Boolean,
        default_enabled: false,
        default_limit: None,
        visibility: Visibility::Public,
    },
    // --- tenant payments (09) -------------------------------------------------
    // GA from 0.10.0 (09.7): on by default for new tenants. The
    // entitlement still gates every route, so a tenant that must not take
    // payments can have it revoked individually; the default is the
    // release's statement that payments are part of the product.
    FeatureSeed {
        key: "payments.stripe_connect",
        name: "Stripe Connect",
        kind: FeatureKind::Boolean,
        default_enabled: true,
        default_limit: None,
        visibility: Visibility::Public,
    },
    // --- advertising (10) -----------------------------------------------------
    FeatureSeed {
        key: "advertising.google_ads",
        name: "Google Ads",
        kind: FeatureKind::Boolean,
        default_enabled: false,
        default_limit: None,
        visibility: Visibility::Public,
    },
    FeatureSeed {
        key: "advertising.meta_ads",
        name: "Meta Ads",
        kind: FeatureKind::Boolean,
        default_enabled: false,
        default_limit: None,
        visibility: Visibility::Public,
    },
    FeatureSeed {
        key: "advertising.budget_guardrails",
        name: "Budget Guardrails",
        kind: FeatureKind::Boolean,
        default_enabled: false,
        default_limit: None,
        visibility: Visibility::Public,
    },
    FeatureSeed {
        key: "advertising.campaign_limit",
        name: "Campaign Limit",
        kind: FeatureKind::Quota,
        default_enabled: true,
        default_limit: None,
        visibility: Visibility::Public,
    },
];

/// Validate the seeds: unique keys, parseable keys, and kind-consistent
/// defaults.
pub fn validate_seeds() -> Result<(), AccessError> {
    let mut seen = std::collections::BTreeSet::new();
    for seed in FEATURE_SEEDS {
        if !seen.insert(seed.key) {
            return Err(AccessError::InvalidFeatureDefinition(format!(
                "duplicate feature seed: {}",
                seed.key
            )));
        }
        FeatureKey::parse(seed.key)?;
        Feature::new(
            FeatureKey::parse(seed.key)?,
            seed.name.to_string(),
            seed.kind,
            seed.default_enabled,
            seed.default_limit,
            seed.visibility,
        )?;
    }
    Ok(())
}

/// Registry lookup by key.
pub fn seed_by_key(key: &str) -> Option<&'static FeatureSeed> {
    FEATURE_SEEDS.iter().find(|seed| seed.key == key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seeds_are_valid_and_unique() {
        validate_seeds().unwrap();
    }

    #[test]
    fn feature_key_parse_normalizes_and_enforces_the_grammar() {
        assert_eq!(
            FeatureKey::parse("Domains.Custom").unwrap().as_str(),
            "domains.custom"
        );
        assert_eq!(
            FeatureKey::parse("advertising.google_ads")
                .unwrap()
                .as_str(),
            "advertising.google_ads"
        );
        for bad in [
            "",
            "domains",
            "a.b.c.d.e",
            "domains.custom!",
            "domains..custom",
        ] {
            assert!(FeatureKey::parse(bad).is_err(), "should reject {bad:?}");
        }
    }

    #[test]
    fn boolean_features_cannot_carry_a_limit() {
        assert!(matches!(
            Feature::new(
                FeatureKey::parse("domains.custom").unwrap(),
                "Custom Domains".to_string(),
                FeatureKind::Boolean,
                true,
                Some(5),
                Visibility::Public,
            ),
            Err(AccessError::InvalidFeatureDefinition(_))
        ));
    }

    #[test]
    fn negative_limits_are_rejected() {
        assert!(matches!(
            Feature::new(
                FeatureKey::parse("tenancy.member_limit").unwrap(),
                "Member Limit".to_string(),
                FeatureKind::Quota,
                true,
                Some(-1),
                Visibility::Public,
            ),
            Err(AccessError::InvalidLimit(_))
        ));
    }

    #[test]
    fn deprecation_is_terminal() {
        let mut feature = Feature::new(
            FeatureKey::parse("domains.custom").unwrap(),
            "Custom Domains".to_string(),
            FeatureKind::Boolean,
            false,
            None,
            Visibility::Public,
        )
        .unwrap();
        feature.deprecate(Utc::now()).unwrap();
        assert!(feature.is_deprecated());
        assert!(feature.deprecate(Utc::now()).is_err());
    }

    #[test]
    fn update_defaults_respects_the_kind() {
        let mut feature = Feature::new(
            FeatureKey::parse("tenancy.member_limit").unwrap(),
            "Member Limit".to_string(),
            FeatureKind::Quota,
            true,
            None,
            Visibility::Public,
        )
        .unwrap();
        feature
            .update_defaults(
                "Member Limit".to_string(),
                true,
                Some(25),
                Visibility::Public,
            )
            .unwrap();
        assert_eq!(feature.default_limit(), Some(25));
        // A quota feature can never become a boolean-with-limit.
        assert!(
            feature
                .update_defaults(
                    "Member Limit".to_string(),
                    true,
                    Some(25),
                    Visibility::Public,
                )
                .is_ok()
        );
    }
}
