//! The tenant aggregate: identity, lifecycle state machine and invariants.

use chrono::{DateTime, Utc};
pub use sanvi_kernel::{TenantId, TenantTag};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::domain::values::{DataRegion, Locale, SuspensionReason, TenantName, TenantSlug};
use crate::domain::{TenancyError, events::TenantEvent};

/// Tenant lifecycle:
/// `provisioning → active → suspended → archived → purged` with
/// `suspended → active` (resume) and `suspended → archived`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum TenantStatus {
    Provisioning,
    Active,
    Suspended,
    Archived,
    Purged,
}

impl TenantStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Provisioning => "provisioning",
            Self::Active => "active",
            Self::Suspended => "suspended",
            Self::Archived => "archived",
            Self::Purged => "purged",
        }
    }

    /// Parse the stored (snake_case) representation.
    pub fn parse(input: &str) -> Result<Self, TenancyError> {
        match input {
            "provisioning" => Ok(Self::Provisioning),
            "active" => Ok(Self::Active),
            "suspended" => Ok(Self::Suspended),
            "archived" => Ok(Self::Archived),
            "purged" => Ok(Self::Purged),
            other => Err(TenancyError::InvalidStatus(format!(
                "unknown tenant status: {other}"
            ))),
        }
    }

    /// Only active tenants serve traffic; suspended keeps admin/billing
    /// (enforced by the resolution middleware's allow-list).
    pub fn serves_traffic(self) -> bool {
        self == Self::Active
    }
}

/// The tenant aggregate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tenant {
    id: TenantId,
    slug: TenantSlug,
    display_name: TenantName,
    status: TenantStatus,
    region: DataRegion,
    default_locale: Locale,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    archived_at: Option<DateTime<Utc>>,
    purge_requested_at: Option<DateTime<Utc>>,
    suspended_reason: Option<SuspensionReason>,
}

impl Tenant {
    /// Create a tenant in `Provisioning`. Emits `TenantSlugReserved` and
    /// `TenantProvisioned`.
    pub fn provision(
        id: TenantId,
        slug: TenantSlug,
        display_name: TenantName,
        region: DataRegion,
        default_locale: Locale,
        now: DateTime<Utc>,
    ) -> (Self, Vec<TenantEvent>) {
        let tenant = Self {
            id,
            slug,
            display_name,
            status: TenantStatus::Provisioning,
            region,
            default_locale,
            created_at: now,
            updated_at: now,
            archived_at: None,
            purge_requested_at: None,
            suspended_reason: None,
        };
        let events = vec![
            TenantEvent::SlugReserved {
                tenant_id: tenant.id,
                slug: tenant.slug.clone(),
            },
            TenantEvent::Provisioned {
                tenant_id: tenant.id,
                slug: tenant.slug.clone(),
                region: tenant.region,
            },
        ];
        (tenant, events)
    }

    /// `provisioning → active`.
    pub fn activate(&mut self, now: DateTime<Utc>) -> Result<TenantEvent, TenancyError> {
        self.require_status(TenantStatus::Provisioning, TenantStatus::Active)?;
        self.status = TenantStatus::Active;
        self.updated_at = now;
        Ok(TenantEvent::Activated {
            tenant_id: self.id,
            slug: self.slug.clone(),
        })
    }

    /// `active → suspended` (records the machine-readable reason).
    pub fn suspend(
        &mut self,
        reason: SuspensionReason,
        now: DateTime<Utc>,
    ) -> Result<TenantEvent, TenancyError> {
        self.require_status(TenantStatus::Active, TenantStatus::Suspended)?;
        self.status = TenantStatus::Suspended;
        self.suspended_reason = Some(reason);
        self.updated_at = now;
        Ok(TenantEvent::Suspended {
            tenant_id: self.id,
            slug: self.slug.clone(),
            reason,
        })
    }

    /// `suspended → active`.
    pub fn resume(&mut self, now: DateTime<Utc>) -> Result<TenantEvent, TenancyError> {
        self.require_status(TenantStatus::Suspended, TenantStatus::Active)?;
        self.status = TenantStatus::Active;
        self.suspended_reason = None;
        self.updated_at = now;
        Ok(TenantEvent::Resumed {
            tenant_id: self.id,
            slug: self.slug.clone(),
        })
    }

