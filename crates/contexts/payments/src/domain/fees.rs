//! Application-fee arithmetic (09.6).
//!
//! The platform *can* take a cut of a direct charge via
//! `payment_intent_data.application_fee_amount` — the only correct
//! mechanism for this charge pattern, and never combined with transfers
//! (there are none here). In 0.10.0 the fee ships **disabled**: the
//! platform monetises via subscription. This module owns the one
//! definition of the money math so that when a commercial model turns the
//! fee on, rounding is defined exactly once:
//!
//! > **Round half up on the fee, never on the total.** Everything is
//! > integer minor-unit arithmetic; the basis-points share rounds half up
//! > to the nearest minor unit, and the charge total is never altered.
//!
//! The configured minimum is interpreted **in the charge currency's minor
//! units**, which makes it currency-aware by construction (a minimum of
//! `30` is ¢0.30 for USD charges and ¥30 for JPY charges) — zero-decimal
//! currencies take the same code path because every amount already counts
//! the currency's own minor unit.

use crate::domain::money::Money;

/// The per-tenant fee configuration as stored by the platform-admin
/// context. `Default` is the shipped state: disabled everywhere.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeeConfiguration {
    /// Basis points of the charge total (100 bps = 1%). Absent = no
    /// percentage component.
    pub basis_points: Option<u32>,
    /// A flat component in minor units, charged only when its currency
    /// matches the charge's.
    pub fixed_minor: Option<i64>,
    pub fixed_currency: Option<String>,
    /// The floor on the computed fee, in the charge currency's minor
    /// units. Currency-aware by interpretation; see the module docs.
    pub minimum_minor: i64,
}

impl Default for FeeConfiguration {
    fn default() -> Self {
        Self {
            basis_points: None,
            fixed_minor: None,
            fixed_currency: None,
            minimum_minor: 0,
        }
    }
}

impl FeeConfiguration {
    /// Round-half-up share of `amount_minor` for `basis_points`, in i128
    /// so no realistic charge × bps product overflows.
    fn basis_points_share(amount_minor: i64, basis_points: u32) -> i64 {
        let product = (i128::from(amount_minor)) * (i128::from(basis_points));
        // Half up on non-negative values: add half the divisor, then
        // truncate toward zero (= floor for positives).
        ((product + 5_000) / 10_000) as i64
    }

    /// Compute the platform's fee for one charge, or `None` when nothing
    /// is chargeable under this configuration (including the disabled /
    /// empty configuration). The result is clamped to `[minimum, total]`:
    /// the fee can never exceed what the tenant keeps, and the remainder
    /// is always the tenant's. A minimum *above* the charge total yields
    /// `None` — the dust guard means "no fee this small", never "take the
    /// whole order".
    pub fn compute(&self, charge: &Money) -> Option<i64> {
        let total = charge.amount_minor;
        if total <= 0 {
            return None;
        }
        let mut fee = self
            .basis_points
            .map(|bps| Self::basis_points_share(total, bps))
            .unwrap_or(0);
        // The flat component applies only to charges in its own currency;
        // cross-currency charges get the percentage alone rather than an
        // amount denominated in the wrong unit.
        if self.fixed_currency.as_deref() == Some(charge.currency.as_str()) {
            fee += self.fixed_minor.unwrap_or(0);
        }
        // Floor first (the operator's dust guard), then refuse when the
        // floor exceeds the total — a capped-at-total fee from a
        // misconfigured minimum would silently confiscate small orders.
        fee = fee.max(self.minimum_minor);
        if fee > total {
            return None;
        }
        (fee > 0).then_some(fee)
    }
}

