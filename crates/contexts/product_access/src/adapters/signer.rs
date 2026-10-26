//! Ed25519 compact-JWT signer (`alg: EdDSA`). The header carries the key id
//! so services can verify across a key rotation.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ring::signature::{Ed25519KeyPair, KeyPair};

use crate::domain::{DelegationClaims, ProductAccessError, TokenSigner};

pub struct Ed25519Signer {
    kid: String,
    key: Ed25519KeyPair,
}

impl std::fmt::Debug for Ed25519Signer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ed25519Signer")
            .field("kid", &self.kid)
            .field("key", &"[REDACTED]")
            .finish()
    }
}

impl Ed25519Signer {
    /// `seed_hex` is the 32-byte private seed in hex.
    pub fn from_seed_hex(kid: &str, seed_hex: &str) -> anyhow::Result<Self> {
        if kid.trim().is_empty() {
            anyhow::bail!("signing key id must not be empty");
        }
        let seed: [u8; 32] = hex::decode(seed_hex.trim())
            .ok()
            .and_then(|bytes| bytes.try_into().ok())
            .ok_or_else(|| anyhow::anyhow!("signing key seed must be 32 bytes of hex"))?;
        let key = Ed25519KeyPair::from_seed_unchecked(&seed)
            .map_err(|_| anyhow::anyhow!("signing key seed is not a valid Ed25519 seed"))?;
        Ok(Self {
            kid: kid.trim().to_owned(),
            key,
        })
    }

    /// `kid:hex` for the product service's verification-key setting.
    pub fn public_key_entry(&self) -> String {
        format!(
            "{}:{}",
            self.kid,
            hex::encode(self.key.public_key().as_ref())
        )
    }
}

impl TokenSigner for Ed25519Signer {
    fn sign(&self, claims: &DelegationClaims) -> Result<String, ProductAccessError> {
        let header = serde_json::json!({ "alg": "EdDSA", "typ": "JWT", "kid": self.kid });
        let encode = |value: &serde_json::Value| -> Result<String, ProductAccessError> {
            serde_json::to_vec(value)
                .map(|bytes| URL_SAFE_NO_PAD.encode(bytes))
                .map_err(|err| ProductAccessError::Storage(format!("encode token: {err}")))
        };
        let claims = serde_json::to_value(claims)
            .map_err(|err| ProductAccessError::Storage(format!("encode claims: {err}")))?;
        let signing_input = format!("{}.{}", encode(&header)?, encode(&claims)?);
        let signature = self.key.sign(signing_input.as_bytes());
        Ok(format!(
            "{signing_input}.{}",
            URL_SAFE_NO_PAD.encode(signature.as_ref())
        ))
    }
}

/// Stands in when no signing key is configured: nothing is ever forwarded.
pub struct DisabledSigner;

impl TokenSigner for DisabledSigner {
    fn sign(&self, _: &DelegationClaims) -> Result<String, ProductAccessError> {
        Err(ProductAccessError::Unavailable(
            "no delegation signing key is configured".to_owned(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use ring::signature::{ED25519, UnparsedPublicKey};

    use super::*;

    const SEED: &str = "0707070707070707070707070707070707070707070707070707070707070707";

    fn claims() -> DelegationClaims {
        DelegationClaims {
            iss: "sanvi-backend",
            aud: "ecommerce-service",
            sub: "3001".into(),
            tenant_id: "1001".into(),
            permissions: vec!["ecommerce.store.read".into()],
            limits: BTreeMap::from([("ecommerce.stores.max".into(), 3)]),
            iat: 1_800_000_000,
            exp: 1_800_000_060,
        }
    }

    #[test]
    fn signs_a_verifiable_eddsa_jwt_with_its_kid() {
        let signer = Ed25519Signer::from_seed_hex("dev-ed25519-1", SEED).unwrap();
        let token = signer.sign(&claims()).unwrap();
        let parts: Vec<_> = token.split('.').collect();
        assert_eq!(parts.len(), 3);

        let header: serde_json::Value =
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[0]).unwrap()).unwrap();
        assert_eq!(header["alg"], "EdDSA");
        assert_eq!(header["kid"], "dev-ed25519-1");
        let payload: serde_json::Value =
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[1]).unwrap()).unwrap();
        assert_eq!(payload["aud"], "ecommerce-service");
        assert_eq!(payload["limits"]["ecommerce.stores.max"], 3);

        let public_hex = signer.public_key_entry();
        let public = hex::decode(public_hex.split_once(':').unwrap().1).unwrap();
        let signature = URL_SAFE_NO_PAD.decode(parts[2]).unwrap();
        UnparsedPublicKey::new(&ED25519, public)
            .verify(format!("{}.{}", parts[0], parts[1]).as_bytes(), &signature)
            .expect("signature verifies");
    }

    #[test]
    fn matches_the_ecommerce_dev_key() {
        // ecommerce-service's `.env.example` trusts this key for the public
        // dev seed [7; 32]; a mismatch means the two repos drifted.
        let signer = Ed25519Signer::from_seed_hex("dev-ed25519-1", SEED).unwrap();
        assert_eq!(
            signer.public_key_entry(),
            "dev-ed25519-1:ea4a6c63e29c520abef5507b132ec5f9954776aebebe7b92421eea691446d22c"
        );
    }

    #[test]
    fn rejects_malformed_keys_without_echoing_them() {
        for (kid, seed) in [("k", "abcd"), ("k", "zz"), ("", SEED)] {
            let error = Ed25519Signer::from_seed_hex(kid, seed)
                .unwrap_err()
                .to_string();
            assert!(!error.contains(SEED));
        }
    }
}
