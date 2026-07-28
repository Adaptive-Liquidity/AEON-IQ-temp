//! The read-only tenancy mismatch scanner.
//!
//! Step 4A's diagnostic gate. It answers one question per row — *which tenant
//! authoritatively owns this?* — and reports every row it cannot answer for.
//! It changes nothing: no `INSERT`, `UPDATE`, `DELETE`, `MERGE`, DDL, ledger
//! write or repair. That is enforced by PostgreSQL, not by convention: the
//! whole run happens inside [`AUDIT_TRANSACTION`].
//!
//! `READ ONLY` is what makes a stray write fail rather than succeed quietly.
//! `REPEATABLE READ` is what makes the report *coherent*: under the default
//! `READ COMMITTED` every statement takes a fresh snapshot, so a parent could
//! be read before a concurrent commit and its child after it, and the audit
//! would report a cross-tenant link that never existed. One snapshot for one
//! report.
//!
//! ## The identifier boundary
//!
//! The scanner builds its queries from [`inventory::REGISTRY`] rather than
//! carrying twenty-two hand-written ownership queries, which would be
//! twenty-two places for the rule to drift from the registry documenting it.
//!
//! sqlx 0.9 accepts only `&'static str` directly; a runtime-built string must
//! be wrapped in [`AssertSqlSafe`], which is a deliberate speed bump. Rather
//! than assert safety by argument, this module enforces it:
//!
//! * every interpolated identifier comes from the registry's `&'static str`
//!   literals — never from row content;
//! * each one is validated against `[a-z_][a-z0-9_]*` by [`SqlIdentifier`];
//! * each one is emitted quoted, so it can only ever be read as an identifier;
//! * a value that fails validation is rejected **before** any query string is
//!   built, and surfaces as [`AuditError::UnsafeIdentifier`].
//!
//! There is **no** exception. Row-identity columns are declared in the registry
//! too, so the live catalog is only ever an oracle: it says whether the
//! declared objects exist, carry the declared type and form the primary key. A
//! disagreement is `SCHEMA_RELATIONSHIP_DRIFT` and that table's scanner query
//! is not constructed at all. No name returned by the catalog is interpolated
//! into SQL, validated and quoted or otherwise.

use std::collections::BTreeMap;

use serde::Serialize;
use sqlx::{postgres::PgRow, AssertSqlSafe, PgPool, Row};

use super::inventory::{
    self, DiscoveredSchema, IdentityKind, OwnershipPath, PathKind, RowIdentity, TableClass,
    TableEntry, Tranche,
};
use super::report::{TenancyAuditReport, TrancheReadiness};

/// The transaction the audit runs in, shared with the test that proves a write
/// inside it is rejected — so that proof cannot drift onto a different
/// transaction than the one the audit actually opens.
pub const AUDIT_TRANSACTION: &str = "BEGIN TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY";

/// Domain separator for row pseudonyms.
///
/// Versioned because changing it changes every pseudonym: two reports may only
/// be compared row-for-row when their domain strings match.
pub const ROW_ID_DOMAIN: &str = "aeon-tenancy-audit-row-id-v1";

/// The report's schema version. Bumped whenever the serialized shape changes,
/// so a consumer can refuse a report it does not understand instead of
/// misreading one.
pub const REPORT_SCHEMA_VERSION: &str = "step4a.1";

// ── Errors ──────────────────────────────────────────────────────────────────

/// An identifier that failed validation. Reaching this is a bug in the registry
/// rather than anything a caller can cause, which is why it aborts the audit
/// instead of being reported as a finding: a report built from a query this
/// module refused to construct would be a report about nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnsafeIdentifier {
    pub raw: String,
    pub reason: &'static str,
}

impl std::fmt::Display for UnsafeIdentifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "identifier {:?} is not a safe SQL identifier: {}",
            self.raw, self.reason
        )
    }
}

impl std::error::Error for UnsafeIdentifier {}

#[derive(Debug)]
pub enum AuditError {
    Sql(sqlx::Error),
    UnsafeIdentifier(UnsafeIdentifier),
}

impl std::fmt::Display for AuditError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Sql(e) => write!(f, "{e}"),
            Self::UnsafeIdentifier(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for AuditError {}

impl From<sqlx::Error> for AuditError {
    fn from(value: sqlx::Error) -> Self {
        Self::Sql(value)
    }
}

impl From<UnsafeIdentifier> for AuditError {
    fn from(value: UnsafeIdentifier) -> Self {
        Self::UnsafeIdentifier(value)
    }
}

// ── The identifier boundary ─────────────────────────────────────────────────

/// A validated, quoted PostgreSQL identifier.
///
/// The only way to obtain one is [`SqlIdentifier::new`], so a name that never
/// passed validation cannot reach a query string by construction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqlIdentifier(String);

impl SqlIdentifier {
    /// Validate against `[a-z_][a-z0-9_]*` and quote.
    ///
    /// Deliberately narrower than PostgreSQL allows: every identifier in this
    /// schema is lowercase snake_case, so anything else is a mistake worth
    /// failing on rather than accommodating. Because the pattern admits no `"`,
    /// quoting is a wrap with nothing to escape — but the quotes are still
    /// emitted, so a future keyword collision cannot turn a column name into
    /// syntax.
    pub fn new(raw: &str) -> Result<Self, UnsafeIdentifier> {
        let rejected = |reason| UnsafeIdentifier {
            raw: raw.to_string(),
            reason,
        };

        let mut chars = raw.chars();
        let Some(first) = chars.next() else {
            return Err(rejected("identifier is empty"));
        };
        if !(first.is_ascii_lowercase() || first == '_') {
            return Err(rejected(
                "must begin with a lowercase ASCII letter or underscore",
            ));
        }
        for c in chars {
            if !(c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_') {
                return Err(rejected(
                    "may contain only lowercase ASCII letters, digits and underscores",
                ));
            }
        }
        if raw.len() > 63 {
            return Err(rejected("exceeds PostgreSQL's 63-byte identifier limit"));
        }
        Ok(Self(format!("\"{raw}\"")))
    }

    /// The quoted form, ready to interpolate.
    pub fn as_sql(&self) -> &str {
        &self.0
    }

    /// `t."column"`, the form used throughout the generated SQL.
    fn on(&self, alias: &str) -> String {
        format!("{alias}.{}", self.0)
    }
}

/// A SQL string literal.
///
/// Only ever given static values, but escaped anyway: a literal built by
/// concatenation is exactly where an unescaped quote would go unnoticed.
fn sql_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

/// `public."name"`, for a table the generator joins to.
fn parent(table: &str) -> Result<String, UnsafeIdentifier> {
    Ok(format!("public.{}", SqlIdentifier::new(table)?.as_sql()))
}

// ── Severity and reason codes ───────────────────────────────────────────────

/// Whether a finding stops work or merely informs it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Severity {
    /// Prevents the affected table or tranche from being migrated or activated.
    Blocking,
    /// Information that does not grant permission and never makes a blocking
    /// condition pass.
    Advisory,
}

impl Severity {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Blocking => "BLOCKING",
            Self::Advisory => "ADVISORY",
        }
    }
}

impl std::fmt::Display for Severity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The stable diagnostic contract.
///
/// These strings are consumed by Step 4B's readiness gates and by operators
/// reading the report, so they are part of the interface and their exact
/// serialized values are tested.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ReasonCode {
    // ── Inventory and contract ──
    UnclassifiedTable,
    InventoryTableMissing,
    MultipleClassifications,
    MissingCanonicalOwnershipPath,
    SchemaRelationshipDrift,
    // ── Row ownership ──
    LegacyUnmapped,
    OrphanedAgentReference,
    UnmappedAgent,
    OrphanedSessionReference,
    OrphanedMemoryReference,
    /// Part of the stable catalogue and currently **unemittable**: no table in
    /// the live schema carries a reference to `memory_versions.id`, so there is
    /// no memory-version ownership path to be orphaned. Its serialized value is
    /// unit-tested; there is deliberately no live emission test and no
    /// fabricated table to produce one.
    OrphanedVersionReference,
    UnresolvableOwner,
    NullOwnershipLink,
    OwnershipPathDisagreement,
    CrossTenantParentChild,
    // ── Identifier and future constraint ──
    MalformedLegacyIdentifier,
    AmbiguousLegacyIdentifier,
    FutureTenantUniquenessCollision,
    FutureCompositeFkMismatch,
    GlobalScopeUnverified,
}