/// The shipped policy value: fees are off for everyone until the
/// platform-admin context configures otherwise.
pub fn disabled_configuration() -> FeeConfiguration {
    FeeConfiguration::default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_configuration_is_off() {
        assert_eq!(
            FeeConfiguration::default().compute(&Money::new(2000, "JPY").unwrap()),
            None,
            "release default: no application fee anywhere"
        );
    }

    #[test]
    fn basis_points_round_half_up_in_usd_and_jpy() {
        // USD: 2.5% of $19.99 (1999¢) = 49.975¢ → 50¢ (half up).
        let config = FeeConfiguration {
            basis_points: Some(250),
            ..FeeConfiguration::default()
        };
        assert_eq!(config.compute(&Money::new(1999, "USD").unwrap()), Some(50));

        // JPY zero-decimal: 2.5% of ¥2000 = 50 → same code path, same
        // units, no ÷100 anywhere.
        assert_eq!(config.compute(&Money::new(2000, "JPY").unwrap()), Some(50));

        // Exact .5 rounds up: 1% of 1050 = 10.5 → 11.
        let config = FeeConfiguration {
            basis_points: Some(100),
            ..FeeConfiguration::default()
        };
        assert_eq!(config.compute(&Money::new(1050, "EUR").unwrap()), Some(11));

        // Below half stays down: 1% of 1040 = 10.4 → 10.
        assert_eq!(config.compute(&Money::new(1040, "EUR").unwrap()), Some(10));
    }

    #[test]
    fn the_fixed_component_applies_only_in_its_own_currency() {
        let config = FeeConfiguration {
            fixed_minor: Some(20),
            fixed_currency: Some("JPY".to_string()),
            ..FeeConfiguration::default()
        };
        assert_eq!(config.compute(&Money::new(2000, "JPY").unwrap()), Some(20));
        // A USD charge must not carry a yen-denominated flat fee.
        assert_eq!(config.compute(&Money::new(2000, "USD").unwrap()), None);
    }

    #[test]
    fn percentage_and_fixed_components_add_before_the_minimum() {
        let config = FeeConfiguration {
            basis_points: Some(250), // 50 on ¥2000
            fixed_minor: Some(10),
            fixed_currency: Some("JPY".to_string()),
            ..FeeConfiguration::default()
        };
        assert_eq!(config.compute(&Money::new(2000, "JPY").unwrap()), Some(60));
    }

    #[test]
    fn the_minimum_is_a_floor_interpreted_in_the_charge_currency() {
        let config = FeeConfiguration {
            basis_points: Some(100), // would be 12 on 1200
            minimum_minor: 30,
            ..FeeConfiguration::default()
        };
        // USD: 12¢ below the 30¢ floor → lifted to 30¢.
        assert_eq!(config.compute(&Money::new(1200, "USD").unwrap()), Some(30));
        // JPY: same integer minimum reads as ¥30 on a ¥1200 charge —
        // currency-aware through minor units, no separate code path.
        assert_eq!(config.compute(&Money::new(1200, "JPY").unwrap()), Some(30));
    }

    #[test]
    fn the_fee_never_exceeds_the_total_the_remainder_is_the_tenants() {
        // A pathological minimum above the total caps at the total…
        let config = FeeConfiguration {
            minimum_minor: 999_999,
            ..FeeConfiguration::default()
        };
        assert_eq!(config.compute(&Money::new(500, "JPY").unwrap()), None);

        // …and a capped fee of the whole total still computes, though a
        // 100% fee is a configuration review, not a math bug.
        let config = FeeConfiguration {
            basis_points: Some(10_000),
            ..FeeConfiguration::default()
        };
        assert_eq!(config.compute(&Money::new(500, "JPY").unwrap()), Some(500));

        // Non-positive totals never produce fees.
        assert_eq!(
            FeeConfiguration {
                minimum_minor: 10,
                ..FeeConfiguration::default()
            }
            .compute(&Money::new(0, "JPY").unwrap()),
            None
        );
    }

    #[test]
    fn huge_charges_do_not_overflow_the_rounding_arithmetic() {
        let config = FeeConfiguration {
            basis_points: Some(299),
            ..FeeConfiguration::default()
        };
        // 9.2 trillion minor units × 299 bps fits i128 comfortably.
        let huge = Money::new(9_223_372_036_854, "JPY").unwrap();
        let expected = ((i128::from(huge.amount_minor) * 299 + 5_000) / 10_000) as i64;
        assert_eq!(config.compute(&huge), Some(expected));
    }
}
