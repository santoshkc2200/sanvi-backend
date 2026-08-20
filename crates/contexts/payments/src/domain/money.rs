//! Money: an integer count of a currency's minor units plus the ISO-4217
//! code — never a float, and never a `numeric` column at rest.
//!
//! The representation is fixed here because every later slice (checkout,
//! refunds, payouts, fees) and the frontend's money helpers depend on the
//! shapes being stable. JPY is zero-decimal and is the test case that
//! catches every decimal assumption: ¥2,000 is the minor-unit integer
//! `2000`, not `200000`.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::error::PaymentsError;

/// ISO 4217 zero-decimal currencies (per Stripe's list). A currency absent
/// from this set has a 1/100 minor unit; three-decimal currencies (BHD,
/// KWD, TND) are deliberately not modeled until a real requirement exists.
const ZERO_DECIMAL_CURRENCIES: &[&str] = &[
    "BIF", "CLP", "DJF", "GNF", "JPY", "KMF", "KRW", "MGA", "PYG", "RWF", "UGX", "VND", "VUV",
    "XAF", "XOF", "XPF",
];

/// A validated ISO-4217 currency code (uppercase, exactly three letters).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Currency(String);

impl Currency {
    /// Validate and normalize (uppercase) an ISO-4217 alphabetic code.
    pub fn parse(raw: &str) -> Result<Self, PaymentsError> {
        let code = raw.trim().to_ascii_uppercase();
        if code.len() != 3 || !code.chars().all(|c| c.is_ascii_alphabetic()) {
            return Err(PaymentsError::InvalidMoney(format!(
                "currency {raw:?} is not a 3-letter ISO-4217 code"
            )));
        }
        Ok(Self(code))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether this currency has no minor unit (JPY: true, USD: false).
    /// The zero-decimal guard every money-touching slice consults.
    pub fn is_zero_decimal(&self) -> bool {
        ZERO_DECIMAL_CURRENCIES.contains(&self.0.as_str())
    }
}

/// A minor-unit amount: `2000` + `JPY` is ¥2,000; `1999` + `USD` is
/// $19.99. Amounts are non-negative (direction is the caller's concept).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct Money {
    /// Integer count of the currency's minor units — ¥2,000 is `2000`.
    #[schema(example = 2000)]
    pub amount_minor: i64,
    /// Uppercase ISO-4217 code; zero-decimal currencies are honored.
    #[schema(example = "JPY", value_type = String)]
    pub currency: Currency,
}

impl Money {
    pub fn new(amount_minor: i64, currency: &str) -> Result<Self, PaymentsError> {
        if amount_minor < 0 {
            return Err(PaymentsError::InvalidMoney(format!(
                "amount {amount_minor} is negative; money amounts are non-negative minor units"
            )));
        }
        Ok(Self {
            amount_minor,
            currency: Currency::parse(currency)?,
        })
    }

    /// The zero-decimal guard for partial amounts (refunds): a portion
    /// larger than the total — or a negative portion — is rejected at the
    /// type boundary, before any provider call. Returns the validated
    /// portion so callers can treat the guard as parsing, not branching.
    pub fn portion_of(&self, portion_minor: i64) -> Result<Money, PaymentsError> {
        if portion_minor < 0 {
            return Err(PaymentsError::InvalidMoney(format!(
                "portion {portion_minor} is negative"
            )));
        }
        if portion_minor > self.amount_minor {
            return Err(PaymentsError::InvalidMoney(format!(
                "portion {portion_minor} exceeds the total {} {}",
                self.amount_minor,
                self.currency.as_str()
            )));
        }
        Ok(Money {
            amount_minor: portion_minor,
            currency: self.currency.clone(),
        })
    }

    /// Currency-checked addition. Cross-currency arithmetic is rejected —
    /// `Money` has no implicit conversion, because exchange is a business
    /// decision, not a convenience. Overflow is *defined*: a checked
    /// error, never a wrap and never a panic.
    pub fn checked_add(&self, other: &Money) -> Result<Money, PaymentsError> {
        if self.currency != other.currency {
            return Err(PaymentsError::InvalidMoney(format!(
                "cannot add {} to {} — cross-currency arithmetic is not defined",
                other.currency.as_str(),
                self.currency.as_str()
            )));
        }
        let amount_minor = self
            .amount_minor
            .checked_add(other.amount_minor)
            .ok_or_else(|| {
                PaymentsError::InvalidMoney(format!(
                    "addition of {} + {} {} overflows the minor-unit integer",
                    self.amount_minor,
                    other.amount_minor,
                    self.currency.as_str()
                ))
            })?;
        Ok(Money {
            amount_minor,
            currency: self.currency.clone(),
        })
    }

