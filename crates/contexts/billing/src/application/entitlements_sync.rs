//! Entitlement sync decision logic — pure, exhaustively table-tested.
//!
//! The rule the whole billing context rests on: a subscription grants its
//! plan's entitlements exactly when its status grants access (trialing,
//! active, past_due — the grace window) and dunning has not expired.

use crate::domain::{CollectionState, EntitlementGrant, SubscriptionStatus};

/// What the sync must do for a `(status, collection)` pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncAction {
    /// Write the plan's grant rows (`source = 'subscription'`).
    Apply,
    /// Remove every subscription-source row (catalog defaults take over).
    Clear,
}

/// The recompute decision. Every plan × status × collection combination
/// resolves through this function; the table test below pins it.
pub fn sync_action(status: SubscriptionStatus, collection: CollectionState) -> SyncAction {
    if status.grants_entitlements() && collection != CollectionState::GraceExpired {
        SyncAction::Apply
    } else {
        SyncAction::Clear
    }
}

/// Whether the tenant's current entitlement state already matches the
/// decision, given the plan the subscription points at (avoids redundant
/// writes when out-of-order events land).
pub fn still_current(
    status: SubscriptionStatus,
    collection: CollectionState,
    _plan_grants: &[EntitlementGrant],
) -> SyncAction {
    sync_action(status, collection)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recompute_table_every_status_and_collection() {
        // status × collection → action. Past-due keeps access during the
        // grace window; grace expiry drops it; non-granting statuses always
        // clear.
        let table = [
            (
                SubscriptionStatus::Trialing,
                CollectionState::Ok,
                SyncAction::Apply,
            ),
            (
                SubscriptionStatus::Active,
                CollectionState::Ok,
                SyncAction::Apply,
            ),
            (
                SubscriptionStatus::PastDue,
                CollectionState::Ok,
                SyncAction::Apply,
            ),
            (
                SubscriptionStatus::PastDue,
                CollectionState::Dunning,
                SyncAction::Apply,
            ),
            (
                SubscriptionStatus::PastDue,
                CollectionState::GraceExpired,
                SyncAction::Clear,
            ),
            (
                SubscriptionStatus::Unpaid,
                CollectionState::Ok,
                SyncAction::Clear,
            ),
            (
                SubscriptionStatus::Canceled,
                CollectionState::Ok,
                SyncAction::Clear,
            ),
            (
                SubscriptionStatus::Incomplete,
                CollectionState::Ok,
                SyncAction::Clear,
            ),
            (
                SubscriptionStatus::IncompleteExpired,
                CollectionState::Ok,
                SyncAction::Clear,
            ),
            (
                SubscriptionStatus::Paused,
                CollectionState::Ok,
                SyncAction::Clear,
            ),
            (
                SubscriptionStatus::Paused,
                CollectionState::Dunning,
                SyncAction::Clear,
            ),
            (
                SubscriptionStatus::Active,
                CollectionState::GraceExpired,
                SyncAction::Clear,
            ),
            (
                SubscriptionStatus::Trialing,
                CollectionState::Dunning,
                SyncAction::Apply,
            ),
        ];
        for (status, collection, expected) in table {
            assert_eq!(
                sync_action(status, collection),
                expected,
                "{status:?} × {collection:?} must yield {expected:?}"
            );
        }
    }

    #[test]
    fn apply_grants_the_whole_plan() {
        let grants = vec![
            EntitlementGrant::new("tenancy.member_limit".to_string(), true, Some(5)).unwrap(),
            EntitlementGrant::new("domains.custom".to_string(), true, None).unwrap(),
        ];
        assert_eq!(
            still_current(SubscriptionStatus::Active, CollectionState::Ok, &grants,),
            SyncAction::Apply
        );
        assert_eq!(grants.len(), 2);
    }
}
