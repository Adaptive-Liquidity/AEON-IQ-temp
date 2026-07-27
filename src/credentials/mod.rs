//! Credential authentication (plan §2, decision A-1) — step 2 of the AEON
//! authentication & tenancy plan accepted at
//! `Adaptive-Liquidity/nexus-planning @ b1fe06505d400e435c3ef8d10dc197f15641bebd`.
//!
//! # What this step does
//!
//! Establishes *who is calling*: a credential registry, the
//! `<credential_id>.<secret>` format, HMAC verification against a pepper held
//! outside the database, scopes, revocation, a bounded cache, and an audit
//! record per attempt.
//!
//! # What it deliberately does not do
//!
//! Nothing here decides *what the caller may reach*. There is no route
//! enforcement, no tenant predicate on any query, and no agent authorization —
//! those are plan steps 3 and 5. In particular [`AeonAuthContext`] carries no
//! agent identifier; see `context.rs` for why.
//!
//! # Inert by default
//!
//! With `AEON_CREDENTIAL_AUTH_ENABLED` unset, [`CredentialAuthSettings::build`]
//! returns `None`, no pepper is required, no authenticator exists and no
//! request path changes. `MANAGEMENT_API_KEY` continues to be the only
//! authentication in effect. An existing V1 deployment upgraded to this build
//! starts and behaves exactly as before.

// Most of this surface is called by plan step 5 (per-route scope and grant
// enforcement), which has not landed. Until it does, the authenticator is
// constructed at startup but nothing routes through it, so `dead_code` would
// fire on nearly every item here and on all five submodules.
//
// Allowed once at module scope, with that expiry stated, rather than scattered
// per item where the reason would be lost. The cost is real and worth naming:
// genuinely unused code inside this module will not be flagged until step 5
// removes this attribute.
#![allow(dead_code)]

mod cache;
mod context;
mod scope;
mod secret;
mod store;

#[cfg(test)]
mod db_tests;

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context as _, Result};
use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

pub use cache::{Clock, SystemClock, DEFAULT_CAPACITY, MAX_TTL};
pub use context::{AeonAuthContext, CredentialMode};
// `Scope` is re-exported for the step-5 route checks and for the tests; nothing
// in the binary names it yet, and `#![allow(dead_code)]` above does not cover
// unused imports.
#[allow(unused_imports)]
pub use scope::{Scope, ScopeSet};
pub use secret::{IssuedCredential, Pepper};
pub use store::{CredentialRecord, CredentialStatus};

/// The single message any rejected caller sees.
///
/// Every rejection — malformed string, unknown id, wrong secret, revoked,
/// expired, disabled — renders identically, so the response body carries no
/// information about which credentials exist or what state they are in.
const UNIFORM_REJECTION: &str = "invalid credential";

// ── Configuration ────────────────────────────────────────────────────────────

/// Raw environment values.
///
/// Split out from [`CredentialAuthSettings::from_env`] so every parsing rule is
/// testable without mutating the process environment — the same shape as
/// `tenancy::TenancyEnv`, and necessary for the same reason: Rust runs tests in
/// threads that share one environment.
#[derive(Debug, Clone, Default)]
pub struct CredentialAuthEnv {
    pub enabled: Option<String>,
    /// Contents (not path) of the pepper.
    pub pepper: Option<String>,
    pub cache_capacity: Option<String>,
}

#[derive(Debug, Clone)]
pub struct CredentialAuthSettings {
    pub enabled: bool,
    pub pepper: Option<Pepper>,
    pub cache_capacity: usize,
}

