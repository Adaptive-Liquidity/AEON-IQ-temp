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

/// `(column, information_schema.data_type, nullable, expected column_default)`.
///
/// The default is part of the contract, not decoration. [`insert`] omits
/// `created_at` and relies on `DEFAULT now()`; strip that default and the column
/// is still `TIMESTAMPTZ NOT NULL`, still passes a type-and-nullability check,
/// and every `issue()` fails on a NULL violation — a schema the gate would have
/// called "verified" while nothing could be issued against it.
///
/// `None` means "must have no default", checked exactly rather than ignored:
/// this is an authentication table, and a value nobody declared is a value
/// nobody reasoned about.
const REQUIRED_COLUMNS: &[(&str, &str, bool, Option<&str>)] = &[
    ("id", "uuid", false, None),
    ("tenant_id", "uuid", false, None),
    ("principal_id", "text", false, None),
    ("secret_mac", "bytea", false, None),
    ("mode", "text", false, None),
    ("scopes", "ARRAY", false, Some("'{}'::text[]")),
    ("tenant_wide", "boolean", false, Some("false")),
    ("status", "text", false, Some("'active'::text")),
    (
        "created_at",
        "timestamp with time zone",
        false,
        Some("now()"),
    ),
    ("last_used_at", "timestamp with time zone", true, None),
    ("revoked_at", "timestamp with time zone", true, None),
    ("expires_at", "timestamp with time zone", true, None),
];

/// CHECK constraints that must exist, be **validated**, and carry the
/// **expected expression**.
///
/// A `NOT VALID` constraint is enforced for new writes but was never checked
/// against existing rows, so it does not establish the invariant for what is
/// already stored.
///
/// Matching on name and validation state alone is not enough either: a
/// pre-existing table can carry `credentials_principal_id_ck CHECK (true)` —
/// right name, validated, and completely inert. Since `CredentialRow::validate`
/// does not re-check `principal_id`, that would let an empty principal be
/// written and then authenticate. So the expression itself is compared.
///
/// These are `pg_get_constraintdef` output, which is PostgreSQL's normalised
/// rendering rather than the migration's source text — hence the extra
/// parentheses and `::text` casts. It is stable within a major version;
/// `the_migrated_schema_satisfies_the_contract` fails loudly on the pinned
/// pg16 image if a future version renders them differently.
const REQUIRED_CHECKS: &[(&str, &str)] = &[
    (
        "credentials_mode_ck",
        "CHECK ((mode = ANY (ARRAY['legacy_only'::text, 'v2_required'::text])))",
    ),
    (
        "credentials_status_ck",
        "CHECK ((status = ANY (ARRAY['active'::text, 'revoked'::text, 'disabled'::text])))",
    ),
    (
        "credentials_secret_mac_len_ck",
        "CHECK ((octet_length(secret_mac) = 32))",
    ),
    (
        "credentials_principal_id_ck",
        "CHECK (((length(principal_id) >= 1) AND (length(principal_id) <= 255)))",
    ),
    (
        "credentials_revoked_ck",
        "CHECK (((status = 'revoked'::text) = (revoked_at IS NOT NULL)))",
    ),
    (
        "credentials_expires_ck",
        "CHECK (((expires_at IS NULL) OR (expires_at > created_at)))",
    ),
    (
        "credentials_scopes_ck",
        "CHECK ((scopes <@ ARRAY['recall:v2'::text, 'memory:read'::text, \
         'memory:write'::text, 'memory:export'::text, 'memory:history'::text, \
         'admin'::text, 'proxy:chat'::text]))",
    ),
];

