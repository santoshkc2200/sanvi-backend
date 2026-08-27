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
        AdPlatformPort, AdvertisingError, AuthorizationExchange, AuthorizationRequest,
        AuthorizationStart, BudgetType, Campaign, CampaignStatus, ConversionEvent,
        ConversionSinkPort, CreativeAssetSpec, CreativePlacement, ExchangedAuthorization,
        MetricPoint, MetricQuery, Money, PlatformAccount, PlatformDescriptor, ScheduleGranularity,
        TargetingDimension, UploadReceipt, UploadedConversion,
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
                &[BudgetType::Daily, BudgetType::Lifetime],
                &[
                    "age",
                    "custom_audiences",
                    "gender",
                    "geo",
                    "interests",
                    "languages",
                ],
                (40, 25),
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

fn descriptor(
    key: &str,
    display_name: &str,
    entitlement_key: &str,
    objectives: &[&str],
    budget_types: &[BudgetType],
    dimensions: &[&str],
    headline_limits: (usize, usize),
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
            matrix_version: "2026-08-24".to_string(),
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
        self.campaigns
            .lock()
            .map_err(|_| {
                AdvertisingError::PlatformUnavailable("fake state lock poisoned".to_string())
            })?
            .insert(campaign.id.clone(), campaign.clone());
        Ok(campaign)
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
        Ok(campaigns
            .values()
            .map(|campaign| MetricPoint {
                campaign_id: campaign.id.clone(),
                date: request.from.clone(),
                impressions: 0,
                clicks: 0,
                spend: Money {
                    amount_minor: 0,
                    currency: campaign.budget.amount.currency.clone(),
                },
                conversions: 0,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{
        Ad, AdGroup, BidSettings, Budget, Creative, CreativeText, DriftState, Schedule, Targeting,
        UploadState, ValidationViolation, validate_campaign,
    };

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
            id: uuid::Uuid::now_v7(),
            tenant_id: uuid::Uuid::now_v7(),
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
        }
    }

    #[tokio::test]
    async fn fake_conversion_sink_upload_succeeds_and_records_receipt() {
        let sink = FakeConversionSink::new(FakeAdPlatform::fixtures()[0].clone());
        let connection = ConnectionHandle {
            id: "conn-1".to_string(),
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

        event
            .upload_states
            .insert("google_ads".to_string(), UploadState::Uploaded);
        event.upload_states.insert(
            "meta".to_string(),
            UploadState::Failed {
                reason: "invalid access token".to_string(),
            },
        );

        assert_eq!(
            event.upload_states.get("google_ads"),
            Some(&UploadState::Uploaded)
        );
        assert_eq!(
            event.upload_states.get("meta"),
            Some(&UploadState::Failed {
                reason: "invalid access token".to_string(),
            })
        );

        // Mutating google_ads does not clobber or affect meta
        event
            .upload_states
            .insert("google_ads".to_string(), UploadState::Pending);

        assert_eq!(
            event.upload_states.get("google_ads"),
            Some(&UploadState::Pending)
        );
        assert_eq!(
            event.upload_states.get("meta"),
            Some(&UploadState::Failed {
                reason: "invalid access token".to_string(),
            })
        );
    }

    #[tokio::test]
    async fn retract_calls_fake_conversion_sink_and_does_not_mutate_unrelated_platforms() {
        let sink = FakeConversionSink::new(FakeAdPlatform::fixtures()[0].clone());
        let connection = ConnectionHandle {
            id: "conn-google".to_string(),
        };

        let mut event = sample_conversion_event("evt-retract-1");
        event
            .upload_states
            .insert("google_ads".to_string(), UploadState::Uploaded);
        event
            .upload_states
            .insert("meta".to_string(), UploadState::Uploaded);

        sink.retract(&connection, &event).await.unwrap();

        let retracted = sink.retracted_events();
        assert_eq!(retracted.len(), 1);
        assert_eq!(retracted[0].event_id, "evt-retract-1");

        // Retraction state update on the caller side removes/updates google_ads
        // but does not mutate meta's entry
        event.upload_states.remove("google_ads");
        assert_eq!(event.upload_states.get("google_ads"), None);
        assert_eq!(
            event.upload_states.get("meta"),
            Some(&UploadState::Uploaded)
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
        };
        let event = sample_conversion_event("evt-retract-err");

        let result = sink.retract(&connection, &event).await;
        assert!(matches!(
            result,
            Err(AdvertisingError::PlatformUnavailable(_))
        ));
    }
}
