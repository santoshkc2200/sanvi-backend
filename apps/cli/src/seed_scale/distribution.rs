//! The production-shaped distribution fixture (TASK-019 step 2, FR-1101).
//!
//! This file describes the shape production will have — deliberately *not* a
//! uniform one: a long tail of small tenants, a middle band, and three
//! deliberately hot tenants. The three hot tenants exist for three specific
//! later tests: per-tenant fairness (TASK-023), single-tenant restore
//! (TASK-025), and bloat measurement (TASK-028). **Do not simplify the
//! distribution.**
//!
//! Everything here is pure: a seed in, a plan out, no clocks and no I/O — so
//! the same seed produces identical row counts and checksums anywhere.

use serde::Serialize;
use sha2::{Digest, Sha256};

/// A tiny deterministic PRNG (SplitMix64). Chosen by hand rather than by
/// crate: the generated distribution must be identical across machines and
/// dependency versions, and 64 bits of well-mixed state is more than the
/// fixture needs.
pub struct SplitMix64(u64);

impl SplitMix64 {
    pub fn new(seed: u64) -> Self {
        Self(seed)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in `[0, n)`. Panics on `n == 0` (a fixture bug, not input).
    pub fn below(&mut self, n: u64) -> u64 {
        assert!(n > 0, "below(0) is not a distribution");
        // Modulo bias is irrelevant at fixture magnitudes; determinism is
        // the whole value.
        self.next_u64() % n
    }

    /// Uniform in `[lo, hi]` (inclusive).
    pub fn range(&mut self, lo: u64, hi: u64) -> u64 {
        debug_assert!(lo <= hi);
        lo + self.below(hi - lo + 1)
    }

    pub fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len() as u64) as usize]
    }
}

/// The band a tenant sits in. `Hot` tenants are the deliberately hot ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Band {
    /// The long tail: many tenants, little data each.
    LongTail,
    /// The middle band: real usage, no extremes.
    Middle,
    /// Deliberately hot, one dimension each.
    Hot,
}

/// One custom domain at a specific lifecycle state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct DomainPlan {
    pub kind: &'static str,
    pub role: &'static str,
    /// A `domains.custom_domains.status`.
    pub status: &'static str,
    /// Whether the live-hostname projection row exists (the live set).
    pub projected: bool,
}

/// One tenant's full row budget.
#[derive(Debug, Clone, Serialize)]
pub struct TenantPlan {
    pub slug: String,
    pub display_name: String,
    pub band: Band,
    /// What makes a hot tenant hot, in one line each.
    pub hot_dimension: Option<&'static str>,
    pub region: String,
    pub default_locale: String,
    pub enabled_locales: Vec<String>,
    pub audit_entries: u32,
    pub theme_revisions: u32,
    pub domains: Vec<DomainPlan>,
    pub ad_connections: u32,
    pub ad_campaigns_per_connection: u32,
    pub metrics_days: u32,
    pub payments: u32,
    /// Percentage of succeeded payments carrying a refund.
    pub refund_ratio_pct: u32,
}

impl TenantPlan {
    /// Checkout rows: roughly every payment plus a fraction that never
    /// completed.
    pub fn checkouts(&self) -> u32 {
        self.payments + self.payments / 4
    }

    pub fn refunds(&self) -> u32 {
        if self.payments == 0 {
            0
        } else {
            (self.payments * self.refund_ratio_pct) / 100
        }
    }

    pub fn metrics_daily_rows(&self) -> u32 {
        self.ad_connections * self.ad_campaigns_per_connection * self.metrics_days
    }
}

/// The whole distribution for one seed.
#[derive(Debug, Clone, Serialize)]
pub struct DistributionPlan {
    pub seed: u64,
    pub tenants: Vec<TenantPlan>,
}

