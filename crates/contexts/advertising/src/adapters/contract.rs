//! Reusable behavioural contract for every `AdPlatformPort` adapter.

use std::collections::BTreeMap;

use crate::domain::{
    Ad, AdGroup, AdPlatformPort, AdvertisingError, AuthorizationRequest, BidSettings, Budget,
    BudgetType, Campaign, CampaignStatus, ConnectionHandle, ConsentSnapshot, Creative,
    CreativeText, DriftState, HashedIds, MetricQuery, Money, Objective, Schedule, Targeting,
    UploadedConversion,
};
use uuid::Uuid;

/// Exercise the provider-neutral operations expected from every adapter.
///
/// Adapter-specific suites can add provider constraints, while this suite keeps the
/// shared port behaviour from drifting as integrations are added.
pub async fn assert_ad_platform_contract(
    adapter: &dyn AdPlatformPort,
) -> Result<(), AdvertisingError> {
    let connection = ConnectionHandle {
        id: "contract-connection".to_string(),
    };
    let authorization = adapter
        .begin_authorization(
            &connection,
            &AuthorizationRequest {
                redirect_uri: "https://tenant.example/advertising/callback".to_string(),
                state: "contract-state".to_string(),
            },
        )
        .await?;
    if authorization.authorization_url.is_empty() {
        return Err(AdvertisingError::PlatformUnavailable(
            "authorization did not return a URL".to_string(),
        ));
    }
    if adapter.list_accounts(&connection).await?.is_empty() {
        return Err(AdvertisingError::PlatformUnavailable(
            "adapter did not return an account".to_string(),
        ));
    }

    let created = adapter
        .create_campaign(&connection, contract_campaign()?)
        .await?;
    if created.status != CampaignStatus::Draft {
        return Err(AdvertisingError::PlatformUnavailable(
            "new campaign did not retain draft status".to_string(),
        ));
    }
    if adapter
        .get_campaign(&connection, &created.id)
        .await?
        .is_none()
    {
        return Err(AdvertisingError::PlatformUnavailable(
            "created campaign could not be retrieved".to_string(),
        ));
    }

    let mut changed = created.clone();
    changed.name = "Updated contract campaign".to_string();
    let updated = adapter
        .update_campaign(&connection, changed, Uuid::now_v7())
        .await?;
    if updated.name != "Updated contract campaign" {
        return Err(AdvertisingError::PlatformUnavailable(
            "campaign update was not retained".to_string(),
        ));
    }
    if adapter
        .pause_campaign(&connection, &updated.id)
        .await?
        .status
        != CampaignStatus::Paused
    {
        return Err(AdvertisingError::PlatformUnavailable(
            "campaign pause was not retained".to_string(),
        ));
    }
    if adapter
        .resume_campaign(&connection, &updated.id)
        .await?
        .status
        != CampaignStatus::Active
    {
        return Err(AdvertisingError::PlatformUnavailable(
            "campaign resume was not retained".to_string(),
        ));
    }

    let metrics = adapter
        .fetch_metrics(
            &connection,
            &MetricQuery {
                from: "2026-08-01".to_string(),
                to: "2026-08-02".to_string(),
            },
        )
        .await?;
    if !metrics.iter().any(|point| point.campaign_id == updated.id) {
        return Err(AdvertisingError::PlatformUnavailable(
            "metric response did not contain campaign data".to_string(),
        ));
    }

    adapter
        .upload(
            &connection,
            UploadedConversion {
                event_id: "contract-event".to_string(),
            },
            HashedIds {
                values: BTreeMap::from([(
                    "hashed_identifier".to_string(),
                    "0123456789abcdef".to_string(),
                )]),
            },
            ConsentSnapshot {
                directive_version: "contract-v1".to_string(),
                allowed_purposes: vec!["ads_measurement".to_string()],
            },
        )
        .await?;
    adapter.delete_campaign(&connection, &updated.id).await?;
    if adapter
        .get_campaign(&connection, &updated.id)
        .await?
        .is_some()
    {
        return Err(AdvertisingError::PlatformUnavailable(
            "deleted campaign remained retrievable".to_string(),
        ));
    }
    Ok(())
}

fn contract_campaign() -> Result<Campaign, AdvertisingError> {
    Ok(Campaign {
        id: "contract-campaign".to_string(),
        name: "Contract campaign".to_string(),
        status: CampaignStatus::Draft,
        objective: Objective::parse("sales")?,
        budget: Budget {
            kind: BudgetType::Daily,
            amount: Money::new(100, "JPY")?,
        },
        schedule: Some(Schedule {
            starts_at: None,
            ends_at: None,
        }),
        ad_groups: vec![AdGroup {
            id: "contract-group".to_string(),
            name: "Contract group".to_string(),
            targeting: Targeting::default(),
            bid: BidSettings {
                strategy: "manual".to_string(),
                maximum_bid: None,
            },
            ads: vec![Ad {
                id: "contract-ad".to_string(),
                creative: Creative {
                    id: "contract-creative".to_string(),
                    placement: "feed".to_string(),
                    texts: vec![CreativeText {
                        locale: "en".to_string(),
                        headline: "Contract headline".to_string(),
                        body: "Contract body".to_string(),
                    }],
                    asset_references: vec!["contract-asset".to_string()],
                },
                landing_url: "https://tenant.example/landing".to_string(),
                tracking_template: None,
            }],
        }],
        drift: DriftState {
            drifted: false,
            changed_fields: Vec::new(),
        },
    })
}