impl ReasonCode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::UnclassifiedTable => "UNCLASSIFIED_TABLE",
            Self::InventoryTableMissing => "INVENTORY_TABLE_MISSING",
            Self::MultipleClassifications => "MULTIPLE_CLASSIFICATIONS",
            Self::MissingCanonicalOwnershipPath => "MISSING_CANONICAL_OWNERSHIP_PATH",
            Self::SchemaRelationshipDrift => "SCHEMA_RELATIONSHIP_DRIFT",
            Self::LegacyUnmapped => "LEGACY_UNMAPPED",
            Self::OrphanedAgentReference => "ORPHANED_AGENT_REFERENCE",
            Self::UnmappedAgent => "UNMAPPED_AGENT",
            Self::OrphanedSessionReference => "ORPHANED_SESSION_REFERENCE",
            Self::OrphanedMemoryReference => "ORPHANED_MEMORY_REFERENCE",
            Self::OrphanedVersionReference => "ORPHANED_VERSION_REFERENCE",
            Self::UnresolvableOwner => "UNRESOLVABLE_OWNER",
            Self::NullOwnershipLink => "NULL_OWNERSHIP_LINK",
            Self::OwnershipPathDisagreement => "OWNERSHIP_PATH_DISAGREEMENT",
            Self::CrossTenantParentChild => "CROSS_TENANT_PARENT_CHILD",
            Self::MalformedLegacyIdentifier => "MALFORMED_LEGACY_IDENTIFIER",
            Self::AmbiguousLegacyIdentifier => "AMBIGUOUS_LEGACY_IDENTIFIER",
            Self::FutureTenantUniquenessCollision => "FUTURE_TENANT_UNIQUENESS_COLLISION",
            Self::FutureCompositeFkMismatch => "FUTURE_COMPOSITE_FK_MISMATCH",
            Self::GlobalScopeUnverified => "GLOBAL_SCOPE_UNVERIFIED",
        }
    }

    /// Every code, so the catalogue can be rendered and its strings tested as a
    /// set rather than one at a time.
    pub const ALL: &'static [ReasonCode] = &[
        Self::UnclassifiedTable,
        Self::InventoryTableMissing,
        Self::MultipleClassifications,
        Self::MissingCanonicalOwnershipPath,
        Self::SchemaRelationshipDrift,
        Self::LegacyUnmapped,
        Self::OrphanedAgentReference,
        Self::UnmappedAgent,
        Self::OrphanedSessionReference,
        Self::OrphanedMemoryReference,
        Self::OrphanedVersionReference,
        Self::UnresolvableOwner,
        Self::NullOwnershipLink,
        Self::OwnershipPathDisagreement,
        Self::CrossTenantParentChild,
        Self::MalformedLegacyIdentifier,
        Self::AmbiguousLegacyIdentifier,
        Self::FutureTenantUniquenessCollision,
        Self::FutureCompositeFkMismatch,
        Self::GlobalScopeUnverified,
    ];

    /// Every code in this catalogue blocks except the one that describes a
    /// schema fact rather than an unsafe row.
    pub const fn severity(self) -> Severity {
        match self {
            // A blank legacy identifier is worth knowing about, but it is only
            // unsafe because it also fails to resolve — and the resolution
            // codes fire and block on their own for the same rows.
            Self::MalformedLegacyIdentifier => Severity::Advisory,
            _ => Severity::Blocking,
        }
    }

    /// One line explaining what the code means, for the operator report.
    pub const fn description(self) -> &'static str {
        match self {
            Self::UnclassifiedTable => {
                "a discovered application table has no registry entry, so no tenancy decision \
                 has been made for it"
            }
            Self::InventoryTableMissing => "the registry names a table absent from the live schema",
            Self::MultipleClassifications => "a table has more than one semantic inventory entry",
            Self::MissingCanonicalOwnershipPath => {
                "a non-SYSTEM_GLOBAL table lacks exactly one canonical tenant path"
            }
            Self::SchemaRelationshipDrift => {
                "a required column, foreign key, uniqueness rule or relationship no longer \
                 matches the ownership definition"
            }
            Self::LegacyUnmapped => {
                "the row cannot be assigned to a tenant safely from authoritative data"
            }
            Self::OrphanedAgentReference => "an agent reference resolves to no agent",
            Self::UnmappedAgent => "the owning agent exists but agents.tenant_id is NULL",
            Self::OrphanedSessionReference => "a required session reference resolves to no session",
            Self::OrphanedMemoryReference => "a required memory reference resolves to no memory",
            Self::OrphanedVersionReference => {
                "a required memory-version reference resolves to no version (reserved: no table \
                 in the current schema references memory_versions.id, so it cannot be emitted \
                 today)"
            }
            Self::UnresolvableOwner => {
                "the canonical ownership chain terminates without an authoritative owner"
            }
            Self::NullOwnershipLink => "a required link in the canonical ownership chain is NULL",
            Self::OwnershipPathDisagreement => {
                "canonical and secondary ownership paths resolve to different owners within one \
                 tenant"
            }
            Self::CrossTenantParentChild => {
                "two ownership paths for the same row resolve to different tenants"
            }
            Self::MalformedLegacyIdentifier => {
                "a legacy identifier cannot be interpreted according to its schema contract"
            }
            Self::AmbiguousLegacyIdentifier => {
                "one legacy identifier maps to more than one possible owner"
            }
            Self::FutureTenantUniquenessCollision => {
                "existing rows would violate a planned UNIQUE (tenant_id, ...) rule"
            }
            Self::FutureCompositeFkMismatch => {
                "existing rows would violate a planned composite tenant/owner foreign key"
            }
            Self::GlobalScopeUnverified => {
                "a table is classified SYSTEM_GLOBAL without evidence that global scope is safe"
            }
        }
    }
}

impl std::fmt::Display for ReasonCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One thing the audit found.
///
/// Aggregated per `(table, reason_code)` rather than one finding per row: a
/// legacy database can hold millions of unmapped rows, and a report that lists
/// them individually is not a report. `count` carries the scale and
/// `row_identifier` carries one example to start from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Finding {
    pub severity: Severity,
    pub reason_code: ReasonCode,
    pub table_name: String,
    /// One example row. A UUID or numeric key is emitted as-is, because it is
    /// already an opaque surrogate an operator needs in order to find the row.
    /// A key containing caller-controlled TEXT is replaced by a **pseudonym**:
    /// the lowercase hex of `SHA-256(ROW_ID_DOMAIN || table_name || raw)`.
    ///
    /// The pseudonym is a stable label, not a secret. It keeps caller strings
    /// out of an artifact that gets pasted into tickets and chat, and it is
    /// stable across runs so two reports can be compared. It is **not**
    /// encryption and provides no confidentiality against anyone who can
    /// enumerate candidates: the domain string and table name are published
    /// here, and the inputs are low-entropy values like `assistant`. Its
    /// purpose is to avoid gratuitously copying caller data, not to protect it.
    pub row_identifier: Option<String>,
    pub count: i64,
    pub diagnostic: String,
    /// The ownership path this finding is about, where one applies.
    pub ownership_path: Option<String>,
}

impl Finding {
    /// The total order the report is emitted in. Deterministic and independent
    /// of the order the checks happen to run in, so two runs over one snapshot
    /// are byte-identical.
    fn sort_key(&self) -> (&str, &'static str, &'static str, Option<&str>) {
        (
            self.table_name.as_str(),
            self.severity.as_str(),
            self.reason_code.as_str(),
            self.row_identifier.as_deref(),
        )
    }
}

/// A schema-level finding with no row behind it.
fn contract_finding(
    reason_code: ReasonCode,
    table: &str,
    diagnostic: impl Into<String>,
) -> Finding {
    Finding {
        severity: reason_code.severity(),
        reason_code,
        table_name: table.to_string(),
        row_identifier: None,
        count: 1,
        diagnostic: diagnostic.into(),
        ownership_path: None,
    }
}

