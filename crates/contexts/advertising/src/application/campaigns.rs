//! Campaign tree: draft/publish lifecycle, patch, pause/resume and the
//! attributed change log. See `docs/tasks/phase-10/TASK-012-*.md`.
//!
//! Drafts never touch the platform: `external_id` stays `None` until
//! [`PublishCampaign`] runs. Every write after that goes through the
//! adapter first — remote state is the truth about what is running, ours is
//! the truth about what the tenant asked for (see [`crate::domain::reconciliation`]).

use std::{future::Future, pin::Pin, sync::Arc};

use crate::domain::model::{CampaignId, ConnectionId, CreativeId};
use chrono::Utc;
use sanvi_kernel::{TenantId, UserId};
use sanvi_persistence::TxManager;
use uuid::Uuid;

use crate::{
    application::PlatformRegistry,
    domain::{
        Ad, AdEvent, AdGroup, AdvertisingConnection, AdvertisingConnectionRepository,
        AdvertisingError, BidSettings, Budget, Campaign, CampaignChange,
        CampaignReconciliationState, CampaignRepository, ChangeSource, ConnectionHandle,
        ConnectionStatus, Creative, CreativeRepository, DriftState, EventPublisher, Objective,
        Schedule, StoredCampaign, Targeting, ValidationViolation, validate_campaign,
    },
};

/// The mandatory shape of a brand-new draft. Unlike a patch, every field
/// that the domain model requires is required here too — there is no
/// placeholder value standing in for "not chosen yet".
#[derive(Debug, Clone)]
pub struct NewCampaignFields {
    pub name: String,
    pub objective: Objective,
    pub budget: Budget,
    pub schedule: Option<Schedule>,
}

/// Fields a patch may change. `None` leaves the stored value unchanged;
/// there is no way to clear `schedule` back to `None` in this slice.
#[derive(Debug, Clone, Default)]
pub struct CampaignFields {
    pub name: Option<String>,
    pub objective: Option<Objective>,
    pub budget: Option<Budget>,
    pub schedule: Option<Schedule>,
    pub ad_groups: Option<Vec<crate::domain::AdGroup>>,
}

fn apply_fields(mut campaign: Campaign, fields: &CampaignFields) -> Campaign {
    if let Some(name) = &fields.name {
        campaign.name = name.clone();
    }
    if let Some(objective) = &fields.objective {
        campaign.objective = objective.clone();
    }
    if let Some(budget) = &fields.budget {
        campaign.budget = budget.clone();
    }
    if let Some(schedule) = &fields.schedule {
        campaign.schedule = Some(schedule.clone());
    }
    if let Some(ad_groups) = &fields.ad_groups {
        campaign.ad_groups = ad_groups.clone();
    }
    campaign
}

async fn load_active_connection(
    scope: &mut sanvi_persistence::ScopedTx<'_>,
    connections: &dyn AdvertisingConnectionRepository,
    tenant_id: TenantId,
    connection_id: ConnectionId,
) -> Result<crate::domain::AdvertisingConnection, AdvertisingError> {
    let connection = connections
        .find_by_id(scope, tenant_id, connection_id)
        .await?
        .ok_or(AdvertisingError::ConnectionNotFound)?;
    if connection.status() != ConnectionStatus::Active {
        return Err(AdvertisingError::ConnectionNotActive);
    }
    Ok(connection)
}

/// Build the adapter-facing handle from the connection aggregate. The
/// platform's own account id lives only on the aggregate, so a handle cannot
/// be conjured from a `ConnectionId` alone — every caller loads the connection
/// the campaign belongs to first.
fn connection_handle(connection: &AdvertisingConnection) -> ConnectionHandle {
    ConnectionHandle {
        id: connection.id().to_string(),
        tenant_id: connection.tenant_id(),
        external_account_id: connection.external_account_id().to_string(),
    }
}

/// Load a campaign's connection for handle construction. Unlike
/// [`load_active_connection`] this does not require the connection to be
/// active: a mutation path has its own liveness rules, and tightening them
/// here would change behaviour that is not this lookup's business.
async fn load_connection_for_handle(
    scope: &mut sanvi_persistence::ScopedTx<'_>,
    connections: &dyn AdvertisingConnectionRepository,
    tenant_id: TenantId,
    connection_id: ConnectionId,
) -> Result<AdvertisingConnection, AdvertisingError> {
    connections
        .find_by_id(scope, tenant_id, connection_id)
        .await?
        .ok_or(AdvertisingError::ConnectionNotFound)
}

/// Create a campaign draft. Nothing is sent to the platform: `external_id`
/// stays `None` and the intent is validated against the connection's
/// capability matrix so a doomed draft never reaches the builder as valid.
pub struct CreateCampaign {
    tx: TxManager,
    campaigns: Arc<dyn CampaignRepository>,
    connections: Arc<dyn AdvertisingConnectionRepository>,
    platforms: Arc<PlatformRegistry>,
    events: Arc<dyn EventPublisher>,
    ids: Arc<dyn crate::domain::IdGenerator>,
}

impl CreateCampaign {
    pub fn new(
        tx: TxManager,
        campaigns: Arc<dyn CampaignRepository>,
        connections: Arc<dyn AdvertisingConnectionRepository>,
        platforms: Arc<PlatformRegistry>,
        events: Arc<dyn EventPublisher>,
        ids: Arc<dyn crate::domain::IdGenerator>,
    ) -> Self {
        Self {
            tx,
            campaigns,
            connections,
            platforms,
            events,
            ids,
        }
    }

