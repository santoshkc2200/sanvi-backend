//! Advertising application services.

pub mod campaigns;
pub mod catalog;
pub mod drift_resolution;
pub mod reconciliation;
pub mod reconciliation_sweep;
pub mod registry;

pub use campaigns::{
    AdFields, AdGroupFields, AddAd, AddAdGroup, CampaignFields, CreateCampaign, GetCampaign,
    ListCampaignChanges, ListCampaigns, NewAdFields, NewAdGroupFields, NewCampaignFields,
    PatchCampaign, PauseCampaign, PublishCampaign, ResumeCampaign, UpdateAd, UpdateAdGroup,
    ValidateCampaign,
};
pub use catalog::ListPlatformCatalog;
pub use drift_resolution::{DriftResolution, ResolveCampaignDrift, ResolvedCampaignDrift};
pub use reconciliation::ReconcileCampaign;
pub use reconciliation_sweep::{ReconcileDueCampaigns, ReconciliationSweepReport};
pub use registry::PlatformRegistry;