impl DistributionPlan {
    /// Row counts by table, the summary every run prints and every run of
    /// the same seed reproduces exactly.
    pub fn row_counts(&self) -> RowCounts {
        let mut counts = RowCounts::default();
        for tenant in &self.tenants {
            counts.tenants += 1;
            counts.tenant_settings += 2; // storefront tagline + support email
            counts.tenant_locales += 1;
            counts.tenant_themes += 1;
            counts.tenant_theme_revisions += u64::from(tenant.theme_revisions);
            counts.custom_domains += tenant.domains.len() as u64;
            counts.live_hostnames += tenant.domains.iter().filter(|d| d.projected).count() as u64;
            counts.audit_entries += u64::from(tenant.audit_entries);
            counts.ad_connections += u64::from(tenant.ad_connections);
            counts.ad_campaigns +=
                u64::from(tenant.ad_connections) * u64::from(tenant.ad_campaigns_per_connection);
            counts.metrics_daily += u64::from(tenant.metrics_daily_rows());
            counts.provider_connections += u64::from(tenant.payments.min(1));
            counts.checkouts += u64::from(tenant.checkouts());
            counts.payments += u64::from(tenant.payments);
            counts.refunds += u64::from(tenant.refunds());
        }
        counts
    }

    /// A stable digest of the plan: same seed → identical checksum, any
    /// change to the fixture or the seed → a different one.
    pub fn checksum(&self) -> String {
        let canonical = serde_json::to_vec(self).expect("plan serializes");
        let digest = Sha256::digest(&canonical);
        digest.iter().map(|b| format!("{b:02x}")).collect()
    }
}

/// Row counts per table the generator writes.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct RowCounts {
    pub tenants: u64,
    pub tenant_settings: u64,
    pub tenant_locales: u64,
    pub tenant_themes: u64,
    pub tenant_theme_revisions: u64,
    pub custom_domains: u64,
    pub live_hostnames: u64,
    pub audit_entries: u64,
    pub ad_connections: u64,
    pub ad_campaigns: u64,
    pub metrics_daily: u64,
    pub provider_connections: u64,
    pub checkouts: u64,
    pub payments: u64,
    pub refunds: u64,
}

impl RowCounts {
    pub fn total(&self) -> u64 {
        self.tenants
            + self.tenant_settings
            + self.tenant_locales
            + self.tenant_themes
            + self.tenant_theme_revisions
            + self.custom_domains
            + self.live_hostnames
            + self.audit_entries
            + self.ad_connections
            + self.ad_campaigns
            + self.metrics_daily
            + self.provider_connections
            + self.checkouts
            + self.payments
            + self.refunds
    }
}

const LONG_TAIL_TENANTS: u32 = 120;
const MIDDLE_TENANTS: u32 = 24;

const REGIONS: &[&str] = &["jp", "us", "de"];
const JA_DISPLAY_NAMES: &[&str] = &[
    "スケール商店",
    "株式会社サンビ",
    "ひとう店",
    "さくらストア",
    "山田電器",
];
const EN_DISPLAY_NAMES: &[&str] = &[
    "Scale Shop",
    "Corner Store",
    "Atelier",
    "General Goods",
    "Boutique",
];

/// Every lifecycle state a custom domain can sit in. The distribution
/// scatters each state across tenants so every later query meets every
/// state, and the live set (with its projection row) is non-empty.
const DOMAIN_STATES: &[(&str, &str, &str, bool)] = &[
    // (kind, role, status, projected)
    ("connected", "primary", "pending_setup", false),
    ("connected", "primary", "verifying", false),
    ("connected", "primary", "verified", true),
    ("connected", "primary", "issuing_cert", true),
    ("connected", "primary", "live", true),
    ("connected", "alias", "degraded", true),
    ("connected", "alias", "removed", false),
    ("purchased", "primary", "live", true),
];