    /// `active|suspended → archived`. Archived tenants are read-only and
    /// schedule a purge (the worker sweep emits `TenantPurgeRequested` once
    /// the retention window has passed).
    pub fn archive(&mut self, now: DateTime<Utc>) -> Result<TenantEvent, TenancyError> {
        match self.status {
            TenantStatus::Active | TenantStatus::Suspended => {}
            other => {
                return Err(TenancyError::IllegalTransition {
                    from: other.as_str(),
                    to: TenantStatus::Archived.as_str(),
                });
            }
        }
        self.status = TenantStatus::Archived;
        self.archived_at = Some(now);
        self.suspended_reason = None;
        self.updated_at = now;
        Ok(TenantEvent::Archived {
            tenant_id: self.id,
            slug: self.slug.clone(),
        })
    }

    /// Request purge of an archived tenant (idempotent: at most one request
    /// per tenant — the sweep skips tenants with `purge_requested_at` set).
    pub fn request_purge(
        &mut self,
        now: DateTime<Utc>,
    ) -> Result<Option<TenantEvent>, TenancyError> {
        self.require_status(TenantStatus::Archived, TenantStatus::Purged)?;
        if self.purge_requested_at.is_some() {
            return Ok(None);
        }
        self.purge_requested_at = Some(now);
        self.updated_at = now;
        Ok(Some(TenantEvent::PurgeRequested {
            tenant_id: self.id,
            slug: self.slug.clone(),
        }))
    }

    /// Final transition: `archived → purged` once actual deletion has been
    /// carried out (phase 05 erasure).
    pub fn mark_purged(&mut self, now: DateTime<Utc>) -> Result<TenantEvent, TenancyError> {
        self.require_status(TenantStatus::Archived, TenantStatus::Purged)?;
        self.status = TenantStatus::Purged;
        self.updated_at = now;
        Ok(TenantEvent::Purged {
            tenant_id: self.id,
            slug: self.slug.clone(),
        })
    }

    fn require_status(&self, from: TenantStatus, to: TenantStatus) -> Result<(), TenancyError> {
        if self.status != from {
            return Err(TenancyError::IllegalTransition {
                from: self.status.as_str(),
                to: to.as_str(),
            });
        }
        Ok(())
    }

    pub fn id(&self) -> TenantId {
        self.id
    }

    pub fn slug(&self) -> &TenantSlug {
        &self.slug
    }

    pub fn display_name(&self) -> &TenantName {
        &self.display_name
    }

    pub fn status(&self) -> TenantStatus {
        self.status
    }

    pub fn region(&self) -> DataRegion {
        self.region
    }

    pub fn default_locale(&self) -> &Locale {
        &self.default_locale
    }

    pub fn created_at(&self) -> DateTime<Utc> {
        self.created_at
    }

    pub fn updated_at(&self) -> DateTime<Utc> {
        self.updated_at
    }

    pub fn archived_at(&self) -> Option<DateTime<Utc>> {
        self.archived_at
    }

    pub fn purge_requested_at(&self) -> Option<DateTime<Utc>> {
        self.purge_requested_at
    }

    pub fn suspended_reason(&self) -> Option<SuspensionReason> {
        self.suspended_reason
    }

    /// Rehydrate a tenant from storage (repository use only; validates the
    /// invariants a stored row must satisfy).
    pub fn from_parts(
        id: TenantId,
        slug: TenantSlug,
        display_name: TenantName,
        status: TenantStatus,
        region: DataRegion,
        default_locale: Locale,
        timestamps: TenantTimestamps,
        suspended_reason: Option<SuspensionReason>,
    ) -> Result<Self, TenancyError> {
        if status == TenantStatus::Suspended && suspended_reason.is_none() {
            return Err(TenancyError::InvalidSuspensionReason(
                "suspended tenant without a reason".to_string(),
            ));
        }
        Ok(Self {
            id,
            slug,
            display_name,
            status,
            region,
            default_locale,
            created_at: timestamps.created_at,
            updated_at: timestamps.updated_at,
            archived_at: timestamps.archived_at,
            purge_requested_at: timestamps.purge_requested_at,
            suspended_reason,
        })
    }
}

