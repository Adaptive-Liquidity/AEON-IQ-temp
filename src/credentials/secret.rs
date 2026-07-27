//! Credential format, pepper, and MAC verification (plan §2, decision A-1).
//!
//! # Format
//!
//! ```text
//! <credential_id>.<secret>
//! 0f8fad5b-d9cb-469f-a165-70867728950e.3b1f…(64 lowercase hex chars)
//! ```
//!
//! The id is the hyphenated UUID that keys the `credentials` table — public,
//! and knowing it proves nothing. The secret is 32 bytes of CSPRNG output, hex
//! encoded. Hex rather than base64 because `hex` is already a direct dependency
//! and a fixed-width alphabet makes "wrong length" a cheap, unambiguous
//! rejection; the extra 21 characters buy that at no security cost.
//!
//! Parsing is strict — hyphenated UUID only, lowercase hex only, exactly one
//! `.` — so one credential has exactly one spelling.
//!
//! # Verification
//!
//! `secret_mac = HMAC-SHA-256(pepper, secret)`, compared in constant time.
//!
//! **Not a password hash, deliberately.** The secret is 32 bytes of CSPRNG
//! output, so there is no low-entropy input for Argon2id's cost to protect.
//! Applying a slow KDF here would add tens of milliseconds and tens of megabytes
//! *per authenticated request* and hand an attacker a cheap amplification vector
//! — unauthenticated requests forcing expensive verifies. A keyed MAC is the
//! right primitive for a high-entropy machine credential.
//!
//! **The pepper lives outside PostgreSQL.** A database-only compromise
//! therefore yields no verifiable MACs and no way to mint one: the attacker
//! learns which credential ids exist and nothing else.
//!
//! ## Why the MAC is not bound to the credential id
//!
//! `HMAC(pepper, credential_id || secret)` would stop a `secret_mac` being
//! copied from one row to another. It is not used because the only actor who
//! could perform that copy already has write access to `credentials`, and can
//! reach the same outcome more directly by editing the target row's `tenant_id`
//! or inserting a row whose MAC they generated themselves. Binding would add a
//! step to an attack that is already over, so the plan's frozen
//! `HMAC-SHA-256(pepper, secret)` is used as written.

use std::fmt;

use hmac::{Hmac, Mac};
use rand::RngCore;
use sha2::Sha256;
use subtle::ConstantTimeEq;
use uuid::Uuid;

type HmacSha256 = Hmac<Sha256>;

/// CSPRNG bytes in a credential secret.
pub const SECRET_BYTES: usize = 32;
/// Hex characters in the encoded secret.
pub const SECRET_HEX_LEN: usize = SECRET_BYTES * 2;
/// HMAC-SHA-256 output width, mirrored by `credentials_secret_mac_len_ck`.
pub const MAC_BYTES: usize = 32;
/// Characters in a hyphenated UUID.
const UUID_HYPHENATED_LEN: usize = 36;

// ── Pepper ───────────────────────────────────────────────────────────────────

/// The server-side HMAC key, loaded from outside the database.
///
/// `Debug` is implemented by hand to redact the bytes: this type ends up inside
/// settings structs that are otherwise reasonable to log.
#[derive(Clone)]
pub struct Pepper(Vec<u8>);

impl Pepper {
    /// Shorter than the MAC's own output would weaken the construction for no
    /// operational benefit, so it is a floor rather than a warning.
    pub const MIN_BYTES: usize = 32;

    /// Parse a hex-encoded pepper.
    ///
    /// Trailing whitespace is trimmed because the common delivery mechanism is
    /// a file, and a file almost always ends in a newline.
    pub fn from_hex(raw: &str) -> Result<Self, PepperError> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(PepperError::Empty);
        }
        let bytes = hex::decode(trimmed).map_err(|_| PepperError::NotHex)?;
        if bytes.len() < Self::MIN_BYTES {
            return Err(PepperError::TooShort {
                got: bytes.len(),
                min: Self::MIN_BYTES,
            });
        }
        Ok(Self(bytes))
    }

    fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

impl fmt::Debug for Pepper {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Pepper(<redacted, {} bytes>)", self.0.len())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PepperError {
    #[error("credential pepper is empty; it is never defaulted or generated")]
    Empty,
    #[error("credential pepper is not valid hex")]
    NotHex,
    #[error("credential pepper is {got} bytes; at least {min} are required")]
    TooShort { got: usize, min: usize },
}

// ── Presented credential ─────────────────────────────────────────────────────