impl CredentialAuthSettings {
    pub fn from_env() -> Result<Self> {
        let inline = non_empty(std::env::var("AEON_CREDENTIAL_PEPPER").ok());
        let path = non_empty(std::env::var("AEON_CREDENTIAL_PEPPER_FILE").ok());

        let pepper = match (inline, path) {
            (Some(_), Some(_)) => bail!(
                "AEON_CREDENTIAL_PEPPER and AEON_CREDENTIAL_PEPPER_FILE are both set; \
                 supply exactly one so the effective pepper is unambiguous"
            ),
            (Some(value), None) => Some(value),
            (None, Some(path)) => Some(std::fs::read_to_string(&path).with_context(|| {
                format!("AEON_CREDENTIAL_PEPPER_FILE could not be read: {path}")
            })?),
            (None, None) => None,
        };

        Self::from_values(CredentialAuthEnv {
            enabled: non_empty(std::env::var("AEON_CREDENTIAL_AUTH_ENABLED").ok()),
            pepper,
            cache_capacity: non_empty(std::env::var("AEON_CREDENTIAL_CACHE_CAPACITY").ok()),
        })
    }

    pub fn from_values(env: CredentialAuthEnv) -> Result<Self> {
        let enabled = env
            .enabled
            .as_deref()
            .is_some_and(|v| v.eq_ignore_ascii_case("true"));

        // A malformed pepper is an error whether or not the subsystem is
        // enabled. Someone who set the variable meant to set it, and silently
        // treating a typo as "no pepper configured" would let a deployment that
        // believes it has credential authentication come up without it.
        let pepper = match env.pepper.as_deref() {
            Some(raw) => Some(
                Pepper::from_hex(raw)
                    .context("the credential pepper is malformed; it is never defaulted")?,
            ),
            None => None,
        };

        // …but a *missing* pepper is an error only when the subsystem is on.
        // This is what keeps the pepper from becoming an unconditional startup
        // requirement for deployments that have not adopted credentials.
        if enabled && pepper.is_none() {
            bail!(
                "AEON_CREDENTIAL_AUTH_ENABLED=true but no pepper is configured. \
                 Set AEON_CREDENTIAL_PEPPER or AEON_CREDENTIAL_PEPPER_FILE to at \
                 least {} bytes of hex. The pepper is never generated and never \
                 defaulted: without it, stored MACs cannot be verified and every \
                 credential would fail.",
                Pepper::MIN_BYTES
            );
        }

        let cache_capacity = match env.cache_capacity.as_deref() {
            Some(raw) => {
                let parsed: usize = raw.parse().with_context(|| {
                    format!("AEON_CREDENTIAL_CACHE_CAPACITY is not a whole number: {raw:?}")
                })?;
                if parsed == 0 {
                    bail!("AEON_CREDENTIAL_CACHE_CAPACITY must be at least 1");
                }
                parsed
            }
            None => DEFAULT_CAPACITY,
        };

        Ok(Self {
            enabled,
            pepper,
            cache_capacity,
        })
    }

    /// Build the authenticator, or `None` when the subsystem is inactive.
    ///
    /// `None` is the V1 path: no credential authentication, no behaviour change.
    pub fn build(&self, pool: PgPool) -> Result<Option<Authenticator>> {
        if !self.enabled {
            return Ok(None);
        }
        let Some(pepper) = self.pepper.clone() else {
            // `from_values` already refuses this combination; re-checking here
            // means a hand-constructed settings value fails closed rather than
            // panicking or, worse, building an authenticator with no key.
            bail!("credential authentication is enabled but no pepper is loaded");
        };
        Ok(Some(Authenticator::new(
            pool,
            pepper,
            Arc::new(SystemClock::new()),
            self.cache_capacity,
        )))
    }
}

fn non_empty(value: Option<String>) -> Option<String> {
    value.filter(|v| !v.trim().is_empty())
}

// ── Failure reporting ────────────────────────────────────────────────────────

/// Why an attempt failed. **Internal**: for logs, audit records and tests.
///
/// Never rendered to a caller — [`AuthError`]'s `Display` is uniform, so this
/// distinction cannot become an enumeration oracle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureReason {
    /// The string was not `<credential_id>.<secret>`.
    Malformed,
    /// Well-formed, but no such credential.
    UnknownCredential,
    /// The credential exists; the secret did not verify.
    BadSecret,
    Revoked,
    Expired,
    /// Present but suspended (`status = 'disabled'`).
    Inactive,
    /// A row this build cannot interpret. Fails closed.
    Corrupt,
    /// Database or cache unavailable. Fails closed.
    Backend,
}

