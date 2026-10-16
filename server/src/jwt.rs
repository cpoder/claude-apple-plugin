//! ES256 JWT minting (custom header fields allowed, e.g. WeatherKit's `id`),
//! raw ECDSA signing for CloudKit, and a small token cache.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use base64::prelude::*;
use p256::ecdsa::signature::Signer;
use p256::ecdsa::{Signature, SigningKey};
use p256::pkcs8::DecodePrivateKey;
use serde_json::Value;

use crate::error::ToolError;

pub fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

/// Parse a PEM private key: PKCS#8 (`BEGIN PRIVATE KEY`, Apple .p8) or SEC1 (`BEGIN EC PRIVATE KEY`).
pub fn signing_key(pem: &str) -> Result<SigningKey, ToolError> {
    let pem = pem.trim();
    if pem.contains("BEGIN EC PRIVATE KEY") {
        let sk = p256::SecretKey::from_sec1_pem(pem).map_err(|e| ToolError::new(format!("invalid SEC1 EC key: {e}")))?;
        Ok(SigningKey::from(sk))
    } else if pem.contains("BEGIN PRIVATE KEY") {
        let sk = p256::SecretKey::from_pkcs8_pem(pem).map_err(|e| ToolError::new(format!("invalid PKCS#8 key (expected a P-256 .p8 key): {e}")))?;
        Ok(SigningKey::from(sk))
    } else {
        Err(ToolError::new("private key must be PEM (BEGIN PRIVATE KEY or BEGIN EC PRIVATE KEY)"))
    }
}

fn b64url(data: &[u8]) -> String {
    BASE64_URL_SAFE_NO_PAD.encode(data)
}

/// Build and sign a JWT with ES256. `header` must contain `kid` if the service needs it; `alg` is forced to ES256.
pub fn es256_jwt(pem: &str, mut header: Value, claims: Value) -> Result<String, ToolError> {
    let key = signing_key(pem)?;
    header["alg"] = Value::String("ES256".into());
    let h = b64url(serde_json::to_string(&header)?.as_bytes());
    let c = b64url(serde_json::to_string(&claims)?.as_bytes());
    let signing_input = format!("{h}.{c}");
    let sig: Signature = key.sign(signing_input.as_bytes());
    Ok(format!("{signing_input}.{}", b64url(&sig.to_bytes())))
}

/// ECDSA P-256 / SHA-256 signature in ASN.1 DER form (CloudKit server-to-server auth).
pub fn ecdsa_sign_der(pem: &str, message: &[u8]) -> Result<Vec<u8>, ToolError> {
    let key = signing_key(pem)?;
    let sig: Signature = key.sign(message);
    Ok(sig.to_der().as_bytes().to_vec())
}

/// Decode a JWT/JWS without verifying the signature: (header, payload).
pub fn decode_unverified(token: &str) -> Result<(Value, Value), ToolError> {
    let parts: Vec<&str> = token.trim().split('.').collect();
    if parts.len() != 3 {
        return Err(ToolError::new("not a compact JWS (expected 3 dot-separated parts)"));
    }
    let dec = |s: &str| -> Result<Value, ToolError> {
        let bytes = BASE64_URL_SAFE_NO_PAD
            .decode(s.trim_end_matches('='))
            .map_err(|e| ToolError::new(format!("base64url decode failed: {e}")))?;
        Ok(serde_json::from_slice(&bytes)?)
    };
    Ok((dec(parts[0])?, dec(parts[1])?))
}

#[derive(Default)]
pub struct TokenCache {
    inner: Mutex<HashMap<String, (String, Instant)>>,
}

impl TokenCache {
    /// Return the cached token for `key` if still valid, else mint one with `make` and cache it for `ttl`.
    pub fn get_or_mint(
        &self,
        key: &str,
        ttl: Duration,
        make: impl FnOnce() -> Result<String, ToolError>,
    ) -> Result<String, ToolError> {
        {
            let guard = self.inner.lock().expect("token cache");
            if let Some((tok, exp)) = guard.get(key)
                && Instant::now() < *exp {
                    return Ok(tok.clone());
                }
        }
        let tok = make()?;
        self.inner
            .lock()
            .expect("token cache")
            .insert(key.to_string(), (tok.clone(), Instant::now() + ttl));
        Ok(tok)
    }

    pub fn clear(&self) {
        self.inner.lock().expect("token cache").clear();
    }

    /// Cached token if still valid (for tokens minted asynchronously, e.g. OAuth exchanges).
    pub fn get(&self, key: &str) -> Option<String> {
        let guard = self.inner.lock().expect("token cache");
        guard.get(key).filter(|(_, exp)| Instant::now() < *exp).map(|(t, _)| t.clone())
    }

    pub fn put(&self, key: &str, token: String, ttl: Duration) {
        self.inner.lock().expect("token cache").insert(key.to_string(), (token, Instant::now() + ttl));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use p256::ecdsa::signature::Verifier;

    fn fixed_secret() -> p256::SecretKey {
        // deterministic non-zero scalar below the curve order
        p256::SecretKey::from_slice(&[0x5a; 32]).unwrap()
    }

    fn test_key() -> (String, p256::ecdsa::VerifyingKey) {
        use p256::pkcs8::EncodePrivateKey;
        let sk = fixed_secret();
        let pem = sk.to_pkcs8_pem(p256::pkcs8::LineEnding::LF).unwrap().to_string();
        let vk = *SigningKey::from(sk).verifying_key();
        (pem, vk)
    }

    #[test]
    fn jwt_roundtrip() {
        let (pem, vk) = test_key();
        let tok = es256_jwt(&pem, serde_json::json!({"kid": "K1", "id": "T.S"}), serde_json::json!({"iss": "T", "iat": 1, "exp": 2})).unwrap();
        let (h, c) = decode_unverified(&tok).unwrap();
        assert_eq!(h["alg"], "ES256");
        assert_eq!(h["kid"], "K1");
        assert_eq!(h["id"], "T.S");
        assert_eq!(c["iss"], "T");
        let parts: Vec<&str> = tok.split('.').collect();
        let sig_bytes = BASE64_URL_SAFE_NO_PAD.decode(parts[2]).unwrap();
        let sig = Signature::from_slice(&sig_bytes).unwrap();
        vk.verify(format!("{}.{}", parts[0], parts[1]).as_bytes(), &sig).unwrap();
    }

    #[test]
    fn der_signature_verifies() {
        let (pem, vk) = test_key();
        let der = ecdsa_sign_der(&pem, b"hello").unwrap();
        let sig = Signature::from_der(&der).unwrap();
        vk.verify(b"hello", &sig).unwrap();
    }

    #[test]
    fn sec1_pem_accepted() {
        let sk = fixed_secret();
        let pem = sk.to_sec1_pem(p256::pkcs8::LineEnding::LF).unwrap().to_string();
        assert!(pem.contains("BEGIN EC PRIVATE KEY"));
        signing_key(&pem).unwrap();
    }
}