// ── SQL generation ──────────────────────────────────────────────────────────

/// A path rendered into SQL: the joins it needs and the expressions that say
/// how it resolved.
#[derive(Debug)]
struct PathSql {
    joins: String,
    /// True when this path's starting reference is NULL.
    root_null: String,
    /// True when the reference is present but its parent does not exist.
    orphaned: String,
    /// The owning agent's UUID, or NULL if unresolved.
    agent_uuid: String,
    /// The owning tenant, or NULL if unresolved.
    tenant: String,
}

/// Render one ownership path as SQL against alias `t`.
///
/// `idx` disambiguates aliases so several paths can be joined into one query.
/// Aliases are generated here (`pa0`, `ps1`, …) and never taken from data.
fn path_sql(path: &OwnershipPath, idx: usize) -> Result<PathSql, UnsafeIdentifier> {
    let agents = parent("agents")?;
    Ok(match path.kind {
        PathKind::TenantColumn { column } => {
            let c = SqlIdentifier::new(column)?;
            PathSql {
                joins: String::new(),
                root_null: format!("{} IS NULL", c.on("t")),
                // A tenant column has no parent to be orphaned from; it either
                // has a value or it does not.
                orphaned: "FALSE".to_string(),
                agent_uuid: "NULL::uuid".to_string(),
                tenant: c.on("t"),
            }
        }
        PathKind::AgentText { column } => {
            let c = SqlIdentifier::new(column)?;
            let a = format!("pa{idx}");
            PathSql {
                joins: format!(
                    " LEFT JOIN {agents} {a} ON {a}.\"agent_id\" = {}",
                    c.on("t")
                ),
                root_null: format!("{} IS NULL", c.on("t")),
                orphaned: format!("{} IS NOT NULL AND {a}.\"id\" IS NULL", c.on("t")),
                agent_uuid: format!("{a}.\"id\""),
                tenant: format!("{a}.\"tenant_id\""),
            }
        }
        PathKind::AgentUuid { column } => {
            let c = SqlIdentifier::new(column)?;
            let a = format!("pa{idx}");
            PathSql {
                joins: format!(" LEFT JOIN {agents} {a} ON {a}.\"id\" = {}", c.on("t")),
                root_null: format!("{} IS NULL", c.on("t")),
                orphaned: format!("{} IS NOT NULL AND {a}.\"id\" IS NULL", c.on("t")),
                agent_uuid: format!("{a}.\"id\""),
                tenant: format!("{a}.\"tenant_id\""),
            }
        }
        PathKind::Session {
            agent_column,
            session_column,
        } => {
            let (ac, sc) = (
                SqlIdentifier::new(agent_column)?,
                SqlIdentifier::new(session_column)?,
            );
            let sessions = parent("sessions")?;
            let (s, a) = (format!("ps{idx}"), format!("pa{idx}"));
            PathSql {
                joins: format!(
                    " LEFT JOIN {sessions} {s} ON {s}.\"agent_id\" = {} \
                       AND {s}.\"session_id\" = {} \
                     LEFT JOIN {agents} {a} ON {a}.\"agent_id\" = {s}.\"agent_id\"",
                    ac.on("t"),
                    sc.on("t")
                ),
                root_null: format!("{} IS NULL", sc.on("t")),
                // The agent half being NULL is ambiguity, not absence — handled
                // separately — so it is excluded here.
                orphaned: format!(
                    "{} IS NOT NULL AND {} IS NOT NULL AND {s}.\"id\" IS NULL",
                    sc.on("t"),
                    ac.on("t")
                ),
                agent_uuid: format!("{a}.\"id\""),
                tenant: format!("{a}.\"tenant_id\""),
            }
        }
        PathKind::Memory { column } => derived_path(column, "memories", "pm", idx, &agents)?,
        PathKind::Entity { column } => derived_path(column, "entities", "pe", idx, &agents)?,
        PathKind::ArchivalBatch { column } => {
            derived_path(column, "archival_batches", "pb", idx, &agents)?
        }
        PathKind::Policy { column } => derived_path(column, "rmk_policies", "pp", idx, &agents)?,
    })
}

/// The shared shape of every path that reaches an agent through one
/// intermediate table keyed on `id` and carrying `agent_id`.
fn derived_path(
    column: &str,
    parent_table: &str,
    alias_prefix: &str,
    idx: usize,
    agents: &str,
) -> Result<PathSql, UnsafeIdentifier> {
    let c = SqlIdentifier::new(column)?;
    let p_table = parent(parent_table)?;
    let (p, a) = (format!("{alias_prefix}{idx}"), format!("pa{idx}"));
    Ok(PathSql {
        joins: format!(
            " LEFT JOIN {p_table} {p} ON {p}.\"id\" = {} \
              LEFT JOIN {agents} {a} ON {a}.\"agent_id\" = {p}.\"agent_id\"",
            c.on("t")
        ),
        root_null: format!("{} IS NULL", c.on("t")),
        orphaned: format!("{} IS NOT NULL AND {p}.\"id\" IS NULL", c.on("t")),
        agent_uuid: format!("{a}.\"id\""),
        tenant: format!("{a}.\"tenant_id\""),
    })
}

/// One length-prefixed field: `<byte-length>:<value>`.
///
/// Netstring-style, so the concatenation of several fields parses back to
/// exactly one tuple. Without it, `domain || table || raw` would be ambiguous:
/// `("ab", "c")` and `("a", "bc")` would share a preimage and therefore a
/// digest, which is the collision this framing exists to make impossible.
/// Lengths are in **bytes**, matching PostgreSQL's `octet_length`.
fn frame(part: &str) -> String {
    format!("{}:{}", part.len(), part)
}

/// The exact bytes hashed for a row pseudonym.
///
/// Mirrored verbatim by [`pseudonym_expression`]'s SQL, and a database test
/// compares the two digests rather than trusting that they agree.
/// Step 4A is a diagnostic surface with no production caller yet: the brief
/// forbids a route, a startup hook and any enforcement, so the tests and
/// Step 4B's migration tooling are its only consumers. Narrow allowance on
/// the entry point rather than the module, so anything that becomes
/// genuinely unreachable still shows up.
#[allow(dead_code)]
pub fn row_id_preimage(table: &str, raw: &str) -> String {
    format!("{}{}{}", frame(ROW_ID_DOMAIN), frame(table), frame(raw))
}

/// The Rust side of the pseudonym.
/// Step 4A is a diagnostic surface with no production caller yet: the brief
/// forbids a route, a startup hook and any enforcement, so the tests and
/// Step 4B's migration tooling are its only consumers. Narrow allowance on
/// the entry point rather than the module, so anything that becomes
/// genuinely unreachable still shows up.
#[allow(dead_code)]
pub fn row_pseudonym(table: &str, raw: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(row_id_preimage(table, raw).as_bytes());
    hex::encode(hasher.finalize())
}

/// A SQL expression producing one example identifier per row.
///
/// Built **only** from the registry's [`RowIdentity`]. Surrogate keys are
/// emitted as-is — they are already opaque and an operator needs them to find
/// the row. A key containing caller-supplied text is replaced by the pseudonym
/// defined in [`row_id_preimage`], whose framing this mirrors byte for byte:
/// the domain and table frames are constant and are emitted as one literal,
/// and only the raw component needs a runtime `octet_length`.
fn pseudonym_expression(table: &str, identity: &RowIdentity) -> Result<String, UnsafeIdentifier> {
    if identity.columns.is_empty() {
        return Ok("NULL::text".to_string());
    }

    let mut parts = Vec::with_capacity(identity.columns.len());
    for column in identity.columns {
        parts.push(format!(
            "{}::text",
            SqlIdentifier::new(column.name)?.on("t")
        ));
    }
    let raw = format!("concat_ws('|', {})", parts.join(", "));

    if !identity.has_caller_text() {
        return Ok(raw);
    }
    let prefix = sql_literal(&format!("{}{}", frame(ROW_ID_DOMAIN), frame(table)));
    Ok(format!(
        "encode(sha256(convert_to({prefix} || octet_length({raw})::text || ':' || {raw},          'UTF8')), 'hex')"
    ))
}

