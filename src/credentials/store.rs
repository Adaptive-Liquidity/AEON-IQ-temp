//! Persistence for the credential registry (migration 0029).
//!
//! Every read validates on the way out: a row whose `mode`, `status` or
//! `scopes` this build does not understand becomes [`StoreError::Corrupt`],
//! never a partially-understood record. Silently dropping an unrecognised scope
//! would *narrow* a credential, and silently defaulting an unrecognised status
//! would *widen* one — both are worse than refusing to use the row.
//!
//! Lookup is by primary key, so authentication costs one index probe and one
//! MAC verification regardless of registry size.

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use super::context::{CredentialMode, ModeError};
use super::scope::{ScopeError, ScopeSet};

/// Lifecycle state of a credential.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialStatus {
    Active,
    /// Deliberately withdrawn. Terminal — a revoked credential is never
    /// reactivated, because reactivation would make an audit trail ambiguous
    /// about which window the credential was usable in.
    Revoked,
    /// Suspended without a revocation record.
    Disabled,
}

impl CredentialStatus {
    /// Wire form, shared with `credentials_status_ck` in migration 0029.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Revoked => "revoked",
            Self::Disabled => "disabled",
        }
    }

    pub fn parse(raw: &str) -> Result<Self, RecordError> {
        match raw {
            "active" => Ok(Self::Active),
            "revoked" => Ok(Self::Revoked),
            "disabled" => Ok(Self::Disabled),
            other => Err(RecordError::Status(other.to_string())),
        }
    }
}

impl std::fmt::Display for CredentialStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A validated `credentials` row.
#[derive(Clone)]
pub struct CredentialRecord {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub principal_id: String,
    pub secret_mac: Vec<u8>,
    pub mode: CredentialMode,
    pub scopes: ScopeSet,
    /// §2.2. Inert until `credential_agent_grants` exists (plan step 3), and
    /// deliberately not carried on `AeonAuthContext`.
    pub tenant_wide: bool,
    pub status: CredentialStatus,
    pub created_at: DateTime<Utc>,
    pub last_used_at: Option<DateTime<Utc>>,
    pub revoked_at: Option<DateTime<Utc>>,
    pub expires_at: Option<DateTime<Utc>>,
}

impl std::fmt::Debug for CredentialRecord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `secret_mac` is not the secret, but it is the verification target: if
        // the pepper ever leaks, a logged MAC becomes an offline attack goal.
        // There is no reason for it to appear in a log line.
        f.debug_struct("CredentialRecord")
            .field("id", &self.id)
            .field("tenant_id", &self.tenant_id)
            .field("principal_id", &self.principal_id)
            .field("secret_mac", &"<redacted>")
            .field("mode", &self.mode)
            .field("scopes", &self.scopes)
            .field("tenant_wide", &self.tenant_wide)
            .field("status", &self.status)
            .field("created_at", &self.created_at)
            .field("last_used_at", &self.last_used_at)
            .field("revoked_at", &self.revoked_at)
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

impl CredentialRecord {
    /// Whether the credential is usable at `now`.
    ///
    /// Fails closed on every axis: anything other than `Active`, any revocation
    /// timestamp at all, and any expiry at or before `now`.
    pub fn is_usable_at(&self, now: DateTime<Utc>) -> bool {
        self.status == CredentialStatus::Active
            && self.revoked_at.is_none()
            && self.expires_at.map(|expiry| now < expiry).unwrap_or(true)
    }

