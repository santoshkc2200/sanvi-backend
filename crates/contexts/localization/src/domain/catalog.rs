//! Message catalogs: the key vocabulary, the `Translator` port and the
//! `t!`-macro building blocks.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

pub use crate::domain::message_format::{Arg, Segment};

/// The arguments a message renders with. Keys are the placeholder names
/// used in the catalog pattern.
pub type MessageArgs<'a> = &'a [(&'a str, Arg)];

/// A translation request: the compile-time-checked key plus arguments.
/// Produced by the `t!` macro.
#[derive(Debug, Clone)]
pub struct Message {
    pub key: &'static str,
    pub args: Vec<(&'static str, Arg)>,
}

impl Message {
    pub fn new(key: &'static str) -> Self {
        Self {
            key,
            args: Vec::new(),
        }
    }

    pub fn with_args(mut self, args: Vec<(&'static str, Arg)>) -> Self {
        self.args = args;
        self
    }

    pub fn render(&self, translator: &dyn Translator, scope: &str, locale: &str) -> String {
        translator.translate(scope, locale, self.key, &self.args)
    }
}

/// The localization context's translator port.
///
/// Contract:
/// - Fallback is per key, walking the locale's fallback chain and ending in
///   the source (`en`) catalog — never an empty string, never the raw key.
/// - Missing keys log a metric and fall back (survive at runtime; CI fails
///   at build time via the `t!` macro and `i18n check`).
/// - `scope` is `"platform"` or a tenant id string; tenant overrides merge
///   over platform overrides over the static catalog.
pub trait Translator: Send + Sync {
    fn translate(&self, scope: &str, locale: &str, key: &str, args: MessageArgs<'_>) -> String;
}

/// A runtime-editable override row: tenant tone-of-voice or an urgent copy
/// fix, merged over the static catalog.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct Translation {
    pub scope: String,
    pub key: String,
    pub locale: String,
    pub value: String,
    pub updated_by: Option<uuid::Uuid>,
    pub updated_at: DateTime<Utc>,
}

impl Translation {
    /// Validate an override before it is stored: the key must exist in the
    /// static source catalog (overrides never invent keys — a typo'd key
    /// would silently never render) and the pattern must parse.
    pub fn validate(&self) -> Result<(), crate::domain::LocalizationError> {
        use crate::domain::LocalizationError;
        if !crate::generated::is_known_key(&self.key) {
            return Err(LocalizationError::UnknownMessageKey(self.key.clone()));
        }
        crate::domain::message_format::parse_pattern_with_key(&self.key, &self.value).map(|_| ())
    }
}