/// Storage timestamps carried by a tenant row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TenantTimestamps {
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub archived_at: Option<DateTime<Utc>>,
    pub purge_requested_at: Option<DateTime<Utc>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-08-16T10:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    fn fixture() -> (Tenant, Vec<TenantEvent>) {
        Tenant::provision(
            TenantId::try_from(101_i64).unwrap(),
            TenantSlug::parse("acme").unwrap(),
            TenantName::parse("Acme Corp").unwrap(),
            DataRegion::Us,
            Locale::parse("en").unwrap(),
            now(),
        )
    }

    #[test]
    fn provisioning_emits_reservation_and_creation_events() {
        let (tenant, events) = fixture();
        assert_eq!(tenant.status(), TenantStatus::Provisioning);
        assert_eq!(events.len(), 2);
        assert!(matches!(events[0], TenantEvent::SlugReserved { .. }));
        assert!(matches!(events[1], TenantEvent::Provisioned { .. }));
    }

    #[test]
    fn happy_path_lifecycle() {
        let (mut tenant, _) = fixture();
        tenant.activate(now()).unwrap();
        assert!(tenant.status().serves_traffic());

        tenant.suspend(SuspensionReason::Billing, now()).unwrap();
        assert_eq!(tenant.status(), TenantStatus::Suspended);
        assert_eq!(tenant.suspended_reason(), Some(SuspensionReason::Billing));
        assert!(!tenant.status().serves_traffic());

        tenant.resume(now()).unwrap();
        assert_eq!(tenant.status(), TenantStatus::Active);
        assert_eq!(tenant.suspended_reason(), None);

        tenant.archive(now()).unwrap();
        assert_eq!(tenant.status(), TenantStatus::Archived);
        assert!(tenant.archived_at().is_some());

        // Purge request is idempotent: exactly one event.
        let first = tenant.request_purge(now()).unwrap();
        assert!(first.is_some());
        let second = tenant.request_purge(now()).unwrap();
        assert!(second.is_none());

        tenant.mark_purged(now()).unwrap();
        assert_eq!(tenant.status(), TenantStatus::Purged);
    }

    #[test]
    fn suspend_also_legal_from_archived_suspension_path() {
        let (mut tenant, _) = fixture();
        tenant.activate(now()).unwrap();
        tenant.suspend(SuspensionReason::Abuse, now()).unwrap();
        // suspended → archived is legal.
        tenant.archive(now()).unwrap();
        assert_eq!(tenant.status(), TenantStatus::Archived);
    }

    #[test]
    fn illegal_transitions_are_rejected() {
        let (mut tenant, _) = fixture();
        assert!(matches!(
            tenant.suspend(SuspensionReason::Abuse, now()),
            Err(TenancyError::IllegalTransition { .. })
        ));
        assert!(matches!(
            tenant.archive(now()),
            Err(TenancyError::IllegalTransition { .. })
        ));
        assert!(matches!(
            tenant.resume(now()),
            Err(TenancyError::IllegalTransition { .. })
        ));
        assert!(matches!(
            tenant.mark_purged(now()),
            Err(TenancyError::IllegalTransition { .. })
        ));
    }

    #[test]
    fn rehydration_rejects_suspended_without_reason() {
        let (tenant, _) = fixture();
        assert!(
            Tenant::from_parts(
                tenant.id,
                tenant.slug,
                tenant.display_name,
                TenantStatus::Suspended,
                tenant.region,
                tenant.default_locale,
                TenantTimestamps {
                    created_at: tenant.created_at,
                    updated_at: tenant.updated_at,
                    archived_at: tenant.archived_at,
                    purge_requested_at: tenant.purge_requested_at,
                },
                None,
            )
            .is_err()
        );
    }
}
