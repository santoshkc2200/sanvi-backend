//! Timeline: the support artifact. A first-class projection derived
//! from stored events — never from Stripe per request — ordered by time.
//!
//! The task says: "Timeline — a first-class projection derived from
//! stored events (created, succeeded/failed, refunded, disputed,
//! paid out), each with a timestamp and an actor where one exists.
//! This is the artifact support uses; do not derive it in the UI."
//!
//! So the domain's job is a single ordered list per payment; the
//! application assembles it from the payment row, its refunds, its
//! disputes and (with 09.6) its payout events. The UI receives it
//! ordered and renders it — it never recomputes lifecycle state from
//! capability strings or refund sums.

use chrono::{DateTime, Utc};
use sanvi_kernel::UserId;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::money::Money;

/// One entry in the payment's timeline.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct TimelineEntry {
    /// Stable kind: `created`, `succeeded`, `failed`, `canceled`,
    /// `refunded`, `disputed`, `dispute_closed`, `paid_out` (09.6).
    #[schema(example = "refunded")]
    pub kind: String,
    #[schema(value_type = String)]
    pub at: DateTime<Utc>,
    /// Who caused this entry, when a human exists (refunds). Webhook-
    /// derived entries have no actor — the provider did.
    #[schema(value_type = Option<String>)]
    pub actor: Option<UserId>,
    /// Money attached to the entry (payment total, refund portion,
    /// dispute amount), when relevant.
    pub amount: Option<Money>,
    /// Human detail (failure code, refund reason, dispute reason,
    /// status detail), when relevant.
    pub detail: Option<String>,
}

/// The payment's timeline — an ordered list oldest-first.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct PaymentTimeline {
    pub entries: Vec<TimelineEntry>,
}

impl PaymentTimeline {
    pub fn empty() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    pub fn push(&mut self, entry: TimelineEntry) {
        self.entries.push(entry);
    }

    /// Sort oldest-first (webhooks may arrive out of order; the stored
    /// rows carry provider times, and the timeline's job is to look
    /// truthful regardless).
    pub fn sort(&mut self) {
        self.entries.sort_by(|a, b| a.at.cmp(&b.at));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entries_sort_oldest_first() {
        let now = Utc::now();
        let mut timeline = PaymentTimeline::empty();
        timeline.push(TimelineEntry {
            kind: "succeeded".to_string(),
            at: now + chrono::Duration::seconds(2),
            actor: None,
            amount: None,
            detail: None,
        });
        timeline.push(TimelineEntry {
            kind: "created".to_string(),
            at: now,
            actor: None,
            amount: None,
            detail: None,
        });
        timeline.sort();
        assert_eq!(timeline.entries[0].kind, "created");
        assert_eq!(timeline.entries[1].kind, "succeeded");
    }
}