    /// Currency-checked subtraction. A negative result is rejected —
    /// amounts are non-negative by representation, so "owed" is the
    /// caller's separate concept.
    pub fn checked_sub(&self, other: &Money) -> Result<Money, PaymentsError> {
        if self.currency != other.currency {
            return Err(PaymentsError::InvalidMoney(format!(
                "cannot subtract {} from {} — cross-currency arithmetic is not defined",
                other.currency.as_str(),
                self.currency.as_str()
            )));
        }
        let amount_minor = self
            .amount_minor
            .checked_sub(other.amount_minor)
            .ok_or_else(|| {
                PaymentsError::InvalidMoney(format!(
                    "subtraction of {} - {} {} overflows the minor-unit integer",
                    self.amount_minor,
                    other.amount_minor,
                    self.currency.as_str()
                ))
            })?;
        if amount_minor < 0 {
            return Err(PaymentsError::InvalidMoney(format!(
                "{} {} - {} {} would be negative",
                self.amount_minor,
                self.currency.as_str(),
                other.amount_minor,
                other.currency.as_str()
            )));
        }
        Ok(Money {
            amount_minor,
            currency: self.currency.clone(),
        })
    }

    pub const fn is_zero(&self) -> bool {
        self.amount_minor == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jpy_is_zero_decimal_and_usd_is_not() {
        assert!(Currency::parse("JPY").unwrap().is_zero_decimal());
        assert!(!Currency::parse("USD").unwrap().is_zero_decimal());
    }

    #[test]
    fn currency_codes_are_normalized_and_validated() {
        assert_eq!(Currency::parse(" jpy ").unwrap().as_str(), "JPY");
        assert!(Currency::parse("US").is_err());
        assert!(Currency::parse("DOLLAR").is_err());
        assert!(Currency::parse("US1").is_err());
    }

    #[test]
    fn money_rejects_negative_amounts_and_bad_codes() {
        assert!(Money::new(-1, "USD").is_err());
        assert!(Money::new(100, "usdd").is_err());
        assert_eq!(
            Money::new(2000, "jpy").unwrap(),
            Money {
                amount_minor: 2000,
                currency: Currency::parse("JPY").unwrap()
            }
        );
    }

    #[test]
    fn portions_larger_than_the_total_are_rejected_at_the_type_boundary() {
        let total = Money::new(1999, "USD").unwrap();
        assert_eq!(total.portion_of(0).unwrap().amount_minor, 0);
        assert_eq!(total.portion_of(1999).unwrap().amount_minor, 1999);
        assert!(total.portion_of(2000).is_err());
        assert!(total.portion_of(-1).is_err());

        // The JPY twin of the same guard — no hidden ÷100 anywhere.
        let yen = Money::new(2000, "JPY").unwrap();
        assert_eq!(yen.portion_of(2000).unwrap().amount_minor, 2000);
        assert!(yen.portion_of(2001).is_err());
    }

    #[test]
    fn arithmetic_is_currency_checked() {
        let usd = Money::new(1000, "USD").unwrap();
        let jpy = Money::new(1000, "JPY").unwrap();
        assert!(usd.checked_add(&jpy).is_err());
        assert!(usd.checked_sub(&jpy).is_err());
        assert!(jpy.checked_sub(&usd).is_err());

        assert_eq!(
            usd.checked_add(&Money::new(499, "USD").unwrap()).unwrap(),
            Money::new(1499, "USD").unwrap()
        );
        // JPY stays zero-decimal under arithmetic: ¥1,000 + ¥500 = ¥1,500.
        assert_eq!(
            jpy.checked_add(&Money::new(500, "JPY").unwrap()).unwrap(),
            Money::new(1500, "JPY").unwrap()
        );
        assert_eq!(
            jpy.checked_sub(&Money::new(500, "JPY").unwrap()).unwrap(),
            Money::new(500, "JPY").unwrap()
        );
    }

    #[test]
    fn arithmetic_has_defined_overflow_and_underflow_behaviour() {
        // Overflow: an error naming the operands — never a wrap, never a
        // panic. i64::MAX minor units is ~92 quadrillion majors; nothing
        // legitimate gets there, so refusing is correct.
        let max = Money::new(i64::MAX, "USD").unwrap();
        let one = Money::new(1, "USD").unwrap();
        let err = max.checked_add(&one).unwrap_err();
        assert!(matches!(err, PaymentsError::InvalidMoney(_)));
        assert!(err.to_string().contains("overflows"));

        // Underflow: negative results are rejected, not clamped.
        let small = Money::new(100, "USD").unwrap();
        let big = Money::new(200, "USD").unwrap();
        let err = small.checked_sub(&big).unwrap_err();
        assert!(matches!(err, PaymentsError::InvalidMoney(_)));
        assert!(err.to_string().contains("negative"));

        // The boundary itself is fine.
        assert!(small.checked_sub(&small).unwrap().is_zero());
    }
}