    pub async fn execute(
        &self,
        tenant_id: TenantId,
        connection_id: ConnectionId,
        mutation_id: Uuid,
        actor_id: UserId,
        fields: NewCampaignFields,
    ) -> Result<StoredCampaign, AdvertisingError> {
        let mut scope = self
            .tx
            .begin_for(tenant_id.as_i64())
            .await
            .map_err(|error| AdvertisingError::Storage(error.to_string()))?;
        let connection = load_active_connection(
            &mut scope,
            self.connections.as_ref(),
            tenant_id,
            connection_id,
        )
        .await?;
        let adapter = self.platforms.adapter(connection.platform())?;

        let id = self.ids.next_campaign_id()?;
        let draft = Campaign {
            id: id.to_string(),
            name: fields.name,
            status: crate::domain::CampaignStatus::Draft,
            objective: fields.objective,
            budget: fields.budget,
            schedule: fields.schedule,
            ad_groups: Vec::new(),
            drift: DriftState {
                drifted: false,
                changed_fields: Vec::new(),
            },
        };
        let violations = validate_campaign(&adapter.descriptor().capability_matrix, &draft);
        if !violations.is_empty() {
            return Err(AdvertisingError::Validation(violations));
        }

        let stored = StoredCampaign {
            id,
            tenant_id,
            connection_id,
            platform: connection.platform().clone(),
            external_id: None,
            revision: 1,
            intent: draft.clone(),
            remote_snapshot: draft.clone(),
        };
        self.campaigns.insert(&mut scope, &stored).await?;
        self.campaigns
            .append_change(
                &mut scope,
                &CampaignChange {
                    id: self.ids.next_campaign_change_id()?,
                    tenant_id,
                    campaign_id: id,
                    revision: 1,
                    mutation_id: Some(mutation_id),
                    actor_id: Some(actor_id),
                    source: ChangeSource::Sanvi,
                    changed_fields: vec!["*".to_string()],
                    before_state: draft.clone(),
                    after_state: draft,
                    attribution: None,
                    metadata: None,
                    created_at: Utc::now(),
                },
            )
            .await?;
        self.events
            .publish_scoped(
                &mut scope,
                tenant_id,
                &[AdEvent::CampaignCreated { campaign_id: id }],
            )
            .await?;
        scope
            .commit()
            .await
            .map_err(|error| AdvertisingError::Storage(error.to_string()))?;
        Ok(stored)
    }

    pub async fn platform_for_connection(
        &self,
        tenant_id: TenantId,
        connection_id: ConnectionId,
    ) -> Result<crate::domain::PlatformKey, AdvertisingError> {
        let mut scope = self
            .tx
            .begin_for(tenant_id.as_i64())
            .await
            .map_err(|error| AdvertisingError::Storage(error.to_string()))?;
        let connection = load_active_connection(
            &mut scope,
            self.connections.as_ref(),
            tenant_id,
            connection_id,
        )
        .await?;
        scope
            .rollback()
            .await
            .map_err(|error| AdvertisingError::Storage(error.to_string()))?;
        Ok(connection.platform().clone())
    }
}

/// Read one campaign. No adapter call; the platform is asked only when
/// reconciliation explicitly runs.
pub struct GetCampaign {
    tx: TxManager,
    campaigns: Arc<dyn CampaignRepository>,
}

impl GetCampaign {
    pub fn new(tx: TxManager, campaigns: Arc<dyn CampaignRepository>) -> Self {
        Self { tx, campaigns }
    }

    pub async fn execute(
        &self,
        tenant_id: TenantId,
        campaign_id: CampaignId,
    ) -> Result<StoredCampaign, AdvertisingError> {
        let mut scope = self
            .tx
            .begin_for(tenant_id.as_i64())
            .await
            .map_err(|error| AdvertisingError::Storage(error.to_string()))?;
        let stored = self
            .campaigns
            .find(&mut scope, tenant_id, campaign_id)
            .await?
            .ok_or(AdvertisingError::CampaignNotFound)?;
        scope
            .rollback()
            .await
            .map_err(|error| AdvertisingError::Storage(error.to_string()))?;
        Ok(stored)
    }
}

/// List every campaign owned by the tenant, newest first.
pub struct ListCampaigns {
    tx: TxManager,
    campaigns: Arc<dyn CampaignRepository>,
}

impl ListCampaigns {
    pub fn new(tx: TxManager, campaigns: Arc<dyn CampaignRepository>) -> Self {
        Self { tx, campaigns }
    }

    pub async fn execute(
        &self,
        tenant_id: TenantId,
    ) -> Result<Vec<StoredCampaign>, AdvertisingError> {
        let mut scope = self
            .tx
            .begin_for(tenant_id.as_i64())
            .await
            .map_err(|error| AdvertisingError::Storage(error.to_string()))?;
        let campaigns = self.campaigns.list_by_tenant(&mut scope, tenant_id).await?;
        scope
            .rollback()
            .await
            .map_err(|error| AdvertisingError::Storage(error.to_string()))?;
        Ok(campaigns)
    }
}

/// Dry-run validation. Never writes; returns the same field-addressed
/// violations a failed create or patch would, so the builder can validate a
/// whole step before risking a partial publish.
pub struct ValidateCampaign {
    tx: TxManager,
    campaigns: Arc<dyn CampaignRepository>,
    creatives: Arc<dyn CreativeRepository>,
    platforms: Arc<PlatformRegistry>,
}

impl ValidateCampaign {
    pub fn new(
        tx: TxManager,
        campaigns: Arc<dyn CampaignRepository>,
        creatives: Arc<dyn CreativeRepository>,
        platforms: Arc<PlatformRegistry>,
    ) -> Self {
        Self {
            tx,
            campaigns,
            creatives,
            platforms,
        }
    }

    pub async fn execute(
        &self,
        tenant_id: TenantId,
        campaign_id: CampaignId,
        fields: CampaignFields,
    ) -> Result<Vec<ValidationViolation>, AdvertisingError> {
        let mut scope = self
            .tx
            .begin_for(tenant_id.as_i64())
            .await
            .map_err(|error| AdvertisingError::Storage(error.to_string()))?;
        let stored = self
            .campaigns
            .find(&mut scope, tenant_id, campaign_id)
            .await?
            .ok_or(AdvertisingError::CampaignNotFound)?;
        let adapter = self.platforms.adapter(&stored.platform)?;
        let matrix = &adapter.descriptor().capability_matrix;
        let platform = stored.platform.clone();
        let mut candidate = apply_fields(stored.intent, &fields);
        let mut violations = Vec::new();
        // A patch carrying an ad_groups tree is checked against the creative
        // store on the write path, so the dry run has to make the same trip or
        // it reports a step as valid that PatchCampaign would reject.
        for (group_index, group) in candidate.ad_groups.iter_mut().enumerate() {
            for (ad_index, ad) in group.ads.iter_mut().enumerate() {
                violations.extend(
                    creative_dry_run_violations(
                        &mut scope,
                        self.creatives.as_ref(),
                        matrix,
                        &platform,
                        tenant_id,
                        group_index,
                        ad_index,
                        ad,
                    )
                    .await?,
                );
            }
        }
        violations.extend(validate_campaign(matrix, &candidate));
        scope
            .rollback()
            .await
            .map_err(|error| AdvertisingError::Storage(error.to_string()))?;
        Ok(violations)
    }
}

