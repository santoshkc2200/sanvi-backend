use serde::Serialize;

/// Opaque reference to encrypted credential material. The reference may be
/// persisted, but is never a credential and never renders its locator.
#[derive(Clone, PartialEq, Eq)]
pub struct SecretRef(String);

impl SecretRef {
    pub fn new(locator: impl Into<String>) -> Self {
        Self(locator.into())
    }

    /// Opaque credential-vault metadata. Persistence adapters may write it,
    /// while transport and observability boundaries remain redacted.
    pub(crate) fn persisted_locator(&self) -> &str {
        &self.0
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.0.trim().is_empty()
    }

    #[cfg(test)]
    pub(crate) fn locator(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for SecretRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SecretRef([REDACTED])")
    }
}

impl std::fmt::Display for SecretRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("[REDACTED]")
    }
}

impl Serialize for SecretRef {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str("[REDACTED]")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_reference_never_formats_or_serializes_its_locator() {
        let secret = SecretRef::new("opaque-token-locator");
        assert_eq!(format!("{secret:?}"), "SecretRef([REDACTED])");
        assert_eq!(secret.to_string(), "[REDACTED]");
        assert_eq!(serde_json::to_string(&secret).unwrap(), "\"[REDACTED]\"");
        assert_eq!(secret.locator(), "opaque-token-locator");
    }
}