impl FailureReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Malformed => "malformed",
            Self::UnknownCredential => "unknown_credential",
            Self::BadSecret => "bad_secret",
            Self::Revoked => "revoked",
            Self::Expired => "expired",
            Self::Inactive => "inactive",
            Self::Corrupt => "corrupt",
            Self::Backend => "backend",
        }
    }
}

/// A rejected authentication attempt.
///
/// `Display` is the same string for every reason. `Debug` carries the reason
/// and is intended for logs, which is why handlers must render this with
/// `Display` and never with `{:?}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuthError {
    reason: FailureReason,
}

impl AuthError {
    pub fn reason(&self) -> FailureReason {
        self.reason
    }

    /// Whether this is an infrastructure failure rather than a bad credential.
    ///
    /// Lets a caller answer 503 instead of 401 without telling the caller
    /// anything about the credential: the distinction is between "we are
    /// broken" and "you are wrong", not between two credential states.
    pub fn is_backend(&self) -> bool {
        matches!(self.reason, FailureReason::Backend | FailureReason::Corrupt)
    }
}

impl fmt::Display for AuthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(UNIFORM_REJECTION)
    }
}

impl std::error::Error for AuthError {}

// ── Audit ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthOutcome {
    Authenticated,
    Rejected(FailureReason),
}

/// Attribution for one authentication attempt.
///
/// Carries no secret material by construction: the credential id is the public
/// half of the presented string, and nothing else derived from the secret is
/// recorded.
///
/// `credential_id` is populated whenever the string parsed, including for
/// rejected attempts — an operator investigating an incident needs to know
/// *which* credential was being probed, and the id is not a secret.
#[derive(Debug, Clone)]
pub struct AuthAudit {
    pub at: DateTime<Utc>,
    pub outcome: AuthOutcome,
    pub credential_id: Option<Uuid>,
    pub tenant_id: Option<Uuid>,
    pub principal_id: Option<String>,
}

impl AuthAudit {
    /// Emit as a structured event.
    ///
    /// Rejections are logged at `warn` so a probing campaign is visible without
    /// turning on debug logging; successes at `debug` so normal traffic does not
    /// flood the log.
    pub fn emit(&self) {
        match &self.outcome {
            AuthOutcome::Authenticated => tracing::debug!(
                credential_id = ?self.credential_id,
                tenant_id = ?self.tenant_id,
                principal_id = ?self.principal_id,
                "credential authenticated"
            ),
            AuthOutcome::Rejected(reason) => tracing::warn!(
                credential_id = ?self.credential_id,
                reason = reason.as_str(),
                "credential rejected"
            ),
        }
    }
}

/// The result of one attempt: what the caller gets, and what was recorded.
///
/// Returned together so that audit attribution is a value the caller holds
/// rather than a side effect it has to remember to produce. Step 5 extends the
/// audit with route context before persisting it.
pub struct Authentication {
    pub result: Result<AeonAuthContext, AuthError>,
    pub audit: AuthAudit,
}

// ── Authenticator ────────────────────────────────────────────────────────────

/// Verifies presented credentials against the registry.
pub struct Authenticator {
    pool: PgPool,
    pepper: Pepper,
    cache: cache::BoundedTtlCache<CredentialRecord>,
    clock: Arc<dyn Clock>,
    dummy_macs: AtomicU64,
}

impl fmt::Debug for Authenticator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Authenticator")
            .field("pepper", &self.pepper)
            .field("cache", &self.cache)
            .field("clock", &self.clock)
            .finish_non_exhaustive()
    }
}

impl Authenticator {
    pub fn new(pool: PgPool, pepper: Pepper, clock: Arc<dyn Clock>, capacity: usize) -> Self {
        Self {
            pool,
            pepper,
            cache: cache::BoundedTtlCache::new(capacity, MAX_TTL),
            clock,
            dummy_macs: AtomicU64::new(0),
        }
    }