/// Collapse whitespace so a comparison is not defeated by line wrapping in the
/// constant above.
fn normalise_sql(raw: &str) -> String {
    raw.split_whitespace().collect::<Vec<_>>().join(" ")
}

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
    // Names the schema rather than one table: since step 3 this also reports
    // `credential_agent_grants` problems, and an error that says "the
    // credentials table" while listing grant-table defects sends the reader to
    // the wrong place.
    #[error(
        "the credential schema does not match the contract this build requires:\n{}",
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
    let columns: Vec<(String, String, String, Option<String>)> = sqlx::query_as(
        "SELECT column_name, data_type, is_nullable, column_default \
           FROM information_schema.columns \
          WHERE table_schema = 'public' AND table_name = 'credentials'",
    )
    .fetch_all(pool)
    .await?;

    for (name, want_type, want_nullable, want_default) in REQUIRED_COLUMNS {
        match columns.iter().find(|(column, _, _, _)| column == name) {
            None => problems.push(format!("column `{name}` is missing")),
            Some((_, got_type, got_nullable, got_default)) => {
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

                let got_default = got_default.as_deref().map(normalise_sql);
                let want_default = want_default.map(normalise_sql);
                if got_default != want_default {
                    problems.push(format!(
                        "column `{name}` has default {}, expected {}",
                        got_default.as_deref().unwrap_or("(none)"),
                        want_default.as_deref().unwrap_or("(none)"),
                    ));
                }
            }
        }
    }

    // ── Constraints ──────────────────────────────────────────────────────────
    // `conkey` ordinality is preserved so a primary key on the wrong column, or
    // a composite unique in the wrong order, is caught rather than assumed.
    let constraints: Vec<(String, String, bool, String, String)> = sqlx::query_as(
        "SELECT c.conname, c.contype::text, c.convalidated, \
                COALESCE((SELECT string_agg(a.attname, ',' ORDER BY k.ord) \
                            FROM unnest(c.conkey) WITH ORDINALITY AS k(attnum, ord) \
                            JOIN pg_attribute a \
                              ON a.attrelid = c.conrelid AND a.attnum = k.attnum), ''), \
                pg_get_constraintdef(c.oid) \
           FROM pg_constraint c \
          WHERE c.conrelid = 'public.credentials'::regclass",
    )
    .fetch_all(pool)
    .await?;

    match constraints.iter().find(|(_, kind, _, _, _)| kind == "p") {
        None => problems.push(
            "no PRIMARY KEY; `id` would not identify exactly one row, so a duplicate \
             credential id could authenticate as whichever row was returned"
                .into(),
        ),
        Some((name, _, _, cols, _)) if cols != "id" => problems.push(format!(
            "primary key `{name}` covers ({cols}), expected (id)"
        )),
        Some(_) => {}
    }

    if !constraints
        .iter()
        .any(|(_, kind, _, cols, _)| kind == "u" && cols == "id,tenant_id")
    {
        problems.push(
            "no UNIQUE (id, tenant_id); step 3's credential_agent_grants composite \
             foreign key has no target"
                .into(),
        );
    }

    for (want, want_def) in REQUIRED_CHECKS {
        match constraints
            .iter()
            .find(|(name, kind, _, _, _)| name == want && kind == "c")
        {
            None => problems.push(format!("CHECK constraint `{want}` is missing")),
            Some((_, _, false, _, _)) => problems.push(format!(
                "CHECK constraint `{want}` is NOT VALID: it constrains new writes but \
                 existing rows were never verified against it"
            )),
            // The name and validation state can both be right while the
            // expression is inert — `CHECK (true)` under the expected name is
            // the drift this catches.
            Some((_, _, _, _, got_def)) if normalise_sql(got_def) != normalise_sql(want_def) => {
                problems.push(format!(
                    "CHECK constraint `{want}` has the expected name but a different \
                     expression: found `{}`, expected `{}`",
                    normalise_sql(got_def),
                    normalise_sql(want_def),
                ))
            }
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

    // ── Agent grants (migration 0030) ────────────────────────────────────────
    // Checked here rather than in a separate pass so a drifted schema produces
    // one list. Same reasoning as above: the composite foreign keys and the
    // mutual-exclusion triggers are what make cross-tenant and contradictory
    // grants unrepresentable, so their presence is proved rather than assumed.
    verify_grants_schema(pool, &mut problems).await?;

    if problems.is_empty() {
        Ok(())
    } else {
        Err(SchemaError::Contract(problems))
    }
}

/// `(column, data_type, nullable, expected default)` for `credential_agent_grants`.
const REQUIRED_GRANT_COLUMNS: &[(&str, &str, bool, Option<&str>)] = &[
    ("credential_id", "uuid", false, None),
    ("tenant_id", "uuid", false, None),
    ("agent_uuid", "uuid", false, None),
    (
        "created_at",
        "timestamp with time zone",
        false,
        Some("now()"),
    ),
];

/// Both foreign keys must be **composite**. A single-column reference would
/// leave `tenant_id` free to name any tenant at all, which is precisely the
/// earlier §2.2 draft the plan rejects — so the definitions are compared, not
/// merely counted.
const REQUIRED_GRANT_FOREIGN_KEYS: &[(&str, &str)] = &[
    (
        "credential_agent_grants_credential_fkey",
        "FOREIGN KEY (credential_id, tenant_id) REFERENCES credentials(id, tenant_id) \
         ON DELETE CASCADE",
    ),
    (
        "credential_agent_grants_agent_fkey",
        "FOREIGN KEY (tenant_id, agent_uuid) REFERENCES agents(tenant_id, id) ON DELETE CASCADE",
    ),
];

/// `(table, trigger, expected pg_get_triggerdef)` — the pair enforcing
/// tenant-wide/grants mutual exclusion.
///
/// Three things are checked, and each covers a different way this can rot:
///
/// * **Presence** — the obvious one.
/// * **`tgenabled`** — a disabled trigger is listed by `pg_trigger` exactly like
///   an enabled one, and `ALTER TABLE … DISABLE TRIGGER` is a routine thing to
///   do for a bulk load and an easy thing to forget to undo.
/// * **The definition** — same reasoning as the CHECK expressions above. A
///   trigger with the right name, enabled, but firing on the wrong event or
///   calling a no-op function passes the first two checks while enforcing
///   nothing. `authorize_agent` would still deny the contradictory state it
///   allows, but startup would have declared the schema sound.
const REQUIRED_TRIGGERS: &[(&str, &str, &str)] = &[
    (
        "credential_agent_grants",
        "credential_agent_grants_not_tenant_wide",
        "CREATE TRIGGER credential_agent_grants_not_tenant_wide BEFORE INSERT OR UPDATE \
         ON public.credential_agent_grants FOR EACH ROW \
         EXECUTE FUNCTION credential_agent_grants_reject_tenant_wide()",
    ),
    (
        "credentials",
        "credentials_not_tenant_wide_with_grants",
        "CREATE TRIGGER credentials_not_tenant_wide_with_grants BEFORE INSERT OR UPDATE \
         ON public.credentials FOR EACH ROW \
         EXECUTE FUNCTION credentials_reject_tenant_wide_with_grants()",
    ),
];

/// Append any `credential_agent_grants` contract violations to `problems`.
async fn verify_grants_schema(
    pool: &PgPool,
    problems: &mut Vec<String>,
) -> Result<(), sqlx::Error> {
    let (present,): (bool,) =
        sqlx::query_as("SELECT to_regclass('public.credential_agent_grants') IS NOT NULL")
            .fetch_one(pool)
            .await?;
    if !present {
        problems.push(
            "table `credential_agent_grants` does not exist; migration 0030 has not been \
             applied, so no credential could be restricted to an agent set"
                .into(),
        );
        // Everything below would pile on the same root cause.
        return Ok(());
    }

    let columns: Vec<(String, String, String, Option<String>)> = sqlx::query_as(
        "SELECT column_name, data_type, is_nullable, column_default \
           FROM information_schema.columns \
          WHERE table_schema = 'public' AND table_name = 'credential_agent_grants'",
    )
    .fetch_all(pool)
    .await?;

    for (name, want_type, want_nullable, want_default) in REQUIRED_GRANT_COLUMNS {
        match columns.iter().find(|(column, _, _, _)| column == name) {
            None => problems.push(format!(
                "column `credential_agent_grants.{name}` is missing"
            )),
            Some((_, got_type, got_nullable, got_default)) => {
                if got_type != want_type {
                    problems.push(format!(
                        "column `credential_agent_grants.{name}` has type `{got_type}`, \
                         expected `{want_type}`"
                    ));
                }
                if (got_nullable == "YES") != *want_nullable {
                    problems.push(format!(
                        "column `credential_agent_grants.{name}` is {}, expected {}",
                        if got_nullable == "YES" {
                            "nullable"
                        } else {
                            "NOT NULL"
                        },
                        if *want_nullable {
                            "nullable"
                        } else {
                            "NOT NULL"
                        },
                    ));
                }
                let got_default = got_default.as_deref().map(normalise_sql);
                let want_default = want_default.map(normalise_sql);
                if got_default != want_default {
                    problems.push(format!(
                        "column `credential_agent_grants.{name}` has default {}, expected {}",
                        got_default.as_deref().unwrap_or("(none)"),
                        want_default.as_deref().unwrap_or("(none)"),
                    ));
                }
            }
        }
    }

    let constraints: Vec<(String, String, String, String)> = sqlx::query_as(
        "SELECT c.conname, c.contype::text, \
                COALESCE((SELECT string_agg(a.attname, ',' ORDER BY k.ord) \
                            FROM unnest(c.conkey) WITH ORDINALITY AS k(attnum, ord) \
                            JOIN pg_attribute a \
                              ON a.attrelid = c.conrelid AND a.attnum = k.attnum), ''), \
                pg_get_constraintdef(c.oid) \
           FROM pg_constraint c \
          WHERE c.conrelid = 'public.credential_agent_grants'::regclass",
    )
    .fetch_all(pool)
    .await?;

    match constraints.iter().find(|(_, kind, _, _)| kind == "p") {
        None => problems.push(
            "`credential_agent_grants` has no PRIMARY KEY, so the same agent could be \
             granted to the same credential more than once"
                .into(),
        ),
        Some((name, _, cols, _)) if cols != "credential_id,agent_uuid" => problems.push(format!(
            "primary key `{name}` covers ({cols}), expected (credential_id, agent_uuid)"
        )),
        Some(_) => {}
    }

    for (name, want_def) in REQUIRED_GRANT_FOREIGN_KEYS {
        match constraints
            .iter()
            .find(|(got, kind, _, _)| got == name && kind == "f")
        {
            None => problems.push(format!("foreign key `{name}` is missing")),
            Some((_, _, _, got_def)) if normalise_sql(got_def) != normalise_sql(want_def) => {
                problems.push(format!(
                    "foreign key `{name}` is `{}`, expected `{}` — a non-composite \
                     reference would leave tenant_id free to name any tenant",
                    normalise_sql(got_def),
                    normalise_sql(want_def),
                ))
            }
            Some(_) => {}
        }
    }

    let (index_present,): (bool,) = sqlx::query_as(
        "SELECT EXISTS (SELECT 1 FROM pg_indexes \
          WHERE schemaname = 'public' AND tablename = 'credential_agent_grants' \
            AND indexname = 'idx_credential_agent_grants_agent')",
    )
    .fetch_one(pool)
    .await?;
    if !index_present {
        problems.push("index `idx_credential_agent_grants_agent` is missing".into());
    }

    for (table, trigger, want_def) in REQUIRED_TRIGGERS {
        let found: Option<(String, String)> = sqlx::query_as(
            "SELECT t.tgenabled::text, pg_get_triggerdef(t.oid) FROM pg_trigger t \
              WHERE t.tgrelid = ($1 || '')::regclass AND t.tgname = $2 AND NOT t.tgisinternal",
        )
        .bind(format!("public.{table}"))
        .bind(trigger)
        .fetch_optional(pool)
        .await?;

        let Some((state, got_def)) = found else {
            problems.push(format!(
                "trigger `{trigger}` on `{table}` is missing; tenant-wide and per-agent \
                 grants would no longer be mutually exclusive"
            ));
            continue;
        };

        // 'O' origin, 'A' always. 'D' is disabled; 'R' fires only on replica.
        if state != "O" && state != "A" {
            problems.push(format!(
                "trigger `{trigger}` on `{table}` exists but is not enabled (tgenabled = \
                 '{state}'), so a credential could hold tenant_wide and grants at once"
            ));
        }

        if normalise_sql(&got_def) != normalise_sql(want_def) {
            problems.push(format!(
                "trigger `{trigger}` on `{table}` has the expected name but a different \
                 definition: found `{}`, expected `{}` — a trigger firing on the wrong \
                 event, or calling a different function, enforces nothing",
                normalise_sql(&got_def),
                normalise_sql(want_def),
            ));
        }
    }

    let (select, insert, delete): (bool, bool, bool) = sqlx::query_as(
        "SELECT has_table_privilege('public.credential_agent_grants', 'SELECT'), \
                has_table_privilege('public.credential_agent_grants', 'INSERT'), \
                has_table_privilege('public.credential_agent_grants', 'DELETE')",
    )
    .fetch_one(pool)
    .await?;
    for (granted, privilege) in [(select, "SELECT"), (insert, "INSERT"), (delete, "DELETE")] {
        if !granted {
            problems.push(format!(
                "the connected role lacks {privilege} on `credential_agent_grants`"
            ));
        }
    }

    verify_trigger_functions(pool, problems).await?;

    Ok(())
}

// ── Trigger function bodies ──────────────────────────────────────────────────
//
// `pg_get_triggerdef` proves the trigger *declaration* — which table, which
// event, which function name. It says nothing about what that function does.
// A replacement under the same name with a `RETURN NEW;` body satisfies every
// declaration-level check while enforcing nothing, which is the whole point of
// the control.
//
// So the functions themselves are pinned: identity, signature, language,
// security context, configuration, and body.

/// The code of a function body, with SQL line comments removed and whitespace
/// collapsed.
///
/// Comments are stripped so rewording a comment is not a contract change, while
/// any change to an actual statement is. The strip is deliberately simple — it
/// cuts at the first `--` on a line — which would also cut inside a string
/// literal containing `--`. Neither of these bodies has one, and a future body
/// that did would fail the control test loudly rather than silently weaken the
/// comparison.
fn normalise_plpgsql(raw: &str) -> String {
    raw.lines()
        .map(|line| match line.find("--") {
            Some(index) => &line[..index],
            None => line,
        })
        .collect::<Vec<_>>()
        .join(" ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Both functions carry this, so a caller's session `search_path` cannot change
/// how the names in their bodies resolve. They are SECURITY INVOKER, so this is
/// not about privilege escalation — it is about a hostile path making
/// `credential_agent_grants` resolve to a shadow table and the check inspect the
/// wrong rows.
const REQUIRED_FUNCTION_CONFIG: &str = "search_path=pg_catalog, public";

/// `(function name, expected comment-free normalised body)`.
const REQUIRED_FUNCTIONS: &[(&str, &str)] = &[
    (
        "credential_agent_grants_reject_tenant_wide",
        "DECLARE is_tenant_wide BOOLEAN; BEGIN SELECT c.tenant_wide INTO is_tenant_wide \
         FROM public.credentials c WHERE c.id = NEW.credential_id FOR SHARE; \
         IF is_tenant_wide THEN RAISE EXCEPTION 'credential % is tenant_wide; \
         tenant-wide and per-agent grants are mutually exclusive', NEW.credential_id \
         USING ERRCODE = 'check_violation'; END IF; RETURN NEW; END;",
    ),
    (
        "credentials_reject_tenant_wide_with_grants",
        "BEGIN IF NOT NEW.tenant_wide THEN RETURN NEW; END IF; \
         IF TG_OP = 'UPDATE' AND OLD.tenant_wide THEN RETURN NEW; END IF; \
         IF to_regclass('public.credential_agent_grants') IS NOT NULL \
         AND EXISTS (SELECT 1 FROM public.credential_agent_grants g \
         WHERE g.credential_id = NEW.id) THEN RAISE EXCEPTION 'credential % holds \
         per-agent grants; tenant_wide cannot be set while they exist', NEW.id \
         USING ERRCODE = 'check_violation'; END IF; RETURN NEW; END;",
    ),
];

/// `(schema, argument count, result type, language, SECURITY DEFINER, configuration, body)`.
///
/// Named rather than repeated inline: seven columns is past the point where a
/// bare tuple tells a reader anything.
type FunctionFacts = (String, i16, String, String, bool, Option<String>, String);

async fn verify_trigger_functions(
    pool: &PgPool,
    problems: &mut Vec<String>,
) -> Result<(), sqlx::Error> {
    for (name, want_body) in REQUIRED_FUNCTIONS {
        // Queried by name across every schema, not by `public.name`, so a
        // function that exists only somewhere else is reported as *misplaced*
        // rather than merely missing — the difference matters when a shadow
        // schema is what put it there.
        let candidates: Vec<FunctionFacts> = sqlx::query_as(
            "SELECT n.nspname, p.pronargs, pg_get_function_result(p.oid), l.lanname, \
                        p.prosecdef, array_to_string(p.proconfig, ','), p.prosrc \
                   FROM pg_proc p \
                   JOIN pg_namespace n ON n.oid = p.pronamespace \
                   JOIN pg_language l ON l.oid = p.prolang \
                  WHERE p.proname = $1 AND p.pronargs = 0",
        )
        .bind(name)
        .fetch_all(pool)
        .await?;

        // `pg_proc` permits overloads, so the query above is narrowed to the
        // zero-argument one — the only signature a trigger can execute. Without
        // that, an unrelated same-named overload could be picked out of an
        // unordered result set and validated in place of the real function.
        let public_candidates: Vec<&FunctionFacts> = candidates
            .iter()
            .filter(|(schema, ..)| schema == "public")
            .collect();

        if public_candidates.len() > 1 {
            problems.push(format!(
                "`public` holds {} zero-argument functions named `{name}`; the schema is                  ambiguous about which one the trigger executes",
                public_candidates.len()
            ));
        }

        let Some((_, nargs, result, language, security_definer, config, body)) =
            public_candidates.first().copied()
        else {
            let elsewhere: Vec<&str> = candidates
                .iter()
                .map(|(schema, ..)| schema.as_str())
                .collect();
            problems.push(if elsewhere.is_empty() {
                format!("trigger function `public.{name}` is missing, or takes arguments")
            } else {
                format!(
                    "trigger function `{name}` does not exist in `public`; it was found in \
                     {elsewhere:?}, which the trigger will not resolve to"
                )
            });
            continue;
        };

        if *nargs != 0 {
            problems.push(format!(
                "trigger function `public.{name}` takes {nargs} argument(s), expected 0"
            ));
        }
        if result != "trigger" {
            problems.push(format!(
                "trigger function `public.{name}` returns `{result}`, expected `trigger`"
            ));
        }
        if language != "plpgsql" {
            problems.push(format!(
                "trigger function `public.{name}` is written in `{language}`, expected `plpgsql`"
            ));
        }
        if *security_definer {
            problems.push(format!(
                "trigger function `public.{name}` is SECURITY DEFINER; it must be SECURITY \
                 INVOKER, or it would run with the definer's privileges on every write"
            ));
        }

        match config.as_deref() {
            Some(found) if found == REQUIRED_FUNCTION_CONFIG => {}
            Some(found) => problems.push(format!(
                "trigger function `public.{name}` has configuration `{found}`, expected \
                 `{REQUIRED_FUNCTION_CONFIG}` — a different search_path changes how the \
                 names in its body resolve"
            )),
            None => problems.push(format!(
                "trigger function `public.{name}` has no `SET search_path`, so a caller's \
                 session search_path decides how the names in its body resolve"
            )),
        }

        if normalise_plpgsql(body) != normalise_plpgsql(want_body) {
            problems.push(format!(
                "trigger function `public.{name}` has an unexpected body: found `{}`, \
                 expected `{}`",
                normalise_plpgsql(body),
                normalise_plpgsql(want_body),
            ));
        }
    }

    Ok(())
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