/// Dry-run counterpart of [`validate_and_hydrate_ad_creative`]: same checks,
/// reported as field-addressed violations instead of errors. The candidate ad
/// is authoritatively hydrated in-place so downstream capability matrix validation
/// inspects stored creative content.
///
/// A reference the write path rejects with `CreativeNotFound` (404) or
/// `InvalidInput` (400) comes back here as a violation, because the dry run's
/// contract is to describe every problem in the step rather than fail on the
/// first bad one.
#[allow(clippy::too_many_arguments)]
async fn creative_dry_run_violations(
    scope: &mut sanvi_persistence::ScopedTx<'_>,
    creatives: &dyn CreativeRepository,
    matrix: &crate::domain::CapabilityMatrix,
    campaign_platform: &crate::domain::PlatformKey,
    tenant_id: TenantId,
    group_index: usize,
    ad_index: usize,
    ad: &mut Ad,
) -> Result<Vec<ValidationViolation>, AdvertisingError> {
    match validate_and_hydrate_ad_creative(
        scope,
        creatives,
        matrix,
        campaign_platform,
        tenant_id,
        group_index,
        ad_index,
        ad,
    )
    .await
    {
        Ok(()) => Ok(Vec::new()),
        Err(AdvertisingError::Validation(violations)) => Ok(violations),
        Err(AdvertisingError::CreativeNotFound) => Ok(vec![ValidationViolation::invalid(
            format!("ad_groups[{group_index}].ads[{ad_index}].creative.id"),
            "creative does not exist",
        )]),
        Err(AdvertisingError::InvalidInput(message)) => Ok(vec![ValidationViolation::invalid(
            format!("ad_groups[{group_index}].ads[{ad_index}].creative.id"),
            message,
        )]),
        Err(other) => Err(other),
    }
}

/// Load, mutate, validate, push-if-published, and persist one campaign as a
/// single attributed change. `PATCH` and every ad-group/ad sub-resource
/// endpoint are all this same shape — the only thing that differs is what
/// the closure changes and which field names get logged.
///
/// A published campaign (`external_id` is `Some`) also gets its edit pushed
/// live through the adapter first, attributed the same way
/// [`crate::application::ResolveCampaignDrift`] pushes a `reapply_ours`
/// choice: provider state is authoritative for what actually took, so a
/// clamped budget or rejected field survives into the stored intent instead
/// of silently reporting our request as truth.
#[allow(clippy::too_many_arguments)]
async fn apply_mutation<F>(
    tx: &TxManager,
    campaigns: &dyn CampaignRepository,
    connections: &dyn AdvertisingConnectionRepository,
    events: &dyn EventPublisher,
    platforms: &PlatformRegistry,
    ids: &dyn crate::domain::IdGenerator,
    tenant_id: TenantId,
    campaign_id: CampaignId,
    expected_revision: i64,
    mutation_id: Uuid,
    actor_id: UserId,
    changed_fields: Vec<String>,
    mutate: F,
) -> Result<StoredCampaign, AdvertisingError>
where
    F: for<'a> FnOnce(
        &'a mut Campaign,
        &'a mut sanvi_persistence::ScopedTx<'_>,
        &'a crate::domain::CapabilityMatrix,
        &'a crate::domain::PlatformKey,
    )
        -> Pin<Box<dyn Future<Output = Result<(), AdvertisingError>> + Send + 'a>>,
{
    let mut scope = tx
        .begin_for(tenant_id.as_i64())
        .await
        .map_err(|error| AdvertisingError::Storage(error.to_string()))?;
    let stored = campaigns
        .find(&mut scope, tenant_id, campaign_id)
        .await?
        .ok_or(AdvertisingError::CampaignNotFound)?;
    if stored.revision != expected_revision {
        return Err(AdvertisingError::StaleRevision);
    }
    let adapter = platforms.adapter(&stored.platform)?;
    let before = stored.intent.clone();
    let mut candidate = stored.intent.clone();
    mutate(
        &mut candidate,
        &mut scope,
        &adapter.descriptor().capability_matrix,
        &stored.platform,
    )
    .await?;
    let violations = validate_campaign(&adapter.descriptor().capability_matrix, &candidate);
    if !violations.is_empty() {
        return Err(AdvertisingError::Validation(violations));
    }

    let mut remote_snapshot = stored.remote_snapshot;
    if let Some(external_id) = &stored.external_id {
        candidate.id = external_id.clone();
        let remote = adapter
            .update_campaign(
                &connection_handle(
                    &load_connection_for_handle(
                        &mut scope,
                        connections,
                        tenant_id,
                        stored.connection_id,
                    )
                    .await?,
                ),
                candidate.clone(),
                mutation_id,
            )
            .await?;
        let mut state = CampaignReconciliationState {
            intent: candidate,
            last_known_remote: remote_snapshot,
        };
        let _ = state.reconcile(remote);
        candidate = state.intent;
        remote_snapshot = state.last_known_remote;
    }

    let updated = StoredCampaign {
        intent: candidate,
        remote_snapshot,
        revision: stored.revision + 1,
        ..stored
    };
    if !campaigns
        .update(&mut scope, &updated, expected_revision)
        .await?
    {
        return Err(AdvertisingError::StaleRevision);
    }
    campaigns
        .append_change(
            &mut scope,
            &CampaignChange {
                id: ids.next_campaign_change_id()?,
                tenant_id,
                campaign_id,
                revision: updated.revision,
                mutation_id: Some(mutation_id),
                actor_id: Some(actor_id),
                source: ChangeSource::Sanvi,
                changed_fields,
                before_state: before,
                after_state: updated.intent.clone(),
                attribution: None,
                metadata: None,
                created_at: Utc::now(),
            },
        )
        .await?;
    events
        .publish_scoped(
            &mut scope,
            tenant_id,
            &[AdEvent::CampaignUpdated { campaign_id }],
        )
        .await?;
    scope
        .commit()
        .await
        .map_err(|error| AdvertisingError::Storage(error.to_string()))?;
    Ok(updated)
}