/// Verify the registry's row-identity declaration against the live catalog.
///
/// The catalog is only ever an oracle here: it says whether the declared
/// columns exist, carry the declared type and form the primary key. It never
/// supplies a name. A disagreement is `SCHEMA_RELATIONSHIP_DRIFT` and the
/// caller must not build this table's scanner query — auditing rows through a
/// key the database does not actually have would produce findings about
/// nothing.
fn verify_row_identity(
    entry: &TableEntry,
    table: &inventory::DiscoveredTable,
    catalog_key: &[(String, String)],
    findings: &mut Vec<Finding>,
) -> bool {
    let mut ok = true;
    let mut drift = |diagnostic: String| {
        findings.push(contract_finding(
            ReasonCode::SchemaRelationshipDrift,
            entry.table,
            diagnostic,
        ));
    };

    for column in entry.row_identity.columns {
        match table.column(column.name) {
            None => {
                drift(format!(
                    "row identity names column `{}`, which does not exist",
                    column.name
                ));
                ok = false;
            }
            Some(live) if !live.data_type.contains(column.kind.expected_type()) => {
                drift(format!(
                    "row identity declares `{}` as {} ({}), but the live column is `{}`",
                    column.name,
                    column.kind.expected_type(),
                    match column.kind {
                        IdentityKind::Surrogate => "surrogate, emitted as-is",
                        IdentityKind::CallerText => "caller text, pseudonymised",
                    },
                    live.data_type
                ));
                ok = false;
            }
            Some(_) => {}
        }
    }

    // The declared identity must *be* the primary key, in order. Comparing
    // rather than adopting: the catalog's names are read here and never reach
    // a query string.
    let declared: Vec<&str> = entry.row_identity.columns.iter().map(|c| c.name).collect();
    let actual: Vec<&str> = catalog_key.iter().map(|(n, _)| n.as_str()).collect();
    if declared != actual {
        drift(format!(
            "row identity declares {declared:?} but the live primary key is {actual:?}"
        ));
        ok = false;
    }

    ok
}

/// Primary-key columns and their types, per table.
async fn primary_keys(
    conn: &mut sqlx::PgConnection,
    schema: &str,
) -> Result<BTreeMap<String, Vec<(String, String)>>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT c.relname::text AS table_name, a.attname::text AS column_name, \
                format_type(a.atttypid, a.atttypmod) AS data_type \
           FROM pg_index i \
           JOIN pg_class c ON c.oid = i.indrelid \
           JOIN pg_namespace n ON n.oid = c.relnamespace \
           JOIN LATERAL unnest(i.indkey) WITH ORDINALITY AS k(attnum, ord) ON TRUE \
           JOIN pg_attribute a ON a.attrelid = c.oid AND a.attnum = k.attnum \
          WHERE i.indisprimary AND n.nspname = $1 \
          ORDER BY c.relname, k.ord",
    )
    .bind(schema)
    .fetch_all(&mut *conn)
    .await?;

    let mut out: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
    for row in rows {
        let table: String = row.try_get("table_name")?;
        out.entry(table)
            .or_default()
            .push((row.try_get("column_name")?, row.try_get("data_type")?));
    }
    Ok(out)
}

// ── The audit ───────────────────────────────────────────────────────────────

/// Run the full audit and return the report.
///
/// `generated_at` is injected rather than read from the clock so that two runs
/// over one snapshot are byte-identical and the determinism tests do not have
/// to special-case a timestamp.
/// Step 4A is a diagnostic surface with no production caller yet: the brief
/// forbids a route, a startup hook and any enforcement, so the tests and
/// Step 4B's migration tooling are its only consumers. Narrow allowance on
/// the entry point rather than the module, so anything that becomes
/// genuinely unreachable still shows up.
#[allow(dead_code)]
pub async fn run(
    pool: &PgPool,
    generated_at: Option<String>,
) -> Result<TenancyAuditReport, AuditError> {
    let mut conn = pool.acquire().await?;

    sqlx::raw_sql(AUDIT_TRANSACTION).execute(&mut *conn).await?;
    let outcome = audit_within_snapshot(&mut conn, generated_at).await;

    // COMMIT and ROLLBACK are equivalent for a read-only transaction; COMMIT is
    // used so that ending the transaction is never mistaken for the audit
    // having needed to undo something.
    let _ = sqlx::raw_sql("COMMIT").execute(&mut *conn).await;
    outcome
}

async fn audit_within_snapshot(
    conn: &mut sqlx::PgConnection,
    generated_at: Option<String>,
) -> Result<TenancyAuditReport, AuditError> {
    let schema = inventory::APPLICATION_SCHEMA;
    let discovered = inventory::discover(&mut *conn, schema).await?;
    let keys = primary_keys(&mut *conn, schema).await?;

    let mut findings = Vec::new();
    check_inventory_contract(&discovered, &mut findings);

    for entry in inventory::REGISTRY {
        let Some(table) = discovered.table(entry.table) else {
            continue; // already reported as INVENTORY_TABLE_MISSING
        };
        // Any catalog disagreement — a missing column, a nullability change, a
        // primary key that is not what the registry declares — means the
        // scanner must not build this table's query. A query written against a
        // schema that no longer exists either errors or, worse, silently
        // reports about the wrong columns.
        let relationships_hold = check_schema_relationships(entry, table, &mut findings);

        let empty = Vec::new();
        let catalog_key = keys.get(entry.table).unwrap_or(&empty);
        let identity_holds = verify_row_identity(entry, table, catalog_key, &mut findings);

        if entry.class == TableClass::SystemGlobal {
            check_global_scope(entry, table, &mut findings);
        } else if identity_holds && relationships_hold {
            scan_rows(&mut *conn, entry, &mut findings).await?;
            scan_future_uniqueness(&mut *conn, entry, &mut findings).await?;
        }
        // A drifted identity means no scanner query is constructed at all for
        // this table. The drift finding already blocks its tranche, so the
        // absence of row findings cannot be mistaken for cleanliness.
    }

    findings.sort_by(|a, b| a.sort_key().cmp(&b.sort_key()));

    let blocking_count = findings
        .iter()
        .filter(|f| f.severity == Severity::Blocking)
        .count() as i64;
    let advisory_count = findings.len() as i64 - blocking_count;
    let tranche_readiness = assess_tranches(&findings);

    Ok(TenancyAuditReport {
        schema_version: REPORT_SCHEMA_VERSION,
        generated_at,
        inventory_digest: super::report::inventory_digest(),
        discovered_application_tables: discovered
            .application_tables
            .iter()
            .map(|t| t.name.clone())
            .collect(),
        excluded_objects: discovered.excluded.clone(),
        classified_tables: super::report::classified_tables(),
        blocking_count,
        advisory_count,
        findings,
        tranche_readiness,
    })
}

/// Registry against live schema. This is what forces a future migration adding
/// a table to make a tenancy decision.
fn check_inventory_contract(discovered: &DiscoveredSchema, findings: &mut Vec<Finding>) {
    for table in &discovered.application_tables {
        match inventory::entry_count(&table.name) {
            0 => findings.push(contract_finding(
                ReasonCode::UnclassifiedTable,
                &table.name,
                "table exists in the live schema but has no entry in the tenancy registry; add \
                 one with an explicit classification and canonical ownership path",
            )),
            1 => {}
            n => findings.push(contract_finding(
                ReasonCode::MultipleClassifications,
                &table.name,
                format!("{n} registry entries name this table; exactly one is required"),
            )),
        }
    }

    for entry in inventory::REGISTRY {
        if discovered.table(entry.table).is_none() {
            findings.push(contract_finding(
                ReasonCode::InventoryTableMissing,
                entry.table,
                "registry names a table that does not exist in the live schema",
            ));
        }
        if entry.class != TableClass::SystemGlobal && entry.canonical_path.is_none() {
            findings.push(contract_finding(
                ReasonCode::MissingCanonicalOwnershipPath,
                entry.table,
                format!(
                    "classified {} but declares no canonical ownership path",
                    entry.class
                ),
            ));
        }
    }
}

