//! Ed25519 counter-signing of memory-evidence payloads.
//!
//! Closes the gap named in Nexus CHANGELOG C2: Nexus previously downgraded
//! self-issued memory evidence to `Advisory` because nothing proved the hit
//! set it received from AEON-IQ had not been altered in transit or at rest —
//! its own HMAC covered data it had already received.  AEON-IQ now signs a
//! canonical payload describing exactly what it served (agent, session
//! filter, hit IDs, hit content digests) so Nexus can verify against
//! AEON-IQ's public key before reporting `Attested*` modes.
//!
//! Design mirrors Nexus `src/proof/signing.rs` (dedicated key,
//! ephemeral-by-default with an env-provided seed for production) and
//! `aeon_nexus_bridge`'s canonicalization (recursive JSON key sort → bytes →
//! SHA-256), so both sides derive identical bytes from identical logical
//! content.

use anyhow::{Context, Result};
use ed25519_dalek::{Signer, SigningKey};
use serde::Serialize;
use sha2::{Digest, Sha256};

/// Version tag bound into every signed payload.  Bump on any change to
/// `EvidencePayload`'s shape — verification is byte-exact.
pub const EVIDENCE_SIG_VERSION: &str = "aeon-evidence-sig-v1";

/// The logical content AEON-IQ attests to when serving a search response.
/// Field order does not matter (canonicalization sorts keys), but names and
/// types are part of the wire contract with the Nexus verifier.
#[derive(Serialize)]
pub struct EvidencePayload {
    pub version: String,
    pub agent_id: String,
    pub session_id: Option<String>,
    /// Memory IDs in response order.
    pub hit_ids: Vec<String>,
    /// Lowercase-hex SHA-256 of each hit's content, in response order.
    pub hit_content_digests: Vec<String>,
}

impl EvidencePayload {
    pub fn new(
        agent_id: &str,
        session_id: Option<&str>,
        hits: impl IntoIterator<Item = (String, String)>,
    ) -> Self {
        let (hit_ids, hit_content_digests): (Vec<_>, Vec<_>) = hits
            .into_iter()
            .map(|(id, content)| (id, sha256_hex(content.as_bytes())))
            .unzip();
        Self {
            version: EVIDENCE_SIG_VERSION.to_string(),
            agent_id: agent_id.to_string(),
            session_id: session_id.map(str::to_string),
            hit_ids,
            hit_content_digests,
        }
    }
}

/// Signature material attached to a search response.
#[derive(Serialize, Clone)]
pub struct EvidenceEnvelope {
    pub version: String,
    /// Lowercase-hex Ed25519 verifying key (32 bytes).
    pub key_id: String,
    /// Lowercase-hex Ed25519 signature (64 bytes) over the canonical payload.
    pub signature: String,
    /// Lowercase-hex SHA-256 of the canonical payload bytes.
    pub payload_digest: String,
}

pub struct EvidenceSigner {
    key: SigningKey,
    /// Cached hex verifying key.
    key_id: String,
    /// True when the key came from the environment (survives restarts).
    pub persistent: bool,
}

impl EvidenceSigner {
    /// Build from `AEON_EVIDENCE_SIGNING_KEY` (hex-encoded 32-byte seed,
    /// e.g. `openssl rand -hex 32`); falls back to a fresh ephemeral key so
    /// signing always works, at the cost of a key_id that changes per
    /// process — Nexus deployments that verify must pin the env key.
    pub fn from_env() -> Result<Self> {
        match std::env::var("AEON_EVIDENCE_SIGNING_KEY") {
            Ok(raw) if !raw.trim().is_empty() => {
                let bytes =
                    hex::decode(raw.trim()).context("AEON_EVIDENCE_SIGNING_KEY must be hex")?;
                let seed: [u8; 32] = bytes.as_slice().try_into().map_err(|_| {
                    anyhow::anyhow!("AEON_EVIDENCE_SIGNING_KEY must decode to exactly 32 bytes")
                })?;
                Ok(Self::new(SigningKey::from_bytes(&seed), true))
            }
            _ => {
                let mut rng = rand::rngs::OsRng;
                Ok(Self::new(SigningKey::generate(&mut rng), false))
            }
        }
    }

    fn new(key: SigningKey, persistent: bool) -> Self {
        let key_id = hex::encode(key.verifying_key().to_bytes());
        Self {
            key,
            key_id,
            persistent,
        }
    }

    pub fn key_id(&self) -> &str {
        &self.key_id
    }

    /// Deterministic construction from a raw seed (tests and tooling).
    #[cfg(test)]
    pub fn from_seed(seed: [u8; 32]) -> Self {
        Self::new(SigningKey::from_bytes(&seed), true)
    }

