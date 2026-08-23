//! The disconnect flow's shared vocabulary (sub-phase 09.7).
//!
//! A tenant may always walk away from a payment provider — it is their
//! account — but not while walking away would strand money they cannot
//! reach from the provider's own dashboard without us: a payment still in
//! flight, a dispute that needs their evidence, a payout on its way to
//! their bank. Those three states are exactly [`DisconnectBlocker`], and
//! the refusal names them so the UI can say what to wait for instead of
//! "not allowed".

/// Why disconnect is refused right now. The codes are contract-stable
/// (`api/payments.yaml`) and each carries the i18n key the settings page
/// renders.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisconnectBlocker {
    /// A payment is `requires_action` or `processing` on this connection:
    /// its webhook truth must land on a live connection record.
    InFlightPayments,
    /// A dispute is open (`needs_response` / `under_review` / warning
    /// equivalents): losing the projection loses the response deadline.
    OpenDisputes,
    /// A payout is `pending`: the tenant must see it settle or fail.
    PendingPayouts,
}

impl DisconnectBlocker {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InFlightPayments => "in_flight_payments",
            Self::OpenDisputes => "open_disputes",
            Self::PendingPayouts => "pending_payouts",
        }
    }

    /// The i18n key the frontend renders this blocker from — same
    /// `payments.blocker.*` family as the checkout blockers.
    pub const fn summary_key(self) -> &'static str {
        match self {
            Self::InFlightPayments => "payments.blocker.in_flight_payments",
            Self::OpenDisputes => "payments.blocker.open_disputes",
            Self::PendingPayouts => "payments.blocker.pending_payouts",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocker_codes_are_distinct_and_keyed() {
        let all = [
            DisconnectBlocker::InFlightPayments,
            DisconnectBlocker::OpenDisputes,
            DisconnectBlocker::PendingPayouts,
        ];
        let mut codes = all.iter().map(|b| b.as_str()).collect::<Vec<_>>();
        codes.sort_unstable();
        codes.dedup();
        assert_eq!(codes.len(), all.len(), "blocker codes must be distinct");
        for blocker in all {
            assert!(blocker.summary_key().starts_with("payments.blocker."));
        }
    }
}
