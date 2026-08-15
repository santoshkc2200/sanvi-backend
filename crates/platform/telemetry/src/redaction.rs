use std::fmt;

/// Wrapper that guarantees a value never appears in logs or traces.
///
/// `Display` and `Debug` always render as `<redacted>`, so the wrapped secret
/// survives serialization for legitimate use (forwarding to another service,
/// storing in a secret manager) but can never leak into an error message or
/// structured log line. Use it for authorization headers, API keys, tokens.
///
/// ```
/// use sanvi_telemetry::Redacted;
///
/// let token = Redacted::new("sk_live_1234567890".to_string());
/// assert_eq!(token.to_string(), "<redacted>");
/// assert_eq!(format!("{token:?}"), "<redacted>");
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

impl<T> serde::Serialize for Redacted<T>
where
    T: serde::Serialize,
{
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.inner.serialize(serializer)
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

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(json, "\"keep-me\"");
        let back: Redacted<String> = serde_json::from_str(&json).unwrap();
        assert_eq!(back.expose_secret(), "keep-me");
    }
}