/// A syntactically valid credential string, split into its two halves.
///
/// Holding one means the *shape* was right. It says nothing about whether the
/// credential exists or the secret is correct.
#[derive(Clone)]
pub struct PresentedCredential {
    credential_id: Uuid,
    secret: [u8; SECRET_BYTES],
}

impl PresentedCredential {
    pub fn credential_id(&self) -> Uuid {
        self.credential_id
    }

    fn secret(&self) -> &[u8] {
        &self.secret
    }
}

impl fmt::Debug for PresentedCredential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The id is safe to show and is what an operator needs in an audit
        // trail; the secret never appears, including in a panic message.
        f.debug_struct("PresentedCredential")
            .field("credential_id", &self.credential_id)
            .field("secret", &"<redacted>")
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FormatError {
    #[error("credential must be exactly `<credential_id>.<secret>`")]
    Shape,
    #[error("credential id is not a hyphenated UUID")]
    CredentialId,
    #[error("secret is not {SECRET_HEX_LEN} lowercase hex characters")]
    Secret,
}

/// Parse `<credential_id>.<secret>`.
///
/// These error variants are for logs and tests only. Everything a client sees
/// is the single uniform rejection in [`super::AuthError`], so distinguishing
/// "unknown id" from "wrong secret" here creates no oracle.
pub fn parse_presented(raw: &str) -> Result<PresentedCredential, FormatError> {
    // Exactly one separator. `split_once` alone would silently accept
    // "a.b.c" by treating "b.c" as the secret.
    if raw.matches('.').count() != 1 {
        return Err(FormatError::Shape);
    }
    let (id_part, secret_part) = raw.split_once('.').ok_or(FormatError::Shape)?;

    // `Uuid::parse_str` also accepts simple, braced and URN forms. Requiring
    // the hyphenated one keeps a credential's spelling unique.
    if id_part.len() != UUID_HYPHENATED_LEN {
        return Err(FormatError::CredentialId);
    }
    let credential_id = Uuid::parse_str(id_part).map_err(|_| FormatError::CredentialId)?;

    if secret_part.len() != SECRET_HEX_LEN {
        return Err(FormatError::Secret);
    }
    // `hex::decode` accepts uppercase; the canonical encoding is lowercase.
    if !secret_part
        .bytes()
        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(FormatError::Secret);
    }
    let decoded = hex::decode(secret_part).map_err(|_| FormatError::Secret)?;
    let secret: [u8; SECRET_BYTES] = decoded.try_into().map_err(|_| FormatError::Secret)?;

    Ok(PresentedCredential {
        credential_id,
        secret,
    })
}

// ── Issuance ─────────────────────────────────────────────────────────────────

/// A newly minted credential.
///
/// [`IssuedCredential::presented`] is the only time the secret is available.
/// It is shown once and is not recoverable: only its MAC is persisted.
pub struct IssuedCredential {
    pub credential_id: Uuid,
    pub secret_mac: [u8; MAC_BYTES],
    presented: String,
}

impl IssuedCredential {
    /// The full `<credential_id>.<secret>` string to hand to the operator.
    ///
    /// Treat the return value as secret material: do not log it, and do not
    /// write it anywhere durable.
    pub fn presented(&self) -> &str {
        &self.presented
    }
}

impl fmt::Debug for IssuedCredential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IssuedCredential")
            .field("credential_id", &self.credential_id)
            .field("secret_mac", &"<redacted>")
            .field("presented", &"<redacted>")
            .finish()
    }
}

/// Mint a credential: fresh UUID, 32 CSPRNG bytes, and the matching MAC.
///
/// `OsRng` rather than `thread_rng` so the source is unambiguously the
/// operating system's CSPRNG at the point of use.
pub fn issue(pepper: &Pepper) -> IssuedCredential {
    let credential_id = Uuid::new_v4();
    let mut secret = [0u8; SECRET_BYTES];
    rand::rngs::OsRng.fill_bytes(&mut secret);

    let secret_mac = compute_mac(pepper, &secret);
    let presented = format!("{}.{}", credential_id, hex::encode(secret));

    IssuedCredential {
        credential_id,
        secret_mac,
        presented,
    }
}

// ── Verification ─────────────────────────────────────────────────────────────

/// `HMAC-SHA-256(pepper, secret)`.
pub fn compute_mac(pepper: &Pepper, secret: &[u8]) -> [u8; MAC_BYTES] {
    let mut mac = HmacSha256::new_from_slice(pepper.as_bytes())
        .expect("HMAC-SHA-256 accepts a key of any length");
    mac.update(secret);
    mac.finalize().into_bytes().into()
}