/// Every column an ownership path names must exist, and its nullability must
/// match what the registry claims.
fn check_schema_relationships(
    entry: &TableEntry,
    table: &inventory::DiscoveredTable,
    findings: &mut Vec<Finding>,
) -> bool {
    let mut ok = true;
    let mut check = |path: &OwnershipPath, role: &str| {
        let mut healthy = true;
        // Nullability is a property of the path's *root* column. A session path
        // also names an agent column, but that column's nullability belongs to
        // the AgentText path on the same table — checking it here against the
        // session's flag reported drift on every correct schema, which is how
        // this was found.
        let mut columns = vec![(path.kind.root_column(), true)];
        if let PathKind::Session { agent_column, .. } = path.kind {
            columns.push((agent_column, false));
        }
        for (column, check_nullability) in columns {
            match table.column(column) {
                None => {
                    healthy = false;
                    findings.push(contract_finding(
                        ReasonCode::SchemaRelationshipDrift,
                        entry.table,
                        format!(
                            "{role} ownership path names column `{column}`, which does not exist"
                        ),
                    ));
                }
                // The registry's `nullable` must match the live column, or the
                // scanner would decide whether a NULL is legal from a stale
                // belief about the schema.
                Some(live) if check_nullability && live.not_null == path.nullable => {
                    healthy = false;
                    findings.push(contract_finding(
                        ReasonCode::SchemaRelationshipDrift,
                        entry.table,
                        format!(
                        "{role} ownership path declares `{column}` {}, but the live column is {}",
                        if path.nullable {
                            "NULL-able"
                        } else {
                            "NOT NULL"
                        },
                        if live.not_null { "NOT NULL" } else { "NULL-able" }
                    ),
                    ))
                }
                Some(_) => {}
            }
        }
        healthy
    };

    if let Some(canonical) = &entry.canonical_path {
        ok &= check(canonical, "canonical");
    }
    for secondary in entry.secondary_paths {
        ok &= check(secondary, "secondary");
    }
    ok
}

/// A `SYSTEM_GLOBAL` table must justify itself.
fn check_global_scope(
    entry: &TableEntry,
    table: &inventory::DiscoveredTable,
    findings: &mut Vec<Finding>,
) {
    if entry.global_scope_evidence.is_none() {
        findings.push(contract_finding(
            ReasonCode::GlobalScopeUnverified,
            entry.table,
            "classified SYSTEM_GLOBAL without stated evidence that global scope is safe; a table \
             is not global because nobody has written down who owns it",
        ));
        return;
    }

    // Evidence is required *because* these tables often look tenant-shaped. If
    // one carries ownership-looking columns, the evidence must be the thing
    // that explains them, so their presence is surfaced as advisory context
    // rather than silently accepted.
    // Matched on shape rather than an exact list: `agent_tenancy_migrations`
    // carries `legacy_tenant_id`, which is precisely the kind of column the
    // evidence has to account for, and an exact-name check would have sailed
    // past it.
    let suspicious: Vec<&str> = table
        .columns
        .iter()
        .map(|c| c.name.as_str())
        .filter(|name| name.contains("tenant") || *name == "agent_id" || *name == "agent_uuid")
        .collect();
    if !suspicious.is_empty() {
        findings.push(Finding {
            severity: Severity::Advisory,
            reason_code: ReasonCode::GlobalScopeUnverified,
            table_name: entry.table.to_string(),
            row_identifier: None,
            count: 1,
            diagnostic: format!(
                "SYSTEM_GLOBAL but carries ownership-shaped column(s) {}; accepted on the stated \
                 evidence, which must be re-read if those columns change meaning",
                suspicious.join(", ")
            ),
            ownership_path: None,
        });
    }
}