    /// How many decoy MACs have been computed.
    ///
    /// Exposed so the timing-parity branch can be *observed* to run rather than
    /// assumed to; see the `db_tests` assertion on a lookup miss.
    pub fn dummy_mac_count(&self) -> u64 {
        self.dummy_macs.load(Ordering::Relaxed)
    }

    pub fn cache_ttl(&self) -> Duration {
        self.cache.ttl()
    }

    /// Verify a presented `<credential_id>.<secret>`.
    ///
    /// Order of operations matters and is deliberate:
    ///
    /// 1. parse — a malformed string never reaches the database;
    /// 2. load (cache, then indexed fetch) — a miss performs a decoy MAC;
    /// 3. **verify the MAC**;
    /// 4. only then check status and expiry.
    ///
    /// Step 4 comes last so that "is this credential revoked?" cannot be
    /// answered without holding its secret. Checking status first would make
    /// the registry's lifecycle state readable by anyone who guessed an id.
    pub async fn authenticate(&self, presented: &str) -> Authentication {
        let at = self.clock.now();

        let parsed = match secret::parse_presented(presented) {
            Ok(parsed) => parsed,
            // No id to attribute: the string never resolved to one.
            Err(_) => return self.reject(at, None, FailureReason::Malformed),
        };
        let id = parsed.credential_id();

        let record = match self.load(id).await {
            Ok(Some(record)) => record,
            Ok(None) => {
                self.decoy();
                return self.reject(at, Some(id), FailureReason::UnknownCredential);
            }
            Err(reason) => return self.reject(at, Some(id), reason),
        };

        if !secret::verify(&self.pepper, &parsed, &record.secret_mac) {
            return self.reject(at, Some(id), FailureReason::BadSecret);
        }

        if !record.is_usable_at(at) {
            let reason = match record.status {
                CredentialStatus::Revoked => FailureReason::Revoked,
                CredentialStatus::Disabled => FailureReason::Inactive,
                // Active but unusable means the expiry passed, or a revocation
                // timestamp exists without the matching status.
                CredentialStatus::Active => FailureReason::Expired,
            };
            return self.reject(at, Some(id), reason);
        }

        let context = AeonAuthContext::new(
            record.id,
            record.tenant_id,
            record.principal_id.clone(),
            record.scopes.clone(),
            record.mode,
        );
        let audit = AuthAudit {
            at,
            outcome: AuthOutcome::Authenticated,
            credential_id: Some(record.id),
            tenant_id: Some(record.tenant_id),
            principal_id: Some(record.principal_id.clone()),
        };
        audit.emit();
        Authentication {
            result: Ok(context),
            audit,
        }
    }

    /// Cache first, then one indexed fetch.
    ///
    /// Every error path returns `Err`, never `Ok(None)`: a cache failure that
    /// looked like a miss would silently fall through to the database, and a
    /// database failure that looked like a miss would be indistinguishable from
    /// "no such credential" — which is a rejection either way, but for the
    /// wrong reason and with the wrong audit record.
    async fn load(&self, id: Uuid) -> Result<Option<CredentialRecord>, FailureReason> {
        match self.cache.get(id, self.clock.as_ref()) {
            Ok(Some(record)) => return Ok(Some(record)),
            Ok(None) => {}
            Err(error) => {
                tracing::error!(%error, credential_id = %id, "credential cache unusable");
                return Err(FailureReason::Backend);
            }
        }

        let fetched = match store::fetch(&self.pool, id).await {
            Ok(fetched) => fetched,
            Err(error @ store::StoreError::Corrupt { .. }) => {
                tracing::error!(%error, "stored credential is unusable");
                return Err(FailureReason::Corrupt);
            }
            Err(error) => {
                tracing::error!(%error, credential_id = %id, "credential lookup failed");
                return Err(FailureReason::Backend);
            }
        };

        if let Some(record) = fetched.as_ref() {
            if let Err(error) = self.cache.insert(
                id,
                record.clone(),
                record.cache_bound(),
                self.clock.as_ref(),
            ) {
                tracing::error!(%error, credential_id = %id, "credential cache unusable");
                return Err(FailureReason::Backend);
            }

            // Best-effort attribution. Deliberately does not fail the request:
            // the credential is valid and the caller is authenticated, so
            // rejecting because a bookkeeping UPDATE failed would deny service
            // over a non-security condition. Logged rather than swallowed.
            if let Err(error) = store::touch_last_used(&self.pool, id, self.clock.now()).await {
                tracing::warn!(%error, credential_id = %id, "could not record last_used_at");
            }
        }

        Ok(fetched)
    }