/// `PATCH /campaigns/{id}`: the intent is always updated; a published
/// campaign also gets its edit pushed live (see [`apply_mutation`]).
pub struct PatchCampaign {
    tx: TxManager,
    campaigns: Arc<dyn CampaignRepository>,
    connections: Arc<dyn AdvertisingConnectionRepository>,
    creatives: Arc<dyn CreativeRepository>,
    events: Arc<dyn EventPublisher>,
    platforms: Arc<PlatformRegistry>,
    ids: Arc<dyn crate::domain::IdGenerator>,
}

impl PatchCampaign {
    pub fn new(
        tx: TxManager,
        campaigns: Arc<dyn CampaignRepository>,
        connections: Arc<dyn AdvertisingConnectionRepository>,
        creatives: Arc<dyn CreativeRepository>,
        events: Arc<dyn EventPublisher>,
        platforms: Arc<PlatformRegistry>,
        ids: Arc<dyn crate::domain::IdGenerator>,
    ) -> Self {
        Self {
            tx,
            campaigns,
            connections,
            creatives,
            events,
            platforms,
            ids,
        }
    }

    pub async fn execute(
        &self,
        tenant_id: TenantId,
        campaign_id: CampaignId,
        expected_revision: i64,
        mutation_id: Uuid,
        actor_id: UserId,
        fields: CampaignFields,
    ) -> Result<StoredCampaign, AdvertisingError> {
        let creatives = self.creatives.clone();
        let changed_fields = changed_field_names(&fields);
        apply_mutation(
            &self.tx,
            self.campaigns.as_ref(),
            self.connections.as_ref(),
            self.events.as_ref(),
            &self.platforms,
            self.ids.as_ref(),
            tenant_id,
            campaign_id,
            expected_revision,
            mutation_id,
            actor_id,
            changed_fields,
            move |campaign, scope, matrix, platform| {
                Box::pin(async move {
                    *campaign = apply_fields(campaign.clone(), &fields);
                    if fields.ad_groups.is_some() {
                        for (group_index, group) in campaign.ad_groups.iter_mut().enumerate() {
                            for (ad_index, ad) in group.ads.iter_mut().enumerate() {
                                validate_and_hydrate_ad_creative(
                                    scope,
                                    creatives.as_ref(),
                                    matrix,
                                    platform,
                                    tenant_id,
                                    group_index,
                                    ad_index,
                                    ad,
                                )
                                .await?;
                            }
                        }
                    } else {
                        verify_ad_creatives_exist(scope, creatives.as_ref(), tenant_id, campaign)
                            .await?;
                    }
                    Ok(())
                })
            },
        )
        .await
    }
}

fn changed_field_names(fields: &CampaignFields) -> Vec<String> {
    let mut names = Vec::new();
    if fields.name.is_some() {
        names.push("name".to_string());
    }
    if fields.objective.is_some() {
        names.push("objective".to_string());
    }
    if fields.budget.is_some() {
        names.push("budget".to_string());
    }
    if fields.schedule.is_some() {
        names.push("schedule".to_string());
    }
    if fields.ad_groups.is_some() {
        names.push("ad_groups".to_string());
    }
    names
}

/// Fields for a new ad group. `id` is always minted server-side.
#[derive(Debug, Clone)]
pub struct NewAdGroupFields {
    pub name: String,
    pub targeting: Targeting,
    pub bid: BidSettings,
}

/// Fields an ad-group patch may change. `None` leaves the stored value
/// unchanged.
#[derive(Debug, Clone, Default)]
pub struct AdGroupFields {
    pub name: Option<String>,
    pub targeting: Option<Targeting>,
    pub bid: Option<BidSettings>,
}

/// `POST /campaigns/{id}/ad-groups`: append one ad group to the campaign
/// tree. Goes through the same validate → push-if-published → CAS path as
/// `PATCH`, just scoped to one addition instead of a whole-tree replace.
pub struct AddAdGroup {
    tx: TxManager,
    campaigns: Arc<dyn CampaignRepository>,
    connections: Arc<dyn AdvertisingConnectionRepository>,
    events: Arc<dyn EventPublisher>,
    platforms: Arc<PlatformRegistry>,
    ids: Arc<dyn crate::domain::IdGenerator>,
}