/// The row-level scan for one table: one aggregate query, inside the snapshot.
async fn scan_rows(
    conn: &mut sqlx::PgConnection,
    entry: &TableEntry,
    findings: &mut Vec<Finding>,
) -> Result<(), AuditError> {
    let Some(canonical) = entry.canonical_path else {
        return Ok(());
    };

    // Everything below is built from validated identifiers; a rejection here
    // aborts before any query string exists.
    let table_sql = parent(entry.table)?;
    let ident = pseudonym_expression(entry.table, &entry.row_identity)?;
    let c = path_sql(&canonical, 0)?;

    let mut joins = c.joins.clone();
    let mut selects = vec![
        format!("({}) AS c_null", c.root_null),
        format!("({}) AS c_orphan", c.orphaned),
        format!("({}) AS c_agent", c.agent_uuid),
        format!("({}) AS c_tenant", c.tenant),
        format!("({ident}) AS ident"),
    ];

    // The ambiguity arm only exists for session paths, where the identifier is
    // unique only together with its agent.
    let ambiguous = match canonical.kind {
        PathKind::Session {
            agent_column,
            session_column,
        } => format!(
            "{} IS NOT NULL AND {} IS NULL",
            SqlIdentifier::new(session_column)?.on("t"),
            SqlIdentifier::new(agent_column)?.on("t")
        ),
        _ => "FALSE".to_string(),
    };
    selects.push(format!("({ambiguous}) AS c_ambiguous"));

    // A legacy TEXT identifier that is blank can never match `agents.agent_id`,
    // which is UNIQUE and NOT NULL. It is malformed rather than merely absent.
    let malformed = match canonical.kind {
        PathKind::AgentText { column } => {
            let c = SqlIdentifier::new(column)?;
            format!("{0} IS NOT NULL AND btrim({0}) = ''", c.on("t"))
        }
        _ => "FALSE".to_string(),
    };
    selects.push(format!("({malformed}) AS c_malformed"));

    for (i, secondary) in entry.secondary_paths.iter().enumerate() {
        let s = path_sql(secondary, i + 1)?;
        joins.push_str(&s.joins);
        selects.push(format!("({}) AS s{i}_orphan", s.orphaned));
        selects.push(format!("({}) AS s{i}_agent", s.agent_uuid));
        selects.push(format!("({}) AS s{i}_tenant", s.tenant));
    }

    let table = entry.table;
    let inner = format!("SELECT {} FROM {table_sql} t{joins}", selects.join(", "));

    // Aggregate rather than stream: a legacy database can hold millions of
    // unmapped rows, and the report wants a count and one example, not a list.
    let mut aggregates = vec![
        "count(*) FILTER (WHERE c_null) AS n_null".to_string(),
        "min(ident) FILTER (WHERE c_null) AS id_null".to_string(),
        "count(*) FILTER (WHERE c_orphan) AS n_orphan".to_string(),
        "min(ident) FILTER (WHERE c_orphan) AS id_orphan".to_string(),
        "count(*) FILTER (WHERE c_ambiguous) AS n_ambiguous".to_string(),
        "min(ident) FILTER (WHERE c_ambiguous) AS id_ambiguous".to_string(),
        "count(*) FILTER (WHERE c_malformed) AS n_malformed".to_string(),
        "min(ident) FILTER (WHERE c_malformed) AS id_malformed".to_string(),
        // Resolved to an agent whose tenant is NULL: the agent exists but step
        // 1 deliberately left it unreachable.
        "count(*) FILTER (WHERE c_agent IS NOT NULL AND c_tenant IS NULL) AS n_unmapped"
            .to_string(),
        "min(ident) FILTER (WHERE c_agent IS NOT NULL AND c_tenant IS NULL) AS id_unmapped"
            .to_string(),
        // Nothing NULL, nothing orphaned, and still no tenant: the chain ran
        // out without an authoritative owner.
        "count(*) FILTER (WHERE NOT c_null AND NOT c_orphan AND NOT c_ambiguous \
           AND c_agent IS NULL AND c_tenant IS NULL) AS n_unresolvable"
            .to_string(),
        "min(ident) FILTER (WHERE NOT c_null AND NOT c_orphan AND NOT c_ambiguous \
           AND c_agent IS NULL AND c_tenant IS NULL) AS id_unresolvable"
            .to_string(),
        // The union of everything that leaves a row unassignable.
        "count(*) FILTER (WHERE c_tenant IS NULL) AS n_legacy".to_string(),
        "min(ident) FILTER (WHERE c_tenant IS NULL) AS id_legacy".to_string(),
    ];

    for (i, _) in entry.secondary_paths.iter().enumerate() {
        aggregates.push(format!(
            "count(*) FILTER (WHERE s{i}_orphan) AS n_s{i}_orphan"
        ));
        aggregates.push(format!(
            "min(ident) FILTER (WHERE s{i}_orphan) AS id_s{i}_orphan"
        ));
        // Different tenants means a tenant boundary is crossed; the same tenant
        // with different agents is a structural disagreement that does not leak
        // across tenants. Distinct codes, non-overlapping by construction.
        aggregates.push(format!(
            "count(*) FILTER (WHERE c_tenant IS NOT NULL AND s{i}_tenant IS NOT NULL \
               AND c_tenant <> s{i}_tenant) AS n_s{i}_cross"
        ));
        aggregates.push(format!(
            "min(ident) FILTER (WHERE c_tenant IS NOT NULL AND s{i}_tenant IS NOT NULL \
               AND c_tenant <> s{i}_tenant) AS id_s{i}_cross"
        ));
        aggregates.push(format!(
            "count(*) FILTER (WHERE c_tenant IS NOT NULL AND s{i}_tenant IS NOT NULL \
               AND c_tenant = s{i}_tenant AND s{i}_agent IS NOT NULL \
               AND c_agent IS DISTINCT FROM s{i}_agent) AS n_s{i}_disagree"
        ));
        aggregates.push(format!(
            "min(ident) FILTER (WHERE c_tenant IS NOT NULL AND s{i}_tenant IS NOT NULL \
               AND c_tenant = s{i}_tenant AND s{i}_agent IS NOT NULL \
               AND c_agent IS DISTINCT FROM s{i}_agent) AS id_s{i}_disagree"
        ));
        // A planned composite FK to (id, tenant_id) cannot match a parent whose
        // tenant is NULL, however present the parent row itself is.
        aggregates.push(format!(
            "count(*) FILTER (WHERE NOT s{i}_orphan AND s{i}_agent IS NOT NULL \
               AND s{i}_tenant IS NULL) AS n_s{i}_fk"
        ));
        aggregates.push(format!(
            "min(ident) FILTER (WHERE NOT s{i}_orphan AND s{i}_agent IS NOT NULL \
               AND s{i}_tenant IS NULL) AS id_s{i}_fk"
        ));
    }

    let sql = format!("SELECT {} FROM ({inner}) q", aggregates.join(", "));
    let row = sqlx::query(AssertSqlSafe(sql))
        .fetch_one(&mut *conn)
        .await?;

    let path_label = canonical.label.to_string();
    let mut push = |code: ReasonCode, n: &str, id: &str, diagnostic: String, row: &PgRow| {
        let count: i64 = row.try_get(n).unwrap_or(0);
        if count > 0 {
            findings.push(Finding {
                severity: code.severity(),
                reason_code: code,
                table_name: table.to_string(),
                row_identifier: row.try_get(id).ok().flatten(),
                count,
                diagnostic,
                ownership_path: Some(path_label.clone()),
            });
        }
    };

    // A NULL in the canonical chain is a broken ownership link whether or not
    // the column is *allowed* to be NULL. The `nullable` flag describes what
    // the schema permits — it is checked against the catalog separately — and
    // says nothing about whether this row can be assigned a tenant. Gating the
    // finding on it made the code unreachable except on a drifted table, and a
    // drifted table is never scanned.
    push(
        ReasonCode::NullOwnershipLink,
        "n_null",
        "id_null",
        format!(
            "`{}` is NULL, so the canonical ownership path cannot start",
            canonical.kind.root_column()
        ),
        &row,
    );

    push(
        orphan_reason(&canonical),
        "n_orphan",
        "id_orphan",
        format!(
            "`{}` names a parent that does not exist",
            canonical.kind.root_column()
        ),
        &row,
    );
    push(
        ReasonCode::AmbiguousLegacyIdentifier,
        "n_ambiguous",
        "id_ambiguous",
        "a session reference is present without its agent; `sessions` is unique on \
         (agent_id, session_id) and not on session_id alone, so this does not identify one session"
            .to_string(),
        &row,
    );
    push(
        ReasonCode::MalformedLegacyIdentifier,
        "n_malformed",
        "id_malformed",
        "legacy agent identifier is blank, which cannot match agents.agent_id".to_string(),
        &row,
    );
    push(
        ReasonCode::UnmappedAgent,
        "n_unmapped",
        "id_unmapped",
        "the owning agent exists but its tenant_id is NULL, so no tenant predicate can ever \
         match it"
            .to_string(),
        &row,
    );
    push(
        ReasonCode::UnresolvableOwner,
        "n_unresolvable",
        "id_unresolvable",
        "the canonical chain completed without reaching an authoritative owner".to_string(),
        &row,
    );
    push(
        ReasonCode::LegacyUnmapped,
        "n_legacy",
        "id_legacy",
        "no tenant can be assigned to these rows from authoritative data".to_string(),
        &row,
    );

    for (i, secondary) in entry.secondary_paths.iter().enumerate() {
        let label = secondary.label.to_string();
        let mut push_secondary = |code: ReasonCode, n: String, id: String, diagnostic: String| {
            let count: i64 = row.try_get(n.as_str()).unwrap_or(0);
            if count > 0 {
                findings.push(Finding {
                    severity: code.severity(),
                    reason_code: code,
                    table_name: table.to_string(),
                    row_identifier: row.try_get(id.as_str()).ok().flatten(),
                    count,
                    diagnostic,
                    ownership_path: Some(label.clone()),
                });
            }
        };
        push_secondary(
            orphan_reason(secondary),
            format!("n_s{i}_orphan"),
            format!("id_s{i}_orphan"),
            "a secondary ownership reference names a parent that does not exist".to_string(),
        );
        push_secondary(
            ReasonCode::CrossTenantParentChild,
            format!("n_s{i}_cross"),
            format!("id_s{i}_cross"),
            "the canonical and secondary paths resolve to different tenants; neither is \
             preferred and the row is not assignable"
                .to_string(),
        );
        push_secondary(
            ReasonCode::OwnershipPathDisagreement,
            format!("n_s{i}_disagree"),
            format!("id_s{i}_disagree"),
            "the canonical and secondary paths agree on the tenant but name different agents"
                .to_string(),
        );
        push_secondary(
            ReasonCode::FutureCompositeFkMismatch,
            format!("n_s{i}_fk"),
            format!("id_s{i}_fk"),
            "the parent exists but its tenant is NULL, so a composite FK to (id, tenant_id) \
             could never match it"
                .to_string(),
        );
    }

    Ok(())
}

/// Which orphan code a path's missing parent produces.
fn orphan_reason(path: &OwnershipPath) -> ReasonCode {
    match path.kind {
        PathKind::AgentText { .. } | PathKind::AgentUuid { .. } => {
            ReasonCode::OrphanedAgentReference
        }
        PathKind::Session { .. } => ReasonCode::OrphanedSessionReference,
        PathKind::Memory { .. } => ReasonCode::OrphanedMemoryReference,
        // `entities`, `archival_batches`, `rmk_policies` and a bare tenant
        // column have no dedicated orphan code in the catalogue, so the generic
        // one is used rather than inventing a code per parent table.
        PathKind::Entity { .. }
        | PathKind::ArchivalBatch { .. }
        | PathKind::Policy { .. }
        | PathKind::TenantColumn { .. } => ReasonCode::UnresolvableOwner,
    }
}

