//! Audience management and scheduled sync use cases (TASK-015, slice 10.6).
//!
//! Customer lists are built and refreshed requiring BOTH `SaleOrShare` and
//! `TargetedAdvertising` processing purposes to be allowed for each data subject.
//! Opted-out subjects are excluded from a newly built list and actively removed
//! from existing ones on the next refresh pass.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use chrono::Utc;
use sanvi_kernel::TenantId;
use sanvi_persistence::TxManager;
use sanvi_privacy::domain::{DirectiveResolver, DirectiveState, ProcessingPurpose};
use serde::Serialize;

use super::retry::rebuild_subject;
use crate::adapters::AdvertisingMetrics;
use crate::domain::{
    AdvertisingConnectionRepository, AdvertisingError, Audience, AudienceId, AudienceRepository,
    AudienceStatus, AudienceSyncPort, ConnectionHandle, ConnectionStatus,
    ConversionEventRepository, HashedIdentifier, IdGenerator, PlatformKey,
};

const ACTOR: &str = "advertising-audience-sync";

/// Outcome of one audience sync sweep pass, for metrics and job logging.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct AudienceSyncReport {
    pub checked: u64,
    pub synced: u64,
    pub failed: u64,
    pub removed_identifiers: u64,
    pub unpropagated_removals: u64,
}

/// Helper function to check if an advertising error represents unsupported member removal.
pub(crate) fn is_unsupported_removal(err: &AdvertisingError) -> bool {
    match err {
        AdvertisingError::Validation(violations) => violations
            .iter()
            .any(|v| v.code == "capability_unsupported"),
        _ => false,
    }
}

/// Pure helper computing new `included` and `removed` identifier sets.
pub(crate) fn compute_audience_diff(
    current_included: &[HashedIdentifier],
    eligible_identifiers: BTreeSet<HashedIdentifier>,
) -> (Vec<HashedIdentifier>, Vec<HashedIdentifier>) {
    let included: Vec<HashedIdentifier> = eligible_identifiers.into_iter().collect();
    let included_set: BTreeSet<&HashedIdentifier> = included.iter().collect();
    let removed: Vec<HashedIdentifier> = current_included
        .iter()
        .filter(|id| !included_set.contains(id))
        .cloned()
        .collect();
    (included, removed)
}

/// Evaluate all eligible identifiers for a given tenant across all conversion events.
async fn evaluate_tenant_eligible_identifiers(
    conversion_repo: &dyn ConversionEventRepository,
    directive_resolver: &dyn DirectiveResolver,
    tx: &TxManager,
    tenant_id: TenantId,
) -> Result<BTreeSet<HashedIdentifier>, AdvertisingError> {
    let events = {
        let mut scope = tx
            .begin_for(tenant_id.as_i64())
            .await
            .map_err(|error| AdvertisingError::Storage(error.to_string()))?;
        let list = conversion_repo.list(&mut scope, tenant_id).await?;
        scope
            .commit()
            .await
            .map_err(|error| AdvertisingError::Storage(error.to_string()))?;
        list
    };

    let mut eligible_set = BTreeSet::new();
    for event in events {
        let Some(subject) = rebuild_subject(event.tenant_id, &event.subject_key) else {
            continue;
        };

        let sale_or_share_allowed = match directive_resolver
            .resolve(&subject, ProcessingPurpose::SaleOrShare)
            .await
        {
            Ok(directive) => directive.state == DirectiveState::Allowed,
            Err(_) => false,
        };
        if !sale_or_share_allowed {
            continue;
        }

        let targeted_ads_allowed = match directive_resolver
            .resolve(&subject, ProcessingPurpose::TargetedAdvertising)
            .await
        {
            Ok(directive) => directive.state == DirectiveState::Allowed,
            Err(_) => false,
        };
        if !targeted_ads_allowed {
            continue;
        }

        for id in event.hashed_identifiers.values() {
            eligible_set.insert(id.clone());
        }
    }

    Ok(eligible_set)
}

