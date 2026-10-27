use serde::Deserialize;

/// Product-service connections. Secrets come from env
/// (`SANVI__PRODUCT_ACCESS__ECOMMERCE__SERVICE_KEY`,
/// `SANVI__PRODUCT_ACCESS__SIGNING_KEY_SEED`), never from committed files.
/// The defaults leave every product unconfigured: the process boots and
/// every product request fails closed with 503.
#[derive(Clone, Default, Deserialize)]
pub struct ProductAccessConfig {
    /// Key id placed in delegation-token headers.
    #[serde(default)]
    pub signing_key_id: String,
    /// 32-byte Ed25519 seed, hex. Product services trust its public key.
    #[serde(default)]
    pub signing_key_seed: String,
    #[serde(default)]
    pub ecommerce: UpstreamConfig,
    #[serde(default)]
    pub connectors: UpstreamConfig,
}

#[derive(Clone, Default, Deserialize)]
pub struct UpstreamConfig {
    /// e.g. `http://ecommerce-service:8080`; `/v1/...` is appended.
    #[serde(default)]
    pub base_url: String,
    /// Bearer key the product service accepts from the platform.
    #[serde(default)]
    pub service_key: String,
}

impl std::fmt::Debug for ProductAccessConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProductAccessConfig")
            .field("signing_key_id", &self.signing_key_id)
            .field("signing_key_seed", &"[REDACTED]")
            .field("ecommerce", &self.ecommerce)
            .field("connectors", &self.connectors)
            .finish()
    }
}

impl std::fmt::Debug for UpstreamConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UpstreamConfig")
            .field("base_url", &self.base_url)
            .field("service_key", &"[REDACTED]")
            .finish()
    }
}

impl serde::Serialize for ProductAccessConfig {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut state = serializer.serialize_struct("ProductAccessConfig", 4)?;
        state.serialize_field("signing_key_id", &self.signing_key_id)?;
        state.serialize_field("signing_key_seed", "[REDACTED]")?;
        state.serialize_field("ecommerce", &self.ecommerce)?;
        state.serialize_field("connectors", &self.connectors)?;
        state.end()
    }
}

impl serde::Serialize for UpstreamConfig {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut state = serializer.serialize_struct("UpstreamConfig", 2)?;
        state.serialize_field("base_url", &self.base_url)?;
        state.serialize_field("service_key", "[REDACTED]")?;
        state.end()
    }
}

impl ProductAccessConfig {
    /// A configured upstream needs a service key and a signing key; a
    /// half-configured product is a startup error, not a silent 503.
    pub fn validate(&self) -> anyhow::Result<()> {
        let has_upstream =
            !self.ecommerce.base_url.is_empty() || !self.connectors.base_url.is_empty();
        if !self.ecommerce.base_url.is_empty() {
            reqwest::Url::parse(&self.ecommerce.base_url).map_err(|e| {
                anyhow::anyhow!("product_access.ecommerce.base_url is not a URL: {e}")
            })?;
            if self.ecommerce.service_key.len() < 16 {
                anyhow::bail!("product_access.ecommerce.service_key must be at least 16 bytes");
            }
        }
        if !self.connectors.base_url.is_empty() {
            reqwest::Url::parse(&self.connectors.base_url).map_err(|e| {
                anyhow::anyhow!("product_access.connectors.base_url is not a URL: {e}")
            })?;
            if self.connectors.service_key.len() < 16 {
                anyhow::bail!("product_access.connectors.service_key must be at least 16 bytes");
            }
        }
        if has_upstream {
            crate::adapters::Ed25519Signer::from_seed_hex(
                &self.signing_key_id,
                &self.signing_key_seed,
            )
            .map_err(|e| anyhow::anyhow!("product_access signing key: {e}"))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEED: &str = "0707070707070707070707070707070707070707070707070707070707070707";

    fn configured() -> ProductAccessConfig {
        ProductAccessConfig {
            signing_key_id: "dev-ed25519-1".into(),
            signing_key_seed: SEED.into(),
            ecommerce: UpstreamConfig {
                base_url: "http://127.0.0.1:18081".into(),
                service_key: "dev-service-key-current".into(),
            },
            connectors: UpstreamConfig::default(),
        }
    }

    #[test]
    fn defaults_and_a_complete_config_validate() {
        ProductAccessConfig::default().validate().unwrap();
        configured().validate().unwrap();
    }

    #[test]
    fn half_configured_products_are_refused() {
        let mut no_key = configured();
        no_key.ecommerce.service_key = String::new();
        assert!(no_key.validate().is_err());
        let mut no_seed = configured();
        no_seed.signing_key_seed = String::new();
        assert!(no_seed.validate().is_err());
        let mut short_conn_key = configured();
        short_conn_key.connectors.base_url = "http://127.0.0.1:18082".into();
        short_conn_key.connectors.service_key = "short".into();
        assert!(short_conn_key.validate().is_err());
    }

    #[test]
    fn secrets_never_print() {
        let mut config = configured();
        config.connectors = UpstreamConfig {
            base_url: "http://127.0.0.1:18082".into(),
            service_key: "dev-connector-service-key-current".into(),
        };
        let debug = format!("{config:?}");
        let json = serde_json::to_string(&config).unwrap();
        for text in [debug, json] {
            assert!(!text.contains(SEED));
            assert!(!text.contains("dev-service-key-current"));
            assert!(!text.contains("dev-connector-service-key-current"));
        }
    }
}