/// Would a planned `UNIQUE (tenant_id, ...)` collide on today's rows?
async fn scan_future_uniqueness(
    conn: &mut sqlx::PgConnection,
    entry: &TableEntry,
    findings: &mut Vec<Finding>,
) -> Result<(), AuditError> {
    let (Some(columns), Some(canonical)) = (entry.plan.future_unique_columns, entry.canonical_path)
    else {
        return Ok(());
    };

    let table_sql = parent(entry.table)?;
    let c = path_sql(&canonical, 0)?;
    let mut cols = Vec::with_capacity(columns.len());
    for column in columns {
        cols.push(SqlIdentifier::new(column)?.on("t"));
    }
    let group_by: Vec<String> = (1..=cols.len() + 1).map(|i| i.to_string()).collect();
    let sql = format!(
        "SELECT count(*)::bigint AS n FROM (SELECT ({}) AS tenant, {} FROM {table_sql} t{} \
          GROUP BY {} HAVING count(*) > 1) g",
        c.tenant,
        cols.join(", "),
        c.joins,
        group_by.join(", "),
    );

    let count: i64 = sqlx::query(AssertSqlSafe(sql))
        .fetch_one(&mut *conn)
        .await?
        .try_get("n")?;

    if count > 0 {
        findings.push(Finding {
            severity: ReasonCode::FutureTenantUniquenessCollision.severity(),
            reason_code: ReasonCode::FutureTenantUniquenessCollision,
            table_name: entry.table.to_string(),
            row_identifier: None,
            count,
            diagnostic: format!(
                "{count} group(s) would violate a future UNIQUE (tenant_id, {}); the index build \
                 would fail rather than the collision being discovered afterwards",
                columns.join(", ")
            ),
            ownership_path: Some(canonical.label.to_string()),
        });
    }
    Ok(())
}

