//! Review-status polling — TASK-012's "per-ad review state with rejection
//! reasons surfaced **verbatim** from the platform".
//!
//! Polling is an observation with one write side-effect: the platform's
//! verdict is merged into the campaign intent's ads so the dashboard can show
//! it without another platform call. The verdict never mutates the ad itself
//! and never resolves anything — a rejected ad stays live on the platform
//! until a *tenant* changes it, exactly like drift.

use std::sync::Arc;

use chrono::Utc;
use serde::Serialize;

use sanvi_kernel::TenantId;
use sanvi_persistence::TxManager;

use crate::{
    application::PlatformRegistry,
    domain::{
        AdReview, AdReviewObservation, AdvertisingConnectionRepository, AdvertisingError,
        CampaignId, CampaignRepository, ConnectionHandle, StoredCampaign,
    },
};

/// One ad's refreshed review state, addressed by its platform-facing id so a
/// client can correlate against the campaign tree it already has.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct AdReviewStatusView {
    pub ad_id: String,
    pub review: AdReview,
}

/// What the platform listed that this campaign's tree cannot account for.
/// A published campaign whose ads were never pushed (or whose platform ids
/// were never recorded) can never match the platform's listings — surfacing
/// the gap is what keeps an all-pending response from reading as a clean,
/// successful poll when it is really "nothing correlated".
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct UnmatchedPlatformAds {
    /// Platform-listed ad ids that matched no ad in the campaign tree.
    pub platform_ad_ids: Vec<String>,
    /// Ads in the campaign tree with no recorded platform id — these can
    /// never match anything until the platform's ids are persisted onto them.
    pub ads_without_platform_id: usize,
}

pub struct PollCampaignReviewStatus {
    tx: TxManager,
    campaigns: Arc<dyn CampaignRepository>,
    connections: Arc<dyn AdvertisingConnectionRepository>,
    platforms: Arc<PlatformRegistry>,
}

impl PollCampaignReviewStatus {
    pub fn new(
        tx: TxManager,
        campaigns: Arc<dyn CampaignRepository>,
        connections: Arc<dyn AdvertisingConnectionRepository>,
        platforms: Arc<PlatformRegistry>,
    ) -> Self {
        Self {
            tx,
            campaigns,
            connections,
            platforms,
        }
    }

    /// Pull verdicts from the platform and persist them onto the campaign's
    /// ads. Requires a published campaign — review has no meaning for a draft
    /// the platform has never seen.
    pub async fn execute(
        &self,
        tenant_id: TenantId,
        campaign_id: CampaignId,
    ) -> Result<(Vec<AdReviewStatusView>, UnmatchedPlatformAds), AdvertisingError> {
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
            .ok_or(AdvertisingError::InvalidInput(
                "campaign has not been published; there is nothing to poll review status for"
                    .to_string(),
            ))?;
        let connection = self
            .connections
            .find_by_id(&mut scope, tenant_id, stored.connection_id)
            .await?
            .ok_or(AdvertisingError::ConnectionNotFound)?;
        let handle = ConnectionHandle {
            id: connection.id().to_string(),
            tenant_id: connection.tenant_id(),
            external_account_id: connection.external_account_id().to_string(),
        };

        // Platform I/O stays outside the write transaction: the observations
        // are merged and persisted below in one short tenant-scoped commit.
        scope
            .rollback()
            .await
            .map_err(|error| AdvertisingError::Storage(error.to_string()))?;

        let adapter = self.platforms.adapter(&stored.platform)?;
        let observations = adapter.fetch_review_status(&handle, &external_id).await?;

        let (updated, unmatched) = merge_review_observations(stored, observations);
        let views = ad_review_views(&updated);
        // Persist the merged verdicts, but never let a poll failure beat the
        // response: the views are computed above, and the next poll re-persists.
        self.persist(tenant_id, updated).await;
        Ok((views, unmatched))
    }

