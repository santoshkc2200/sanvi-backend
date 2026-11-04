//! Ed25519 compact-JWT signer (`alg: EdDSA`). The header carries the key id
//! so services can verify across a key rotation.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ring::signature::{ED25519, Ed25519KeyPair, KeyPair, UnparsedPublicKey};

use crate::domain::{
    CheckoutClaims, ConnectorClaims, DelegationClaims, PrivacyClaims, ProductAccessError,
    PurchaseCredentialClaims, TokenSigner,
};

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

    fn sign_json<T: serde::Serialize>(&self, claims: &T) -> Result<String, ProductAccessError> {
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

impl Ed25519Signer {
    /// Verifies a compact JWT signed by this key: three segments, an EdDSA
    /// header carrying this signer's `kid`, and a signature over the
    /// signing input. Returns the claims payload; audience and expiry are
    /// the caller's decision.
    fn verify_compact(&self, token: &str) -> Result<serde_json::Value, ProductAccessError> {
        let bad =
            || ProductAccessError::BadRequest("token is not a valid signed document".to_owned());
        let mut parts = token.split('.');
        let (header_b64, claims_b64, signature_b64) =
            match (parts.next(), parts.next(), parts.next()) {
                (Some(h), Some(c), Some(s)) if parts.next().is_none() => (h, c, s),
                _ => return Err(bad()),
            };
        let header: serde_json::Value = URL_SAFE_NO_PAD
            .decode(header_b64)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .ok_or_else(bad)?;
        if header.get("alg").and_then(|v| v.as_str()) != Some("EdDSA")
            || header.get("kid").and_then(|v| v.as_str()) != Some(self.kid.as_str())
        {
            return Err(bad());
        }
        let signature = URL_SAFE_NO_PAD.decode(signature_b64).map_err(|_| bad())?;
        let signing_input = format!("{header_b64}.{claims_b64}");
        UnparsedPublicKey::new(&ED25519, self.key.public_key().as_ref())
            .verify(signing_input.as_bytes(), &signature)
            .map_err(|_| bad())?;
        let claims: serde_json::Value = URL_SAFE_NO_PAD
            .decode(claims_b64)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .ok_or_else(bad)?;
        Ok(claims)
    }
}

impl TokenSigner for Ed25519Signer {
    fn sign(&self, claims: &DelegationClaims) -> Result<String, ProductAccessError> {
        self.sign_json(claims)
    }

    fn sign_privacy(&self, claims: &PrivacyClaims) -> Result<String, ProductAccessError> {
        self.sign_json(claims)
    }

    fn sign_connector(&self, claims: &ConnectorClaims) -> Result<String, ProductAccessError> {
        self.sign_json(claims)
    }

    fn sign_checkout(&self, claims: &CheckoutClaims) -> Result<String, ProductAccessError> {
        self.sign_json(claims)
    }

    fn sign_credential(
        &self,
        claims: &PurchaseCredentialClaims,
    ) -> Result<String, ProductAccessError> {
        self.sign_json(claims)
    }

    fn verify_json(&self, token: &str) -> Result<serde_json::Value, ProductAccessError> {
        self.verify_compact(token)
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

    fn sign_privacy(&self, _: &PrivacyClaims) -> Result<String, ProductAccessError> {
        Err(ProductAccessError::Unavailable(
            "no delegation signing key is configured".to_owned(),
        ))
    }

    fn sign_connector(&self, _: &ConnectorClaims) -> Result<String, ProductAccessError> {
        Err(ProductAccessError::Unavailable(
            "no delegation signing key is configured".to_owned(),
        ))
    }

    fn sign_checkout(&self, _: &CheckoutClaims) -> Result<String, ProductAccessError> {
        Err(ProductAccessError::Unavailable(
            "no delegation signing key is configured".to_owned(),
        ))
    }

    fn sign_credential(&self, _: &PurchaseCredentialClaims) -> Result<String, ProductAccessError> {
        Err(ProductAccessError::Unavailable(
            "no delegation signing key is configured".to_owned(),
        ))
    }

    fn verify_json(&self, _: &str) -> Result<serde_json::Value, ProductAccessError> {
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
            jti: "018f0000000070008000000000000000".into(),
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
    fn connector_token_verifies_with_the_platform_dev_key() {
        let signer = Ed25519Signer::from_seed_hex("dev-ed25519-1", SEED).unwrap();
        let claims = ConnectorClaims {
            tenant_id: 1001,
            user_id: 3001,
            destination_store_id: "42".into(),
            aud: "ecommerce-connectors",
            iat: 1_800_000_000,
            exp: 1_800_000_060,
            jti: "test-jti".into(),
        };
        let token = signer.sign_connector(&claims).unwrap();
        let parts: Vec<_> = token.split('.').collect();
        assert_eq!(parts.len(), 3);

        let header: serde_json::Value =
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[0]).unwrap()).unwrap();
        assert_eq!(header["alg"], "EdDSA");
        assert_eq!(header["kid"], "dev-ed25519-1");

        let payload: serde_json::Value =
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[1]).unwrap()).unwrap();
        assert_eq!(payload["aud"], "ecommerce-connectors");
        assert_eq!(payload["tenant_id"], 1001);
        assert!(payload["tenant_id"].is_number());

        let public =
            hex::decode("ea4a6c63e29c520abef5507b132ec5f9954776aebebe7b92421eea691446d22c")
                .unwrap();
        let signature = URL_SAFE_NO_PAD.decode(parts[2]).unwrap();
        UnparsedPublicKey::new(&ED25519, public)
            .verify(format!("{}.{}", parts[0], parts[1]).as_bytes(), &signature)
            .expect("signature verifies with the platform dev key");
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

    #[test]
    fn checkout_and_credential_tokens_verify_and_tamper_fails() {
        use crate::domain::{
            CHECKOUT_AUDIENCE, CheckoutLine, PurchaseCredentialClaims, QuotedTerms,
        };
        use chrono::DateTime;

        let signer = Ed25519Signer::from_seed_hex("dev-ed25519-1", SEED).unwrap();
        let now = DateTime::from_timestamp(1_800_000_000, 0).unwrap();
        let checkout = CheckoutClaims::new(
            "555".into(),
            sanvi_kernel::TenantId::try_from(1001_i64).unwrap(),
            42,
            "700".into(),
            "800".into(),
            "JPY".into(),
            vec![CheckoutLine {
                variant_id: "900".into(),
                unit_price_minor: 1200,
                quantity: 2,
            }],
            QuotedTerms {
                pricing_fingerprint: "v1:test".into(),
                shipping_method_id: None,
                subtotal_minor: 2400,
                shipping_minor: 0,
                tax_minor: 0,
                total_minor: 2400,
            },
            "018f0000000070008000000000000000".into(),
            DateTime::from_timestamp(1_800_000_900, 0).unwrap(),
            now,
        );
        let token = signer.sign_checkout(&checkout).unwrap();
        let verified = signer.verify_json(&token).unwrap();
        assert_eq!(verified["aud"], CHECKOUT_AUDIENCE);
        assert_eq!(verified["sub"], "555");
        assert_eq!(verified["lines"][0]["unit_price_minor"], 1200);
        assert_eq!(verified["pricing_fingerprint"], "v1:test");

        let credential = PurchaseCredentialClaims::new(
            "42".into(),
            sanvi_kernel::TenantId::try_from(1001_i64).unwrap(),
            42,
            "018f0000000070008000000000000000".into(),
            3600,
            now,
        );
        let credential_token = signer.sign_credential(&credential).unwrap();
        assert_eq!(signer.verify_json(&credential_token).unwrap()["sub"], "42");

        // A flipped payload byte and a foreign token both fail closed.
        let (head, rest) = token.split_once('.').unwrap();
        let (_payload, sig) = rest.rsplit_once('.').unwrap();
        let tampered = format!("{head}.e30.{sig}");
        assert!(signer.verify_json(&tampered).is_err());
        assert!(signer.verify_json("not-a-token").is_err());
        assert!(signer.verify_json("").is_err());
        let other = Ed25519Signer::from_seed_hex(
            "dev-ed25519-1",
            "0808080808080808080808080808080808080808080808080808080808080808",
        )
        .unwrap();
        assert!(other.verify_json(&token).is_err(), "wrong key fails");
        assert!(DisabledSigner.verify_json(&token).is_err());
    }

    #[test]
    fn a_checkout_token_with_changed_terms_fails_verification() {
        use crate::domain::{CheckoutLine, QuotedTerms};
        use chrono::DateTime;

        let signer = Ed25519Signer::from_seed_hex("dev-ed25519-1", SEED).unwrap();
        let claims = CheckoutClaims::new(
            "555".into(),
            sanvi_kernel::TenantId::try_from(1001_i64).unwrap(),
            42,
            "700".into(),
            "800".into(),
            "JPY".into(),
            vec![CheckoutLine {
                variant_id: "900".into(),
                unit_price_minor: 1200,
                quantity: 2,
            }],
            QuotedTerms {
                pricing_fingerprint: "v1:test".into(),
                shipping_method_id: None,
                subtotal_minor: 2400,
                shipping_minor: 0,
                tax_minor: 0,
                total_minor: 2400,
            },
            "018f0000000070008000000000000000".into(),
            DateTime::from_timestamp(1_800_000_900, 0).unwrap(),
            DateTime::from_timestamp(1_800_000_000, 0).unwrap(),
        );
        let token = signer.sign_checkout(&claims).unwrap();
        assert!(signer.verify_json(&token).is_ok());

        let parts: Vec<_> = token.split('.').collect();
        for (field, value) in [
            ("pricing_fingerprint", serde_json::json!("v1:other")),
            ("total_minor", serde_json::json!(1)),
            ("shipping_method_id", serde_json::json!("301")),
        ] {
            let mut payload: serde_json::Value =
                serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[1]).unwrap()).unwrap();
            payload[field] = value;
            let forged = format!(
                "{}.{}.{}",
                parts[0],
                URL_SAFE_NO_PAD.encode(serde_json::to_vec(&payload).unwrap()),
                parts[2]
            );
            assert!(
                signer.verify_json(&forged).is_err(),
                "changing {field} must break the signature"
            );
        }
    }
}
