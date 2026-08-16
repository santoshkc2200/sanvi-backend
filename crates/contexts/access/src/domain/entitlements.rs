//! Entitlements: `(tenant, feature) -> { enabled, limit, source,
//! expires_at }`. Resolution is a pure function over the four sources with
//! fixed precedence: `override > subscription > plan_default >
//! feature_default` — so support can grant a trial without touching billing,
//! and billing can undercut a feature default without touching operators.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::domain::AccessError;
use crate::domain::features::{Feature, FeatureKey, FeatureKind};

/// Where an entitlement comes from — and therefore its precedence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum EntitlementSource {
    /// Set by a platform operator (support trials, manual grants).
    Override,
    /// Derived from a paid subscription (arrives with phase 04).
    Subscription,
    /// A plan's default for the tenant (arrives with phase 04).
    PlanDefault,
    /// The feature catalog's global default (synthetic; never stored).
    FeatureDefault,
}

impl EntitlementSource {
    pub const PRECEDENCE: [Self; 4] = [
        Self::Override,
        Self::Subscription,
        Self::PlanDefault,
        Self::FeatureDefault,
    ];

    pub fn parse(raw: &str) -> Result<Self, AccessError> {
        match raw {
            "override" => Ok(Self::Override),
            "subscription" => Ok(Self::Subscription),
            "plan_default" => Ok(Self::PlanDefault),
            "feature_default" => Ok(Self::FeatureDefault),
            other => Err(AccessError::InvalidEntitlement(format!(
                "unknown entitlement source: {other}"
            ))),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Override => "override",
            Self::Subscription => "subscription",
            Self::PlanDefault => "plan_default",
            Self::FeatureDefault => "feature_default",
        }
    }
}

/// A stored entitlement row for one tenant + feature + source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct Entitlement {
    tenant_id: Uuid,
    feature: FeatureKey,
    enabled: bool,
    limit: Option<i64>,
    source: EntitlementSource,
    expires_at: Option<DateTime<Utc>>,
    granted_by: Option<Uuid>,
    granted_reason: Option<String>,
    updated_at: DateTime<Utc>,
}

impl Entitlement {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        tenant_id: Uuid,
        feature: FeatureKey,
        enabled: bool,
        limit: Option<i64>,
        source: EntitlementSource,
        expires_at: Option<DateTime<Utc>>,
        granted_by: Option<Uuid>,
        granted_reason: Option<String>,
        now: DateTime<Utc>,
    ) -> Result<Self, AccessError> {
        if let Some(limit) = limit
            && limit < 0
        {
            return Err(AccessError::InvalidLimit(limit.to_string()));
        }
        if let Some(expires_at) = expires_at
            && expires_at <= now
        {
            return Err(AccessError::InvalidEntitlement(
                "expires_at must be in the future".to_string(),
            ));
        }
        Ok(Self {
            tenant_id,
            feature,
            enabled,
            limit,
            source,
            expires_at,
            granted_by,
            granted_reason,
            updated_at: now,
        })
    }

    /// Re-hydration constructor (no validation).
    #[allow(clippy::too_many_arguments)]
    pub fn from_parts(
        tenant_id: Uuid,
        feature: FeatureKey,
        enabled: bool,
        limit: Option<i64>,
        source: EntitlementSource,
        expires_at: Option<DateTime<Utc>>,
        granted_by: Option<Uuid>,
        granted_reason: Option<String>,
        updated_at: DateTime<Utc>,
    ) -> Self {
        Self {
            tenant_id,
            feature,
            enabled,
            limit,
            source,
            expires_at,
            granted_by,
            granted_reason,
            updated_at,
        }
    }

    pub fn tenant_id(&self) -> Uuid {
        self.tenant_id
    }
    pub fn feature(&self) -> &FeatureKey {
        &self.feature
    }
    pub fn enabled(&self) -> bool {
        self.enabled
    }
    pub fn limit(&self) -> Option<i64> {
        self.limit
    }
    pub fn source(&self) -> EntitlementSource {
        self.source
    }
    pub fn expires_at(&self) -> Option<DateTime<Utc>> {
        self.expires_at
    }
    pub fn granted_by(&self) -> Option<Uuid> {
        self.granted_by
    }
    pub fn granted_reason(&self) -> Option<&str> {
        self.granted_reason.as_deref()
    }
    pub fn updated_at(&self) -> DateTime<Utc> {
        self.updated_at
    }

    /// True when this row no longer contributes to resolution.
    pub fn is_expired(&self, now: DateTime<Utc>) -> bool {
        self.expires_at.is_some_and(|expires_at| expires_at <= now)
    }
}

