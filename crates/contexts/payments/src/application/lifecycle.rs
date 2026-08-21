//! Connection-lifecycle application: the one path a provider snapshot
//! becomes stored truth.
//!
//! Webhooks (the fast path) and the reconciliation job (the safety net)
//! both end here, so live events and repairs can never disagree about
//! semantics — the same property billing's dispatcher guarantees by
//! routing its reconciler through `apply_subscription_update`.
//!
//! The decision table is deliberately small:
//!
//! - `card_payments` active → `Active` (via `Onboarding` first when still
//!   `Pending`, because the state machine demands it);
//! - requirements past due while live → `Restricted` (regression);
//! - otherwise refresh the projection in place; status stays.
//!
//! `Active` is only ever derived from the v2 capability check
//! ([`CapabilitySet::is_card_payments_active`]) — never from a deprecated
//! provider boolean (CI-gated repo-wide).

use chrono::{DateTime, Utc};

use crate::domain::connection::ProviderConnection;
use crate::domain::error::PaymentsError;
use crate::domain::events::{EventPublisher, PaymentsEvent};
use crate::domain::ports::{ConnectionLifecycleStore, ConnectionNotifier, ProviderAccountStatus};
use crate::domain::provider::ConnectionStatus;

/// How long before an outstanding requirement's deadline the tenant-admin
/// reminder fires (while the connection still cannot take payments).
pub const DEADLINE_REMINDER_WINDOW_DAYS: i64 = 7;

/// The status steps the snapshot implies for a connection currently in
/// `current`. Empty means "no transition"; at most two steps
/// (`Pending → Onboarding → Active`) are ever produced.
pub fn target_steps(
    current: ConnectionStatus,
    snapshot: &ProviderAccountStatus,
) -> Vec<ConnectionStatus> {
    // Terminal states have no lifecycle left — no snapshot revives them
    // here (disconnect/rejection flows own those paths).
    if matches!(
        current,
        ConnectionStatus::Disconnected | ConnectionStatus::Rejected
    ) {
        return vec![];
    }
    if snapshot.capabilities.is_card_payments_active() {
        return match current {
            ConnectionStatus::Active => vec![],
            // The machine forbids Pending → Active directly: onboarding is
            // where the tenant actually was, even if no webhook said so.
            ConnectionStatus::Pending => {
                vec![ConnectionStatus::Onboarding, ConnectionStatus::Active]
            }
            _ => vec![ConnectionStatus::Active],
        };
    }
    if !snapshot.requirements.past_due.is_empty()
        && matches!(
            current,
            ConnectionStatus::Onboarding | ConnectionStatus::Active
        )
    {
        return vec![ConnectionStatus::Restricted];
    }
    // Requirements appeared on a brand-new account: onboarding has begun,
    // even if the tenant never minted a session through us.
    if current == ConnectionStatus::Pending && !snapshot.requirements.is_empty() {
        return vec![ConnectionStatus::Onboarding];
    }
    vec![]
}

/// What one snapshot application accomplished.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppliedTransition {
    /// Status moved or the projection changed beyond timestamps:
    /// persisted and worth reporting.
    Changed,
    /// Only the sync anchor moved (`last_synced_at`) — the provider
    /// agreed with what we already store. Persisted (the anchor must
    /// advance) but not counted as a repair.
    Touched,
    /// Nothing to do — even the anchor already agreed.
    Unchanged,
}

/// Apply a provider snapshot to a stored connection inside `scope`: walk
/// the implied transitions through the domain's state machine, persist,
/// emit `ProviderConnectionActivated` / `ProviderConnectionRestricted`,
/// and notify the tenant's admins. `observed_at` is the event's own time
/// for webhook-driven calls (the out-of-order anchor) or now for syncs.
///
/// The caller is responsible for the stale-event check *before* fetching;
/// by the time a snapshot exists it is authoritative.
#[allow(clippy::too_many_arguments)]
pub async fn apply_account_snapshot(
    store: &dyn ConnectionLifecycleStore,
    events: &dyn EventPublisher,
    notifier: &dyn ConnectionNotifier,
    scope: &mut sanvi_persistence::PlatformScope<'_>,
    connection: &ProviderConnection,
    snapshot: &ProviderAccountStatus,
    observed_at: DateTime<Utc>,
) -> Result<AppliedTransition, PaymentsError> {
    let mut updated = connection.clone();
    let mut activated = false;
    let mut restricted = false;

    let steps = target_steps(connection.status, snapshot);
    for step in &steps {
        updated.apply_status(snapshot, *step, observed_at)?;
        match step {
            ConnectionStatus::Active => activated = true,
            ConnectionStatus::Restricted => restricted = true,
            _ => {}
        }
    }
    // No transition (or besides one): still refresh capabilities and
    // requirements so the settings page tells the truth between statuses.
    // (`apply_status` already projected the snapshot on transition paths.)
    if steps.is_empty() {
        updated.refresh_snapshot(snapshot, observed_at);
    }

    // A moved `last_synced_at` still persists (the out-of-order anchor and
    // the reconciliation cadence both advance with the observation), but a
    // timestamp-only difference is reported as
    // [`AppliedTransition::Touched`] so callers can tell real repairs from
    // idle passes.
    let significant = updated.status != connection.status
        || updated.capabilities != connection.capabilities
        || updated.requirements != connection.requirements
        || updated.country != connection.country
        || updated.default_currency != connection.default_currency
        || updated.connected_at != connection.connected_at
        || updated.disconnected_at != connection.disconnected_at;
    if !significant && updated.last_synced_at == connection.last_synced_at {
        return Ok(AppliedTransition::Unchanged);
    }

    store.save(scope, &updated).await?;

    let mut outbox = Vec::new();
    if activated {
        outbox.push(PaymentsEvent::ProviderConnectionActivated {
            connection_id: updated.id,
            provider: updated.provider.as_str().to_string(),
        });
    }
    if restricted {
        outbox.push(PaymentsEvent::ProviderConnectionRestricted {
            connection_id: updated.id,
            provider: updated.provider.as_str().to_string(),
            detail: restriction_detail(&updated),
        });
    }
    if !outbox.is_empty() {
        events.publish(scope, updated.tenant_id, &outbox).await?;
    }

    // Notifications are best-effort: a committed transition is never
    // rolled back because the mailer sneezed.
    if activated
        && let Err(err) = notifier
            .connection_activated(updated.tenant_id, updated.id)
            .await
    {
        tracing::error!(connection_id = %updated.id, error = %err, "activation notification failed");
    }
    if restricted {
        if let Err(err) = notifier
            .connection_restricted(updated.tenant_id, updated.id, &restriction_detail(&updated))
            .await
        {
            tracing::error!(connection_id = %updated.id, error = %err, "restriction notification failed");
        }
    } else if significant {
        // Deadline reminders ride real projection changes (and sync passes
        // that observe an approaching deadline), not every idle touch.
        let deadline_reminder = updated.requirement_summary().deadline.filter(|deadline| {
            *deadline > observed_at
                && *deadline - observed_at <= chrono::Duration::days(DEADLINE_REMINDER_WINDOW_DAYS)
                && updated.status != ConnectionStatus::Active
        });
        if let Some(deadline) = deadline_reminder
            && let Err(err) = notifier
                .requirement_deadline_approaching(updated.tenant_id, updated.id, deadline)
                .await
        {
            tracing::error!(connection_id = %updated.id, error = %err, "deadline reminder failed");
        }
    }

    Ok(if significant {
        AppliedTransition::Changed
    } else {
        AppliedTransition::Touched
    })
}

