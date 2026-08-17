//! `localization` domain: locale value objects and negotiation, message
//! catalogs and translation, formatting policy and events.
//!
//! Nothing in here imports infrastructure crates (sqlx/actix/…) — enforced
//! by `scripts/check-layering.sh`.

pub mod catalog;
pub mod error;
pub mod events;
pub mod formatting;
pub mod locale;
pub mod message_format;
pub mod ports;

pub use catalog::{Message, MessageArgs, Translation, Translator};
pub use error::LocalizationError;
pub use events::{LocalizationEvent, LocalizationEventName};
pub use formatting::{
    Address, FormattingPolicy, compare_for_locale, compare_ja, currency_exponent, normalize_input,
    normalize_postal_code, sort_strings,
};
pub use locale::{Locale, LocaleSet, LocaleSource, NegotiatedLocale, NegotiationInput, negotiate};
pub use message_format::{
    Arg, ArgStyle, BranchKind, Segment, parse_pattern, placeholder_names, render,
};
pub use ports::{
    Clock, EventPublisher, SupportedLocale, SupportedLocaleRepository, TenantLocaleRepository,
    TenantLocaleSettings, TranslationRepository, UserLocaleProvider,
};
