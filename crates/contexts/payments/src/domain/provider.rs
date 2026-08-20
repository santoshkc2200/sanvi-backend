//! The provider-agnostic vocabulary every payments slice speaks.
//!
//! `ProviderKind` is the seam a second provider (PayPal, a Japanese
//! provider) slots into: a new enum variant plus a new adapter, not a new
//! checkout codepath. `ConnectionStatus` is the lifecycle state machine
//! 09.3 drives; the values are fixed now because the schema's CHECK
//! constraints and this enum must never disagree.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::error::PaymentsError;

/// A connectable payment provider. Phase 09 ships Stripe Connect only;
/// the abstraction exists so that "add PayPal" is an adapter, not a
/// redesign (proven by the fake provider in tests, sub-phase 09.1).
///
/// `Fake` is the in-memory provider used to build and test 09.2–09.6
/// before Stripe sandbox credentials exist everywhere. Its adapter is
/// registered only in non-production environments (see
/// [`crate::adapters::fake_provider`]); the enum variant exists in every
/// build so parsing and the schema CHECK stay in lockstep.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    StripeConnect,
    Fake,
}

impl ProviderKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::StripeConnect => "stripe_connect",
            Self::Fake => "fake",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "stripe_connect" => Some(Self::StripeConnect),
            "fake" => Some(Self::Fake),
            _ => None,
        }
    }
}

/// The connection lifecycle. A connection is `Active` only when the
/// connected account's `card_payments` capability is `active` (the v2
/// capability check — never the deprecated `charges_enabled` boolean).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionStatus {
    /// Account created, onboarding not yet started.
    Pending,
    /// Onboarding in flight; requirements may be outstanding.
    Onboarding,
    /// Can move no money: requirements past due or capability revoked.
    Restricted,
    /// `card_payments` capability active — checkout is possible.
    Active,
    /// The provider rejected the account (e.g. fraud).
    Rejected,
    /// The tenant disconnected; the row is retained for the statutory
    /// period, and the partial unique index stops counting it.
    Disconnected,
}

impl ConnectionStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Onboarding => "onboarding",
            Self::Restricted => "restricted",
            Self::Active => "active",
            Self::Rejected => "rejected",
            Self::Disconnected => "disconnected",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        Some(match raw {
            "pending" => Self::Pending,
            "onboarding" => Self::Onboarding,
            "restricted" => Self::Restricted,
            "active" => Self::Active,
            "rejected" => Self::Rejected,
            "disconnected" => Self::Disconnected,
            _ => return None,
        })
    }

    /// Every lifecycle value, in declaration order. The exhaustive
    /// transition table test iterates this.
    pub const ALL: [ConnectionStatus; 6] = [
        Self::Pending,
        Self::Onboarding,
        Self::Restricted,
        Self::Active,
        Self::Rejected,
        Self::Disconnected,
    ];

    /// The explicit lifecycle state machine:
    ///
    /// ```text
    /// Pending → Onboarding → {Active, Restricted, Rejected} → Disconnected
    /// ```
    ///
    /// with the trading-time edges real connected accounts exhibit:
    /// `Active ↔ Restricted` (capability regression and restoration) and
    /// `Active/Restricted → Rejected`. Webhook drivers (09.3) call this
    /// and have no transition choices of their own to make; an illegal
    /// transition is an error, never a silent clamp. Same-state transitions
    /// are illegal — deduplicated events must not reach this function.
    ///
    /// `Disconnected` is terminal: reconnection is a *new* connection row
    /// (the partial unique index already encodes that), so no edge leaves it.
    pub fn transition(from: Self, to: Self) -> Result<Self, PaymentsError> {
        use ConnectionStatus::*;
        let legal = matches!(
            (from, to),
            (Pending, Onboarding)
                | (Pending, Disconnected)
                | (Onboarding, Active)
                | (Onboarding, Restricted)
                | (Onboarding, Rejected)
                | (Onboarding, Disconnected)
                | (Restricted, Active)
                | (Restricted, Rejected)
                | (Restricted, Disconnected)
                | (Active, Restricted)
                | (Active, Rejected)
                | (Active, Disconnected)
                | (Rejected, Disconnected)
        );
        if !legal {
            return Err(PaymentsError::InvalidConnection(format!(
                "illegal connection transition {} → {}",
                from.as_str(),
                to.as_str()
            )));
        }
        Ok(to)
    }
}

