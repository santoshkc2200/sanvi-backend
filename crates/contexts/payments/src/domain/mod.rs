//! Domain: entities, value objects, events, errors, ports.
//! No infrastructure imports allowed here (enforced by scripts/check-layering.sh).

pub mod connection;
pub mod error;
pub mod events;
pub mod money;
pub mod ports;
pub mod provider;
pub mod webhook;

pub use connection::ProviderConnection;
pub use error::PaymentsError;
pub use events::{EventPublisher, PaymentsEvent, PaymentsEventName};
pub use money::{Currency, Money};
pub use ports::ProviderConnectionRepository;
pub use provider::{ConnectionStatus, ProviderKind};
pub use webhook::{WebhookVerificationError, verify_webhook_signature};