impl AddAdGroup {
    pub fn new(
        tx: TxManager,
        campaigns: Arc<dyn CampaignRepository>,
        connections: Arc<dyn AdvertisingConnectionRepository>,
        events: Arc<dyn EventPublisher>,
        platforms: Arc<PlatformRegistry>,
        ids: Arc<dyn crate::domain::IdGenerator>,
    ) -> Self {
        Self {
            tx,
            campaigns,
            connections,
            events,
            platforms,
            ids,
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn execute(
        &self,
        tenant_id: TenantId,
        campaign_id: CampaignId,
        expected_revision: i64,
        mutation_id: Uuid,
        actor_id: UserId,
        fields: NewAdGroupFields,
    ) -> Result<StoredCampaign, AdvertisingError> {
        let ad_group_id = self.ids.next_ad_group_id()?.to_string();
        apply_mutation(
            &self.tx,
            self.campaigns.as_ref(),
            self.connections.as_ref(),
            self.events.as_ref(),
            &self.platforms,
            self.ids.as_ref(),
            tenant_id,
            campaign_id,
            expected_revision,
            mutation_id,
            actor_id,
            vec!["ad_groups".to_string()],
            move |campaign, _scope, _matrix, _platform| {
                Box::pin(async move {
                    campaign.ad_groups.push(AdGroup {
                        id: ad_group_id,
                        name: fields.name,
                        targeting: fields.targeting,
                        bid: fields.bid,
                        ads: Vec::new(),
                    });
                    Ok(())
                })
            },
        )
        .await
    }
}

/// `PATCH /campaigns/{id}/ad-groups/{ad_group_id}`.
pub struct UpdateAdGroup {
    tx: TxManager,
    campaigns: Arc<dyn CampaignRepository>,
    connections: Arc<dyn AdvertisingConnectionRepository>,
    events: Arc<dyn EventPublisher>,
    platforms: Arc<PlatformRegistry>,
    ids: Arc<dyn crate::domain::IdGenerator>,
}

impl UpdateAdGroup {
    pub fn new(
        tx: TxManager,
        campaigns: Arc<dyn CampaignRepository>,
        connections: Arc<dyn AdvertisingConnectionRepository>,
        events: Arc<dyn EventPublisher>,
        platforms: Arc<PlatformRegistry>,
        ids: Arc<dyn crate::domain::IdGenerator>,
    ) -> Self {
        Self {
            tx,
            campaigns,
            connections,
            events,
            platforms,
            ids,
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn execute(
        &self,
        tenant_id: TenantId,
        campaign_id: CampaignId,
        ad_group_id: String,
        expected_revision: i64,
        mutation_id: Uuid,
        actor_id: UserId,
        fields: AdGroupFields,
    ) -> Result<StoredCampaign, AdvertisingError> {
        apply_mutation(
            &self.tx,
            self.campaigns.as_ref(),
            self.connections.as_ref(),
            self.events.as_ref(),
            &self.platforms,
            self.ids.as_ref(),
            tenant_id,
            campaign_id,
            expected_revision,
            mutation_id,
            actor_id,
            vec!["ad_groups".to_string()],
            move |campaign, _scope, _matrix, _platform| {
                Box::pin(async move {
                    let group = campaign
                        .ad_groups
                        .iter_mut()
                        .find(|group| group.id == ad_group_id)
                        .ok_or(AdvertisingError::AdGroupNotFound)?;
                    if let Some(name) = fields.name {
                        group.name = name;
                    }
                    if let Some(targeting) = fields.targeting {
                        group.targeting = targeting;
                    }
                    if let Some(bid) = fields.bid {
                        group.bid = bid;
                    }
                    Ok(())
                })
            },
        )
        .await
    }
}

/// Fields for a new ad. `id` is always minted server-side.
#[derive(Debug, Clone)]
pub struct NewAdFields {
    pub creative: Creative,
    pub landing_url: String,
    pub tracking_template: Option<String>,
}

/// Fields an ad patch may change. `None` leaves the stored value unchanged;
/// there is no way to clear `tracking_template` back to `None` in this
/// slice (same limitation as `CampaignFields::schedule`).
#[derive(Debug, Clone, Default)]
pub struct AdFields {
    pub creative: Option<Creative>,
    pub landing_url: Option<String>,
    pub tracking_template: Option<String>,
}

#[allow(clippy::too_many_arguments)]
async fn validate_and_hydrate_ad_creative(
    scope: &mut sanvi_persistence::ScopedTx<'_>,
    creatives: &dyn CreativeRepository,
    matrix: &crate::domain::CapabilityMatrix,
    campaign_platform: &crate::domain::PlatformKey,
    tenant_id: TenantId,
    group_index: usize,
    ad_index: usize,
    ad: &mut Ad,
) -> Result<(), AdvertisingError> {
    let creative_id =
        ad.creative.id.parse::<CreativeId>().map_err(|error| {
            AdvertisingError::InvalidInput(format!("invalid creative id: {error}"))
        })?;
    let stored_creative = creatives
        .find(scope, tenant_id, creative_id)
        .await?
        .ok_or(AdvertisingError::CreativeNotFound)?;
    if &stored_creative.platform != campaign_platform {
        return Err(AdvertisingError::Validation(vec![
            ValidationViolation::unsupported(
                format!("ad_groups[{group_index}].ads[{ad_index}].creative.id"),
                format!(
                    "creative belongs to platform '{}' but campaign is on platform '{}'",
                    stored_creative.platform.as_str(),
                    campaign_platform.as_str()
                ),
            ),
        ]));
    }
    if matrix
        .placement(&stored_creative.creative.placement)
        .is_none()
    {
        return Err(AdvertisingError::Validation(vec![
            ValidationViolation::unsupported(
                format!("ad_groups[{group_index}].ads[{ad_index}].creative.placement"),
                "creative placement is unavailable for this platform",
            ),
        ]));
    }
    if ad.creative.placement != stored_creative.creative.placement {
        return Err(AdvertisingError::Validation(vec![
            ValidationViolation::invalid(
                format!("ad_groups[{group_index}].ads[{ad_index}].creative.placement"),
                "creative placement does not match stored creative",
            ),
        ]));
    }
    ad.creative = stored_creative.creative;
    Ok(())
}

async fn verify_ad_creatives_exist(
    scope: &mut sanvi_persistence::ScopedTx<'_>,
    creatives: &dyn CreativeRepository,
    tenant_id: TenantId,
    campaign: &Campaign,
) -> Result<(), AdvertisingError> {
    for group in &campaign.ad_groups {
        for ad in &group.ads {
            verify_ad_creative_exists(scope, creatives, tenant_id, &ad.creative.id).await?;
        }
    }
    Ok(())
}

async fn verify_ad_creative_exists(
    scope: &mut sanvi_persistence::ScopedTx<'_>,
    creatives: &dyn CreativeRepository,
    tenant_id: TenantId,
    creative_id_str: &str,
) -> Result<(), AdvertisingError> {
    let creative_id = creative_id_str
        .parse::<CreativeId>()
        .map_err(|error| AdvertisingError::InvalidInput(format!("invalid creative id: {error}")))?;
    if creatives
        .find(scope, tenant_id, creative_id)
        .await?
        .is_none()
    {
        return Err(AdvertisingError::CreativeNotFound);
    }
    Ok(())
}

/// `POST /campaigns/{id}/ad-groups/{ad_group_id}/ads`.
pub struct AddAd {
    tx: TxManager,
    campaigns: Arc<dyn CampaignRepository>,
    connections: Arc<dyn AdvertisingConnectionRepository>,
    creatives: Arc<dyn CreativeRepository>,
    events: Arc<dyn EventPublisher>,
    platforms: Arc<PlatformRegistry>,
    ids: Arc<dyn crate::domain::IdGenerator>,
}

impl AddAd {
    pub fn new(
        tx: TxManager,
        campaigns: Arc<dyn CampaignRepository>,
        connections: Arc<dyn AdvertisingConnectionRepository>,
        creatives: Arc<dyn CreativeRepository>,
        events: Arc<dyn EventPublisher>,
        platforms: Arc<PlatformRegistry>,
        ids: Arc<dyn crate::domain::IdGenerator>,
    ) -> Self {
        Self {
            tx,
            campaigns,
            connections,
            creatives,
            events,
            platforms,
            ids,
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn execute(
        &self,
        tenant_id: TenantId,
        campaign_id: CampaignId,
        ad_group_id: String,
        expected_revision: i64,
        mutation_id: Uuid,
        actor_id: UserId,
        fields: NewAdFields,
    ) -> Result<StoredCampaign, AdvertisingError> {
        let creatives = self.creatives.clone();
        let ad_id = self.ids.next_ad_id()?.to_string();
        apply_mutation(
            &self.tx,
            self.campaigns.as_ref(),
            self.connections.as_ref(),
            self.events.as_ref(),
            &self.platforms,
            self.ids.as_ref(),
            tenant_id,
            campaign_id,
            expected_revision,
            mutation_id,
            actor_id,
            vec!["ad_groups".to_string()],
            move |campaign, scope, matrix, platform| {
                Box::pin(async move {
                    let (group_index, group) = campaign
                        .ad_groups
                        .iter_mut()
                        .enumerate()
                        .find(|(_, group)| group.id == ad_group_id)
                        .ok_or(AdvertisingError::AdGroupNotFound)?;
                    let ad_index = group.ads.len();
                    let mut ad = Ad {
                        id: ad_id,
                        creative: fields.creative,
                        landing_url: fields.landing_url,
                        tracking_template: fields.tracking_template,
                    };
                    validate_and_hydrate_ad_creative(
                        scope,
                        creatives.as_ref(),
                        matrix,
                        platform,
                        tenant_id,
                        group_index,
                        ad_index,
                        &mut ad,
                    )
                    .await?;
                    group.ads.push(ad);
                    Ok(())
                })
            },
        )
        .await
    }
}

/// `PATCH /campaigns/{id}/ad-groups/{ad_group_id}/ads/{ad_id}`.
pub struct UpdateAd {
    tx: TxManager,
    campaigns: Arc<dyn CampaignRepository>,
    connections: Arc<dyn AdvertisingConnectionRepository>,
    creatives: Arc<dyn CreativeRepository>,
    events: Arc<dyn EventPublisher>,
    platforms: Arc<PlatformRegistry>,
    ids: Arc<dyn crate::domain::IdGenerator>,
}

impl UpdateAd {
    pub fn new(
        tx: TxManager,
        campaigns: Arc<dyn CampaignRepository>,
        connections: Arc<dyn AdvertisingConnectionRepository>,
        creatives: Arc<dyn CreativeRepository>,
        events: Arc<dyn EventPublisher>,
        platforms: Arc<PlatformRegistry>,
        ids: Arc<dyn crate::domain::IdGenerator>,
    ) -> Self {
        Self {
            tx,
            campaigns,
            connections,
            creatives,
            events,
            platforms,
            ids,
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn execute(
        &self,
        tenant_id: TenantId,
        campaign_id: CampaignId,
        ad_group_id: String,
        ad_id: String,
        expected_revision: i64,
        mutation_id: Uuid,
        actor_id: UserId,
        fields: AdFields,
    ) -> Result<StoredCampaign, AdvertisingError> {
        let creatives = self.creatives.clone();
        apply_mutation(
            &self.tx,
            self.campaigns.as_ref(),
            self.connections.as_ref(),
            self.events.as_ref(),
            &self.platforms,
            self.ids.as_ref(),
            tenant_id,
            campaign_id,
            expected_revision,
            mutation_id,
            actor_id,
            vec!["ad_groups".to_string()],
            move |campaign, scope, matrix, platform| {
                Box::pin(async move {
                    let (group_index, group) = campaign
                        .ad_groups
                        .iter_mut()
                        .enumerate()
                        .find(|(_, group)| group.id == ad_group_id)
                        .ok_or(AdvertisingError::AdGroupNotFound)?;
                    let (ad_index, ad) = group
                        .ads
                        .iter_mut()
                        .enumerate()
                        .find(|(_, ad)| ad.id == ad_id)
                        .ok_or(AdvertisingError::AdNotFound)?;
                    if let Some(creative) = fields.creative {
                        let mut temp_ad = Ad {
                            id: ad.id.clone(),
                            creative,
                            landing_url: ad.landing_url.clone(),
                            tracking_template: ad.tracking_template.clone(),
                        };
                        validate_and_hydrate_ad_creative(
                            scope,
                            creatives.as_ref(),
                            matrix,
                            platform,
                            tenant_id,
                            group_index,
                            ad_index,
                            &mut temp_ad,
                        )
                        .await?;
                        ad.creative = temp_ad.creative;
                    } else {
                        verify_ad_creative_exists(
                            scope,
                            creatives.as_ref(),
                            tenant_id,
                            &ad.creative.id,
                        )
                        .await?;
                    }
                    if let Some(landing_url) = fields.landing_url {
                        ad.landing_url = landing_url;
                    }
                    if let Some(tracking_template) = fields.tracking_template {
                        ad.tracking_template = Some(tracking_template);
                    }
                    Ok(())
                })
            },
        )
        .await
    }
}

/// Validate → create remotely → store the platform-assigned id. This is the
/// first point a tenant's money can move; the mutation id travels with the
/// HTTP idempotency claim (`state.idempotency`), and a prior publish for the
/// same campaign is detected here by `external_id` already being set rather
/// than re-created.
pub struct PublishCampaign {
    tx: TxManager,
    campaigns: Arc<dyn CampaignRepository>,
    connections: Arc<dyn AdvertisingConnectionRepository>,
    creatives: Arc<dyn CreativeRepository>,
    events: Arc<dyn EventPublisher>,
    platforms: Arc<PlatformRegistry>,
    ids: Arc<dyn crate::domain::IdGenerator>,
}

impl PublishCampaign {
    pub fn new(
        tx: TxManager,
        campaigns: Arc<dyn CampaignRepository>,
        connections: Arc<dyn AdvertisingConnectionRepository>,
        creatives: Arc<dyn CreativeRepository>,
        events: Arc<dyn EventPublisher>,
        platforms: Arc<PlatformRegistry>,
        ids: Arc<dyn crate::domain::IdGenerator>,
    ) -> Self {
        Self {
            tx,
            campaigns,
            connections,
            creatives,
            events,
            platforms,
            ids,
        }
    }

    pub async fn execute(
        &self,
        tenant_id: TenantId,
        campaign_id: CampaignId,
        mutation_id: Uuid,
        actor_id: UserId,
    ) -> Result<StoredCampaign, AdvertisingError> {
        let mut scope = self
            .tx
            .begin_for(tenant_id.as_i64())
            .await
            .map_err(|error| AdvertisingError::Storage(error.to_string()))?;
        let stored = self
            .campaigns
            .find(&mut scope, tenant_id, campaign_id)
            .await?
            .ok_or(AdvertisingError::CampaignNotFound)?;
        if stored.external_id.is_some() {
            // Already live. A retried publish is a no-op success, not a
            // second campaign.
            scope
                .rollback()
                .await
                .map_err(|error| AdvertisingError::Storage(error.to_string()))?;
            return Ok(stored);
        }
        let adapter = self.platforms.adapter(&stored.platform)?;
        let violations = validate_campaign(&adapter.descriptor().capability_matrix, &stored.intent);
        if !violations.is_empty() {
            return Err(AdvertisingError::Validation(violations));
        }
        verify_ad_creatives_exist(
            &mut scope,
            self.creatives.as_ref(),
            tenant_id,
            &stored.intent,
        )
        .await?;
        let before = stored.intent.clone();
        let mut published = stored.intent.clone();
        published.status = crate::domain::CampaignStatus::Active;
        let remote = adapter
            .create_campaign(
                &connection_handle(
                    &load_connection_for_handle(
                        &mut scope,
                        self.connections.as_ref(),
                        tenant_id,
                        stored.connection_id,
                    )
                    .await?,
                ),
                published,
                mutation_id,
            )
            .await?;
        let external_id = remote.id.clone();

        let updated = StoredCampaign {
            external_id: Some(external_id),
            revision: stored.revision + 1,
            intent: remote.clone(),
            remote_snapshot: remote,
            ..stored
        };
        if !self
            .campaigns
            .update(&mut scope, &updated, updated.revision - 1)
            .await?
        {
            return Err(AdvertisingError::StaleRevision);
        }
        self.campaigns
            .append_change(
                &mut scope,
                &CampaignChange {
                    id: self.ids.next_campaign_change_id()?,
                    tenant_id,
                    campaign_id,
                    revision: updated.revision,
                    mutation_id: Some(mutation_id),
                    actor_id: Some(actor_id),
                    source: ChangeSource::Sanvi,
                    changed_fields: vec!["status".to_string(), "external_id".to_string()],
                    before_state: before,
                    after_state: updated.intent.clone(),
                    attribution: None,
                    metadata: None,
                    created_at: Utc::now(),
                },
            )
            .await?;
        self.events
            .publish_scoped(
                &mut scope,
                tenant_id,
                &[AdEvent::CampaignUpdated { campaign_id }],
            )
            .await?;
        scope
            .commit()
            .await
            .map_err(|error| AdvertisingError::Storage(error.to_string()))?;
        Ok(updated)
    }
}

enum PauseOrResume {
    Pause,
    Resume,
}

/// Shared body for pause/resume: both act on a published campaign only, and
/// both are ordinary attributed mutations — never automatic, never silent.
struct SetCampaignRunState {
    tx: TxManager,
    campaigns: Arc<dyn CampaignRepository>,
    connections: Arc<dyn AdvertisingConnectionRepository>,
    events: Arc<dyn EventPublisher>,
    platforms: Arc<PlatformRegistry>,
    ids: Arc<dyn crate::domain::IdGenerator>,
}

impl SetCampaignRunState {
    async fn execute(
        &self,
        tenant_id: TenantId,
        campaign_id: CampaignId,
        mutation_id: Uuid,
        actor_id: UserId,
        direction: PauseOrResume,
    ) -> Result<StoredCampaign, AdvertisingError> {
        self.execute_with_attribution(
            tenant_id,
            campaign_id,
            mutation_id,
            Some(actor_id),
            None,
            None,
            direction,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn execute_with_attribution(
        &self,
        tenant_id: TenantId,
        campaign_id: CampaignId,
        mutation_id: Uuid,
        actor_id: Option<UserId>,
        attribution: Option<String>,
        metadata: Option<serde_json::Value>,
        direction: PauseOrResume,
    ) -> Result<StoredCampaign, AdvertisingError> {
        let mut scope = self
            .tx
            .begin_for(tenant_id.as_i64())
            .await
            .map_err(|error| AdvertisingError::Storage(error.to_string()))?;
        let stored = self
            .campaigns
            .find(&mut scope, tenant_id, campaign_id)
            .await?
            .ok_or(AdvertisingError::CampaignNotFound)?;
        let external_id = stored
            .external_id
            .clone()
            .ok_or(AdvertisingError::CampaignNotPublished)?;
        let adapter = self.platforms.adapter(&stored.platform)?;
        let before = stored.intent.clone();
        let handle = connection_handle(
            &load_connection_for_handle(
                &mut scope,
                self.connections.as_ref(),
                tenant_id,
                stored.connection_id,
            )
            .await?,
        );
        let (remote, event) = match direction {
            PauseOrResume::Pause => (
                adapter.pause_campaign(&handle, &external_id).await?,
                AdEvent::CampaignPaused { campaign_id },
            ),
            PauseOrResume::Resume => (
                adapter.resume_campaign(&handle, &external_id).await?,
                AdEvent::CampaignResumed { campaign_id },
            ),
        };

        let updated = StoredCampaign {
            revision: stored.revision + 1,
            intent: remote.clone(),
            remote_snapshot: remote,
            ..stored
        };
        if !self
            .campaigns
            .update(&mut scope, &updated, updated.revision - 1)
            .await?
        {
            return Err(AdvertisingError::StaleRevision);
        }
        self.campaigns
            .append_change(
                &mut scope,
                &CampaignChange {
                    id: self.ids.next_campaign_change_id()?,
                    tenant_id,
                    campaign_id,
                    revision: updated.revision,
                    mutation_id: Some(mutation_id),
                    actor_id,
                    attribution,
                    source: ChangeSource::Sanvi,
                    changed_fields: vec!["status".to_string()],
                    before_state: before,
                    after_state: updated.intent.clone(),
                    metadata,
                    created_at: Utc::now(),
                },
            )
            .await?;
        self.events
            .publish_scoped(&mut scope, tenant_id, &[event])
            .await?;
        scope
            .commit()
            .await
            .map_err(|error| AdvertisingError::Storage(error.to_string()))?;
        Ok(updated)
    }
}

pub struct PauseCampaign(SetCampaignRunState);

impl PauseCampaign {
    pub fn new(
        tx: TxManager,
        campaigns: Arc<dyn CampaignRepository>,
        connections: Arc<dyn AdvertisingConnectionRepository>,
        events: Arc<dyn EventPublisher>,
        platforms: Arc<PlatformRegistry>,
        ids: Arc<dyn crate::domain::IdGenerator>,
    ) -> Self {
        Self(SetCampaignRunState {
            tx,
            campaigns,
            connections,
            events,
            platforms,
            ids,
        })
    }

    pub async fn execute(
        &self,
        tenant_id: TenantId,
        campaign_id: CampaignId,
        mutation_id: Uuid,
        actor_id: UserId,
    ) -> Result<StoredCampaign, AdvertisingError> {
        self.0
            .execute(
                tenant_id,
                campaign_id,
                mutation_id,
                actor_id,
                PauseOrResume::Pause,
            )
            .await
    }

    pub async fn execute_guardrail(
        &self,
        tenant_id: TenantId,
        campaign_id: CampaignId,
        mutation_id: Uuid,
        attribution: &str,
        metadata: serde_json::Value,
    ) -> Result<StoredCampaign, AdvertisingError> {
        self.0
            .execute_with_attribution(
                tenant_id,
                campaign_id,
                mutation_id,
                None,
                Some(attribution.to_string()),
                Some(metadata),
                PauseOrResume::Pause,
            )
            .await
    }
}

pub struct ResumeCampaign(SetCampaignRunState);

impl ResumeCampaign {
    pub fn new(
        tx: TxManager,
        campaigns: Arc<dyn CampaignRepository>,
        connections: Arc<dyn AdvertisingConnectionRepository>,
        events: Arc<dyn EventPublisher>,
        platforms: Arc<PlatformRegistry>,
        ids: Arc<dyn crate::domain::IdGenerator>,
    ) -> Self {
        Self(SetCampaignRunState {
            tx,
            campaigns,
            connections,
            events,
            platforms,
            ids,
        })
    }

    pub async fn execute(
        &self,
        tenant_id: TenantId,
        campaign_id: CampaignId,
        mutation_id: Uuid,
        actor_id: UserId,
    ) -> Result<StoredCampaign, AdvertisingError> {
        self.0
            .execute(
                tenant_id,
                campaign_id,
                mutation_id,
                actor_id,
                PauseOrResume::Resume,
            )
            .await
    }

    pub async fn execute_guardrail_rollover(
        &self,
        tenant_id: TenantId,
        campaign_id: CampaignId,
        mutation_id: Uuid,
        attribution: &str,
    ) -> Result<StoredCampaign, AdvertisingError> {
        self.0
            .execute_with_attribution(
                tenant_id,
                campaign_id,
                mutation_id,
                None,
                Some(attribution.to_string()),
                None,
                PauseOrResume::Resume,
            )
            .await
    }
}

/// Newest-first change log for one campaign — the answer to "who paused
/// this and when."
pub struct ListCampaignChanges {
    tx: TxManager,
    campaigns: Arc<dyn CampaignRepository>,
}

impl ListCampaignChanges {
    pub fn new(tx: TxManager, campaigns: Arc<dyn CampaignRepository>) -> Self {
        Self { tx, campaigns }
    }

    pub async fn execute(
        &self,
        tenant_id: TenantId,
        campaign_id: CampaignId,
        limit: i64,
    ) -> Result<Vec<CampaignChange>, AdvertisingError> {
        let mut scope = self
            .tx
            .begin_for(tenant_id.as_i64())
            .await
            .map_err(|error| AdvertisingError::Storage(error.to_string()))?;
        // 404 rather than an empty list when the campaign itself is missing
        // or belongs to another tenant.
        self.campaigns
            .find(&mut scope, tenant_id, campaign_id)
            .await?
            .ok_or(AdvertisingError::CampaignNotFound)?;
        let changes = self
            .campaigns
            .list_changes(&mut scope, tenant_id, campaign_id, limit)
            .await?;
        scope
            .rollback()
            .await
            .map_err(|error| AdvertisingError::Storage(error.to_string()))?;
        Ok(changes)
    }
}