    fn decoy(&self) {
        secret::dummy_verify(&self.pepper);
        self.dummy_macs.fetch_add(1, Ordering::Relaxed);
    }

    fn reject(
        &self,
        at: DateTime<Utc>,
        credential_id: Option<Uuid>,
        reason: FailureReason,
    ) -> Authentication {
        let audit = AuthAudit {
            at,
            outcome: AuthOutcome::Rejected(reason),
            credential_id,
            // Never populated on a rejection: no tenant has been established,
            // and guessing one would be exactly the unauthenticated tenant
            // attribution this design exists to prevent.
            tenant_id: None,
            principal_id: None,
        };
        audit.emit();
        Authentication {
            result: Err(AuthError { reason }),
            audit,
        }
    }

    /// Mint a credential and persist its MAC.
    ///
    /// The returned [`IssuedCredential`] is the only time the secret exists;
    /// after this call only its MAC is recoverable, and only the MAC is stored.
    pub async fn issue(&self, spec: IssueSpec<'_>) -> Result<IssuedCredential> {
        let issued = secret::issue(&self.pepper);
        store::insert(
            &self.pool,
            store::NewCredential {
                id: issued.credential_id,
                tenant_id: spec.tenant_id,
                principal_id: spec.principal_id,
                secret_mac: &issued.secret_mac,
                mode: spec.mode,
                scopes: &spec.scopes,
                tenant_wide: spec.tenant_wide,
                expires_at: spec.expires_at,
            },
        )
        .await?;

        tracing::info!(
            credential_id = %issued.credential_id,
            tenant_id = %spec.tenant_id,
            principal_id = %spec.principal_id,
            scopes = %spec.scopes,
            mode = spec.mode.as_str(),
            tenant_wide = spec.tenant_wide,
            "credential issued"
        );
        Ok(issued)
    }

    /// Revoke a credential. Returns whether a row changed.
    ///
    /// The local cache entry is dropped immediately, but that is an
    /// optimisation for this process only. **The guarantee is the 30-second
    /// TTL**: a revocation performed by another replica, or by direct SQL, is
    /// not visible here until this process's cached record expires.
    pub async fn revoke(&self, id: Uuid) -> Result<bool> {
        let changed = store::revoke(&self.pool, id, self.clock.now()).await?;
        if let Err(error) = self.cache.invalidate(id) {
            // Not fatal: the TTL bound still holds without local invalidation.
            tracing::warn!(%error, credential_id = %id, "could not drop cached credential");
        }
        if changed {
            tracing::info!(credential_id = %id, "credential revoked");
        }
        Ok(changed)
    }
}

/// What to mint. Grouped so adding a field cannot transpose two arguments.
pub struct IssueSpec<'a> {
    pub tenant_id: Uuid,
    pub principal_id: &'a str,
    pub mode: CredentialMode,
    pub scopes: ScopeSet,
    /// §2.2. `false` — agent-restricted — is the safe default and should stay
    /// that way for every issued credential; see the plan before setting it.
    pub tenant_wide: bool,
    pub expires_at: Option<DateTime<Utc>>,
}

// ── Startup ──────────────────────────────────────────────────────────────────