/// A tranche is ready only when every blocking code its tables require is
/// absent. Advisory findings are never permission.
fn assess_tranches(findings: &[Finding]) -> Vec<TrancheReadiness> {
    Tranche::ALL
        .iter()
        .map(|tranche| {
            let tables: Vec<&TableEntry> = inventory::REGISTRY
                .iter()
                .filter(|e| e.tranche == *tranche)
                .collect();

            let mut blocking: Vec<String> = Vec::new();
            for entry in &tables {
                for finding in findings
                    .iter()
                    .filter(|f| f.table_name == entry.table && f.severity == Severity::Blocking)
                {
                    // Contract findings block regardless of the per-table list:
                    // a table with no classification has no list to consult.
                    let is_contract = matches!(
                        finding.reason_code,
                        ReasonCode::UnclassifiedTable
                            | ReasonCode::InventoryTableMissing
                            | ReasonCode::MultipleClassifications
                            | ReasonCode::MissingCanonicalOwnershipPath
                            | ReasonCode::SchemaRelationshipDrift
                    );
                    if is_contract
                        || entry
                            .plan
                            .required_zero_codes
                            .contains(&finding.reason_code.as_str())
                    {
                        blocking.push(format!("{}: {}", entry.table, finding.reason_code));
                    }
                }
            }
            blocking.sort();
            blocking.dedup();

            TrancheReadiness {
                tranche: *tranche,
                tables: tables.iter().map(|e| e.table.to_string()).collect(),
                ready: blocking.is_empty(),
                blocking_reasons: blocking,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_reason_code_serializes_to_its_contract_string() {
        // These strings are the diagnostic contract: Step 4B's readiness gates
        // and operators both key on them, so a rename is a breaking change and
        // has to fail here rather than in a consumer.
        let expected = [
            "UNCLASSIFIED_TABLE",
            "INVENTORY_TABLE_MISSING",
            "MULTIPLE_CLASSIFICATIONS",
            "MISSING_CANONICAL_OWNERSHIP_PATH",
            "SCHEMA_RELATIONSHIP_DRIFT",
            "LEGACY_UNMAPPED",
            "ORPHANED_AGENT_REFERENCE",
            "UNMAPPED_AGENT",
            "ORPHANED_SESSION_REFERENCE",
            "ORPHANED_MEMORY_REFERENCE",
            "ORPHANED_VERSION_REFERENCE",
            "UNRESOLVABLE_OWNER",
            "NULL_OWNERSHIP_LINK",
            "OWNERSHIP_PATH_DISAGREEMENT",
            "CROSS_TENANT_PARENT_CHILD",
            "MALFORMED_LEGACY_IDENTIFIER",
            "AMBIGUOUS_LEGACY_IDENTIFIER",
            "FUTURE_TENANT_UNIQUENESS_COLLISION",
            "FUTURE_COMPOSITE_FK_MISMATCH",
            "GLOBAL_SCOPE_UNVERIFIED",
        ];
        let actual: Vec<&str> = ReasonCode::ALL.iter().map(|c| c.as_str()).collect();
        assert_eq!(actual, expected);

        // …and the serde form must match `as_str`, or the machine report and
        // the operator report would disagree about the same finding.
        for code in ReasonCode::ALL {
            let json = serde_json::to_string(code).unwrap();
            assert_eq!(json, format!("\"{}\"", code.as_str()));
        }
    }

    #[test]
    fn the_reserved_version_code_keeps_its_serialized_value() {
        // ORPHANED_VERSION_REFERENCE is part of the stable catalogue and cannot
        // currently be emitted: no table in the live schema references
        // `memory_versions.id`, so there is no memory-version ownership path to
        // be orphaned. Its value is pinned here rather than demonstrated
        // against a fabricated table, because a table invented to make a test
        // pass would prove nothing about this schema.
        assert_eq!(
            ReasonCode::OrphanedVersionReference.as_str(),
            "ORPHANED_VERSION_REFERENCE"
        );
        assert_eq!(
            serde_json::to_string(&ReasonCode::OrphanedVersionReference).unwrap(),
            "\"ORPHANED_VERSION_REFERENCE\""
        );
        assert_eq!(
            ReasonCode::OrphanedVersionReference.severity(),
            Severity::Blocking
        );
        assert!(ReasonCode::OrphanedVersionReference
            .description()
            .contains("reserved"));

        // The claim is checked rather than asserted: `memory_versions` is a
        // parent of nothing, so no path can resolve through it.
        let referenced = inventory::REGISTRY.iter().any(|e| {
            e.canonical_path
                .iter()
                .chain(e.secondary_paths.iter())
                .any(|p| p.label.contains("memory_versions.id"))
        });
        assert!(
            !referenced,
            "a memory-version ownership path now exists; give this code a live emission test"
        );
    }

    #[test]
    fn every_reason_code_carries_a_description() {
        for code in ReasonCode::ALL {
            assert!(!code.description().is_empty(), "{code} has no description");
        }
    }

    #[test]
    fn only_the_documented_code_is_advisory() {
        // Advisory findings must never be mistaken for permission, so the set
        // of them is small, deliberate and pinned.
        let advisory: Vec<&str> = ReasonCode::ALL
            .iter()
            .filter(|c| c.severity() == Severity::Advisory)
            .map(|c| c.as_str())
            .collect();
        assert_eq!(advisory, vec!["MALFORMED_LEGACY_IDENTIFIER"]);
    }

    #[test]
    fn an_unsafe_identifier_is_rejected_before_any_query_is_built() {
        // The boundary itself. Each of these would be a registry or catalog
        // mistake, and each is refused before it can reach a query string.
        for (raw, why) in [
            ("agent_id; DROP TABLE agents", "statement injection"),
            ("agent_id\"", "quote injection"),
            ("Agent_Id", "uppercase"),
            ("1_leading_digit", "leading digit"),
            ("", "empty"),
            ("agent id", "embedded space"),
            ("tenant-id", "hyphen"),
        ] {
            assert!(
                SqlIdentifier::new(raw).is_err(),
                "{why}: {raw:?} must be rejected"
            );
        }

        // …and a legitimate identifier is accepted and emitted quoted, so it
        // can only ever be read as an identifier.
        let ok = SqlIdentifier::new("agent_uuid").expect("valid identifier");
        assert_eq!(ok.as_sql(), "\"agent_uuid\"");
        assert_eq!(ok.on("t"), "t.\"agent_uuid\"");
    }

    #[test]
    fn a_path_naming_an_unsafe_column_fails_before_query_construction() {
        // The rejection has to happen at the layer that builds SQL, not only in
        // the validator, or a future call site could bypass it.
        let hostile = OwnershipPath {
            label: "hostile",
            kind: PathKind::AgentText {
                column: "agent_id\"; DROP TABLE agents; --",
            },
            nullable: false,
        };
        let error = path_sql(&hostile, 0).expect_err("must refuse to build SQL");
        assert!(error.raw.contains("DROP TABLE"), "{error}");

        // The same for a table name.
        assert!(parent("agents; DROP TABLE agents").is_err());

        // And for a row-identity column, which the registry declares like
        // every other identifier.
        let bad_identity = RowIdentity {
            columns: &[inventory::IdentityColumn {
                name: "bad name",
                kind: IdentityKind::CallerText,
            }],
        };
        assert!(pseudonym_expression("memories", &bad_identity).is_err());
    }

    #[test]
    fn a_text_key_is_pseudonymised_and_a_uuid_key_is_not() {
        // `amp_controller_state`'s primary key *is* the caller-supplied
        // `agent_id`; copying it into a diagnostic artifact is gratuitous, so
        // it is replaced by a domain-separated digest.
        let entry = inventory::entry("amp_controller_state").unwrap();
        let text = pseudonym_expression(entry.table, &entry.row_identity).unwrap();
        assert!(text.contains("sha256("), "{text}");
        assert!(text.contains(ROW_ID_DOMAIN), "{text}");
        assert!(
            text.contains("amp_controller_state"),
            "the table must take part in the framing, so one identifier cannot produce the same \
             pseudonym in two tables: {text}"
        );
        assert!(
            text.contains("octet_length"),
            "the raw component must be length-prefixed in SQL exactly as in Rust: {text}"
        );

        // A UUID surrogate is already opaque and stays readable, so an operator
        // can actually find the row.
        let memories = inventory::entry("memories").unwrap();
        let uuid = pseudonym_expression(memories.table, &memories.row_identity).unwrap();
        assert!(!uuid.contains("sha256("), "{uuid}");
        assert!(uuid.contains("::text"), "{uuid}");
    }

    #[test]
    fn a_literal_with_a_quote_is_escaped() {
        // Only static values reach this today; escaping is what keeps that from
        // becoming load-bearing.
        assert_eq!(sql_literal("plain"), "'plain'");
        assert_eq!(sql_literal("it's"), "'it''s'");
    }

    #[test]
    fn every_registry_identifier_passes_the_boundary() {
        // The generator is only safe if the registry it reads from is. Proven
        // over the whole registry rather than sampled.
        for entry in inventory::REGISTRY {
            assert!(
                SqlIdentifier::new(entry.table).is_ok(),
                "table {}",
                entry.table
            );
            for path in entry.canonical_path.iter().chain(entry.secondary_paths) {
                assert!(
                    SqlIdentifier::new(path.kind.root_column()).is_ok(),
                    "{}: column {}",
                    entry.table,
                    path.kind.root_column()
                );
                if let PathKind::Session { agent_column, .. } = path.kind {
                    assert!(SqlIdentifier::new(agent_column).is_ok());
                }
                assert!(path_sql(path, 0).is_ok(), "{}", entry.table);
            }
        }
    }

    #[test]
    fn generated_sql_contains_no_write_verb() {
        // A structural guard on the generator. PostgreSQL enforces read-only at
        // run time; this catches a generator change that would have to be
        // rejected there.
        for entry in inventory::REGISTRY {
            let Some(canonical) = entry.canonical_path else {
                continue;
            };
            let sql = path_sql(&canonical, 0).unwrap();
            let all = format!(
                "{} {} {} {} {}",
                sql.joins, sql.root_null, sql.orphaned, sql.agent_uuid, sql.tenant
            )
            .to_uppercase();
            for verb in [
                "INSERT ",
                "UPDATE ",
                "DELETE ",
                "MERGE ",
                "ALTER ",
                "CREATE ",
                "DROP ",
                "TRUNCATE ",
            ] {
                assert!(!all.contains(verb), "{}: generated `{verb}`", entry.table);
            }
        }
    }

    #[test]
    fn the_audit_transaction_is_read_only_and_repeatable_read() {
        // Both halves matter and both are easy to lose in an edit: READ ONLY
        // makes a stray write fail, REPEATABLE READ makes the report one
        // coherent snapshot.
        assert!(AUDIT_TRANSACTION.contains("READ ONLY"));
        assert!(AUDIT_TRANSACTION.contains("REPEATABLE READ"));
    }
    #[test]
    fn framing_makes_component_boundaries_unambiguous() {
        // The collision this framing exists to prevent: without length
        // prefixes, ("ab","c") and ("a","bc") concatenate identically and so
        // hash identically, letting one table's row impersonate another's.
        assert_ne!(row_id_preimage("ab", "c"), row_id_preimage("a", "bc"));
        assert_ne!(row_pseudonym("ab", "c"), row_pseudonym("a", "bc"));

        // A separator alone would not be enough, because the separator can
        // occur inside a value.
        assert_ne!(row_id_preimage("a|b", "c"), row_id_preimage("a", "b|c"));
        assert_ne!(row_pseudonym("a|b", "c"), row_pseudonym("a", "b|c"));

        // Empty components stay distinguishable from absent ones.
        assert_ne!(row_id_preimage("", "ab"), row_id_preimage("ab", ""));

        // The frame is exactly `<bytes>:<value>`, counted in bytes rather than
        // characters so it matches PostgreSQL's octet_length.
        assert_eq!(frame("abc"), "3:abc");
        assert_eq!(frame("\u{e9}"), "2:\u{e9}");
        assert_eq!(
            row_id_preimage("t", "r"),
            format!("{}:{}1:t1:r", ROW_ID_DOMAIN.len(), ROW_ID_DOMAIN)
        );

        // Same inputs, same digest: the pseudonym is a stable label.
        assert_eq!(
            row_pseudonym("memories", "x"),
            row_pseudonym("memories", "x")
        );
        let digest = row_pseudonym("memories", "x");
        assert_eq!(digest.len(), 64);
        assert!(digest
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    }

    #[test]
    fn every_generated_identifier_traces_to_the_registry() {
        // The whole-generator form of the boundary: collect every quoted
        // identifier the scanner would emit and require each to be a name the
        // registry declares, or one of the fixed join keys this module writes
        // itself.
        let fixed = ["agent_id", "id", "tenant_id", "session_id"];
        let joined = [
            "agents",
            "sessions",
            "memories",
            "entities",
            "archival_batches",
            "rmk_policies",
        ];
        for entry in inventory::REGISTRY {
            let mut declared: Vec<&str> = vec![entry.table];
            declared.extend(entry.row_identity.columns.iter().map(|c| c.name));
            for path in entry.canonical_path.iter().chain(entry.secondary_paths) {
                declared.push(path.kind.root_column());
                if let PathKind::Session { agent_column, .. } = path.kind {
                    declared.push(agent_column);
                }
            }
            declared.extend(joined);

            let mut sql = pseudonym_expression(entry.table, &entry.row_identity).unwrap();
            sql.push_str(&parent(entry.table).unwrap());
            for (i, path) in entry
                .canonical_path
                .iter()
                .chain(entry.secondary_paths)
                .enumerate()
            {
                let p = path_sql(path, i).unwrap();
                sql.push_str(&p.joins);
                sql.push_str(&p.orphaned);
                sql.push_str(&p.tenant);
                sql.push_str(&p.root_null);
                sql.push_str(&p.agent_uuid);
            }

            for quoted in sql.split('"').skip(1).step_by(2) {
                assert!(
                    declared.contains(&quoted) || fixed.contains(&quoted),
                    "{}: generated identifier `{quoted}` is not declared in the registry",
                    entry.table
                );
            }
        }
    }

    #[test]
    fn the_registry_declares_a_row_identity_for_every_table() {
        for entry in inventory::REGISTRY {
            assert!(
                !entry.row_identity.columns.is_empty(),
                "{}: no row identity declared",
                entry.table
            );
            for column in entry.row_identity.columns {
                assert!(
                    SqlIdentifier::new(column.name).is_ok(),
                    "{}: identity column {}",
                    entry.table,
                    column.name
                );
            }
        }
    }
}
