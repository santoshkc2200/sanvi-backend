//! Money: minor-unit amounts with an ISO 4217 currency code.
//!
//! Deliberately kept small: the domains context needs to quote, display and
//! hand amounts to the billing context — not to do accounting. Conversion,
//! tax and reconciliation live in the billing context.

use std::fmt;

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// An amount in the currency's minor unit (cents for USD/JPY).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, ToSchema)]
#[schema(as = DomainsMoney)]
pub struct Money {
    pub minor: i64,
    pub currency: String,
}

impl Money {
    pub fn new(minor: i64, currency: impl Into<String>) -> Self {
        Self {
            minor,
            currency: currency.into(),
        }
    }

    /// `12.99` for 1299 minor units; JPY renders without decimals.
    pub fn display(&self) -> String {
        if self.currency == "jpy" {
            return format!("¥{}", self.minor);
        }
        let major = self.minor / 100;
        let minor = self.minor.abs() % 100;
        match self.currency.as_str() {
            "usd" => format!("${}.{:02}", major, minor),
            "eur" => format!("€{}.{:02}", major, minor),
            "gbp" => format!("£{}.{:02}", major, minor),
            other => format!("{major}.{minor:02} {other}"),
        }
    }
}

impl fmt::Display for Money {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.display())
    }
}

/// Normalized pricing for one TLD: what a year of registration and renewal
/// cost through the selected registrar.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct TldPrice {
    pub tld: String,
    pub register: Money,
    pub renew: Money,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn displays_major_currencies() {
        assert_eq!(Money::new(1299, "usd").display(), "$12.99");
        assert_eq!(Money::new(2400, "usd").display(), "$24.00");
        assert_eq!(Money::new(2400, "jpy").display(), "¥2400");
    }
}