/// Refuse to enable multi-tenant mode against an empty registry (plan §2
/// "Startup", test #31).
///
/// Inert unless multi-tenant mode is on, which nothing sets by default.
///
/// Without this check a deployment could enable multi-tenancy with no
/// credentials at all. Combined with the step-1 gate — which refuses while
/// `MANAGEMENT_API_KEY` is still accepted — that leaves a deployment nothing
/// can authenticate against, and the obvious "fix" is to put the legacy key
/// back, which is precisely the migration this plan is trying to complete.
pub async fn assert_registry_ready_for_multi_tenant(
    pool: &PgPool,
    multi_tenant_enabled: bool,
) -> Result<()> {
    if !multi_tenant_enabled {
        return Ok(());
    }

    let active = store::active_count(pool)
        .await
        .context("could not count active credentials")?;

    if active == 0 {
        bail!(
            "MULTI_TENANT_ENABLED=true but the credential registry holds no active \
             credentials. Nothing could authenticate, and no implicit administrator \
             is created. Issue at least one credential before enabling multi-tenant \
             mode."
        );
    }

    tracing::info!(
        active_credentials = active,
        "credential registry satisfies the multi-tenant startup precondition"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex_pepper() -> String {
        "ab".repeat(32)
    }

    fn env(enabled: Option<&str>, pepper: Option<&str>) -> CredentialAuthEnv {
        CredentialAuthEnv {
            enabled: enabled.map(str::to_string),
            pepper: pepper.map(str::to_string),
            cache_capacity: None,
        }
    }

    // ── Inactive by default ──────────────────────────────────────────────────

    #[test]
    fn an_unconfigured_deployment_is_inactive_and_needs_no_pepper() {
        // The V1 compatibility property: an existing deployment upgraded to
        // this build sets none of these variables and must still start.
        let settings = CredentialAuthSettings::from_values(CredentialAuthEnv::default())
            .expect("an unconfigured deployment must start");
        assert!(!settings.enabled);
        assert!(settings.pepper.is_none());
        assert_eq!(settings.cache_capacity, DEFAULT_CAPACITY);
    }

    #[test]
    fn a_missing_pepper_does_not_break_an_inactive_deployment() {
        for enabled in [None, Some("false"), Some("no"), Some("")] {
            assert!(
                CredentialAuthSettings::from_values(env(enabled, None)).is_ok(),
                "enabled={enabled:?} with no pepper must still start"
            );
        }
    }

    #[test]
    fn only_the_literal_true_enables_the_subsystem() {
        for raw in ["true", "TRUE", "True"] {
            let settings =
                CredentialAuthSettings::from_values(env(Some(raw), Some(&hex_pepper()))).unwrap();
            assert!(settings.enabled, "{raw:?} should enable");
        }
        for raw in ["false", "1", "yes", "on", ""] {
            let settings =
                CredentialAuthSettings::from_values(env(Some(raw), Some(&hex_pepper()))).unwrap();
            assert!(!settings.enabled, "{raw:?} should not enable");
        }
    }

    // ── Fail closed ──────────────────────────────────────────────────────────

    #[test]
    fn a_missing_pepper_fails_closed_when_enabled() {
        let error = CredentialAuthSettings::from_values(env(Some("true"), None))
            .expect_err("enabling without a pepper must refuse to start");
        let rendered = error.to_string();
        assert!(rendered.contains("no pepper is configured"), "{rendered}");
        assert!(
            rendered.contains("never generated"),
            "the error must say the pepper is not defaulted: {rendered}"
        );
    }

    #[test]
    fn a_malformed_pepper_fails_closed_whether_or_not_it_is_enabled() {
        for enabled in [None, Some("false"), Some("true")] {
            let error = CredentialAuthSettings::from_values(env(enabled, Some("not-hex")))
                .expect_err("a malformed pepper must refuse to start");
            assert!(
                error.to_string().contains("malformed"),
                "enabled={enabled:?}"
            );
        }
    }

    #[test]
    fn a_short_pepper_is_refused() {
        let error = CredentialAuthSettings::from_values(env(Some("true"), Some(&"ab".repeat(8))))
            .expect_err("an 8-byte pepper must be refused");
        assert!(error.to_string().contains("malformed"));
    }

    #[test]
    fn a_zero_or_unparseable_cache_capacity_is_refused() {
        for raw in ["0", "-1", "many", "1.5"] {
            let mut e = env(Some("true"), Some(&hex_pepper()));
            e.cache_capacity = Some(raw.to_string());
            assert!(
                CredentialAuthSettings::from_values(e).is_err(),
                "capacity {raw:?} must be refused"
            );
        }
    }

    // `#[tokio::test]`, not `#[test]`: sqlx 0.9's `connect_lazy` registers a
    // pool maintenance task and panics outside a runtime, even though it opens
    // no connection.
    #[tokio::test]
    async fn build_returns_nothing_while_inactive() {
        let settings = CredentialAuthSettings {
            enabled: false,
            pepper: None,
            cache_capacity: DEFAULT_CAPACITY,
        };
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://unused:unused@127.0.0.1:1/unused")
            .expect("lazy pool construction does not connect");
        assert!(
            settings
                .build(pool)
                .expect("inactive must not error")
                .is_none(),
            "an inactive deployment builds no authenticator"
        );
    }

    #[tokio::test]
    async fn a_hand_built_enabled_settings_without_a_pepper_fails_rather_than_panics() {
        let settings = CredentialAuthSettings {
            enabled: true,
            pepper: None,
            cache_capacity: DEFAULT_CAPACITY,
        };
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://unused:unused@127.0.0.1:1/unused")
            .expect("lazy pool construction does not connect");
        let error = settings
            .build(pool)
            .expect_err("enabled without a pepper must fail closed");
        assert!(error.to_string().contains("no pepper is loaded"));
    }

    // ── Uniform rejection ────────────────────────────────────────────────────

    #[test]
    fn every_rejection_renders_identically() {
        // Plan test #17/#29: an attacker must not be able to tell "unknown
        // credential" from "wrong secret" — or from "revoked" — by reading the
        // response.
        let reasons = [
            FailureReason::Malformed,
            FailureReason::UnknownCredential,
            FailureReason::BadSecret,
            FailureReason::Revoked,
            FailureReason::Expired,
            FailureReason::Inactive,
            FailureReason::Corrupt,
            FailureReason::Backend,
        ];
        for reason in reasons {
            assert_eq!(
                AuthError { reason }.to_string(),
                UNIFORM_REJECTION,
                "{reason:?} must render like every other rejection"
            );
        }
    }

    #[test]
    fn only_infrastructure_failures_are_flagged_as_backend() {
        assert!(AuthError {
            reason: FailureReason::Backend
        }
        .is_backend());
        assert!(AuthError {
            reason: FailureReason::Corrupt
        }
        .is_backend());
        for reason in [
            FailureReason::Malformed,
            FailureReason::UnknownCredential,
            FailureReason::BadSecret,
            FailureReason::Revoked,
            FailureReason::Expired,
            FailureReason::Inactive,
        ] {
            assert!(
                !AuthError { reason }.is_backend(),
                "{reason:?} is a credential state, not an outage"
            );
        }
    }

    #[test]
    fn an_error_carries_no_credential_material() {
        let rendered = format!(
            "{:?}",
            AuthError {
                reason: FailureReason::BadSecret
            }
        );
        assert!(!rendered.to_lowercase().contains("pepper"));
        assert!(!rendered.contains('.'), "no presented string in the error");
    }

    // ── Audit ────────────────────────────────────────────────────────────────

    #[test]
    fn a_rejection_audit_attributes_no_tenant() {
        let audit = AuthAudit {
            at: Utc::now(),
            outcome: AuthOutcome::Rejected(FailureReason::BadSecret),
            credential_id: Some(Uuid::from_u128(1)),
            tenant_id: None,
            principal_id: None,
        };
        assert!(
            audit.tenant_id.is_none(),
            "no tenant is established by a failed attempt"
        );
        // The id is retained: an operator investigating a probing campaign
        // needs to know which credential was targeted, and it is not secret.
        assert!(audit.credential_id.is_some());
    }
}
