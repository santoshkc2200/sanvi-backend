use std::fmt;

/// Wrapper that guarantees a value never appears in logs, traces, or
/// serialized output.
///
/// `Display`, `Debug` **and `Serialize`** always render as `<redacted>`, so
/// the wrapped secret can never leak into an error message, a structured log
/// line, or any JSON the type is serialized into (a response DTO, a persisted
/// document, a config dump). Deserialization still accepts the real value —
/// that asymmetry is deliberate: secrets flow *in* from configuration, never
/// *out* through a serializer. Code that legitimately needs the value (a
/// provider client, a crypto module) calls [`Redacted::expose_secret`].
/// Use it for authorization headers, API keys, tokens, passwords, keys.
///
/// ```
/// use sanvi_telemetry::Redacted;
///
/// let token = Redacted::new("sk_live_1234567890".to_string());
/// assert_eq!(token.to_string(), "<redacted>");
/// assert_eq!(format!("{token:?}"), "<redacted>");
/// assert_eq!(serde_json::to_string(&token).unwrap(), "\"<redacted>\"");
/// assert_eq!(token.expose_secret(), "sk_live_1234567890");
/// ```
pub struct Redacted<T> {
    inner: T,
}

impl<T> Redacted<T> {
    pub fn new(inner: T) -> Self {
        Self { inner }
    }

    /// Deliberate, audited access to the wrapped value.
    pub fn expose_secret(&self) -> &T {
        &self.inner
    }

    pub fn into_inner(self) -> T {
        self.inner
    }
}

impl<T> fmt::Display for Redacted<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

impl<T> fmt::Debug for Redacted<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

impl<T> Clone for Redacted<T>
where
    T: Clone,
{
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<T> Copy for Redacted<T> where T: Copy {}

impl<T> serde::Serialize for Redacted<T> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        // TASK-024 11.5f finding: this impl previously forwarded the inner
        // value, so any `Redacted` field inside a serializable struct
        // (response DTO, persisted document, config dump) emitted the secret
        // in the clear while every other render path redacted it. Serialize
        // now emits the same `<redacted>` marker as Debug/Display.
        serializer.serialize_str("<redacted>")
    }
}

impl<'de, T> serde::Deserialize<'de> for Redacted<T>
where
    T: serde::Deserialize<'de>,
{
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(Self {
            inner: T::deserialize(deserializer)?,
        })
    }
}

impl<T> AsRef<T> for Redacted<T> {
    fn as_ref(&self) -> &T {
        &self.inner
    }
}

impl<T> std::ops::Deref for Redacted<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl<T: PartialEq> PartialEq for Redacted<T> {
    fn eq(&self, other: &Self) -> bool {
        self.inner == other.inner
    }
}

impl<T: Eq> Eq for Redacted<T> {}

impl<T: Default> Default for Redacted<T> {
    fn default() -> Self {
        Self {
            inner: T::default(),
        }
    }
}

impl std::str::FromStr for Redacted<String> {
    type Err = std::convert::Infallible;

    /// Lets CLI argument parsers (clap) hold secrets in `Redacted` directly,
    /// so `--db-password` never renders through a derived `Debug`.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self::new(s.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use tracing_subscriber::layer::{Layer as _, SubscriberExt as _};

    #[test]
    fn display_and_debug_never_reveal_the_secret() {
        let secret = Redacted::new("sk_live_very_secret");
        assert_eq!(secret.to_string(), "<redacted>");
        assert_eq!(format!("{secret:?}"), "<redacted>");
        assert_eq!(format!("{secret}"), "<redacted>");
    }

    #[test]
    fn embedded_in_structures_still_hides() {
        #[derive(Debug)]
        struct Request {
            authorization: Redacted<String>,
            path: String,
        }
        let req = Request {
            authorization: Redacted::new("Bearer abc.def.ghi".to_string()),
            path: "/api/v1/system".to_string(),
        };
        let _ = (&req.authorization, &req.path);
        let debug = format!("{req:?}");
        assert!(!debug.contains("abc.def.ghi"));
        assert!(debug.contains("<redacted>"));
        assert!(debug.contains("/api/v1/system"));
    }

    #[test]
    fn deliberate_access_is_possible() {
        let secret = Redacted::new(42_u32);
        assert_eq!(*secret.expose_secret(), 42);
        assert_eq!(secret.as_ref(), &42);
        assert_eq!(*secret, 42);
    }

    #[test]
    fn serde_roundtrip_preserves_value() {
        let secret = Redacted::new("keep-me".to_string());
        let json = serde_json::to_string(&secret).unwrap();
        assert_eq!(json, "\"<redacted>\"");
        // Deserialization still accepts the real value: secrets flow in from
        // configuration, the asymmetry with Serialize is the design.
        let back: Redacted<String> = serde_json::from_str("\"keep-me\"").unwrap();
        assert_eq!(back.expose_secret(), "keep-me");
    }

    /// TASK-024 11.5f: the four render paths a secret-bearing struct can
    /// meet — Debug, Display, serde, and tracing capture — must never emit
    /// the inner value. This is the regression test for the Serialize
    /// passthrough finding: against the vulnerable commit (Serialize
    /// forwarding the inner value) the serde assertion fails.
    #[test]
    fn all_four_render_paths_redact() {
        #[derive(Debug, serde::Serialize)]
        struct Carrier {
            label: String,
            api_key: Redacted<String>,
        }
        let carrier = Carrier {
            label: "stripe".to_string(),
            api_key: Redacted::new("sk_live_do_not_render".to_string()),
        };

        // Debug (direct and nested-in-struct).
        assert!(!format!("{carrier:?}").contains("do_not_render"));
        // Display.
        assert!(!carrier.api_key.to_string().contains("do_not_render"));
        // serde — the path this gate exists for.
        let json = serde_json::to_string(&carrier).unwrap();
        assert!(!json.contains("do_not_render"), "serde leaked: {json}");
        assert!(json.contains("<redacted>"));

        // tracing capture: the emitter sees only what Debug/Display render.
        let buffer: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        #[derive(Clone)]
        struct Buf(Arc<Mutex<Vec<u8>>>);
        impl std::io::Write for Buf {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().write(b)
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let writer = {
            let buffer = buffer.clone();
            move || Buf(buffer.clone())
        };
        let subscriber = tracing_subscriber::registry().with(
            tracing_subscriber::fmt::layer()
                .json()
                .with_writer(writer)
                .with_ansi(false)
                .with_filter(tracing_subscriber::filter::LevelFilter::INFO),
        );
        let _guard = tracing::subscriber::set_default(subscriber);
        tracing::info!(carrier = ?carrier, key = %carrier.api_key, "render probe");
        let captured = String::from_utf8(buffer.lock().unwrap().clone()).unwrap();
        assert!(
            !captured.contains("do_not_render"),
            "tracing leaked: {captured}"
        );
        assert!(captured.matches("<redacted>").count() >= 2);
    }

    #[test]
    fn default_and_fromstr_support_config_and_cli_shapes() {
        let unset: Redacted<String> = Redacted::default();
        assert!(unset.expose_secret().is_empty());
        let parsed: Redacted<String> = "hunter2".parse().unwrap();
        assert_eq!(parsed.expose_secret(), "hunter2");
        assert_eq!(parsed.to_string(), "<redacted>");
    }
}
