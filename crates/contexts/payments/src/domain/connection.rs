//! The provider-connection aggregate.
//!
//! One row per (tenant, provider) while connected — enforced by the
//! partial unique index `one_active_provider_per_tenant` on
//! `disconnected_at IS NULL`. The aggregate carries identifiers and status
//! only: capabilities/requirements arrive as the provider's own JSON
//! (mapped views land in 09.2), and no onboarding PII is ever stored here.

use chrono::{DateTime, Utc};
use sanvi_kernel::{Id, TenantId};
use serde_json::Value;

use super::capability::{CapabilitySet, RequirementSummary};
use super::error::PaymentsError;
use super::ports::ProviderAccountStatus;
use super::provider::{ConnectionStatus, ProviderKind};

pub enum ConnectionTag {}
pub type ConnectionId = Id<ConnectionTag>;

/// A tenant's connection to a payment provider.
#[derive(Debug, Clone, PartialEq)]
pub struct ProviderConnection {
    pub id: ConnectionId,
    pub tenant_id: TenantId,
    pub provider: ProviderKind,
    /// The provider's account identifier (`acct_…` for Stripe).
    pub external_account_id: String,
    pub status: ConnectionStatus,
    /// The provider's capability map, verbatim from the last sync
    /// (`{"card_payments": {"status": "active"}, …}` for Stripe v2).
    /// Read-model views over this land with 09.2/09.3.
    pub capabilities: Value,
    /// The provider's outstanding-requirements summary, verbatim.
    pub requirements: Value,
    pub country: Option<String>,
    pub default_currency: Option<String>,
    pub connected_at: Option<DateTime<Utc>>,
    pub last_synced_at: Option<DateTime<Utc>>,
    pub disconnected_at: Option<DateTime<Utc>>,
}

impl ProviderConnection {
    /// A connection ready for onboarding (09.2 creates the external
    /// account and starts here).
    pub fn pending(
        id: ConnectionId,
        tenant_id: TenantId,
        provider: ProviderKind,
        external_account_id: String,
        now: DateTime<Utc>,
    ) -> Result<Self, PaymentsError> {
        if external_account_id.trim().is_empty() {
            return Err(PaymentsError::InvalidConnection(
                "external_account_id must not be empty".to_string(),
            ));
        }
        Ok(Self {
            id,
            tenant_id,
            provider,
            external_account_id,
            status: ConnectionStatus::Pending,
            capabilities: serde_json::json!({}),
            requirements: serde_json::json!({}),
            country: None,
            default_currency: None,
            connected_at: None,
            last_synced_at: Some(now),
            disconnected_at: None,
        })
    }

    /// Apply a provider snapshot as a lifecycle transition. All three
    /// aggregate invariants meet here:
    ///
    /// 1. the transition must be legal per
    ///    [`ConnectionStatus::transition`];
    /// 2. **`Active` requires an active `card_payments` capability** — a
    ///    provider claiming activity without the capability is refused,
    ///    not recorded;
    /// 3. becoming `Active` stamps `connected_at` exactly once; becoming
    ///    `Disconnected` stamps `disconnected_at` exactly once.
    ///
    /// Webhook drivers (09.3) call this and trust it to have opinions.
    pub fn apply_status(
        &mut self,
        snapshot: &ProviderAccountStatus,
        next: ConnectionStatus,
        now: DateTime<Utc>,
    ) -> Result<(), PaymentsError> {
        ConnectionStatus::transition(self.status, next)?;
        if next == ConnectionStatus::Active && !snapshot.capabilities.is_card_payments_active() {
            return Err(PaymentsError::CapabilityMissing {
                capability: "card_payments".to_string(),
                detail: format!(
                    "cannot mark {} active: card_payments capability is {}",
                    self.external_account_id,
                    snapshot
                        .capabilities
                        .card_payments()
                        .map(|s| s.as_str())
                        .unwrap_or("absent")
                ),
            });
        }
        if next == ConnectionStatus::Active && self.connected_at.is_none() {
            self.connected_at = Some(now);
        }
        if next == ConnectionStatus::Disconnected && self.disconnected_at.is_none() {
            self.disconnected_at = Some(now);
        }
        self.status = next;
        self.capabilities = snapshot.capabilities.to_provider_json();
        self.requirements =
            serde_json::to_value(&snapshot.requirements).expect("requirement summaries serialize");
        self.country = snapshot.country.as_ref().map(|c| c.as_str().to_string());
        self.default_currency = snapshot
            .default_currency
            .as_ref()
            .map(|c| c.as_str().to_string());
        self.last_synced_at = Some(now);
        Ok(())
    }