/// Build the plan for `seed`: deterministic, with the three hot tenants,
/// a non-empty long tail and middle band, a Japanese-skewed locale mix,
/// themes published at several revisions, and custom domains at every
/// lifecycle state.
pub fn plan(seed: u64) -> DistributionPlan {
    let mut rng = SplitMix64::new(seed);
    let mut tenants = Vec::with_capacity((LONG_TAIL_TENANTS + MIDDLE_TENANTS + 3) as usize);

    // The three hot tenants, one hot dimension each, fixed slugs so later
    // tasks can name them.
    tenants.push(TenantPlan {
        slug: "scale-hot-audit".to_string(),
        display_name: "Scale Hot Audit (seed data)".to_string(),
        band: Band::Hot,
        hot_dimension: Some("audit_entries"),
        region: "jp".to_string(),
        default_locale: "ja".to_string(),
        enabled_locales: vec!["ja".to_string(), "en".to_string()],
        audit_entries: 20_000,
        theme_revisions: 5,
        domains: vec![domain_plan(0)],
        ad_connections: 1,
        ad_campaigns_per_connection: 3,
        metrics_days: 30,
        payments: 50,
        refund_ratio_pct: 5,
    });
    tenants.push(TenantPlan {
        slug: "scale-hot-ads".to_string(),
        display_name: "Scale Hot Ads (seed data)".to_string(),
        band: Band::Hot,
        hot_dimension: Some("advertising_metrics_daily"),
        region: "us".to_string(),
        default_locale: "en".to_string(),
        enabled_locales: vec!["en".to_string(), "ja".to_string()],
        audit_entries: 500,
        theme_revisions: 3,
        domains: vec![domain_plan(4)],
        ad_connections: 2,
        ad_campaigns_per_connection: 15,
        metrics_days: 90,
        payments: 0,
        refund_ratio_pct: 0,
    });
    tenants.push(TenantPlan {
        slug: "scale-hot-payments".to_string(),
        display_name: "Scale Hot Payments (seed data)".to_string(),
        band: Band::Hot,
        hot_dimension: Some("payments_with_refunds"),
        region: "us".to_string(),
        default_locale: "en".to_string(),
        enabled_locales: vec!["en".to_string()],
        audit_entries: 800,
        theme_revisions: 2,
        domains: vec![domain_plan(7)],
        ad_connections: 0,
        ad_campaigns_per_connection: 0,
        metrics_days: 0,
        payments: 4_000,
        refund_ratio_pct: 25,
    });

    // The middle band.
    for i in 0..MIDDLE_TENANTS {
        let ja_first = rng.below(100) < 55;
        let (default_locale, display_name) = if ja_first {
            ("ja", format!("{} {}", rng.pick(JA_DISPLAY_NAMES), i))
        } else {
            ("en", format!("{} {}", rng.pick(EN_DISPLAY_NAMES), i))
        };
        tenants.push(TenantPlan {
            slug: format!("scale-mid-{}-{i:03}", seed),
            display_name,
            band: Band::Middle,
            hot_dimension: None,
            region: rng.pick(REGIONS).to_string(),
            default_locale: default_locale.to_string(),
            enabled_locales: if rng.below(100) < 70 {
                vec!["ja".to_string(), "en".to_string()]
            } else {
                vec![default_locale.to_string()]
            },
            audit_entries: rng.range(150, 250) as u32,
            theme_revisions: rng.range(1, 5) as u32,
            domains: vec![domain_plan(rng.below(DOMAIN_STATES.len() as u64) as usize)],
            ad_connections: 1,
            ad_campaigns_per_connection: rng.range(1, 4) as u32,
            metrics_days: 30,
            payments: rng.range(30, 80) as u32,
            refund_ratio_pct: rng.range(2, 15) as u32,
        });
    }

    // The long tail.
    for i in 0..LONG_TAIL_TENANTS {
        let ja_first = rng.below(100) < 55;
        let (default_locale, display_name) = if ja_first {
            (
                "ja",
                format!("{} {}", rng.pick(JA_DISPLAY_NAMES), 1_000 + i),
            )
        } else {
            (
                "en",
                format!("{} {}", rng.pick(EN_DISPLAY_NAMES), 1_000 + i),
            )
        };
        tenants.push(TenantPlan {
            slug: format!("scale-tail-{}-{i:03}", seed),
            display_name,
            band: Band::LongTail,
            hot_dimension: None,
            region: rng.pick(REGIONS).to_string(),
            default_locale: default_locale.to_string(),
            enabled_locales: vec![default_locale.to_string()],
            audit_entries: rng.range(5, 40) as u32,
            theme_revisions: 1,
            domains: if rng.below(100) < 20 {
                vec![domain_plan(rng.below(DOMAIN_STATES.len() as u64) as usize)]
            } else {
                Vec::new()
            },
            ad_connections: 0,
            ad_campaigns_per_connection: 0,
            metrics_days: 0,
            payments: rng.range(0, 4) as u32,
            refund_ratio_pct: rng.range(0, 20) as u32,
        });
    }

    DistributionPlan { seed, tenants }
}