/// Constant-time verification of a presented secret against a stored MAC.
///
/// The comparison never short-circuits on the first differing byte, so response
/// time carries no information about how many leading bytes matched — which is
/// what would otherwise let an attacker construct a valid MAC one byte at a
/// time.
pub fn verify(pepper: &Pepper, presented: &PresentedCredential, stored_mac: &[u8]) -> bool {
    let computed = compute_mac(pepper, presented.secret());
    // `subtle`'s slice impl returns 0 for a length mismatch without leaking
    // where it diverged; `credentials_secret_mac_len_ck` makes that case a
    // corrupted row rather than an attacker-reachable path.
    bool::from(computed[..].ct_eq(stored_mac))
}

/// A fixed decoy, used only to equalise the cost of a lookup miss.
const DECOY_SECRET: [u8; SECRET_BYTES] = [0x5a; SECRET_BYTES];

/// Perform the same HMAC work as a real verification, and discard it.
///
/// An indexed miss is otherwise structurally cheaper than hit + HMAC +
/// compare, which leaves a measurable oracle for *which credential ids exist*
/// even though both paths return the same rejection. Doing the work anyway
/// removes that difference.
///
/// **What this does not cover, stated rather than implied:** a *malformed*
/// credential is rejected before any database round-trip, and that round-trip
/// dominates the timing. So malformed input remains distinguishable from
/// well-formed input by timing. That is not an oracle — the attacker already
/// knows whether the string they sent was well-formed, and it reveals nothing
/// about which credentials exist. Equalising it would mean issuing a decoy
/// query for every malformed byte-string, which is a denial-of-service
/// amplifier bought for no information-theoretic gain.
pub fn dummy_verify(pepper: &Pepper) {
    let mac = compute_mac(pepper, &DECOY_SECRET);
    // Keep the result observable so the optimiser cannot delete the call: the
    // timing cost *is* the purpose, which makes it dead code by every measure
    // except the one that matters.
    std::hint::black_box(mac);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pepper() -> Pepper {
        Pepper::from_hex(&"ab".repeat(32)).expect("64 bytes of hex is a valid pepper")
    }

    // ── Pepper ───────────────────────────────────────────────────────────────

    #[test]
    fn a_valid_hex_pepper_parses() {
        assert!(Pepper::from_hex(&"00".repeat(32)).is_ok());
        // Trailing newline, as delivered by a file.
        assert!(Pepper::from_hex(&format!("{}\n", "ab".repeat(32))).is_ok());
    }

    #[test]
    fn a_malformed_pepper_is_rejected() {
        // `unwrap_err` rather than comparing the whole `Result`: `Pepper` has no
        // `PartialEq` on purpose, so that a stray `==` cannot become a
        // non-constant-time comparison of key material.
        assert_eq!(Pepper::from_hex("").unwrap_err(), PepperError::Empty);
        assert_eq!(Pepper::from_hex("   ").unwrap_err(), PepperError::Empty);
        assert_eq!(
            Pepper::from_hex(&"zz".repeat(32)).unwrap_err(),
            PepperError::NotHex
        );
        assert_eq!(
            Pepper::from_hex("abc").unwrap_err(),
            PepperError::NotHex,
            "odd-length hex is not decodable"
        );
        assert_eq!(
            Pepper::from_hex(&"ab".repeat(16)).unwrap_err(),
            PepperError::TooShort { got: 16, min: 32 }
        );
    }

    #[test]
    fn pepper_debug_redacts_the_key() {
        let rendered = format!("{:?}", pepper());
        assert_eq!(rendered, "Pepper(<redacted, 32 bytes>)");
        assert!(!rendered.contains("abab"));
    }

    // ── Format ───────────────────────────────────────────────────────────────

    #[test]
    fn an_issued_credential_parses_back() {
        let issued = issue(&pepper());
        let parsed = parse_presented(issued.presented()).expect("issued credential must parse");
        assert_eq!(parsed.credential_id(), issued.credential_id);
        assert!(verify(&pepper(), &parsed, &issued.secret_mac));
    }

    #[test]
    fn issued_credentials_have_the_frozen_shape() {
        let issued = issue(&pepper());
        let presented = issued.presented();
        let (id, secret) = presented.split_once('.').expect("one separator");
        assert_eq!(id.len(), UUID_HYPHENATED_LEN);
        assert_eq!(secret.len(), SECRET_HEX_LEN, "32 bytes, hex encoded");
        assert_eq!(issued.secret_mac.len(), MAC_BYTES);
    }

    #[test]
    fn two_issued_credentials_never_collide() {
        // A weak or fixed RNG would show up here first.
        let a = issue(&pepper());
        let b = issue(&pepper());
        assert_ne!(a.credential_id, b.credential_id);
        assert_ne!(a.presented(), b.presented());
        assert_ne!(a.secret_mac, b.secret_mac);
    }

    #[test]
    fn malformed_credentials_are_rejected() {
        let id = Uuid::from_u128(1).to_string();
        let secret = "ab".repeat(32);

        let cases: Vec<(String, FormatError)> = vec![
            (String::new(), FormatError::Shape),
            ("no-separator".to_string(), FormatError::Shape),
            (format!("{id}.{secret}.{secret}"), FormatError::Shape),
            (format!(".{secret}"), FormatError::CredentialId),
            (format!("not-a-uuid.{secret}"), FormatError::CredentialId),
            // Simple (unhyphenated) UUID form is deliberately not accepted.
            (
                format!("{}.{secret}", Uuid::from_u128(1).simple()),
                FormatError::CredentialId,
            ),
            (format!("{id}."), FormatError::Secret),
            (format!("{id}.{}", "ab".repeat(31)), FormatError::Secret),
            (format!("{id}.{}", "ab".repeat(33)), FormatError::Secret),
            (format!("{id}.{}", "zz".repeat(32)), FormatError::Secret),
            // Uppercase hex decodes, but is not the canonical spelling.
            (format!("{id}.{}", "AB".repeat(32)), FormatError::Secret),
        ];

        for (raw, expected) in cases {
            // Same reason as the pepper above: `PresentedCredential` holds a
            // secret and is deliberately not `PartialEq`.
            assert_eq!(
                parse_presented(&raw).unwrap_err(),
                expected,
                "for input {raw:?}"
            );
        }
    }

    #[test]
    fn presented_debug_redacts_the_secret() {
        let issued = issue(&pepper());
        let parsed = parse_presented(issued.presented()).unwrap();
        let rendered = format!("{parsed:?}");
        assert!(rendered.contains(&parsed.credential_id().to_string()));
        assert!(rendered.contains("<redacted>"));
        let secret_hex = issued.presented().split_once('.').unwrap().1;
        assert!(!rendered.contains(secret_hex));
    }

    #[test]
    fn issued_debug_redacts_the_secret_and_the_mac() {
        let issued = issue(&pepper());
        let rendered = format!("{issued:?}");
        assert!(!rendered.contains(issued.presented()));
        assert!(!rendered.contains(&hex::encode(issued.secret_mac)));
        assert!(rendered.contains(&issued.credential_id.to_string()));
    }

    // ── Verification ─────────────────────────────────────────────────────────

    #[test]
    fn an_altered_secret_fails() {
        let issued = issue(&pepper());
        let (id, secret) = issued.presented().split_once('.').unwrap();

        // Flip one hex character — the smallest possible change.
        let mut altered: Vec<char> = secret.chars().collect();
        altered[0] = if altered[0] == 'a' { 'b' } else { 'a' };
        let altered: String = altered.into_iter().collect();
        assert_ne!(altered, secret);

        let parsed = parse_presented(&format!("{id}.{altered}")).unwrap();
        assert!(!verify(&pepper(), &parsed, &issued.secret_mac));
    }

    #[test]
    fn a_different_pepper_fails() {
        // The property that makes the pepper worth keeping out of the database:
        // the stored MAC is useless without it.
        let issued = issue(&pepper());
        let parsed = parse_presented(issued.presented()).unwrap();
        let other = Pepper::from_hex(&"cd".repeat(32)).unwrap();
        assert!(!verify(&other, &parsed, &issued.secret_mac));
    }

    #[test]
    fn a_truncated_or_padded_stored_mac_fails() {
        let issued = issue(&pepper());
        let parsed = parse_presented(issued.presented()).unwrap();
        assert!(!verify(&pepper(), &parsed, &issued.secret_mac[..31]));
        let mut padded = issued.secret_mac.to_vec();
        padded.push(0);
        assert!(!verify(&pepper(), &parsed, &padded));
    }

    #[test]
    fn the_mac_is_deterministic_for_one_pepper() {
        let p = pepper();
        assert_eq!(
            compute_mac(&p, b"same input"),
            compute_mac(&p, b"same input")
        );
        assert_ne!(compute_mac(&p, b"a"), compute_mac(&p, b"b"));
    }

    #[test]
    fn the_decoy_verification_runs_without_a_credential() {
        // Cheap smoke test; the observable proof that it runs on a real lookup
        // miss is the counter assertion in `db_tests`.
        dummy_verify(&pepper());
    }
}