    /// Persist merged verdicts. This is a *read-side* observation: the
    /// campaign's revision token is left untouched, so a client polling
    /// review status never invalidates the `If-Match` revision its next
    /// mutation carries. The same-revision write still loses to a real
    /// mutation racing it (its revision bump fails the optimistic check),
    /// which is fine — verdicts are re-fetched and re-merged on the next
    /// poll, so a skipped write self-heals.
    async fn persist(&self, tenant_id: TenantId, updated: StoredCampaign) {
        let Ok(mut scope) = self.tx.begin_for(tenant_id.as_i64()).await else {
            tracing::debug!(
                "review-status poll could not open a transaction; verdicts not persisted"
            );
            return;
        };
        match self
            .campaigns
            .update(&mut scope, &updated, updated.revision)
            .await
        {
            Ok(true) => {
                if let Err(error) = scope.commit().await {
                    tracing::debug!(%error, "review-status poll commit failed; verdicts not persisted");
                }
            }
            Ok(false) => {
                tracing::debug!(
                    "review-status poll raced a campaign mutation; verdicts not persisted"
                );
            }
            Err(error) => {
                tracing::debug!(%error, "review-status poll persist failed");
            }
        }
    }
}

/// Merge platform verdicts into the stored intent, keyed strictly by each
/// ad's recorded platform id (`Ad::external_id`). An ad with no recorded
/// platform id has never been correlated with the platform — it cannot match
/// anything and keeps its current verdict, and the caller reports it under
/// `ads_without_platform_id` rather than pretending it was refreshed. Ads the
/// platform did not mention keep their previous verdict — silently clearing
/// them would turn a platform-side listing gap into "approved" or "pending"
/// out of thin air. Returns the observations that matched nothing so the
/// response can surface the listing gap instead of dropping it silently.
pub(crate) fn merge_review_observations(
    mut stored: StoredCampaign,
    observations: Vec<AdReviewObservation>,
) -> (StoredCampaign, UnmatchedPlatformAds) {
    let by_external_id = observations
        .into_iter()
        .map(|observation| (observation.ad_external_id.clone(), observation))
        .collect::<std::collections::BTreeMap<String, AdReviewObservation>>();
    let now = Utc::now();
    let mut ads_without_platform_id = 0_usize;
    for group in &mut stored.intent.ad_groups {
        for ad in &mut group.ads {
            let Some(external_id) = &ad.external_id else {
                ads_without_platform_id += 1;
                continue;
            };
            let Some(observation) = by_external_id.get(external_id) else {
                continue;
            };
            ad.review = AdReview {
                state: observation.state,
                reasons: observation.reasons.clone(),
                checked_at: Some(now),
            };
        }
    }
    let matched = stored
        .intent
        .ad_groups
        .iter()
        .flat_map(|group| group.ads.iter())
        .filter_map(|ad| ad.external_id.as_deref())
        .collect::<std::collections::BTreeSet<_>>();
    let platform_ad_ids = by_external_id
        .keys()
        .filter(|id| !matched.contains(id.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    (
        stored,
        UnmatchedPlatformAds {
            platform_ad_ids,
            ads_without_platform_id,
        },
    )
}

fn ad_review_views(stored: &StoredCampaign) -> Vec<AdReviewStatusView> {
    stored
        .intent
        .ad_groups
        .iter()
        .flat_map(|group| group.ads.iter())
        .map(|ad| AdReviewStatusView {
            ad_id: ad.id.clone(),
            review: ad.review.clone(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{
        Ad, AdGroup, AdReviewObservation, AdReviewState, BidSettings, Budget, BudgetType, Campaign,
        CampaignId, ConnectionId, Creative, CreativeText, Money, Objective, PlatformKey,
    };

    fn campaign_with_ad(ad: Ad) -> StoredCampaign {
        let tree = Campaign {
            id: "c-1".to_string(),
            name: "Campaign".to_string(),
            status: crate::domain::CampaignStatus::Active,
            objective: Objective::parse("sales").unwrap(),
            budget: Budget {
                kind: BudgetType::Daily,
                amount: Money::new(100, "JPY").unwrap(),
            },
            schedule: None,
            ad_groups: vec![AdGroup {
                id: "g-1".to_string(),
                name: "Group".to_string(),
                targeting: Default::default(),
                bid: BidSettings {
                    strategy: "manual".to_string(),
                    maximum_bid: None,
                },
                ads: vec![ad],
            }],
            drift: crate::domain::DriftState {
                drifted: false,
                changed_fields: Vec::new(),
            },
        };
        StoredCampaign {
            id: CampaignId::try_from(1_000_001_i64).unwrap(),
            tenant_id: TenantId::try_from(1_i64).unwrap(),
            connection_id: ConnectionId::try_from(1_i64).unwrap(),
            platform: PlatformKey::parse("test_platform").unwrap(),
            external_id: Some("platform-campaign".to_string()),
            revision: 7,
            intent: tree.clone(),
            remote_snapshot: tree,
            publishing_mutation_id: None,
        }
    }

    fn ad(id: &str, external_id: Option<&str>) -> Ad {
        Ad {
            id: id.to_string(),
            external_id: external_id.map(str::to_string),
            creative: Creative {
                id: "creative-1".to_string(),
                placement: "feed".to_string(),
                texts: vec![CreativeText {
                    locale: "en".to_string(),
                    headline: "h".to_string(),
                    body: "b".to_string(),
                }],
                asset_references: Vec::new(),
            },
            landing_url: "https://tenant.example/landing".to_string(),
            tracking_template: None,
            review: AdReview::default(),
        }
    }

    fn observation(id: &str, state: AdReviewState) -> AdReviewObservation {
        AdReviewObservation {
            ad_external_id: id.to_string(),
            state,
            reasons: vec!["policy topic".to_string()],
        }
    }

    #[test]
    fn verdicts_match_only_recorded_platform_ids() {
        // The ad's platform id differs from its Sanvi id: the verdict rides
        // the platform id, and only that.
        let stored = campaign_with_ad(ad("sanvi-ad-1", Some("platform-99")));
        let (merged, unmatched) = merge_review_observations(
            stored,
            vec![observation("platform-99", AdReviewState::Rejected)],
        );
        assert_eq!(
            merged.intent.ad_groups[0].ads[0].review.state,
            AdReviewState::Rejected
        );
        assert!(unmatched.platform_ad_ids.is_empty());
        assert_eq!(unmatched.ads_without_platform_id, 0);
    }

    #[test]
    fn ads_without_platform_id_never_match_their_own_sanvi_id() {
        // Exactly the correlation gap on real platforms: an ad the platform
        // has no recorded id for must not match an observation just because
        // it echoes the Sanvi id — and the gap must be reported, not hidden.
        let stored = campaign_with_ad(ad("sanvi-ad-1", None));
        let (merged, unmatched) = merge_review_observations(
            stored,
            vec![observation("sanvi-ad-1", AdReviewState::Approved)],
        );
        assert_eq!(
            merged.intent.ad_groups[0].ads[0].review.state,
            AdReviewState::Pending
        );
        assert_eq!(unmatched.ads_without_platform_id, 1);
        assert_eq!(unmatched.platform_ad_ids, vec!["sanvi-ad-1".to_string()]);
    }

    #[test]
    fn platform_listings_that_match_nothing_are_surfaced() {
        let stored = campaign_with_ad(ad("sanvi-ad-1", Some("platform-99")));
        let (merged, unmatched) = merge_review_observations(
            stored,
            vec![
                observation("platform-99", AdReviewState::Approved),
                observation("platform-other", AdReviewState::Rejected),
            ],
        );
        assert_eq!(
            merged.intent.ad_groups[0].ads[0].review.state,
            AdReviewState::Approved
        );
        assert_eq!(
            unmatched.platform_ad_ids,
            vec!["platform-other".to_string()]
        );
    }
}
