//! Domain: entities, value objects, events, errors, ports.
//! No infrastructure imports allowed here (enforced by scripts/check-layering.sh).

pub mod capability;
pub mod checkout;
pub mod connection;
pub mod dispute;
pub mod error;
pub mod events;
pub mod fees;
pub mod money;
pub mod payment;
pub mod payout;
pub mod ports;
pub mod provider;
pub mod tax;
pub mod timeline;
pub mod webhook;

pub use capability::{CapabilitySet, CapabilityStatus, RequirementSummary};
pub use checkout::{Checkout, CheckoutStatus, MerchantOrderRef};
pub use connection::ProviderConnection;
pub use dispute::{DisputeRecord, DisputeStatus};
pub use error::PaymentsError;
pub use events::{EventPublisher, PaymentsEvent, PaymentsEventName};
pub use fees::{FeeConfiguration, disabled_configuration};
pub use money::{Currency, Money};
pub use payment::{PaymentFailure, PaymentRecord, PaymentStatus, RefundRecord, RefundStatus};
pub use payout::PayoutRecord;
pub use ports::{
    ApplicationFeeStore, AutomaticTaxLiability, CheckoutLifecycleStore, CheckoutRepository,
    CheckoutThrottle, ConnectionLifecycleStore, ConnectionNotifier, CreateAccountRequest,
    CreateCheckoutCommand, DisputeFilter, DisputePage, DisputeRepository, PaymentFilter,
    PaymentPage, PaymentProviderPort, PaymentRepository, PaymentSort, PayoutReadPort,
    PayoutRepository, PayoutStatus, ProviderAccount, ProviderAccountSession, ProviderAccountStatus,
    ProviderCheckoutSession, ProviderConnectionRepository, ProviderDescriptor, ProviderPayout,
    ProviderRefund, ProviderRefundCommand, RefundRepository, TaxSettingsRepository,
    TenantDomainAllowList, WebhookEventRepository, WebhookVerifierPort,
};
pub use provider::{ConnectionStatus, CountryCode, ProviderKind};
pub use tax::{TaxComplianceSnapshot, TaxPreflightRejection, TaxSettingsRecord, TaxWarning};
pub use timeline::{PaymentTimeline, TimelineEntry};
pub use webhook::{
    WebhookEventRecord, WebhookVerificationError, event_account_id, event_created,
    verify_webhook_signature,
};