/// The resolved answer for one feature: which source won, and what it says.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ResolvedEntitlement {
    pub feature: FeatureKey,
    pub kind: FeatureKind,
    pub enabled: bool,
    pub limit: Option<i64>,
    pub source: EntitlementSource,
    pub expires_at: Option<DateTime<Utc>>,
}

/// Resolve one feature from the catalog entry and the tenant's rows.
/// Expired rows are ignored; rows for other features are ignored; among the
/// surviving rows, the highest precedence source wins (latest `updated_at`
/// within a source).
pub fn resolve_entitlement(
    feature: &Feature,
    rows: &[Entitlement],
    now: DateTime<Utc>,
) -> ResolvedEntitlement {
    let mut best: Option<&Entitlement> = None;
    for source in EntitlementSource::PRECEDENCE {
        let candidate = rows
            .iter()
            .filter(|row| {
                row.feature() == feature.key() && row.source() == source && !row.is_expired(now)
            })
            .max_by_key(|row| row.updated_at());
        if candidate.is_some() {
            best = candidate;
            break;
        }
    }
    match best {
        Some(row) => ResolvedEntitlement {
            feature: feature.key().clone(),
            kind: feature.kind(),
            enabled: row.enabled(),
            limit: row.limit(),
            source: row.source(),
            expires_at: row.expires_at(),
        },
        None => ResolvedEntitlement {
            feature: feature.key().clone(),
            kind: feature.kind(),
            enabled: feature.default_enabled(),
            limit: feature.default_limit(),
            source: EntitlementSource::FeatureDefault,
            expires_at: None,
        },
    }
}