fn domain_plan(state_index: usize) -> DomainPlan {
    let (kind, role, status, projected) = DOMAIN_STATES[state_index];
    DomainPlan {
        kind,
        role,
        status,
        projected,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn the_distribution_has_the_three_hot_tenants() {
        let plan = plan(1);
        let hot: BTreeSet<&str> = plan
            .tenants
            .iter()
            .filter(|t| t.band == Band::Hot)
            .map(|t| t.slug.as_str())
            .collect();
        assert_eq!(
            hot,
            BTreeSet::from(["scale-hot-audit", "scale-hot-ads", "scale-hot-payments"]),
            "the three hot tenants must exist by slug"
        );

        // And each is actually hot in its dimension, not just named so.
        let by_slug = |slug: &str| {
            plan.tenants
                .iter()
                .find(|t| t.slug == slug)
                .expect("hot tenant exists")
        };
        assert!(by_slug("scale-hot-audit").audit_entries >= 10_000);
        assert!(by_slug("scale-hot-ads").metrics_daily_rows() >= 2_000);
        let payments = by_slug("scale-hot-payments");
        assert!(payments.payments >= 1_000);
        assert!(
            payments.refunds() >= payments.payments / 5,
            "the payments hot tenant carries many refunds"
        );
    }

    #[test]
    fn no_tenant_band_is_empty() {
        for seed in [1u64, 2, 12345] {
            let plan = plan(seed);
            assert!(plan.tenants.iter().any(|t| t.band == Band::LongTail));
            assert!(plan.tenants.iter().any(|t| t.band == Band::Middle));
            assert!(plan.tenants.iter().any(|t| t.band == Band::Hot));
        }
    }

    #[test]
    fn the_same_seed_produces_an_identical_plan_and_checksum() {
        let a = plan(7);
        let b = plan(7);
        assert_eq!(a.checksum(), b.checksum());
        assert_eq!(a.row_counts(), b.row_counts());
    }

    #[test]
    fn a_different_seed_produces_a_different_but_valid_distribution() {
        let a = plan(1);
        let b = plan(2);
        assert_ne!(a.checksum(), b.checksum());
        // Validity: same band sizes, same hot slugs.
        assert_eq!(a.row_counts().tenants, b.row_counts().tenants);
        fn hot_slugs(p: &DistributionPlan) -> Vec<&str> {
            let mut slugs: Vec<&str> = p
                .tenants
                .iter()
                .filter(|t| t.band == Band::Hot)
                .map(|t| t.slug.as_str())
                .collect();
            slugs.sort();
            slugs
        }
        assert_eq!(hot_slugs(&a), hot_slugs(&b));
    }

    #[test]
    fn every_domain_lifecycle_state_appears_across_the_distribution() {
        let plan = plan(3);
        let statuses: BTreeSet<&str> = plan
            .tenants
            .iter()
            .flat_map(|t| t.domains.iter().map(|d| d.status))
            .collect();
        for (_, _, status, _) in DOMAIN_STATES {
            assert!(
                statuses.contains(status),
                "domain state {status} missing from the distribution"
            );
        }
    }

    #[test]
    fn the_locale_mix_is_skewed_toward_japanese() {
        // The 55 % ja skew must hold for the whole distribution, hot tenants
        // included, so ja-shaped content is always the majority.
        for seed in [1u64, 2, 3, 4, 5] {
            let plan = plan(seed);
            let ja = plan
                .tenants
                .iter()
                .filter(|t| t.default_locale == "ja")
                .count();
            assert!(
                ja * 2 > plan.tenants.len(),
                "seed {seed}: ja default-locale tenants ({ja}) must be the majority"
            );
        }
    }

    #[test]
    fn themes_publish_at_several_revisions() {
        let plan = plan(11);
        let revisions: BTreeSet<u32> = plan.tenants.iter().map(|t| t.theme_revisions).collect();
        assert!(revisions.len() >= 3, "revisions published at {revisions:?}");
    }
}
