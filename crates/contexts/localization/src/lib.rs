//! `localization` — locale negotiation, message catalogs and formatting
//! policy for everything the backend emits.
//!
//! The context owns: locale negotiation (request → use case → notification
//! → template), ICU-style message catalogs embedded in the binary with
//! tenant/platform overrides, and formatting policy (dates, numbers,
//! currency with per-currency minor-unit exponents, addresses, names,
//! Japanese collation).
//!
//! Usage of the `t!` macro (compile-time key check):
//!
//! ```ignore
//! let msg = t!("emails.invitation.subject", tenant = "Acme");
//! let subject = msg.render(translator.as_ref(), "platform", "ja");
//! ```
//!
//! Nothing under `src/domain/` imports infrastructure crates — enforced by
//! `scripts/check-layering.sh`.

pub mod adapters;
pub mod application;
pub mod config;
pub mod domain;
pub mod generated;

pub use config::LocalizationConfig;
pub use domain::{
    Address, Arg, Clock, EventPublisher, FormattingPolicy, Locale, LocaleSet, LocaleSource,
    LocalizationError, LocalizationEvent, LocalizationEventName, Message, NegotiatedLocale,
    NegotiationInput, Segment, SupportedLocale, SupportedLocaleRepository, TenantLocaleRepository,
    TenantLocaleSettings, Translation, TranslationRepository, Translator, UserLocaleProvider,
    compare_for_locale, compare_ja, currency_exponent, negotiate, normalize_input,
    normalize_postal_code, parse_pattern, sort_strings,
};

/// Wire localization routes into an actix `ServiceConfig`.
pub mod routes {
    pub use crate::adapters::http::{
        localization_platform_routes, localization_public_routes, localization_tenant_scope_routes,
    };
}

/// The permission requirements of this context's routes (for the
/// "no route without a permission" CI test).
pub mod catalog {
    pub use crate::adapters::http::routes::PERMISSIONED_ROUTES;
}

/// A compile-time-checked message reference.
///
/// ```ignore
/// let greeting = t!("emails.invitation.subject", tenant = tenant_name);
/// let subject = greeting.render(&translator, scope, locale);
/// ```
///
/// The key literal is checked against the embedded `en` catalog at compile
/// time: a typo'd or not-yet-translated key fails the build instead of
/// silently rendering English at runtime.
#[macro_export]
macro_rules! t {
    ($key:literal) => {{
        const _: () = {
            if !$crate::generated::is_known_key($key) {
                panic!(
                    "unknown i18n key — add it to crates/contexts/localization/catalogs/en/"
                )
            }
        };
        $crate::domain::Message::new($key)
    }};
    ($key:literal, $($name:ident = $value:expr),+ $(,)?) => {{
        const _: () = {
            if !$crate::generated::is_known_key($key) {
                panic!(
                    "unknown i18n key — add it to crates/contexts/localization/catalogs/en/"
                )
            }
        };
        $crate::domain::Message::new($key).with_args(vec![$((stringify!($name), $value.into())),+])
    }};
}

#[cfg(test)]
mod tests {
    #[test]
    fn t_macro_produces_messages() {
        let message = t!("emails.invitation.subject", tenant = "Acme");
        assert_eq!(message.key, "emails.invitation.subject");
        assert_eq!(message.args.len(), 1);
        let bare = t!("errors.title.error");
        assert_eq!(bare.key, "errors.title.error");
        assert!(bare.args.is_empty());
    }
}