/// Resolve the whole catalog for one tenant (rows are assumed to already
/// be filtered to that tenant).
pub fn resolve_all(
    features: &[Feature],
    rows: &[Entitlement],
    now: DateTime<Utc>,
) -> Vec<ResolvedEntitlement> {
    features
        .iter()
        .map(|feature| resolve_entitlement(feature, rows, now))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::features::Visibility;

    fn feature(
        key: &str,
        kind: FeatureKind,
        default_enabled: bool,
        default_limit: Option<i64>,
    ) -> Feature {
        Feature::new(
            FeatureKey::parse(key).unwrap(),
            key.to_string(),
            kind,
            default_enabled,
            default_limit,
            Visibility::Public,
        )
        .unwrap()
    }

    fn row(
        feature: &str,
        source: EntitlementSource,
        enabled: bool,
        limit: Option<i64>,
        updated_at: DateTime<Utc>,
        expires_at: Option<DateTime<Utc>>,
    ) -> Entitlement {
        Entitlement::from_parts(
            Uuid::now_v7(),
            FeatureKey::parse(feature).unwrap(),
            enabled,
            limit,
            source,
            expires_at,
            None,
            Some("test".to_string()),
            updated_at,
        )
    }

    #[test]
    fn default_applies_when_no_rows_exist() {
        let f = feature("advertising.google_ads", FeatureKind::Boolean, false, None);
        let resolved = resolve_entitlement(&f, &[], Utc::now());
        assert_eq!(resolved.source, EntitlementSource::FeatureDefault);
        assert!(!resolved.enabled);
        assert_eq!(resolved.limit, None);
    }

    #[test]
    fn override_beats_everything() {
        let now = Utc::now();
        let f = feature("advertising.google_ads", FeatureKind::Boolean, false, None);
        let rows = vec![
            row(
                "advertising.google_ads",
                EntitlementSource::PlanDefault,
                true,
                None,
                now,
                None,
            ),
            row(
                "advertising.google_ads",
                EntitlementSource::Override,
                false,
                None,
                now,
                None,
            ),
        ];
        let resolved = resolve_entitlement(&f, &rows, now);
        assert_eq!(resolved.source, EntitlementSource::Override);
        assert!(!resolved.enabled);
    }

    #[test]
    fn subscription_beats_plan_default() {
        let now = Utc::now();
        let f = feature("advertising.google_ads", FeatureKind::Boolean, false, None);
        let rows = vec![
            row(
                "advertising.google_ads",
                EntitlementSource::PlanDefault,
                false,
                None,
                now,
                None,
            ),
            row(
                "advertising.google_ads",
                EntitlementSource::Subscription,
                true,
                None,
                now,
                None,
            ),
        ];
        let resolved = resolve_entitlement(&f, &rows, now);
        assert_eq!(resolved.source, EntitlementSource::Subscription);
        assert!(resolved.enabled);
    }

    #[test]
    fn plan_default_beats_feature_default() {
        let now = Utc::now();
        let f = feature("advertising.google_ads", FeatureKind::Boolean, true, None);
        let rows = vec![row(
            "advertising.google_ads",
            EntitlementSource::PlanDefault,
            false,
            None,
            now,
            None,
        )];
        let resolved = resolve_entitlement(&f, &rows, now);
        assert_eq!(resolved.source, EntitlementSource::PlanDefault);
        assert!(!resolved.enabled);
    }

    #[test]
    fn expired_rows_fall_through_to_the_next_source() {
        let now = Utc::now();
        let f = feature("advertising.google_ads", FeatureKind::Boolean, false, None);
        let rows = vec![
            row(
                "advertising.google_ads",
                EntitlementSource::Override,
                true,
                None,
                now,
                Some(now - chrono::Duration::seconds(1)),
            ),
            row(
                "advertising.google_ads",
                EntitlementSource::PlanDefault,
                true,
                None,
                now,
                None,
            ),
        ];
        let resolved = resolve_entitlement(&f, &rows, now);
        assert_eq!(resolved.source, EntitlementSource::PlanDefault);
        assert!(resolved.enabled);
    }

    #[test]
    fn latest_row_wins_within_a_source() {
        let now = Utc::now();
        let later = now + chrono::Duration::seconds(10);
        let f = feature("tenancy.member_limit", FeatureKind::Quota, true, Some(5));
        let rows = vec![
            row(
                "tenancy.member_limit",
                EntitlementSource::Override,
                true,
                Some(10),
                now,
                None,
            ),
            row(
                "tenancy.member_limit",
                EntitlementSource::Override,
                true,
                Some(25),
                later,
                None,
            ),
        ];
        let resolved = resolve_entitlement(&f, &rows, now);
        assert_eq!(resolved.limit, Some(25));
        assert_eq!(resolved.kind, FeatureKind::Quota);
    }

    #[test]
    fn precedence_table_exhaustive_shape() {
        // All four sources declared, each with enabled/limit variants; the
        // table must pick the first non-expired source in precedence order.
        let now = Utc::now();
        let f = feature("tenancy.member_limit", FeatureKind::Quota, false, None);
        for (expected_source, rows) in [
            (
                EntitlementSource::Override,
                vec![row(
                    "tenancy.member_limit",
                    EntitlementSource::Override,
                    true,
                    Some(1),
                    now,
                    None,
                )],
            ),
            (
                EntitlementSource::Subscription,
                vec![row(
                    "tenancy.member_limit",
                    EntitlementSource::Subscription,
                    false,
                    Some(2),
                    now,
                    None,
                )],
            ),
            (
                EntitlementSource::PlanDefault,
                vec![row(
                    "tenancy.member_limit",
                    EntitlementSource::PlanDefault,
                    true,
                    Some(3),
                    now,
                    None,
                )],
            ),
            (EntitlementSource::FeatureDefault, vec![]),
        ] {
            let resolved = resolve_entitlement(&f, &rows, now);
            assert_eq!(resolved.source, expected_source);
        }
    }

    #[test]
    fn resolve_all_covers_every_catalog_feature() {
        let now = Utc::now();
        let features = vec![
            feature("domains.custom", FeatureKind::Boolean, false, None),
            feature("tenancy.member_limit", FeatureKind::Quota, true, None),
        ];
        let rows = vec![row(
            "advertising.google_ads",
            EntitlementSource::Override,
            true,
            None,
            now,
            None,
        )];
        let resolved = resolve_all(&features, &rows, now);
        assert_eq!(resolved.len(), 2);
        // The override row targets advertising.google_ads; other features
        // keep their catalog defaults.
        let member_limit = resolved
            .iter()
            .find(|entry| entry.feature.as_str() == "tenancy.member_limit")
            .unwrap();
        assert_eq!(member_limit.source, EntitlementSource::FeatureDefault);
        assert!(member_limit.enabled);
    }
}
