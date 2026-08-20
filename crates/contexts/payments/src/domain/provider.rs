//! The provider-agnostic vocabulary every payments slice speaks.
//!
//! `ProviderKind` is the seam a second provider (PayPal, a Japanese
//! provider) slots into: a new enum variant plus a new adapter, not a new
//! checkout codepath. `ConnectionStatus` is the lifecycle state machine
//! 09.3 drives; the values are fixed now because the schema's CHECK
//! constraints and this enum must never disagree.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// A connectable payment provider. Phase 09 ships Stripe Connect only;
/// the abstraction exists so that "add PayPal" is an adapter, not a
/// redesign (proven by the fake provider in tests, sub-phase 09.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    StripeConnect,
}

impl ProviderKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::StripeConnect => "stripe_connect",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "stripe_connect" => Some(Self::StripeConnect),
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_values_round_trip_the_schema_check_constraint() {
        // Must stay in lockstep with the CHECK on
        // payments.provider_connections.status — one source of truth.
        for status in [
            ConnectionStatus::Pending,
            ConnectionStatus::Onboarding,
            ConnectionStatus::Restricted,
            ConnectionStatus::Active,
            ConnectionStatus::Rejected,
            ConnectionStatus::Disconnected,
        ] {
            assert_eq!(ConnectionStatus::parse(status.as_str()), Some(status));
        }
        assert_eq!(ConnectionStatus::parse("deleted"), None);
    }

    #[test]
    fn provider_kinds_round_trip() {
        assert_eq!(
            ProviderKind::parse(ProviderKind::StripeConnect.as_str()),
            Some(ProviderKind::StripeConnect)
        );
        assert_eq!(ProviderKind::parse("paypal"), None);
    }
}