    /// Sign the canonical bytes of `payload`.
    pub fn sign(&self, payload: &EvidencePayload) -> Result<EvidenceEnvelope> {
        let bytes = canonical_bytes(payload)?;
        let signature = self.key.sign(&bytes);
        Ok(EvidenceEnvelope {
            version: EVIDENCE_SIG_VERSION.to_string(),
            key_id: self.key_id.clone(),
            signature: hex::encode(signature.to_bytes()),
            payload_digest: sha256_hex(&bytes),
        })
    }
}

impl std::fmt::Debug for EvidenceSigner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never expose the signing key, mirroring Nexus's redacted Debug.
        f.debug_struct("EvidenceSigner")
            .field("key_id", &self.key_id)
            .field("persistent", &self.persistent)
            .finish()
    }
}

/// Canonical JSON bytes: serialize, recursively sort every object's keys,
/// re-serialize.  Must stay byte-compatible with
/// `aeon_nexus_bridge::canonical_bytes` — any drift breaks verification
/// silently on the Nexus side.
pub fn canonical_bytes<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    let mut v = serde_json::to_value(value)?;
    sort_json_value(&mut v);
    Ok(serde_json::to_vec(&v)?)
}

fn sort_json_value(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            let mut entries: Vec<(String, serde_json::Value)> =
                std::mem::take(map).into_iter().collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            for (_, v) in entries.iter_mut() {
                sort_json_value(v);
            }
            *map = entries.into_iter().collect();
        }
        serde_json::Value::Array(items) => {
            for item in items {
                sort_json_value(item);
            }
        }
        _ => {}
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signature, Verifier, VerifyingKey};

    fn payload() -> EvidencePayload {
        EvidencePayload::new(
            "agent-1",
            Some("sess-1"),
            vec![
                ("id-a".to_string(), "alpha content".to_string()),
                ("id-b".to_string(), "beta content".to_string()),
            ],
        )
    }

    /// Signatures verify against the advertised key over the canonical bytes,
    /// exactly as the Nexus side will check them.
    #[test]
    fn sign_verify_round_trip() {
        let signer = EvidenceSigner::from_seed([7u8; 32]);
        let p = payload();
        let envelope = signer.sign(&p).unwrap();

        let key_bytes: [u8; 32] = hex::decode(&envelope.key_id).unwrap().try_into().unwrap();
        let vk = VerifyingKey::from_bytes(&key_bytes).unwrap();
        let sig_bytes: [u8; 64] = hex::decode(&envelope.signature)
            .unwrap()
            .try_into()
            .unwrap();
        let sig = Signature::from_bytes(&sig_bytes);

        let bytes = canonical_bytes(&p).unwrap();
        assert_eq!(sha256_hex(&bytes), envelope.payload_digest);
        vk.verify(&bytes, &sig).expect("signature must verify");
    }

    /// Any change to logical content changes the canonical bytes, so a
    /// signature over the original cannot be replayed for altered hits.
    #[test]
    fn tampered_content_fails_verification() {
        let signer = EvidenceSigner::from_seed([7u8; 32]);
        let envelope = signer.sign(&payload()).unwrap();

        let tampered = EvidencePayload::new(
            "agent-1",
            Some("sess-1"),
            vec![
                ("id-a".to_string(), "alpha content".to_string()),
                ("id-b".to_string(), "TAMPERED".to_string()),
            ],
        );

        let key_bytes: [u8; 32] = hex::decode(&envelope.key_id).unwrap().try_into().unwrap();
        let vk = VerifyingKey::from_bytes(&key_bytes).unwrap();
        let sig_bytes: [u8; 64] = hex::decode(&envelope.signature)
            .unwrap()
            .try_into()
            .unwrap();
        let sig = Signature::from_bytes(&sig_bytes);

        let bytes = canonical_bytes(&tampered).unwrap();
        assert!(
            vk.verify(&bytes, &sig).is_err(),
            "tampered payload must fail"
        );
    }

    /// Canonicalization is order-insensitive for object keys.
    #[test]
    fn canonical_bytes_sort_keys() {
        let a = serde_json::json!({"b": 1, "a": {"d": 2, "c": 3}});
        let mut v = a.clone();
        sort_json_value(&mut v);
        assert_eq!(
            serde_json::to_string(&v).unwrap(),
            r#"{"a":{"c":3,"d":2},"b":1}"#
        );
    }

    /// Seeded signers are deterministic across constructions (persistent
    /// key_id), which is what lets operators pin the verifying key in Nexus.
    /// (Constructed via from_seed rather than env mutation so parallel tests
    /// calling from_env are not raced.)
    #[test]
    fn seeded_key_is_deterministic() {
        let s1 = EvidenceSigner::from_seed([0x11u8; 32]);
        let s2 = EvidenceSigner::from_seed([0x11u8; 32]);
        assert!(s1.persistent);
        assert_eq!(s1.key_id(), s2.key_id());
    }
}