/// A validated ISO-3166-1 alpha-2 country code (uppercase, exactly two
/// letters). Provider support matrices and connection countries speak
/// this, never free-form strings — `"jp"` and `"JPN"` are the two ways a
/// country check silently passes everything.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CountryCode(String);

impl CountryCode {
    /// Validate and normalize (uppercase) an alpha-2 code.
    pub fn parse(raw: &str) -> Result<Self, PaymentsError> {
        let code = raw.trim().to_ascii_uppercase();
        if code.len() != 2 || !code.chars().all(|c| c.is_ascii_alphabetic()) {
            return Err(PaymentsError::InvalidConnection(format!(
                "country {raw:?} is not a 2-letter ISO-3166-1 code"
            )));
        }
        Ok(Self(code))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_values_round_trip_the_schema_check_constraint() {
        // Must stay in lockstep with the CHECK on
        // payments.provider_connections.status — one source of truth.
        for status in ConnectionStatus::ALL {
            assert_eq!(ConnectionStatus::parse(status.as_str()), Some(status));
        }
        assert_eq!(ConnectionStatus::parse("deleted"), None);
    }

    #[test]
    fn provider_kinds_round_trip() {
        for kind in [ProviderKind::StripeConnect, ProviderKind::Fake] {
            assert_eq!(ProviderKind::parse(kind.as_str()), Some(kind));
        }
        assert_eq!(ProviderKind::parse("paypal"), None);
    }

    #[test]
    fn countries_are_normalized_and_validated() {
        assert_eq!(CountryCode::parse(" jp ").unwrap().as_str(), "JP");
        assert!(CountryCode::parse("jpn").is_err());
        assert!(CountryCode::parse("J1").is_err());
        assert!(CountryCode::parse("").is_err());
    }

    // -------------------------------------------------------------------
    // The exhaustive transition table: every legal edge accepted, every
    // illegal edge rejected — 6×6, no cell skipped, including the
    // same-state diagonal.
    // -------------------------------------------------------------------

    #[test]
    fn the_transition_table_accepts_exactly_the_legal_edges() {
        let legal = [
            (ConnectionStatus::Pending, ConnectionStatus::Onboarding),
            (ConnectionStatus::Pending, ConnectionStatus::Disconnected),
            (ConnectionStatus::Onboarding, ConnectionStatus::Active),
            (ConnectionStatus::Onboarding, ConnectionStatus::Restricted),
            (ConnectionStatus::Onboarding, ConnectionStatus::Rejected),
            (ConnectionStatus::Onboarding, ConnectionStatus::Disconnected),
            (ConnectionStatus::Restricted, ConnectionStatus::Active),
            (ConnectionStatus::Restricted, ConnectionStatus::Rejected),
            (ConnectionStatus::Restricted, ConnectionStatus::Disconnected),
            (ConnectionStatus::Active, ConnectionStatus::Restricted),
            (ConnectionStatus::Active, ConnectionStatus::Rejected),
            (ConnectionStatus::Active, ConnectionStatus::Disconnected),
            (ConnectionStatus::Rejected, ConnectionStatus::Disconnected),
        ];
        for from in ConnectionStatus::ALL {
            for to in ConnectionStatus::ALL {
                let result = ConnectionStatus::transition(from, to);
                if legal.contains(&(from, to)) {
                    assert_eq!(result.unwrap(), to, "{from:?} → {to:?} must be legal");
                } else {
                    assert!(result.is_err(), "{from:?} → {to:?} must be illegal");
                }
            }
        }
    }

    #[test]
    fn disconnected_is_terminal_and_same_state_transitions_are_errors() {
        for status in ConnectionStatus::ALL {
            assert!(ConnectionStatus::transition(status, status).is_err());
        }
        for to in [
            ConnectionStatus::Pending,
            ConnectionStatus::Onboarding,
            ConnectionStatus::Restricted,
            ConnectionStatus::Active,
            ConnectionStatus::Rejected,
        ] {
            assert!(ConnectionStatus::transition(ConnectionStatus::Disconnected, to).is_err());
        }
        // Pending may not leap straight to a trading status.
        for to in [
            ConnectionStatus::Restricted,
            ConnectionStatus::Active,
            ConnectionStatus::Rejected,
        ] {
            assert!(ConnectionStatus::transition(ConnectionStatus::Pending, to).is_err());
        }
    }
}