/// Shared single-audience sync routine called both by on-demand refresh and scheduled sweep.
#[allow(clippy::too_many_arguments)]
pub async fn sync_single_audience(
    tx: &TxManager,
    audience_repo: &dyn AudienceRepository,
    conversion_repo: &dyn ConversionEventRepository,
    connections: &dyn AdvertisingConnectionRepository,
    directive_resolver: &dyn DirectiveResolver,
    sinks: &BTreeMap<PlatformKey, Arc<dyn AudienceSyncPort>>,
    metrics: &AdvertisingMetrics,
    mut audience: Audience,
    fail_on_missing_connection: bool,
) -> Result<Option<Audience>, AdvertisingError> {
    let Some(sink) = sinks.get(&audience.platform) else {
        if fail_on_missing_connection {
            return Err(AdvertisingError::PlatformUnavailable(format!(
                "sink for platform {} is unavailable",
                audience.platform.as_str()
            )));
        }
        return Ok(None);
    };

    let connections_list = {
        let mut scope = tx
            .begin_for(audience.tenant_id.as_i64())
            .await
            .map_err(|error| AdvertisingError::Storage(error.to_string()))?;
        let list = connections.list(&mut scope, audience.tenant_id).await?;
        scope
            .commit()
            .await
            .map_err(|error| AdvertisingError::Storage(error.to_string()))?;
        list
    };

    let active_conn = connections_list
        .iter()
        .find(|c| c.platform() == &audience.platform && c.status() == ConnectionStatus::Active);

    let handle = match active_conn {
        Some(conn) => ConnectionHandle {
            id: conn.id().to_string(),
            tenant_id: conn.tenant_id(),
            external_account_id: conn.external_account_id().to_string(),
        },
        None => {
            if fail_on_missing_connection {
                return Err(AdvertisingError::ConnectionNotFound);
            }
            return Ok(None);
        }
    };

    let eligible_set = evaluate_tenant_eligible_identifiers(
        conversion_repo,
        directive_resolver,
        tx,
        audience.tenant_id,
    )
    .await?;

    let (included, removed) = compute_audience_diff(&audience.included_identifiers, eligible_set);

    let sync_result = sink
        .sync_audience(
            &handle,
            audience.external_ref.as_deref(),
            &included,
            &removed,
        )
        .await;

    match sync_result {
        Ok(receipt) => {
            audience.external_ref = Some(receipt.external_ref);
            audience.included_identifiers = included;
            audience.last_synced_at = Some(Utc::now());
            audience.status = AudienceStatus::Active;

            let mut scope = tx
                .begin_unscoped("advertising_audience_sync_update", ACTOR)
                .await
                .map_err(|error| AdvertisingError::Storage(error.to_string()))?;
            audience_repo
                .update_after_sync(&mut scope, &audience)
                .await?;
            scope
                .commit()
                .await
                .map_err(|error| AdvertisingError::Storage(error.to_string()))?;

            metrics
                .audience_sync_total()
                .with_label_values(&[audience.platform.as_str(), "synced"])
                .inc();

            Ok(Some(audience))
        }
        Err(err) if is_unsupported_removal(&err) => {
            audience.included_identifiers = included;
            audience.last_synced_at = Some(Utc::now());
            audience.status = AudienceStatus::Failed {
                reason:
                    "capability_unsupported: platform does not support removing opted-out subjects"
                        .to_string(),
            };

            let mut scope = tx
                .begin_unscoped("advertising_audience_sync_update", ACTOR)
                .await
                .map_err(|error| AdvertisingError::Storage(error.to_string()))?;
            audience_repo
                .update_after_sync(&mut scope, &audience)
                .await?;
            scope
                .commit()
                .await
                .map_err(|error| AdvertisingError::Storage(error.to_string()))?;

            metrics
                .audience_sync_total()
                .with_label_values(&[audience.platform.as_str(), "unsupported_removal"])
                .inc();

            tracing::warn!(
                tenant_id = %audience.tenant_id,
                platform = audience.platform.as_str(),
                "audience member removal unsupported by platform"
            );

            Ok(Some(audience))
        }
        Err(err) => {
            metrics
                .audience_sync_total()
                .with_label_values(&[audience.platform.as_str(), "failed"])
                .inc();
            tracing::error!(
                tenant_id = %audience.tenant_id,
                platform = audience.platform.as_str(),
                error = %err,
                "audience sync failed"
            );
            Err(err)
        }
    }
}

/// Create a new draft audience.
pub struct CreateAudience {
    tx: TxManager,
    repo: Arc<dyn AudienceRepository>,
    ids: Arc<dyn IdGenerator>,
}