/// What to tell the tenant (and the audit trail) about a restriction:
/// the outstanding past-due codes, or the capability status when the
/// requirement lists are empty.
fn restriction_detail(connection: &ProviderConnection) -> String {
    let requirements = connection.requirement_summary();
    if !requirements.past_due.is_empty() {
        format!("past due: {}", requirements.past_due.join(", "))
    } else {
        format!(
            "card_payments is {}",
            connection
                .capability_set()
                .card_payments()
                .map(|s| s.as_str())
                .unwrap_or("absent")
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::capability::{CapabilitySet, CapabilityStatus, RequirementSummary};
    use crate::domain::money::Currency;
    use crate::domain::ports::ProviderAccountStatus;
    use crate::domain::provider::CountryCode;

    fn snapshot(card_payments: CapabilityStatus, past_due: &[&str]) -> ProviderAccountStatus {
        ProviderAccountStatus {
            external_account_id: "acct_test".to_string(),
            capabilities: CapabilitySet::empty().with("card_payments", card_payments),
            requirements: RequirementSummary {
                past_due: past_due.iter().map(|s| s.to_string()).collect(),
                ..RequirementSummary::default()
            },
            country: Some(CountryCode::parse("JP").unwrap()),
            default_currency: Some(Currency::parse("JPY").unwrap()),
            status_hint: None,
        }
    }

    #[test]
    fn the_decision_table_covers_the_lifecycle() {
        use ConnectionStatus::*;

        // Activation paths.
        assert_eq!(
            target_steps(Onboarding, &snapshot(CapabilityStatus::Active, &[])),
            [Active]
        );
        assert_eq!(
            target_steps(Restricted, &snapshot(CapabilityStatus::Active, &[])),
            [Active]
        );
        // Pending must pass through Onboarding — the machine says so.
        assert_eq!(
            target_steps(Pending, &snapshot(CapabilityStatus::Active, &[])),
            [Onboarding, Active]
        );
        // Already there: nothing to do.
        assert_eq!(
            target_steps(Active, &snapshot(CapabilityStatus::Active, &[])),
            []
        );

        // Regression: past-due requirements restrict a live account.
        assert_eq!(
            target_steps(
                Active,
                &snapshot(CapabilityStatus::Inactive, &["individual.id_number"])
            ),
            [Restricted]
        );
        assert_eq!(
            target_steps(
                Onboarding,
                &snapshot(CapabilityStatus::Pending, &["individual.id_number"])
            ),
            [Restricted]
        );

        // Merely-due requirements do not restrict; onboarding stays.
        assert_eq!(
            target_steps(Onboarding, &snapshot(CapabilityStatus::Pending, &[])),
            []
        );

        // A fresh account with requirements: onboarding began.
        let mut with_requirements = snapshot(CapabilityStatus::Pending, &[]);
        with_requirements.requirements.currently_due = vec!["business_profile.url".to_string()];
        assert_eq!(target_steps(Pending, &with_requirements), [Onboarding]);

        // Terminal states are never revived here.
        assert_eq!(
            target_steps(Disconnected, &snapshot(CapabilityStatus::Active, &[])),
            []
        );
        assert_eq!(
            target_steps(Rejected, &snapshot(CapabilityStatus::Active, &[])),
            []
        );
    }

    #[test]
    fn activation_is_derived_only_from_the_v2_capability_check() {
        // A snapshot whose card_payments capability is not active never
        // produces an Active step, whatever else it claims.
        for status in [CapabilityStatus::Pending, CapabilityStatus::Inactive] {
            let steps = target_steps(ConnectionStatus::Onboarding, &snapshot(status, &[]));
            assert!(!steps.contains(&ConnectionStatus::Active));
        }
    }
}
