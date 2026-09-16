//! Deterministic in-process adapter and capability fixtures for frontend work.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
};

use async_trait::async_trait;

use crate::domain::{ConsentSnapshot, HashedIds};
use crate::{
    CapabilityMatrix, ConnectionHandle, Objective, PlatformKey,
    application::PlatformRegistry,
    domain::{
        AdPlatformPort, AdReviewObservation, AdReviewState, AdvertisingError, AudienceSyncPort,
        AudienceSyncReceipt, AuthorizationExchange, AuthorizationRequest, AuthorizationStart,
        BudgetType, Campaign, CampaignStatus, ConversionEvent, ConversionSinkPort,
        CreativeAssetSpec, CreativePlacement, ExchangedAuthorization, HashedIdentifier,
        MetricPoint, MetricQuery, Money, PlatformAccount, PlatformDescriptor, ScheduleGranularity,
        TargetingDimension, UploadReceipt, UploadedConversion, ValidationViolation,
    },
};

#[derive(Clone)]
pub struct FakeAdPlatform {
    descriptor: PlatformDescriptor,
    campaigns: Arc<Mutex<BTreeMap<String, Campaign>>>,
}

impl FakeAdPlatform {
    pub fn new(descriptor: PlatformDescriptor) -> Self {
        Self {
            descriptor,
            campaigns: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    pub fn registered_catalog() -> PlatformRegistry {
        let mut registry = PlatformRegistry::new();
        for descriptor in Self::fixtures().into_iter().take(2) {
            registry
                .register(Arc::new(Self::new(descriptor)))
                .expect("static fake descriptors have unique keys");
        }
        registry
    }

    /// Two production-shaped fixtures plus an asymmetric third-adapter proof.
    pub fn fixtures() -> Vec<PlatformDescriptor> {
        vec![
            descriptor(
                "google_ads",
                "Google Ads",
                "advertising.google_ads",
                &["app_promotion", "awareness", "leads", "sales", "traffic"],
                &[BudgetType::Daily, BudgetType::Lifetime],
                &[
                    "audiences",
                    "demographics",
                    "devices",
                    "geo",
                    "keywords",
                    "topics",
                ],
                (30, 30),
                "2026-08-24",
                vec![
                    CreativePlacement {
                        key: "feed".to_string(),
                        asset_spec: CreativeAssetSpec {
                            image_required: true,
                            video_allowed: true,
                            aspect_ratios: vec!["1:1".to_string(), "4:5".to_string()],
                            min_width_px: Some(300),
                            min_height_px: Some(300),
                            max_file_size_bytes: Some(5_242_880),
                            min_duration_seconds: Some(5),
                            max_duration_seconds: Some(180),
                        },
                    },
                    CreativePlacement {
                        key: "stories".to_string(),
                        asset_spec: CreativeAssetSpec {
                            image_required: true,
                            video_allowed: true,
                            aspect_ratios: vec!["9:16".to_string()],
                            min_width_px: Some(480),
                            min_height_px: Some(854),
                            max_file_size_bytes: Some(5_242_880),
                            min_duration_seconds: Some(5),
                            max_duration_seconds: Some(60),
                        },
                    },
                ],
            ),
            descriptor(
                "meta",
                "Meta Ads",
                "advertising.meta_ads",
                &[
                    "app_promotion",
                    "awareness",
                    "engagement",
                    "leads",
                    "sales",
                    "traffic",
                ],
                &[
                    BudgetType::Daily,
                    BudgetType::Lifetime,
                    BudgetType::CampaignBudgetOptimization,
                ],
                &[
                    "age",
                    "custom_audiences",
                    "gender",
                    "geo",
                    "interests",
                    "languages",
                ],
                (40, 25),
                // CBO joined the matrix; the version bump is the cache key the
                // frontend's form engine uses to re-fetch schemas.
                "2026-09-16",
                vec![
                    CreativePlacement {
                        key: "feed".to_string(),
                        asset_spec: CreativeAssetSpec {
                            image_required: true,
                            video_allowed: true,
                            aspect_ratios: vec!["1:1".to_string(), "4:5".to_string()],
                            min_width_px: Some(600),
                            min_height_px: Some(600),
                            max_file_size_bytes: Some(31_457_280),
                            min_duration_seconds: Some(1),
                            // 241 minutes = 14,460 seconds
                            max_duration_seconds: Some(14460),
                        },
                    },
                    CreativePlacement {
                        key: "stories".to_string(),
                        asset_spec: CreativeAssetSpec {
                            image_required: true,
                            video_allowed: true,
                            aspect_ratios: vec!["9:16".to_string()],
                            min_width_px: Some(500),
                            min_height_px: Some(888),
                            max_file_size_bytes: Some(31_457_280),
                            min_duration_seconds: Some(1),
                            max_duration_seconds: Some(120),
                        },
                    },
                    CreativePlacement {
                        key: "reels".to_string(),
                        asset_spec: CreativeAssetSpec {
                            image_required: false,
                            video_allowed: true,
                            aspect_ratios: vec!["9:16".to_string()],
                            min_width_px: Some(500),
                            min_height_px: Some(888),
                            max_file_size_bytes: Some(31_457_280),
                            min_duration_seconds: Some(3),
                            max_duration_seconds: Some(900),
                        },
                    },
                ],
            ),
            descriptor(
                "asymmetric_demo",
                "Asymmetric test network",
                "advertising.asymmetric_demo",
                &["app_installs"],
                &[BudgetType::Lifetime],
                &["device"],
                (18, 12),
                "2026-08-24",
                vec![CreativePlacement {
                    key: "feed".to_string(),
                    asset_spec: CreativeAssetSpec {
                        image_required: true,
                        video_allowed: false,
                        aspect_ratios: vec!["1:1".to_string()],
                        min_width_px: Some(1080),
                        min_height_px: Some(1080),
                        max_file_size_bytes: Some(1_048_576),
                        min_duration_seconds: None,
                        max_duration_seconds: None,
                    },
                }],
            ),
        ]
    }
}

#[allow(clippy::too_many_arguments)]
fn descriptor(
    key: &str,
    display_name: &str,
    entitlement_key: &str,
    objectives: &[&str],
    budget_types: &[BudgetType],
    dimensions: &[&str],
    headline_limits: (usize, usize),
    matrix_version: &str,
    creative_placements: Vec<CreativePlacement>,
) -> PlatformDescriptor {
    let (english_headline_limit, japanese_headline_limit) = headline_limits;
    let objective_set = objectives
        .iter()
        .map(|value| Objective::parse(value).expect("static objective"))
        .collect::<BTreeSet<_>>();
    let dimensions = dimensions
        .iter()
        .map(|value| TargetingDimension::parse(value).expect("static dimension"))
        .collect::<BTreeSet<_>>();
    let budget_types = budget_types.iter().copied().collect::<BTreeSet<_>>();
    let mut minimums = BTreeMap::new();
    minimums.insert(
        "JPY".to_string(),
        budget_types.iter().map(|kind| (*kind, 100)).collect(),
    );
    let mut text_limits = BTreeMap::new();
    text_limits.insert(
        "en".to_string(),
        BTreeMap::from([
            ("headline".to_string(), english_headline_limit),
            ("body".to_string(), 90),
        ]),
    );
    text_limits.insert(
        "ja".to_string(),
        BTreeMap::from([
            ("headline".to_string(), japanese_headline_limit),
            ("body".to_string(), 45),
        ]),
    );
    PlatformDescriptor {
        key: PlatformKey::parse(key).expect("static key"),
        display_name: display_name.to_string(),
        entitlement_key: entitlement_key.to_string(),
        capability_matrix: CapabilityMatrix {
            matrix_version: matrix_version.to_string(),
            objectives: objective_set,
            budget_types,
            minimum_budgets_minor: minimums,
            schedule_granularity: ScheduleGranularity::Daily,
            targeting_dimensions: dimensions,
            creative_placements,
            text_limits,
        },
    }
}

/// Cheap, dependency-free deterministic hash for seeding
/// [`FakeAdPlatform::fetch_metrics`]'s synthetic figures — not a
/// cryptographic or collision-resistant hash, just a stable seed.
fn fnv1a(bytes: &[u8]) -> u64 {
    const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    bytes.iter().fold(OFFSET_BASIS, |hash, byte| {
        (hash ^ *byte as u64).wrapping_mul(PRIME)
    })
}

#[async_trait]
impl AdPlatformPort for FakeAdPlatform {
    fn descriptor(&self) -> PlatformDescriptor {
        self.descriptor.clone()
    }

    fn required_scopes(&self) -> Vec<String> {
        vec!["ads.read".to_string(), "ads.manage".to_string()]
    }

    async fn begin_authorization(
        &self,
        connection: &ConnectionHandle,
        request: &AuthorizationRequest,
    ) -> Result<AuthorizationStart, AdvertisingError> {
        if connection.id.trim().is_empty() || request.redirect_uri.trim().is_empty() {
            return Err(AdvertisingError::InvalidInput(
                "authorization requires a connection and redirect URI".to_string(),
            ));
        }
        Ok(AuthorizationStart {
            authorization_url: format!(
                "https://fake.invalid/authorize/{}?state={}",
                connection.id, request.state
            ),
        })
    }

    async fn list_accounts(
        &self,
        connection: &ConnectionHandle,
    ) -> Result<Vec<PlatformAccount>, AdvertisingError> {
        Ok(vec![PlatformAccount {
            external_id: connection.id.clone(),
            display_name: "Fake account".to_string(),
        }])
    }

    async fn exchange_authorization(
        &self,
        exchange: &AuthorizationExchange,
    ) -> Result<ExchangedAuthorization, AdvertisingError> {
        if exchange.code.trim().is_empty() || exchange.redirect_uri.trim().is_empty() {
            return Err(AdvertisingError::InvalidInput(
                "authorization exchange requires a code and redirect URI".to_string(),
            ));
        }
        Ok(ExchangedAuthorization {
            refresh_token: format!("fake-refresh-token-for-{}", exchange.code),
            granted_scopes: vec!["ads.read".to_string(), "ads.manage".to_string()],
            expires_at: None,
            accounts: vec![PlatformAccount {
                external_id: format!("fake-account-for-{}", exchange.code),
                display_name: "Fake account".to_string(),
            }],
        })
    }

    async fn create_campaign(
        &self,
        _connection: &ConnectionHandle,
        campaign: Campaign,
        _mutation_id: uuid::Uuid,
    ) -> Result<Campaign, AdvertisingError> {
        // This fake's platform assigns each ad an id equal to the id the
        // caller sent, and hands the tree straight back — exactly the
        // `create_campaign` contract the real adapters must honour once they
        // create ads remotely. Publish persists the assignment onto the
        // intent, which is what lets review polling match by platform id.
        let mut assigned = campaign.clone();
        for group in &mut assigned.ad_groups {
            for ad in &mut group.ads {
                if ad.external_id.is_none() {
                    ad.external_id = Some(ad.id.clone());
                }
            }
        }
        self.campaigns
            .lock()
            .map_err(|_| {
                AdvertisingError::PlatformUnavailable("fake state lock poisoned".to_string())
            })?
            .insert(assigned.id.clone(), assigned.clone());
        Ok(assigned)
    }

    async fn update_campaign(
        &self,
        _connection: &ConnectionHandle,
        campaign: Campaign,
        _mutation_id: uuid::Uuid,
    ) -> Result<Campaign, AdvertisingError> {
        let mut campaigns = self.campaigns.lock().map_err(|_| {
            AdvertisingError::PlatformUnavailable("fake state lock poisoned".to_string())
        })?;
        if !campaigns.contains_key(&campaign.id) {
            return Err(AdvertisingError::PlatformUnavailable(
                "campaign not found".to_string(),
            ));
        }
        campaigns.insert(campaign.id.clone(), campaign.clone());
        Ok(campaign)
    }

    async fn get_campaign(
        &self,
        _connection: &ConnectionHandle,
        campaign_id: &str,
    ) -> Result<Option<Campaign>, AdvertisingError> {
        Ok(self
            .campaigns
            .lock()
            .map_err(|_| {
                AdvertisingError::PlatformUnavailable("fake state lock poisoned".to_string())
            })?
            .get(campaign_id)
            .cloned())
    }

    async fn pause_campaign(
        &self,
        _connection: &ConnectionHandle,
        campaign_id: &str,
    ) -> Result<Campaign, AdvertisingError> {
        let mut campaigns = self.campaigns.lock().map_err(|_| {
            AdvertisingError::PlatformUnavailable("fake state lock poisoned".to_string())
        })?;
        let campaign = campaigns.get_mut(campaign_id).ok_or_else(|| {
            AdvertisingError::PlatformUnavailable("campaign not found".to_string())
        })?;
        campaign.status = CampaignStatus::Paused;
        Ok(campaign.clone())
    }

    async fn resume_campaign(
        &self,
        _connection: &ConnectionHandle,
        campaign_id: &str,
    ) -> Result<Campaign, AdvertisingError> {
        let mut campaigns = self.campaigns.lock().map_err(|_| {
            AdvertisingError::PlatformUnavailable("fake state lock poisoned".to_string())
        })?;
        let campaign = campaigns.get_mut(campaign_id).ok_or_else(|| {
            AdvertisingError::PlatformUnavailable("campaign not found".to_string())
        })?;
        campaign.status = CampaignStatus::Active;
        Ok(campaign.clone())
    }

    async fn delete_campaign(
        &self,
        _connection: &ConnectionHandle,
        campaign_id: &str,
    ) -> Result<(), AdvertisingError> {
        let removed = self
            .campaigns
            .lock()
            .map_err(|_| {
                AdvertisingError::PlatformUnavailable("fake state lock poisoned".to_string())
            })?
            .remove(campaign_id);
        if removed.is_some() {
            Ok(())
        } else {
            Err(AdvertisingError::PlatformUnavailable(
                "campaign not found".to_string(),
            ))
        }
    }

    async fn fetch_metrics(
        &self,
        _connection: &ConnectionHandle,
        request: &MetricQuery,
    ) -> Result<Vec<MetricPoint>, AdvertisingError> {
        let campaigns = self.campaigns.lock().map_err(|_| {
            AdvertisingError::PlatformUnavailable("fake state lock poisoned".to_string())
        })?;
        let from = chrono::NaiveDate::parse_from_str(&request.from, "%Y-%m-%d").map_err(|_| {
            AdvertisingError::InvalidInput(
                "fake ad platform: invalid metrics query from".to_string(),
            )
        })?;
        let to = chrono::NaiveDate::parse_from_str(&request.to, "%Y-%m-%d").map_err(|_| {
            AdvertisingError::InvalidInput("fake ad platform: invalid metrics query to".to_string())
        })?;

        let mut points = Vec::new();
        let mut date = from;
        let mut day_index: i64 = 0;
        while date <= to {
            for campaign in campaigns.values() {
                // Deterministic, non-zero, per-campaign-per-day figures — no
                // real reporting API to call while live credentials are
                // blocked (same gap as every other adapter in this phase).
                // Stable across runs so ingestion/rollup/ROAS tests can
                // assert on it, and varied enough day-to-day and
                // campaign-to-campaign to exercise idempotent restatement.
                let seed = fnv1a(campaign.id.as_bytes())
                    ^ (day_index as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
                let impressions = 1_000 + (seed % 5_000) as i64;
                let clicks = 20 + (seed % 200) as i64;
                let spend_minor = 5_000 + (seed % 50_000) as i64;
                // Fractional on purpose: real platforms weight view-through
                // and cross-device conversions rather than always reporting
                // whole units, and `MetricPoint::conversions` is `f64` for
                // exactly that reason.
                let conversions = 1.0 + (seed % 1000) as f64 / 100.0;
                let conversion_value_minor = spend_minor + (seed % 20_000) as i64;
                let currency = campaign.budget.amount.currency.clone();
                points.push(MetricPoint {
                    campaign_id: campaign.id.clone(),
                    date: date.format("%Y-%m-%d").to_string(),
                    impressions,
                    clicks,
                    spend: Money {
                        amount_minor: spend_minor,
                        currency: currency.clone(),
                    },
                    conversions,
                    conversion_value: Money {
                        amount_minor: conversion_value_minor,
                        currency,
                    },
                });
            }
            let Some(next) = date.succ_opt() else {
                break;
            };
            date = next;
            day_index += 1;
        }
        Ok(points)
    }

    async fn fetch_review_status(
        &self,
        _connection: &ConnectionHandle,
        campaign_id: &str,
    ) -> Result<Vec<AdReviewObservation>, AdvertisingError> {
        let campaigns = self.campaigns.lock().map_err(|_| {
            AdvertisingError::PlatformUnavailable("fake state lock poisoned".to_string())
        })?;
        let Some(campaign) = campaigns.get(campaign_id) else {
            return Ok(Vec::new());
        };
        // A live platform reviews ads asynchronously after publish. The fake
        // short-circuits that: an Active campaign's ads review clean, anything
        // else is still pending — deterministic for the contract suite.
        let state = if campaign.status == CampaignStatus::Active {
            AdReviewState::Approved
        } else {
            AdReviewState::Pending
        };
        Ok(campaign
            .ad_groups
            .iter()
            .flat_map(|group| group.ads.iter())
            .map(|ad| AdReviewObservation {
                // Ads the platform assigned ids to are listed by that id;
                // ads created after publish (added through a later update,
                // never assigned one by this fake) are listed by the id the
                // caller sent — this fake treats everything as known.
                ad_external_id: ad.external_id.clone().unwrap_or_else(|| ad.id.clone()),
                state,
                reasons: Vec::new(),
            })
            .collect())
    }

    async fn upload(
        &self,
        _connection: &ConnectionHandle,
        conversion: UploadedConversion,
        ids: HashedIds,
        consent: ConsentSnapshot,
    ) -> Result<(), AdvertisingError> {
        if conversion.event_id.trim().is_empty()
            || ids.values.values().any(|value| value.trim().is_empty())
            || consent.resolver_version.trim().is_empty()
        {
            return Err(AdvertisingError::InvalidInput(
                "conversion upload requires a complete directive proof".to_string(),
            ));
        }
        Ok(())
    }
}

#[derive(Clone)]
pub struct FakeConversionSink {
    descriptor: PlatformDescriptor,
    receipts: Arc<Mutex<Vec<UploadReceipt>>>,
    uploaded_events: Arc<Mutex<Vec<(ConnectionHandle, ConversionEvent)>>>,
    retracted_events: Arc<Mutex<Vec<(ConnectionHandle, ConversionEvent)>>>,
    upload_error: Arc<Mutex<Option<AdvertisingError>>>,
    retract_error: Arc<Mutex<Option<AdvertisingError>>>,
}

impl FakeConversionSink {
    pub fn new(descriptor: PlatformDescriptor) -> Self {
        Self {
            descriptor,
            receipts: Arc::new(Mutex::new(Vec::new())),
            uploaded_events: Arc::new(Mutex::new(Vec::new())),
            retracted_events: Arc::new(Mutex::new(Vec::new())),
            upload_error: Arc::new(Mutex::new(None)),
            retract_error: Arc::new(Mutex::new(None)),
        }
    }

    pub fn with_upload_error(self, error: AdvertisingError) -> Self {
        *self.upload_error.lock().unwrap() = Some(error);
        self
    }

    pub fn with_retract_error(self, error: AdvertisingError) -> Self {
        *self.retract_error.lock().unwrap() = Some(error);
        self
    }

    pub fn set_upload_error(&self, error: Option<AdvertisingError>) {
        *self.upload_error.lock().unwrap() = error;
    }

    pub fn set_retract_error(&self, error: Option<AdvertisingError>) {
        *self.retract_error.lock().unwrap() = error;
    }

    pub fn uploaded_events(&self) -> Vec<ConversionEvent> {
        self.uploaded_events
            .lock()
            .unwrap()
            .iter()
            .map(|(_, e)| e.clone())
            .collect()
    }

    pub fn uploaded_records(&self) -> Vec<(ConnectionHandle, ConversionEvent)> {
        self.uploaded_events.lock().unwrap().clone()
    }

    pub fn receipts(&self) -> Vec<UploadReceipt> {
        self.receipts.lock().unwrap().clone()
    }

    pub fn retracted_events(&self) -> Vec<ConversionEvent> {
        self.retracted_events
            .lock()
            .unwrap()
            .iter()
            .map(|(_, e)| e.clone())
            .collect()
    }

    pub fn retracted_records(&self) -> Vec<(ConnectionHandle, ConversionEvent)> {
        self.retracted_events.lock().unwrap().clone()
    }
}

#[async_trait]
impl ConversionSinkPort for FakeConversionSink {
    fn descriptor(&self) -> PlatformDescriptor {
        self.descriptor.clone()
    }

    async fn upload(
        &self,
        connection: &ConnectionHandle,
        event: &ConversionEvent,
    ) -> Result<UploadReceipt, AdvertisingError> {
        if let Some(err) = self.upload_error.lock().unwrap().clone() {
            return Err(err);
        }

        if event.event_id.trim().is_empty() {
            return Err(AdvertisingError::InvalidInput(
                "conversion upload requires a valid event_id".to_string(),
            ));
        }

        let receipt = UploadReceipt {
            platform: self.descriptor.key.clone(),
            external_id: format!(
                "receipt-{}-{}",
                self.descriptor.key.as_str(),
                event.event_id
            ),
            uploaded_at: chrono::Utc::now(),
        };

        self.uploaded_events
            .lock()
            .unwrap()
            .push((connection.clone(), event.clone()));
        self.receipts.lock().unwrap().push(receipt.clone());

        Ok(receipt)
    }

    async fn retract(
        &self,
        connection: &ConnectionHandle,
        event: &ConversionEvent,
    ) -> Result<(), AdvertisingError> {
        if let Some(err) = self.retract_error.lock().unwrap().clone() {
            return Err(err);
        }

        if event.event_id.trim().is_empty() {
            return Err(AdvertisingError::InvalidInput(
                "conversion retraction requires a valid event_id".to_string(),
            ));
        }

        self.retracted_events
            .lock()
            .unwrap()
            .push((connection.clone(), event.clone()));

        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FakeAudienceSyncCall {
    pub connection: ConnectionHandle,
    pub external_ref: Option<String>,
    pub included: Vec<HashedIdentifier>,
    pub removed: Vec<HashedIdentifier>,
}

#[derive(Clone)]
pub struct FakeAudienceSync {
    descriptor: PlatformDescriptor,
    sync_calls: Arc<Mutex<Vec<FakeAudienceSyncCall>>>,
    sync_error: Arc<Mutex<Option<AdvertisingError>>>,
    unsupported_removal: Arc<Mutex<bool>>,
}

impl FakeAudienceSync {
    pub fn new(descriptor: PlatformDescriptor) -> Self {
        Self {
            descriptor,
            sync_calls: Arc::new(Mutex::new(Vec::new())),
            sync_error: Arc::new(Mutex::new(None)),
            unsupported_removal: Arc::new(Mutex::new(false)),
        }
    }

    pub fn with_sync_error(self, error: AdvertisingError) -> Self {
        *self.sync_error.lock().unwrap() = Some(error);
        self
    }

    pub fn with_unsupported_removal(self, unsupported: bool) -> Self {
        *self.unsupported_removal.lock().unwrap() = unsupported;
        self
    }

    pub fn set_sync_error(&self, error: Option<AdvertisingError>) {
        *self.sync_error.lock().unwrap() = error;
    }

    pub fn set_unsupported_removal(&self, unsupported: bool) {
        *self.unsupported_removal.lock().unwrap() = unsupported;
    }

    pub fn sync_calls(&self) -> Vec<FakeAudienceSyncCall> {
        self.sync_calls.lock().unwrap().clone()
    }
}

#[async_trait]
impl AudienceSyncPort for FakeAudienceSync {
    fn descriptor(&self) -> PlatformDescriptor {
        self.descriptor.clone()
    }

    async fn sync_audience(
        &self,
        connection: &ConnectionHandle,
        external_ref: Option<&str>,
        included: &[HashedIdentifier],
        removed: &[HashedIdentifier],
    ) -> Result<AudienceSyncReceipt, AdvertisingError> {
        if let Some(err) = self.sync_error.lock().unwrap().clone() {
            return Err(err);
        }

        if *self.unsupported_removal.lock().unwrap() && !removed.is_empty() {
            return Err(AdvertisingError::Validation(vec![
                ValidationViolation::unsupported(
                    "removed",
                    "this platform does not support active member removal",
                ),
            ]));
        }

        let ref_val = external_ref.map(ToString::to_string).unwrap_or_else(|| {
            format!(
                "aud-{}-{}",
                self.descriptor.key.as_str(),
                uuid::Uuid::now_v7()
            )
        });

        self.sync_calls.lock().unwrap().push(FakeAudienceSyncCall {
            connection: connection.clone(),
            external_ref: external_ref.map(ToString::to_string),
            included: included.to_vec(),
            removed: removed.to_vec(),
        });

        Ok(AudienceSyncReceipt {
            external_ref: ref_val,
        })
    }
}

#[cfg(test)]
mod tests {
    fn handle_tenant() -> sanvi_kernel::TenantId {
        sanvi_kernel::TenantId::try_from(1_i64).unwrap()
    }

    use super::*;
    use crate::domain::{
        Ad, AdGroup, BidSettings, Budget, ConversionId, Creative, CreativeText, DriftState,
        Schedule, Targeting, UploadState, ValidationViolation, validate_campaign,
    };
    use sanvi_kernel::TenantId;

    fn campaign(objective: &str, kind: BudgetType, locale: &str, headline: &str) -> Campaign {
        Campaign {
            id: "campaign-1".to_string(),
            name: "Campaign".to_string(),
            status: CampaignStatus::Draft,
            objective: Objective::parse(objective).unwrap(),
            budget: Budget {
                kind,
                amount: Money::new(100, "JPY").unwrap(),
            },
            schedule: Some(Schedule {
                starts_at: None,
                ends_at: None,
            }),
            ad_groups: vec![AdGroup {
                id: "group-1".to_string(),
                name: "Group".to_string(),
                targeting: Targeting::default(),
                bid: BidSettings {
                    strategy: "manual".to_string(),
                    maximum_bid: None,
                },
                ads: vec![Ad {
                    id: "ad-1".to_string(),
                    external_id: None,
                    creative: Creative {
                        id: "creative-1".to_string(),
                        placement: "feed".to_string(),
                        texts: vec![CreativeText {
                            locale: locale.to_string(),
                            headline: headline.to_string(),
                            body: "body".to_string(),
                        }],
                        asset_references: vec!["asset-1".to_string()],
                    },
                    landing_url: "https://tenant.example/landing".to_string(),
                    tracking_template: None,
                    review: Default::default(),
                }],
            }],
            drift: DriftState {
                drifted: false,
                changed_fields: vec![],
            },
        }
    }

    #[test]
    fn fixture_file_is_the_frontend_export() {
        let exported: Vec<PlatformDescriptor> =
            serde_json::from_str(include_str!("../../fixtures/capability-matrices.json")).unwrap();
        assert_eq!(exported, FakeAdPlatform::fixtures());
    }

    #[test]
    fn meta_fixture_uses_the_persisted_platform_key() {
        assert_eq!(FakeAdPlatform::fixtures()[1].key.as_str(), "meta");
    }

    #[test]
    fn forbidden_objective_and_budget_report_field_paths() {
        let matrix = &FakeAdPlatform::fixtures()[2].capability_matrix;
        let violations =
            validate_campaign(matrix, &campaign("sales", BudgetType::Daily, "en", "valid"));
        let paths = violations
            .iter()
            .map(|violation: &ValidationViolation| violation.field_path.as_str())
            .collect::<Vec<_>>();
        assert!(paths.contains(&"objective"));
        assert!(paths.contains(&"budget.kind"));
        assert!(!paths.contains(&"budget.amount.currency"));
    }

    #[test]
    fn campaign_budget_optimization_is_meta_only() {
        let meta_matrix = &FakeAdPlatform::fixtures()[1].capability_matrix;
        let google_matrix = &FakeAdPlatform::fixtures()[0].capability_matrix;

        assert!(
            validate_campaign(
                meta_matrix,
                &campaign(
                    "sales",
                    BudgetType::CampaignBudgetOptimization,
                    "en",
                    "valid"
                )
            )
            .is_empty(),
            "meta declares CBO, so a CBO campaign must validate cleanly"
        );

        let violations = validate_campaign(
            google_matrix,
            &campaign(
                "sales",
                BudgetType::CampaignBudgetOptimization,
                "en",
                "valid",
            ),
        );
        let paths = violations
            .iter()
            .map(|violation: &ValidationViolation| violation.field_path.as_str())
            .collect::<Vec<_>>();
        assert!(
            paths.contains(&"budget.kind"),
            "google does not declare CBO, so it must be rejected on the budget.kind path"
        );
    }

    #[test]
    fn japanese_limit_can_reject_text_that_english_limit_accepts() {
        let matrix = &FakeAdPlatform::fixtures()[1].capability_matrix;
        let text = "あ".repeat(30);
        assert!(
            validate_campaign(matrix, &campaign("sales", BudgetType::Daily, "en", &text))
                .is_empty()
        );
        assert!(
            validate_campaign(matrix, &campaign("sales", BudgetType::Daily, "ja", &text))
                .iter()
                .any(|violation| violation.field_path.ends_with(".headline"))
        );
    }

    #[test]
    fn missing_locale_limit_rejects_creative_text() {
        let mut matrix = FakeAdPlatform::fixtures()[1].capability_matrix.clone();
        matrix.text_limits.remove("ja");
        let violations = validate_campaign(
            &matrix,
            &campaign("sales", BudgetType::Daily, "ja", "valid"),
        );
        assert!(violations.iter().any(|violation| {
            violation.field_path.ends_with(".headline")
                && violation.code == "capability_unsupported"
        }));
    }

    #[test]
    fn every_validator_capability_has_complete_matrix_data() {
        for descriptor in FakeAdPlatform::fixtures() {
            let matrix = descriptor.capability_matrix;
            assert!(!matrix.objectives.is_empty());
            assert!(!matrix.budget_types.is_empty());
            assert!(!matrix.targeting_dimensions.is_empty());
            assert!(!matrix.creative_placements.is_empty());
            for limits in matrix.text_limits.values() {
                assert!(limits.contains_key("headline"));
                assert!(limits.contains_key("body"));
            }
            for currency_minimums in matrix.minimum_budgets_minor.values() {
                for kind in &matrix.budget_types {
                    assert!(currency_minimums.contains_key(kind));
                }
            }
        }
    }

    #[tokio::test]
    async fn fake_adapter_satisfies_shared_ad_platform_port_contract() {
        let adapter = FakeAdPlatform::new(FakeAdPlatform::fixtures()[0].clone());
        crate::adapters::contract::assert_ad_platform_contract(&adapter)
            .await
            .unwrap();
    }

    #[test]
    fn third_fixture_registers_through_the_same_catalog_path() {
        let mut registry = PlatformRegistry::new();
        for descriptor in FakeAdPlatform::fixtures() {
            registry
                .register(Arc::new(FakeAdPlatform::new(descriptor)))
                .unwrap();
        }
        assert_eq!(registry.adapters().len(), 3);
    }

    fn sample_conversion_event(event_id: &str) -> ConversionEvent {
        ConversionEvent {
            id: ConversionId::try_from(1_000_900_i64).unwrap(),
            tenant_id: TenantId::try_from(1_000_901_i64).unwrap(),
            event_id: event_id.to_string(),
            dedupe_key: format!("test:{}", event_id),
            name: "purchase".to_string(),
            occurred_at: chrono::Utc::now(),
            value: Some(Money::new(1000, "USD").unwrap()),
            value_source: Some(crate::domain::ValueSource::PaymentRecord),
            order_ref: Some("order-123".to_string()),
            click_ids: crate::domain::ClickIds::default(),
            hashed_identifiers: BTreeMap::new(),
            consent: ConsentSnapshot {
                resolver_version: "v1".to_string(),
                jurisdiction: "us".to_string(),
                purposes_asked: vec!["ads_measurement".to_string()],
                answers: BTreeMap::from([("ads_measurement".to_string(), "allowed".to_string())]),
                signal_source: "default".to_string(),
            },
            upload_states: BTreeMap::new(),
            subject_key: crate::domain::SubjectKeyRef::TenantDevice {
                device_ref: "device-stub".to_string(),
            },
        }
    }

    #[tokio::test]
    async fn fake_conversion_sink_upload_succeeds_and_records_receipt() {
        let sink = FakeConversionSink::new(FakeAdPlatform::fixtures()[0].clone());
        let connection = ConnectionHandle {
            id: "conn-1".to_string(),
            tenant_id: handle_tenant(),
            external_account_id: "conn-1".to_string(),
        };
        let event = sample_conversion_event("evt-upload-1");

        let receipt = sink.upload(&connection, &event).await.unwrap();

        assert_eq!(receipt.platform, sink.descriptor().key);
        assert_eq!(receipt.external_id, "receipt-google_ads-evt-upload-1");

        let uploaded = sink.uploaded_events();
        assert_eq!(uploaded.len(), 1);
        assert_eq!(uploaded[0].event_id, "evt-upload-1");

        let records = sink.uploaded_records();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].0.id, "conn-1");
        assert_eq!(records[0].1.event_id, "evt-upload-1");

        let receipts = sink.receipts();
        assert_eq!(receipts.len(), 1);
        assert_eq!(receipts[0], receipt);
    }

    #[tokio::test]
    async fn fake_conversion_sink_upload_error_configuration() {
        let sink = FakeConversionSink::new(FakeAdPlatform::fixtures()[0].clone())
            .with_upload_error(AdvertisingError::PlatformUnavailable(
                "provider down".to_string(),
            ));
        let connection = ConnectionHandle {
            id: "conn-1".to_string(),
            tenant_id: handle_tenant(),
            external_account_id: "conn-1".to_string(),
        };
        let event = sample_conversion_event("evt-err-1");

        let result = sink.upload(&connection, &event).await;
        assert!(matches!(
            result,
            Err(AdvertisingError::PlatformUnavailable(_))
        ));
    }

    #[test]
    fn upload_states_map_carries_independent_state_for_different_platforms() {
        let mut event = sample_conversion_event("evt-multi-platform");
        assert!(event.upload_states.is_empty());

        let google_ads = PlatformKey::parse("google_ads").unwrap();
        let meta = PlatformKey::parse("meta").unwrap();

        event.upload_states.insert(
            google_ads.clone(),
            UploadState::Uploaded { attempt_count: 0 },
        );
        event.upload_states.insert(
            meta.clone(),
            UploadState::Failed {
                reason: "invalid access token".to_string(),
                attempt_count: 0,
            },
        );

        assert_eq!(
            event.upload_states.get(&google_ads),
            Some(&UploadState::Uploaded { attempt_count: 0 })
        );
        assert_eq!(
            event.upload_states.get(&meta),
            Some(&UploadState::Failed {
                reason: "invalid access token".to_string(),
                attempt_count: 0,
            })
        );

        // Mutating google_ads does not clobber or affect meta
        event.upload_states.insert(
            google_ads.clone(),
            UploadState::Pending { attempt_count: 0 },
        );

        assert_eq!(
            event.upload_states.get(&google_ads),
            Some(&UploadState::Pending { attempt_count: 0 })
        );
        assert_eq!(
            event.upload_states.get(&meta),
            Some(&UploadState::Failed {
                reason: "invalid access token".to_string(),
                attempt_count: 0,
            })
        );
    }

    #[tokio::test]
    async fn retract_calls_fake_conversion_sink_and_does_not_mutate_unrelated_platforms() {
        let sink = FakeConversionSink::new(FakeAdPlatform::fixtures()[0].clone());
        let connection = ConnectionHandle {
            id: "conn-google".to_string(),
            tenant_id: handle_tenant(),
            external_account_id: "conn-google".to_string(),
        };

        let google_ads = PlatformKey::parse("google_ads").unwrap();
        let meta = PlatformKey::parse("meta").unwrap();

        let mut event = sample_conversion_event("evt-retract-1");
        event.upload_states.insert(
            google_ads.clone(),
            UploadState::Uploaded { attempt_count: 0 },
        );
        event
            .upload_states
            .insert(meta.clone(), UploadState::Uploaded { attempt_count: 0 });

        sink.retract(&connection, &event).await.unwrap();

        let retracted = sink.retracted_events();
        assert_eq!(retracted.len(), 1);
        assert_eq!(retracted[0].event_id, "evt-retract-1");

        // Retraction state update on the caller side removes/updates google_ads
        // but does not mutate meta's entry
        event.upload_states.remove(&google_ads);
        assert_eq!(event.upload_states.get(&google_ads), None);
        assert_eq!(
            event.upload_states.get(&meta),
            Some(&UploadState::Uploaded { attempt_count: 0 })
        );
    }

    #[tokio::test]
    async fn fake_conversion_sink_retract_error_configuration() {
        let sink = FakeConversionSink::new(FakeAdPlatform::fixtures()[0].clone())
            .with_retract_error(AdvertisingError::PlatformUnavailable(
                "service unavailable".to_string(),
            ));
        let connection = ConnectionHandle {
            id: "conn-1".to_string(),
            tenant_id: handle_tenant(),
            external_account_id: "conn-1".to_string(),
        };
        let event = sample_conversion_event("evt-retract-err");

        let result = sink.retract(&connection, &event).await;
        assert!(matches!(
            result,
            Err(AdvertisingError::PlatformUnavailable(_))
        ));
    }
}