    /// Disconnect, guarded: the transition rules decide eligibility, and
    /// the caller (09.7) is responsible for the in-flight-payment and
    /// open-dispute guards that surround this.
    pub fn disconnect(&mut self, now: DateTime<Utc>) -> Result<(), PaymentsError> {
        let next = ConnectionStatus::Disconnected;
        ConnectionStatus::transition(self.status, next)?;
        if self.disconnected_at.is_none() {
            self.disconnected_at = Some(now);
        }
        self.status = next;
        self.last_synced_at = Some(now);
        Ok(())
    }

    /// Out-of-order guard (NFR-905): an event older than the last state
    /// we already applied is dropped, never re-applied.
    ///
    /// `last_synced_at` carries *provider* time after a webhook-driven
    /// update (the event's own `created`), and wall-clock time after a
    /// full sync — which observed every event created before it, so
    /// dropping those remains correct. Comparison is strictly older;
    /// same-second events are applied (redeliveries are already deduped
    /// by event id).
    pub fn is_event_stale(&self, event_created: DateTime<Utc>) -> bool {
        self.last_synced_at.is_some_and(|last| event_created < last)
    }

    /// Refresh the provider-state projection (capabilities, requirements,
    /// country, currency) **without** a status transition — requirements
    /// move while onboarding stays onboarding. Returns whether anything
    /// changed; `last_synced_at` moves to `now` either way so the
    /// reconciliation job's "not seen recently" cadence and the stale
    /// guard both advance.
    pub fn refresh_snapshot(
        &mut self,
        snapshot: &ProviderAccountStatus,
        now: DateTime<Utc>,
    ) -> bool {
        let capabilities = snapshot.capabilities.to_provider_json();
        let requirements =
            serde_json::to_value(&snapshot.requirements).expect("requirement summaries serialize");
        let country = snapshot.country.as_ref().map(|c| c.as_str().to_string());
        let default_currency = snapshot
            .default_currency
            .as_ref()
            .map(|c| c.as_str().to_string());
        let changed = self.capabilities != capabilities
            || self.requirements != requirements
            || self.country != country
            || self.default_currency != default_currency;
        self.capabilities = capabilities;
        self.requirements = requirements;
        self.country = country;
        self.default_currency = default_currency;
        self.last_synced_at = Some(now);
        changed
    }

    /// The typed capability view over the stored provider JSON.
    pub fn capability_set(&self) -> CapabilitySet {
        CapabilitySet::from_provider_json(&self.capabilities)
    }