impl CreateAudience {
    pub fn new(
        tx: TxManager,
        repo: Arc<dyn AudienceRepository>,
        ids: Arc<dyn IdGenerator>,
    ) -> Self {
        Self { tx, repo, ids }
    }

    pub async fn execute(
        &self,
        tenant_id: TenantId,
        name: String,
        platform: PlatformKey,
    ) -> Result<Audience, AdvertisingError> {
        let trimmed_name = name.trim().to_string();
        if trimmed_name.is_empty() {
            return Err(AdvertisingError::InvalidInput(
                "audience name is required".to_string(),
            ));
        }

        let id = self.ids.next_audience_id()?;
        let audience = Audience {
            id,
            tenant_id,
            platform,
            name: trimmed_name,
            status: AudienceStatus::Building,
            external_ref: None,
            included_identifiers: Vec::new(),
            last_synced_at: None,
            created_at: Utc::now(),
        };

        let mut scope = self
            .tx
            .begin_unscoped("advertising_create_audience", ACTOR)
            .await
            .map_err(|error| AdvertisingError::Storage(error.to_string()))?;
        self.repo.insert(&mut scope, &audience).await?;
        scope
            .commit()
            .await
            .map_err(|error| AdvertisingError::Storage(error.to_string()))?;

        Ok(audience)
    }
}

/// List all audiences for a tenant.
pub struct ListAudiences {
    tx: TxManager,
    repo: Arc<dyn AudienceRepository>,
}

impl ListAudiences {
    pub fn new(tx: TxManager, repo: Arc<dyn AudienceRepository>) -> Self {
        Self { tx, repo }
    }

    pub async fn execute(&self, tenant_id: TenantId) -> Result<Vec<Audience>, AdvertisingError> {
        let mut scope = self
            .tx
            .begin_unscoped("advertising_list_audiences", ACTOR)
            .await
            .map_err(|error| AdvertisingError::Storage(error.to_string()))?;
        let list = self.repo.list(&mut scope, tenant_id).await?;
        scope
            .commit()
            .await
            .map_err(|error| AdvertisingError::Storage(error.to_string()))?;

        Ok(list)
    }
}

/// Manual refresh use case for a single audience.
pub struct RefreshAudience {
    tx: TxManager,
    audience_repo: Arc<dyn AudienceRepository>,
    conversion_repo: Arc<dyn ConversionEventRepository>,
    connections: Arc<dyn AdvertisingConnectionRepository>,
    directive_resolver: Arc<dyn DirectiveResolver>,
    sinks: BTreeMap<PlatformKey, Arc<dyn AudienceSyncPort>>,
    metrics: Arc<AdvertisingMetrics>,
}

impl RefreshAudience {
    pub fn new(
        tx: TxManager,
        audience_repo: Arc<dyn AudienceRepository>,
        conversion_repo: Arc<dyn ConversionEventRepository>,
        connections: Arc<dyn AdvertisingConnectionRepository>,
        directive_resolver: Arc<dyn DirectiveResolver>,
        sinks: BTreeMap<PlatformKey, Arc<dyn AudienceSyncPort>>,
        metrics: Arc<AdvertisingMetrics>,
    ) -> Self {
        Self {
            tx,
            audience_repo,
            conversion_repo,
            connections,
            directive_resolver,
            sinks,
            metrics,
        }
    }

    pub async fn execute(
        &self,
        tenant_id: TenantId,
        id: AudienceId,
    ) -> Result<Audience, AdvertisingError> {
        let audience = {
            let mut scope = self
                .tx
                .begin_unscoped("advertising_get_audience_for_refresh", ACTOR)
                .await
                .map_err(|error| AdvertisingError::Storage(error.to_string()))?;
            let found = self
                .audience_repo
                .find_by_id(&mut scope, tenant_id, id)
                .await?
                .ok_or(AdvertisingError::AudienceNotFound)?;
            scope
                .commit()
                .await
                .map_err(|error| AdvertisingError::Storage(error.to_string()))?;
            found
        };

        let synced = sync_single_audience(
            &self.tx,
            self.audience_repo.as_ref(),
            self.conversion_repo.as_ref(),
            self.connections.as_ref(),
            self.directive_resolver.as_ref(),
            &self.sinks,
            self.metrics.as_ref(),
            audience,
            true,
        )
        .await?;

        synced.ok_or(AdvertisingError::ConnectionNotFound)
    }
}

