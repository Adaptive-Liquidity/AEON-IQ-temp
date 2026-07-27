//! The authenticated principal (plan §1).
//!
//! [`AeonAuthContext`] answers exactly one question: *which credential
//! authenticated, and what does it represent?* It answers **no** question about
//! which agent the caller may act for.
//!
//! That separation is deliberate and is why there is no `agent_id` or
//! `agent_uuid` field here. Authentication establishes the credential's tenant,
//! principal and capabilities; agent authority is a distinct
//! `credential_agent_grants` decision (plan §2.2, step 3) applied at route
//! enforcement (step 5). Carrying a requested agent identifier on the
//! authenticated context would blur the two, and the blur is precisely the hole
//! test #5 exists to catch: a valid credential naming *any* agent in its own
//! tenant.
//!
//! Four properties are enforced rather than documented:
//!
//! * **Constructible only inside this module tree.** All fields are private and
//!   the only constructor is `pub(super)`, so nothing outside `credentials` can
//!   build one — a handler cannot fabricate an identity, only receive one.
//! * **No `Default`.** There is no "unknown tenant" or "unset principal", so
//!   "forgot to authenticate" cannot compile into a permissive value.
//! * **Not deserializable.** No `Deserialize` impl exists, so a request body can
//!   never become an identity. Both this and the previous property are asserted
//!   at compile time by the probes in this file's tests.
//! * **No plaintext credential material.** `credential_id` is the public half
//!   of `<id>.<secret>`; the secret never enters this type.

use uuid::Uuid;

use super::scope::{Scope, ScopeSet};

/// Which recall endpoint a credential may use.
///
/// Stored in `credentials.mode`. Cross-endpoint enforcement (a `V2_REQUIRED`
/// credential rejected on `/api/v1/...`, and the converse) is plan step 5 —
/// this step only carries the value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialMode {
    /// May use the V1 recall surface only.
    LegacyOnly,
    /// Must use RecallEnvelopeV2.
    V2Required,
}

impl CredentialMode {
    /// Wire form, shared with the `credentials_mode_ck` CHECK in migration 0029.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::LegacyOnly => "legacy_only",
            Self::V2Required => "v2_required",
        }
    }

    pub fn parse(raw: &str) -> Result<Self, ModeError> {
        match raw {
            "legacy_only" => Ok(Self::LegacyOnly),
            "v2_required" => Ok(Self::V2Required),
            other => Err(ModeError(other.to_string())),
        }
    }
}

impl std::fmt::Display for CredentialMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown credential mode {0:?}; expected legacy_only or v2_required")]
pub struct ModeError(pub String);

/// A successfully authenticated caller.
///
/// Deliberately **not** `Default` and **not** `Deserialize` — see the module
/// docs. `Clone` is fine: cloning requires already holding one.
#[derive(Debug, Clone)]
pub struct AeonAuthContext {
    credential_id: Uuid,
    tenant_id: Uuid,
    principal_id: String,
    scopes: ScopeSet,
    mode: CredentialMode,
}

impl AeonAuthContext {
    /// The only constructor.
    ///
    /// `pub(super)` restricts it to the `credentials` module tree — the
    /// authentication code itself. Every argument comes from a `credentials`
    /// row that a verified MAC has already been matched against; none of them
    /// can originate in a request.
    pub(super) fn new(
        credential_id: Uuid,
        tenant_id: Uuid,
        principal_id: String,
        scopes: ScopeSet,
        mode: CredentialMode,
    ) -> Self {
        Self {
            credential_id,
            tenant_id,
            principal_id,
            scopes,
            mode,
        }
    }

    pub fn credential_id(&self) -> Uuid {
        self.credential_id
    }

    /// The authoritative tenant, read from `credentials.tenant_id`.
    ///
    /// Where a request body also carries a tenant it exists **solely to be
    /// compared**; a mismatch is a hard rejection and never a source of truth
    /// (plan §1).
    pub fn tenant_id(&self) -> Uuid {
        self.tenant_id
    }

    pub fn principal_id(&self) -> &str {
        &self.principal_id
    }

    pub fn scopes(&self) -> &ScopeSet {
        &self.scopes
    }

    pub fn mode(&self) -> CredentialMode {
        self.mode
    }

