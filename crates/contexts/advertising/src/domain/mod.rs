//! Provider-neutral advertising vocabulary.

pub mod connection_lifecycle;
pub mod events;
pub mod matrix;
pub mod model;
pub mod oauth_state;
pub mod ports;
pub mod secrets;
pub mod validation;

pub use connection_lifecycle::{
    ConnectionHealth, ConnectionStatus, RefreshFailureKind, RefreshTransition,
    refresh_failure_transition,
};
pub use events::{AdEvent, EVENT_TYPES, EventPublisher};
pub use matrix::{CapabilityMatrix, CreativeAssetSpec, CreativePlacement, ScheduleGranularity};
pub use model::{
    Ad, AdGroup, BidSettings, Budget, BudgetType, Campaign, CampaignStatus, ConnectionHandle,
    ConnectionState, ConsentSnapshot, Creative, CreativeText, DriftState, HashedIds, Money,
    Objective, PlatformKey, Schedule, Targeting, TargetingDimension,
};
pub use oauth_state::{
    InMemoryOAuthStateStore, OAuthStateClaims, OAuthStateError, OAuthStateManager, OAuthStateStore,
};
pub use ports::{
    AdPlatformPort, AdvertisingError, AuthorizationRequest, AuthorizationStart, MetricPoint,
    MetricQuery, PlatformAccount, PlatformDescriptor, UploadedConversion,
};
pub use secrets::SecretRef;
pub use validation::{ValidationViolation, validate_campaign};