/// Scheduled worker sweep syncing due/stale audiences across all tenants.
pub struct SyncAudiences {
    tx: TxManager,
    audience_repo: Arc<dyn AudienceRepository>,
    conversion_repo: Arc<dyn ConversionEventRepository>,
    connections: Arc<dyn AdvertisingConnectionRepository>,
    directive_resolver: Arc<dyn DirectiveResolver>,
    sinks: BTreeMap<PlatformKey, Arc<dyn AudienceSyncPort>>,
    metrics: Arc<AdvertisingMetrics>,
}

impl SyncAudiences {
    pub fn new(
        tx: TxManager,
        audience_repo: Arc<dyn AudienceRepository>,
        conversion_repo: Arc<dyn ConversionEventRepository>,
        connections: Arc<dyn AdvertisingConnectionRepository>,
        directive_resolver: Arc<dyn DirectiveResolver>,
        sinks: BTreeMap<PlatformKey, Arc<dyn AudienceSyncPort>>,
        metrics: Arc<AdvertisingMetrics>,
    ) -> Self {
        Self {
            tx,
            audience_repo,
            conversion_repo,
            connections,
            directive_resolver,
            sinks,
            metrics,
        }
    }

    pub async fn execute(
        &self,
        stale_after_secs: i64,
        limit: i64,
    ) -> Result<AudienceSyncReport, AdvertisingError> {
        let mut report = AudienceSyncReport::default();

        let batch = {
            let mut scope = self
                .tx
                .begin_unscoped("advertising_audience_sync_sweep", ACTOR)
                .await
                .map_err(|error| AdvertisingError::Storage(error.to_string()))?;
            let batch = self
                .audience_repo
                .list_due_for_refresh(&mut scope, Utc::now(), stale_after_secs, limit)
                .await?;
            scope
                .commit()
                .await
                .map_err(|error| AdvertisingError::Storage(error.to_string()))?;
            batch
        };

        for audience in batch {
            report.checked += 1;
            let previous_included_count = audience.included_identifiers.len();

            match sync_single_audience(
                &self.tx,
                self.audience_repo.as_ref(),
                self.conversion_repo.as_ref(),
                self.connections.as_ref(),
                self.directive_resolver.as_ref(),
                &self.sinks,
                self.metrics.as_ref(),
                audience,
                false,
            )
            .await
            {
                Ok(Some(updated)) => match updated.status {
                    AudienceStatus::Active => {
                        report.synced += 1;
                        let removed_count = previous_included_count
                            .saturating_sub(updated.included_identifiers.len());
                        report.removed_identifiers += removed_count as u64;
                    }
                    AudienceStatus::Failed { .. } => {
                        report.unpropagated_removals += 1;
                    }
                    _ => {}
                },
                Ok(None) => {
                    // Skipped (e.g. no active connection for this platform) -> retry next pass
                }
                Err(_) => {
                    report.failed += 1;
                }
            }
        }

        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::ValidationViolation;
    use crate::domain::hashing::hash_email;

    #[test]
    fn test_compute_audience_diff_adds_and_removes() {
        let id1 = hash_email("user1@example.com");
        let id2 = hash_email("user2@example.com");
        let id3 = hash_email("user3@example.com");

        let current_included = vec![id1.clone(), id2.clone()];
        let mut eligible = BTreeSet::new();
        eligible.insert(id2.clone());
        eligible.insert(id3.clone());

        let (new_included, removed) = compute_audience_diff(&current_included, eligible);

        assert_eq!(new_included.len(), 2);
        assert!(new_included.contains(&id2));
        assert!(new_included.contains(&id3));

        assert_eq!(removed.len(), 1);
        assert_eq!(removed[0], id1);
    }

    #[test]
    fn test_is_unsupported_removal() {
        let unsupported_err = AdvertisingError::Validation(vec![ValidationViolation::unsupported(
            "removed",
            "removal unsupported",
        )]);
        assert!(is_unsupported_removal(&unsupported_err));

        let invalid_err =
            AdvertisingError::Validation(vec![ValidationViolation::invalid("field", "invalid")]);
        assert!(!is_unsupported_removal(&invalid_err));

        let other_err = AdvertisingError::ConnectionNotFound;
        assert!(!is_unsupported_removal(&other_err));
    }
}