    /// Convenience for `self.scopes().allows(scope)`.
    ///
    /// Returns `false` for reserved scopes; see [`ScopeSet::allows`].
    pub fn allows(&self, scope: Scope) -> bool {
        self.scopes.allows(scope)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Compile-time negative-impl probes ────────────────────────────────────
    //
    // `Probe::<T>::VALUE` resolves to the *inherent* const when the impl's bound
    // holds, and otherwise falls back to the blanket trait const `false`. So a
    // `false` below is rustc reporting that the bound does not hold — not an
    // assertion hand-written to agree with itself. Each probe carries a control
    // case with a type that *does* implement the trait, which fails loudly if
    // the fallback mechanism ever stops working and makes everything `false`.

    trait NotImplemented {
        const VALUE: bool = false;
    }

    struct DeProbe<T>(std::marker::PhantomData<T>);
    impl<T> NotImplemented for DeProbe<T> {}
    impl<T: serde::de::DeserializeOwned> DeProbe<T> {
        const VALUE: bool = true;
    }

    struct DefaultProbe<T>(std::marker::PhantomData<T>);
    impl<T> NotImplemented for DefaultProbe<T> {}
    impl<T: Default> DefaultProbe<T> {
        const VALUE: bool = true;
    }

    // A constant value is the whole point: rustc resolving these at compile
    // time is what makes them a proof about trait impls rather than a runtime
    // check. Clippy's `assertions_on_constants` is right in general and wrong
    // here.
    #[test]
    #[allow(clippy::assertions_on_constants)]
    fn context_is_not_deserializable_from_request_json() {
        assert!(
            DeProbe::<String>::VALUE,
            "control: String does implement DeserializeOwned — if this fails the \
             probe is broken and the assertion below proves nothing"
        );
        assert!(
            !DeProbe::<AeonAuthContext>::VALUE,
            "AeonAuthContext must not implement Deserialize: a request body could \
             then supply its own tenant_id and scopes"
        );
    }

    #[test]
    #[allow(clippy::assertions_on_constants)]
    fn context_has_no_default() {
        assert!(
            DefaultProbe::<String>::VALUE,
            "control: String does implement Default"
        );
        assert!(
            !DefaultProbe::<AeonAuthContext>::VALUE,
            "AeonAuthContext must not implement Default: there is no safe \
             'unset tenant' value, so failing to authenticate must not be \
             expressible as a value at all"
        );
    }

    // ── Shape ────────────────────────────────────────────────────────────────

    fn sample() -> AeonAuthContext {
        AeonAuthContext::new(
            Uuid::from_u128(1),
            Uuid::from_u128(2),
            "svc-ingest".to_string(),
            ScopeSet::parse_list(&["memory:read", "admin"]).unwrap(),
            CredentialMode::V2Required,
        )
    }

    #[test]
    fn accessors_return_what_was_constructed() {
        let ctx = sample();
        assert_eq!(ctx.credential_id(), Uuid::from_u128(1));
        assert_eq!(ctx.tenant_id(), Uuid::from_u128(2));
        assert_eq!(ctx.principal_id(), "svc-ingest");
        assert_eq!(ctx.mode(), CredentialMode::V2Required);
        assert!(ctx.allows(Scope::MemoryRead));
        assert!(!ctx.allows(Scope::MemoryExport));
    }

    #[test]
    fn debug_output_carries_no_credential_material() {
        // The context holds only the public half of "<id>.<secret>". This
        // catches a future field addition that changes that.
        let rendered = format!("{:?}", sample());
        assert!(rendered.contains("credential_id"));
        assert!(!rendered.to_lowercase().contains("secret"));
        assert!(!rendered.to_lowercase().contains("mac"));
        assert!(!rendered.to_lowercase().contains("pepper"));
    }

    #[test]
    fn context_carries_no_agent_identity() {
        // Authentication does not establish agent authority (plan §2.2). If an
        // agent field is ever added here, this fails and forces the author to
        // revisit step 3 rather than quietly widening the context.
        let rendered = format!("{:?}", sample());
        assert!(
            !rendered.contains("agent"),
            "AeonAuthContext must not carry an agent identifier: agent \
             authorization is a separate credential_agent_grants decision. Got: \
             {rendered}"
        );
    }

    // ── Mode ─────────────────────────────────────────────────────────────────

    #[test]
    fn mode_round_trips_and_rejects_anything_else() {
        for mode in [CredentialMode::LegacyOnly, CredentialMode::V2Required] {
            assert_eq!(CredentialMode::parse(mode.as_str()), Ok(mode));
        }
        assert!(CredentialMode::parse("admin").is_err());
        assert!(CredentialMode::parse("LEGACY_ONLY").is_err());
        assert!(CredentialMode::parse("").is_err());
    }

    #[test]
    fn mode_wire_values_match_the_migration_check_constraint() {
        let migration = include_str!("../../migrations/0029_credentials.sql");
        for mode in [CredentialMode::LegacyOnly, CredentialMode::V2Required] {
            assert!(
                migration.contains(&format!("'{}'", mode.as_str())),
                "migration 0029 does not list {mode}"
            );
        }
    }
}
