//! Provider-neutral advertising vocabulary.

pub mod campaign_store;
pub mod connection;
pub mod connection_lifecycle;
pub mod events;
pub mod matrix;
pub mod model;
pub mod oauth_state;
pub mod ports;
pub mod reconciliation;
pub mod secrets;
pub mod token_vault;
pub mod validation;

pub use campaign_store::{CampaignChange, CampaignRepository, ChangeSource, StoredCampaign};
pub use connection::{AdvertisingConnection, SyncError};
pub use connection_lifecycle::{
    ConnectionHealth, ConnectionStatus, RefreshFailureKind, RefreshTransition,
    refresh_failure_transition,
};
pub use events::{AdEvent, EVENT_TYPES, EventPublisher};
pub use matrix::{
    AssetMetadata, CapabilityMatrix, CreativeAssetSpec, CreativePlacement, ScheduleGranularity,
};
pub use model::{
    Ad, AdGroup, BidSettings, Budget, BudgetType, Campaign, CampaignStatus, ConnectionHandle,
    ConnectionState, ConsentSnapshot, Creative, CreativeText, DriftState, HashedIds, Money,
    Objective, PlatformKey, Schedule, Targeting, TargetingDimension,
};
pub use oauth_state::{
    InMemoryOAuthStateStore, OAuthStateClaims, OAuthStateError, OAuthStateManager, OAuthStateStore,
};
pub use ports::{
    AdPlatformPort, AdvertisingConnectionRepository, AdvertisingError, AuthorizationExchange,
    AuthorizationRequest, AuthorizationStart, CreativeRepository, ExchangedAuthorization,
    MetricPoint, MetricQuery, PlatformAccount, PlatformDescriptor, StoredCreative,
    UploadedConversion,
};
pub use reconciliation::{CampaignReconciliationState, ReconciliationResult};
pub use secrets::SecretRef;
pub use token_vault::{
    ConnectionTokenRepository, TenantEnvelopeCrypto, TenantKeyRepository, TenantKeyRow, TokenVault,
};
pub use validation::{ValidationViolation, validate_campaign};
pub mod hashing;

pub use model::{
    ClickIds, ConversionDiagnostics, ConversionEvent, SuppressedByDirective, UploadReceipt,
    UploadState, ValueSource,
};
pub use ports::{
    ConversionEventRepository, ConversionSinkPort, PaymentLookup, PaymentLookupPort,
    TrackingSettings, TrackingSettingsRepository, VendorRole, VendorRolePort,
};

pub use hashing::HashedIdentifier;