    /// The latest instant a cache entry for this record may remain live.
    ///
    /// `None` means "no bound of its own", so only the cache TTL applies.
    pub fn cache_bound(&self) -> Option<DateTime<Utc>> {
        self.expires_at
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RecordError {
    #[error("unknown status {0:?}")]
    Status(String),
    #[error(transparent)]
    Mode(#[from] ModeError),
    #[error(transparent)]
    Scopes(#[from] ScopeError),
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("credential store is unavailable")]
    Backend(#[from] sqlx::Error),
    /// A row exists but this build cannot interpret it. Fails closed.
    #[error("credential {id} is stored in a form this build cannot use: {source}")]
    Corrupt { id: Uuid, source: RecordError },
}

/// The raw column shapes, converted into [`CredentialRecord`] only after
/// validation.
#[derive(sqlx::FromRow)]
struct CredentialRow {
    id: Uuid,
    tenant_id: Uuid,
    principal_id: String,
    secret_mac: Vec<u8>,
    mode: String,
    scopes: Vec<String>,
    tenant_wide: bool,
    status: String,
    created_at: DateTime<Utc>,
    last_used_at: Option<DateTime<Utc>>,
    revoked_at: Option<DateTime<Utc>>,
    expires_at: Option<DateTime<Utc>>,
}

impl CredentialRow {
    fn validate(self) -> Result<CredentialRecord, StoreError> {
        let id = self.id;
        let convert = || -> Result<CredentialRecord, RecordError> {
            Ok(CredentialRecord {
                id,
                tenant_id: self.tenant_id,
                principal_id: self.principal_id.clone(),
                secret_mac: self.secret_mac.clone(),
                mode: CredentialMode::parse(&self.mode)?,
                scopes: ScopeSet::parse_list(&self.scopes)?,
                tenant_wide: self.tenant_wide,
                status: CredentialStatus::parse(&self.status)?,
                created_at: self.created_at,
                last_used_at: self.last_used_at,
                revoked_at: self.revoked_at,
                expires_at: self.expires_at,
            })
        };
        convert().map_err(|source| StoreError::Corrupt { id, source })
    }
}

/// Every column, in a fixed order shared by the queries below.
const SELECT_COLUMNS: &str = "id, tenant_id, principal_id, secret_mac, mode, scopes, \
     tenant_wide, status, created_at, last_used_at, revoked_at, expires_at";

/// Indexed fetch by primary key.
///
/// Returns `Ok(None)` for "no such credential" — the caller is responsible for
/// performing the decoy MAC so that a miss and a wrong secret cost the same.
pub async fn fetch(pool: &PgPool, id: Uuid) -> Result<Option<CredentialRecord>, StoreError> {
    // The literal is spelled out rather than built from SELECT_COLUMNS because
    // sqlx 0.9 requires a &'static str here; the test below keeps the two in
    // agreement.
    let row: Option<CredentialRow> = sqlx::query_as(
        "SELECT id, tenant_id, principal_id, secret_mac, mode, scopes, \
                tenant_wide, status, created_at, last_used_at, revoked_at, expires_at \
           FROM credentials WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;

    row.map(CredentialRow::validate).transpose()
}

/// A credential about to be written. Grouped rather than passed positionally so
/// that adding a column cannot silently transpose two arguments of the same
/// type.
pub struct NewCredential<'a> {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub principal_id: &'a str,
    pub secret_mac: &'a [u8],
    pub mode: CredentialMode,
    pub scopes: &'a ScopeSet,
    pub tenant_wide: bool,
    pub expires_at: Option<DateTime<Utc>>,
}

/// Write a credential in **one** statement.
///
/// An earlier revision inserted the row and then set `expires_at` in a second
/// `UPDATE`, on the mistaken belief that `credentials_expires_ck` needed
/// `created_at` to already exist. It does not: a `CHECK` is evaluated against
/// the completed row, after `DEFAULT NOW()` has been applied, so a single
/// `INSERT` satisfies it.
///
/// The two-statement version was not merely redundant, it was unsafe. Without a
/// transaction, a failed `UPDATE` left a committed, active, **non-expiring** row
/// whose plaintext secret the caller had already discarded — an orphan nobody
/// can authenticate with, which nonetheless counted towards
/// [`active_count`] and so towards the multi-tenant readiness gate.
pub async fn insert(pool: &PgPool, new: NewCredential<'_>) -> Result<(), StoreError> {
    sqlx::query(
        "INSERT INTO credentials \
             (id, tenant_id, principal_id, secret_mac, mode, scopes, tenant_wide, \
              status, expires_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, 'active', $8)",
    )
    .bind(new.id)
    .bind(new.tenant_id)
    .bind(new.principal_id)
    .bind(new.secret_mac)
    .bind(new.mode.as_str())
    .bind(new.scopes.to_wire())
    .bind(new.tenant_wide)
    .bind(new.expires_at)
    .execute(pool)
    .await?;
    Ok(())
}

/// Mark a credential revoked. Returns whether a row changed.
///
/// Idempotent: revoking an already-revoked credential is a no-op returning
/// `false`, not an error, because the caller's intent is already satisfied.
pub async fn revoke(pool: &PgPool, id: Uuid, at: DateTime<Utc>) -> Result<bool, StoreError> {
    let result = sqlx::query(
        "UPDATE credentials SET status = 'revoked', revoked_at = $2 \
          WHERE id = $1 AND status <> 'revoked'",
    )
    .bind(id)
    .bind(at)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// Best-effort "seen around then" marker.
///
/// Written on cache misses only, so at most once per credential per TTL window
/// rather than once per request. It is not an audit log and must not be read as
/// one — the audit trail is the structured event emitted per authentication.
pub async fn touch_last_used(pool: &PgPool, id: Uuid, at: DateTime<Utc>) -> Result<(), StoreError> {
    sqlx::query("UPDATE credentials SET last_used_at = $2 WHERE id = $1")
        .bind(id)
        .bind(at)
        .execute(pool)
        .await?;
    Ok(())
}

/// How many credentials could authenticate anyone right now.
///
/// Used by the §2 startup check: an empty registry plus multi-tenant mode means
/// nothing can authenticate, so the deployment would come up unreachable or —
/// worse — be "fixed" by re-enabling the legacy key.
///
/// Expiry is part of the question, not a detail. `status = 'active'` alone would
/// count a registry whose every row expired an hour ago, which
/// [`CredentialRecord::is_usable_at`] rejects one by one — so the gate would
/// call a deployment ready that nobody can authenticate against, which is the
/// exact condition it exists to catch.
pub async fn active_count(pool: &PgPool) -> Result<i64, StoreError> {
    let (count,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM credentials \
          WHERE status = 'active' AND (expires_at IS NULL OR expires_at > NOW())",
    )
    .fetch_one(pool)
    .await?;
    Ok(count)
}

// ── Schema contract ──────────────────────────────────────────────────────────
//
// `CREATE TABLE IF NOT EXISTS` is idempotent in the sense that re-running it is
// harmless, but it is *not* a guarantee about what is there. If a table named
// `credentials` already exists — hand-made, restored from an older dump, or
// created by something else entirely — PostgreSQL skips the whole definition
// and the migration records success against a table that may have no primary
// key, no MAC-length check and no per-tenant uniqueness.
//
// For an authentication authority that is not acceptable: `fetch` assumes `id`
// identifies exactly one authoritative row, and the CHECK constraints are what
// make a truncated MAC or a forged status unrepresentable. So when credential
// authentication is enabled the schema is *proved* before the authenticator is
// built, and startup fails closed on any mismatch.
//
// None of this runs when credential authentication is disabled, so a V1
// deployment that never adopted credentials still starts exactly as before.

/// `(column, information_schema.data_type, nullable)`.
const REQUIRED_COLUMNS: &[(&str, &str, bool)] = &[
    ("id", "uuid", false),
    ("tenant_id", "uuid", false),
    ("principal_id", "text", false),
    ("secret_mac", "bytea", false),
    ("mode", "text", false),
    ("scopes", "ARRAY", false),
    ("tenant_wide", "boolean", false),
    ("status", "text", false),
    ("created_at", "timestamp with time zone", false),
    ("last_used_at", "timestamp with time zone", true),
    ("revoked_at", "timestamp with time zone", true),
    ("expires_at", "timestamp with time zone", true),
];

/// CHECK constraints that must exist **and be validated**. A `NOT VALID`
/// constraint is enforced for new writes but was never checked against existing
/// rows, so it does not establish the invariant for what is already stored.
const REQUIRED_CHECKS: &[&str] = &[
    "credentials_mode_ck",
    "credentials_status_ck",
    "credentials_secret_mac_len_ck",
    "credentials_principal_id_ck",
    "credentials_revoked_ck",
    "credentials_expires_ck",
    "credentials_scopes_ck",
];

const REQUIRED_INDEXES: &[&str] = &[
    "credentials_pkey",
    "credentials_id_tenant_id_key",
    "idx_credentials_tenant_principal",
    "idx_credentials_active",
];

#[derive(Debug, thiserror::Error)]
pub enum SchemaError {
    /// The schema could not be inspected at all. Fails closed: an
    /// unverifiable schema is treated exactly like a wrong one.
    #[error("the credential schema could not be inspected")]
    Backend(#[from] sqlx::Error),
    #[error(
        "the `credentials` table does not match the contract this build requires:\n{}",
        .0.iter().map(|p| format!("  • {p}")).collect::<Vec<_>>().join("\n")
    )]
    Contract(Vec<String>),
}

/// Prove the `credentials` table matches what this build assumes.
///
/// Collects **every** mismatch rather than failing on the first, so an operator
/// repairing a drifted schema sees the whole list in one startup attempt
/// instead of discovering it one restart at a time.
pub async fn verify_schema(pool: &PgPool) -> Result<(), SchemaError> {
    let (present,): (bool,) =
        sqlx::query_as("SELECT to_regclass('public.credentials') IS NOT NULL")
            .fetch_one(pool)
            .await?;
    if !present {
        return Err(SchemaError::Contract(vec![
            "table `credentials` does not exist; migration 0029 has not been applied".into(),
        ]));
    }

    let mut problems = Vec::new();

    // ── Columns, types and nullability ───────────────────────────────────────
    let columns: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT column_name, data_type, is_nullable \
           FROM information_schema.columns \
          WHERE table_schema = 'public' AND table_name = 'credentials'",
    )
    .fetch_all(pool)
    .await?;

    for (name, want_type, want_nullable) in REQUIRED_COLUMNS {
        match columns.iter().find(|(column, _, _)| column == name) {
            None => problems.push(format!("column `{name}` is missing")),
            Some((_, got_type, got_nullable)) => {
                if got_type != want_type {
                    problems.push(format!(
                        "column `{name}` has type `{got_type}`, expected `{want_type}`"
                    ));
                }
                let got_nullable = got_nullable == "YES";
                if got_nullable != *want_nullable {
                    problems.push(format!(
                        "column `{name}` is {}, expected {}",
                        if got_nullable { "nullable" } else { "NOT NULL" },
                        if *want_nullable {
                            "nullable"
                        } else {
                            "NOT NULL"
                        },
                    ));
                }
            }
        }
    }

    // ── Constraints ──────────────────────────────────────────────────────────
    // `conkey` ordinality is preserved so a primary key on the wrong column, or
    // a composite unique in the wrong order, is caught rather than assumed.
    let constraints: Vec<(String, String, bool, String)> = sqlx::query_as(
        "SELECT c.conname, c.contype::text, c.convalidated, \
                COALESCE((SELECT string_agg(a.attname, ',' ORDER BY k.ord) \
                            FROM unnest(c.conkey) WITH ORDINALITY AS k(attnum, ord) \
                            JOIN pg_attribute a \
                              ON a.attrelid = c.conrelid AND a.attnum = k.attnum), '') \
           FROM pg_constraint c \
          WHERE c.conrelid = 'public.credentials'::regclass",
    )
    .fetch_all(pool)
    .await?;

    match constraints.iter().find(|(_, kind, _, _)| kind == "p") {
        None => problems.push(
            "no PRIMARY KEY; `id` would not identify exactly one row, so a duplicate \
             credential id could authenticate as whichever row was returned"
                .into(),
        ),
        Some((name, _, _, cols)) if cols != "id" => problems.push(format!(
            "primary key `{name}` covers ({cols}), expected (id)"
        )),
        Some(_) => {}
    }

    if !constraints
        .iter()
        .any(|(_, kind, _, cols)| kind == "u" && cols == "id,tenant_id")
    {
        problems.push(
            "no UNIQUE (id, tenant_id); step 3's credential_agent_grants composite \
             foreign key has no target"
                .into(),
        );
    }

    for want in REQUIRED_CHECKS {
        match constraints
            .iter()
            .find(|(name, kind, _, _)| name == want && kind == "c")
        {
            None => problems.push(format!("CHECK constraint `{want}` is missing")),
            Some((_, _, false, _)) => problems.push(format!(
                "CHECK constraint `{want}` is NOT VALID: it constrains new writes but \
                 existing rows were never verified against it"
            )),
            Some(_) => {}
        }
    }

    // ── Indexes ──────────────────────────────────────────────────────────────
    let indexes: Vec<(String,)> = sqlx::query_as(
        "SELECT indexname FROM pg_indexes \
          WHERE schemaname = 'public' AND tablename = 'credentials'",
    )
    .fetch_all(pool)
    .await?;

    for want in REQUIRED_INDEXES {
        if !indexes.iter().any(|(name,)| name == want) {
            problems.push(format!("index `{want}` is missing"));
        }
    }

    // ── Privileges ───────────────────────────────────────────────────────────
    // Checked at startup rather than discovered at the first request, when the
    // failure would surface as an authentication outage under load.
    let (select, insert, update): (bool, bool, bool) = sqlx::query_as(
        "SELECT has_table_privilege('public.credentials', 'SELECT'), \
                has_table_privilege('public.credentials', 'INSERT'), \
                has_table_privilege('public.credentials', 'UPDATE')",
    )
    .fetch_one(pool)
    .await?;

    for (granted, privilege) in [(select, "SELECT"), (insert, "INSERT"), (update, "UPDATE")] {
        if !granted {
            problems.push(format!(
                "the connected role lacks {privilege} on `credentials`"
            ));
        }
    }

    if problems.is_empty() {
        Ok(())
    } else {
        Err(SchemaError::Contract(problems))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn epoch() -> DateTime<Utc> {
        DateTime::from_timestamp(1_700_000_000, 0).expect("fixed valid timestamp")
    }

    fn record(status: CredentialStatus, expires_at: Option<DateTime<Utc>>) -> CredentialRecord {
        CredentialRecord {
            id: Uuid::from_u128(1),
            tenant_id: Uuid::from_u128(2),
            principal_id: "svc-ingest".into(),
            secret_mac: vec![0xab; 32],
            mode: CredentialMode::V2Required,
            scopes: ScopeSet::parse_list(&["memory:read"]).unwrap(),
            tenant_wide: false,
            status,
            created_at: epoch(),
            last_used_at: None,
            revoked_at: if status == CredentialStatus::Revoked {
                Some(epoch())
            } else {
                None
            },
            expires_at,
        }
    }

    #[test]
    fn status_round_trips_and_rejects_anything_else() {
        for status in [
            CredentialStatus::Active,
            CredentialStatus::Revoked,
            CredentialStatus::Disabled,
        ] {
            assert_eq!(CredentialStatus::parse(status.as_str()), Ok(status));
        }
        assert!(CredentialStatus::parse("ACTIVE").is_err());
        assert!(CredentialStatus::parse("expired").is_err());
        assert!(CredentialStatus::parse("").is_err());
    }

    #[test]
    fn status_wire_values_match_the_migration_check_constraint() {
        let migration = include_str!("../../migrations/0029_credentials.sql");
        for status in [
            CredentialStatus::Active,
            CredentialStatus::Revoked,
            CredentialStatus::Disabled,
        ] {
            assert!(
                migration.contains(&format!("'{}'", status.as_str())),
                "migration 0029 does not list {status}"
            );
        }
    }

    #[test]
    fn only_an_active_unrevoked_unexpired_credential_is_usable() {
        assert!(record(CredentialStatus::Active, None).is_usable_at(epoch()));
        assert!(!record(CredentialStatus::Revoked, None).is_usable_at(epoch()));
        assert!(!record(CredentialStatus::Disabled, None).is_usable_at(epoch()));
    }

    #[test]
    fn expiry_is_exclusive_at_the_boundary() {
        let expiry = epoch() + chrono::Duration::seconds(10);
        let rec = record(CredentialStatus::Active, Some(expiry));
        assert!(rec.is_usable_at(expiry - chrono::Duration::seconds(1)));
        assert!(
            !rec.is_usable_at(expiry),
            "a credential is not usable at its own expiry instant"
        );
        assert!(!rec.is_usable_at(expiry + chrono::Duration::seconds(1)));
    }

    #[test]
    fn a_revocation_timestamp_alone_makes_a_record_unusable() {
        // Belt and braces against the database CHECK: even if a row somehow
        // carried status='active' with revoked_at set, this must not
        // authenticate.
        let mut rec = record(CredentialStatus::Active, None);
        rec.revoked_at = Some(epoch());
        assert!(!rec.is_usable_at(epoch()));
    }

    #[test]
    fn the_cache_bound_is_the_credentials_own_expiry() {
        assert_eq!(record(CredentialStatus::Active, None).cache_bound(), None);
        let expiry = epoch() + chrono::Duration::seconds(5);
        assert_eq!(
            record(CredentialStatus::Active, Some(expiry)).cache_bound(),
            Some(expiry)
        );
    }

    #[test]
    fn record_debug_redacts_the_mac() {
        let rendered = format!("{:?}", record(CredentialStatus::Active, None));
        assert!(rendered.contains("svc-ingest"));
        assert!(rendered.contains("<redacted>"));
        assert!(
            !rendered.contains("171, 171"),
            "the raw MAC bytes must not be rendered"
        );
        assert!(!rendered.contains("abab"));
    }

    #[test]
    fn the_select_list_matches_the_query_literal() {
        // `SELECT_COLUMNS` documents the column order; sqlx 0.9 needs the query
        // itself to be a &'static str, so the two are written twice. This keeps
        // them from drifting.
        let normalise = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
        let expected = normalise(SELECT_COLUMNS);
        let in_query = normalise(
            "id, tenant_id, principal_id, secret_mac, mode, scopes, \
             tenant_wide, status, created_at, last_used_at, revoked_at, expires_at",
        );
        assert_eq!(expected, in_query);
    }

    #[test]
    fn every_selected_column_exists_in_the_migration() {
        let migration = include_str!("../../migrations/0029_credentials.sql");
        for column in SELECT_COLUMNS.split(',').map(str::trim) {
            assert!(
                migration.contains(column),
                "migration 0029 declares no column {column:?}"
            );
        }
    }
}