    /// The typed requirements view over the stored provider JSON.
    pub fn requirement_summary(&self) -> RequirementSummary {
        RequirementSummary::from_provider_json(&self.requirements)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::capability::CapabilityStatus;
    use crate::domain::money::Currency;
    use crate::domain::provider::CountryCode;

    fn snapshot(card_payments: Option<CapabilityStatus>) -> ProviderAccountStatus {
        let capabilities = match card_payments {
            Some(status) => CapabilitySet::empty().with("card_payments", status),
            None => CapabilitySet::empty(),
        };
        ProviderAccountStatus {
            external_account_id: "acct_test".to_string(),
            capabilities,
            requirements: RequirementSummary {
                currently_due: vec!["business_profile.url".to_string()],
                ..RequirementSummary::default()
            },
            country: Some(CountryCode::parse("JP").unwrap()),
            default_currency: Some(Currency::parse("JPY").unwrap()),
            status_hint: None,
        }
    }

    fn onboarding() -> ProviderConnection {
        let mut conn = ProviderConnection::pending(
            ConnectionId::try_from(1_i64).unwrap(),
            TenantId::try_from(2_i64).unwrap(),
            ProviderKind::StripeConnect,
            "acct_test".to_string(),
            Utc::now(),
        )
        .unwrap();
        conn.apply_status(&snapshot(None), ConnectionStatus::Onboarding, Utc::now())
            .unwrap();
        conn
    }

    #[test]
    fn active_requires_an_active_card_payments_capability() {
        let mut conn = onboarding();

        // Without the capability: refused with the capability error.
        let err = conn
            .apply_status(&snapshot(None), ConnectionStatus::Active, Utc::now())
            .unwrap_err();
        assert!(matches!(err, PaymentsError::CapabilityMissing { .. }));

        // Pending capability is not enough either.
        let err = conn
            .apply_status(
                &snapshot(Some(CapabilityStatus::Pending)),
                ConnectionStatus::Active,
                Utc::now(),
            )
            .unwrap_err();
        assert!(matches!(err, PaymentsError::CapabilityMissing { .. }));

        // With it: allowed, connected_at stamped, snapshot projected.
        conn.apply_status(
            &snapshot(Some(CapabilityStatus::Active)),
            ConnectionStatus::Active,
            Utc::now(),
        )
        .unwrap();
        assert_eq!(conn.status, ConnectionStatus::Active);
        assert!(conn.connected_at.is_some());
        assert!(conn.capability_set().is_card_payments_active());
        assert_eq!(
            conn.requirement_summary().currently_due,
            ["business_profile.url"]
        );
        assert_eq!(conn.country.as_deref(), Some("JP"));
        assert_eq!(conn.default_currency.as_deref(), Some("JPY"));
    }

    #[test]
    fn illegal_transitions_are_refused_without_touching_the_aggregate() {
        let mut conn = onboarding();
        let before = conn.clone();

        // Pending → Restricted is illegal (never entered onboarding).
        let mut fresh = ProviderConnection::pending(
            ConnectionId::try_from(3_i64).unwrap(),
            TenantId::try_from(4_i64).unwrap(),
            ProviderKind::Fake,
            "acct_fake_1".to_string(),
            Utc::now(),
        )
        .unwrap();
        assert!(
            fresh
                .apply_status(&snapshot(None), ConnectionStatus::Restricted, Utc::now())
                .is_err()
        );
        assert_eq!(fresh.status, ConnectionStatus::Pending);
        assert!(fresh.disconnected_at.is_none());

        assert!(
            conn.apply_status(&snapshot(None), ConnectionStatus::Pending, Utc::now())
                .is_err()
        );
        assert_eq!(conn, before, "a refused transition changes nothing");
    }

    #[test]
    fn disconnect_is_guarded_and_stamps_once() {
        let mut conn = onboarding();
        conn.apply_status(
            &snapshot(Some(CapabilityStatus::Active)),
            ConnectionStatus::Active,
            Utc::now(),
        )
        .unwrap();
        conn.disconnect(Utc::now()).unwrap();
        assert_eq!(conn.status, ConnectionStatus::Disconnected);
        let stamped = conn.disconnected_at;
        assert!(stamped.is_some());
        // Terminal: a second disconnect is an illegal transition.
        assert!(conn.disconnect(Utc::now()).is_err());
        assert_eq!(conn.disconnected_at, stamped);
    }

    #[test]
    fn stale_events_are_dropped_fresh_ones_are_not() {
        let t0 = Utc::now();
        // A fresh Pending row carries last_synced_at = its creation time.
        let mut conn = ProviderConnection::pending(
            ConnectionId::try_from(5_i64).unwrap(),
            TenantId::try_from(6_i64).unwrap(),
            ProviderKind::Fake,
            "acct_stale".to_string(),
            t0,
        )
        .unwrap();
        assert!(conn.is_event_stale(t0 - chrono::Duration::seconds(1)));
        // Same second (a distinct event, redelivery is deduped by id): not stale.
        assert!(!conn.is_event_stale(t0));
        assert!(!conn.is_event_stale(t0 + chrono::Duration::seconds(1)));

        // Applying an event moves the anchor to that event's provider time.
        conn.apply_status(
            &snapshot(None),
            ConnectionStatus::Onboarding,
            t0 + chrono::Duration::seconds(5),
        )
        .unwrap();
        assert!(conn.is_event_stale(t0 + chrono::Duration::seconds(4)));
        assert!(!conn.is_event_stale(t0 + chrono::Duration::seconds(5)));

        // A never-synced connection accepts anything.
        let fresh = ProviderConnection {
            last_synced_at: None,
            ..ProviderConnection::pending(
                ConnectionId::try_from(7_i64).unwrap(),
                TenantId::try_from(8_i64).unwrap(),
                ProviderKind::Fake,
                "acct_stale_2".to_string(),
                t0,
            )
            .unwrap()
        };
        assert!(!fresh.is_event_stale(t0 - chrono::Duration::hours(1)));
    }

    #[test]
    fn refresh_snapshot_updates_projection_without_a_transition() {
        let mut conn = onboarding();
        let before = conn.clone();

        let mut updated = snapshot(None);
        updated.requirements.currently_due = vec!["individual.verification.document".to_string()];
        assert!(
            conn.refresh_snapshot(&updated, Utc::now()),
            "changed requirements report as changed"
        );
        assert_eq!(conn.status, before.status, "no transition happened");
        assert_eq!(
            conn.requirement_summary().currently_due,
            ["individual.verification.document"]
        );
        assert!(conn.last_synced_at > before.last_synced_at);

        // The same snapshot again: nothing changed, clock still advances.
        let last_synced = conn.last_synced_at;
        assert!(!conn.refresh_snapshot(&updated, Utc::now()));
        assert!(conn.last_synced_at.unwrap() >= last_synced.unwrap());
    }
}
