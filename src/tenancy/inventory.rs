//! The semantic tenancy inventory: what every application table is, where its
//! tenant comes from, and when it may be migrated.
//!
//! Step 4A of the AEON authentication & tenancy plan. Step 4A is an
//! implementation subdivision derived from the frozen plan
//! (`Adaptive-Liquidity/nexus-planning @ b1fe06505d400e435c3ef8d10dc197f15641bebd`);
//! it is not a named section of that document.
//!
//! Two halves live here and they are deliberately different in kind:
//!
//! * [`REGISTRY`] is a **decision**. Every table is assigned a classification,
//!   exactly one canonical ownership path, any secondary paths that must agree
//!   with it, a migration tranche, and the constraint work Step 4B will owe.
//!   Nothing in it is inferred at runtime — that is the point. A table cannot
//!   acquire a tenancy decision by accident.
//! * [`discover`] is an **observation**. It reads the live PostgreSQL catalogs
//!   and reports what actually exists.
//!
//! The audit's first job is to hold those two against each other. A table that
//! exists but is unregistered, or is registered but does not exist, or is
//! registered twice, is a blocking finding — which is how a future migration
//! that adds a table is forced to make a tenancy decision rather than inherit
//! one.
//!
//! ## Why classification is not name-driven
//!
//! Almost every table in this schema carries a column called `agent_id`, and it
//! is **`TEXT`** — the legacy caller-facing identifier, not `agents.id`. Only
//! `credential_agent_grants` references the internal UUID, and it calls the
//! column `agent_uuid` precisely so the two cannot be confused. A classifier
//! that keyed on the column name would get almost every table wrong in the same
//! direction.

use serde::Serialize;

use super::audit::ReasonCode;

/// What kind of thing a table is, for tenancy purposes.
///
/// Exactly one of these per table. `LEGACY_UNMAPPED` is deliberately absent:
/// it is a row-level mismatch reason, not a table classification, because a
/// table can hold correctly-owned rows and unresolvable ones at the same time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum TableClass {
    /// Stores authoritative tenant ownership directly, and is the terminus
    /// every other table's ownership path resolves *to*.
    TenantRoot,
    /// Ownership derives through an agent — either the internal `agents.id`
    /// UUID or the legacy `agents.agent_id` TEXT identifier — and then
    /// `agents.tenant_id`.
    DirectAgentChild,
    /// Ownership derives through the canonical owning session and its agent.
    SessionChild,
    /// Ownership derives through a memory, memory version, entity link,
    /// archival batch or other memory-lineage parent.
    MemoryLineageChild,
    /// Owned by one tenant but not necessarily by one specific agent.
    TenantGlobal,
    /// Intentionally shared system state that must never receive an inferred
    /// tenant.
    SystemGlobal,
}

impl TableClass {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::TenantRoot => "TENANT_ROOT",
            Self::DirectAgentChild => "DIRECT_AGENT_CHILD",
            Self::SessionChild => "SESSION_CHILD",
            Self::MemoryLineageChild => "MEMORY_LINEAGE_CHILD",
            Self::TenantGlobal => "TENANT_GLOBAL",
            Self::SystemGlobal => "SYSTEM_GLOBAL",
        }
    }
}

impl std::fmt::Display for TableClass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Which future migration wave a table belongs to.
///
/// Derived from the dependency graph, not from the classification: a table's
/// class says where its tenant comes from, its tranche says what has to be
/// migrated before it can be. `credentials` is `TENANT_GLOBAL` and sits in
/// tranche 1 because it is already tenant-scoped and owes no work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub enum Tranche {
    #[serde(rename = "TRANCHE_1_ROOTS_AND_DIRECT_AGENT_CHILDREN")]
    RootsAndDirectAgentChildren,
    #[serde(rename = "TRANCHE_2_SESSIONS")]
    Sessions,
    #[serde(rename = "TRANCHE_3_MEMORIES")]
    Memories,
    #[serde(rename = "TRANCHE_4_LINEAGE_AND_ARCHIVAL")]
    LineageAndArchival,
    /// Named for what it holds. Both its tables — `amp_controller_state` and
    /// `rmk_episodes` — are `DIRECT_AGENT_CHILD`, so the previous name
    /// `TRANCHE_5_TENANT_GLOBAL_OPERATIONS` described neither of them.
    #[serde(rename = "TRANCHE_5_OPERATIONS")]
    Operations,
    #[serde(rename = "FINAL_CONSTRAINT_TIGHTENING")]
    FinalConstraintTightening,
}

impl Tranche {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RootsAndDirectAgentChildren => "TRANCHE_1_ROOTS_AND_DIRECT_AGENT_CHILDREN",
            Self::Sessions => "TRANCHE_2_SESSIONS",
            Self::Memories => "TRANCHE_3_MEMORIES",
            Self::LineageAndArchival => "TRANCHE_4_LINEAGE_AND_ARCHIVAL",
            Self::Operations => "TRANCHE_5_OPERATIONS",
            Self::FinalConstraintTightening => "FINAL_CONSTRAINT_TIGHTENING",
        }
    }

    /// Every tranche, in the order they must be executed.
    pub const ALL: &'static [Tranche] = &[
        Self::RootsAndDirectAgentChildren,
        Self::Sessions,
        Self::Memories,
        Self::LineageAndArchival,
        Self::Operations,
        Self::FinalConstraintTightening,
    ];
}

impl std::fmt::Display for Tranche {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// How a path walks from a row to its owning tenant.
///
/// The scanner is generic over this rather than carrying one bespoke query per
/// table: twenty-two hand-written ownership queries would be twenty-two places
/// for the ownership rule to drift from the registry that documents it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum PathKind {
    /// The row carries `tenant_id` itself and it is authoritative.
    TenantColumn { column: &'static str },
    /// `row.<column>` (TEXT) -> `agents.agent_id` -> `agents.id` ->
    /// `agents.tenant_id`. The legacy identifier path, which is most of them.
    AgentText { column: &'static str },
    /// `row.<column>` (UUID) -> `agents.id` -> `agents.tenant_id`.
    AgentUuid { column: &'static str },
    /// `(row.<agent_column>, row.<session_column>)` -> `sessions` ->
    /// `agents.agent_id` -> tenant.
    ///
    /// The pair is load-bearing. `sessions` is unique on
    /// `(agent_id, session_id)` and **not** on `session_id` alone, so a session
    /// reference without its agent does not identify one session.
    Session {
        agent_column: &'static str,
        session_column: &'static str,
    },
    /// `row.<column>` -> `memories.id` -> `memories.agent_id` -> tenant.
    Memory { column: &'static str },
    /// `row.<column>` -> `entities.id` -> `entities.agent_id` -> tenant.
    Entity { column: &'static str },
    /// `row.<column>` -> `archival_batches.id` -> `agent_id` -> tenant.
    ArchivalBatch { column: &'static str },
    /// `row.<column>` -> `rmk_policies.id` -> `agent_id` -> tenant.
    Policy { column: &'static str },
}

impl PathKind {
    /// The column on the table itself where this path starts.
    pub const fn root_column(&self) -> &'static str {
        match self {
            Self::TenantColumn { column }
            | Self::AgentText { column }
            | Self::AgentUuid { column }
            | Self::Memory { column }
            | Self::Entity { column }
            | Self::ArchivalBatch { column }
            | Self::Policy { column } => column,
            Self::Session { session_column, .. } => session_column,
        }
    }

    /// Whether this path's source reference contains caller-supplied text.
    ///
    /// Drives pseudonymisation of the ambiguity finding's example identifier:
    /// a legacy `agent_id` or `session_id` is a caller string and must not be
    /// copied into the artifact, while a UUID reference is already opaque.
    pub const fn root_is_caller_text(&self) -> bool {
        match self {
            Self::AgentText { .. } | Self::Session { .. } => true,
            Self::TenantColumn { .. }
            | Self::AgentUuid { .. }
            | Self::Memory { .. }
            | Self::Entity { .. }
            | Self::ArchivalBatch { .. }
            | Self::Policy { .. } => false,
        }
    }
}

/// One route from a row to its authoritative tenant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct OwnershipPath {
    /// Human-readable chain, e.g. `row.agent_id -> agents.agent_id ->
    /// agents.id -> agents.tenant_id`. Rendered into the artifacts verbatim.
    pub label: &'static str,
    pub kind: PathKind,
    /// Whether the starting column may legitimately be NULL. A NULL in a
    /// canonical path that is *not* nullable is a blocking structural finding;
    /// a NULL in a nullable path simply means that path does not apply to the
    /// row, and the remaining paths must still agree.
    pub nullable: bool,
}

/// The narrative half of one table's Step 4B work.
///
/// The *objects* — columns, indexes, unique targets, foreign keys, constraints —
/// are not here. They live in [`super::plan::PLANNED_OBJECTS`] as typed data,
/// because carrying them as prose is what let two foreign keys reference parent
/// tuples that no planned unique target covered. This struct keeps only what is
/// genuinely narrative, and the artifacts render the objects from the typed
/// source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct MigrationPlan {
    /// What the added columns are created as, before any backfill.
    pub initial_nullability: &'static str,
    /// The shape of the backfill query, expressed against the canonical path.
    ///
    /// Where a table has more than one ownership path the *authoritative* form —
    /// including the predicate every path must satisfy before ownership is
    /// written — is [`super::plan::BackfillAuthority`]. This field is the shape;
    /// that one is the contract.
    pub backfill_source: &'static str,
    /// Reason codes that must be at zero for this table before Step 4B may
    /// touch it.
    ///
    /// Typed rather than stringly, so a rename cannot silently empty a gate, and
    /// **per table** rather than unioned across a tranche: `audit_logs` may
    /// legitimately hold agentless rows, so `LEGACY_UNMAPPED` is deliberately
    /// absent from its set and a tranche-wide union would put it back.
    pub required_zero_codes: &'static [ReasonCode],
    /// The later `VALIDATE CONSTRAINT` step, if any.
    pub validate_step: Option<&'static str>,
    /// How uniqueness changes when the table becomes tenant-scoped.
    pub future_uniqueness: Option<&'static str>,
    /// The tuple a future `UNIQUE (tenant_id, ...)` would cover, if one is
    /// planned. Drives the collision pre-check.
    pub future_unique_columns: Option<&'static [&'static str]>,
    /// Expected locking / rewrite behaviour, in words.
    ///
    /// The precise locks are typed on [`super::plan::LockProfile`] per planned
    /// object. This field says what is notable about *this table* — its size,
    /// its write pattern, whether the backfill needs batching — not what lock a
    /// generic `VALIDATE` takes.
    pub lock_profile: &'static str,
    /// What must be rolled back before this table can be.
    pub rollback_dependencies: &'static [&'static str],
    /// Request and worker paths that must stay inactive during the migration.
    pub inactive_paths: &'static [&'static str],
}

/// One table's complete tenancy decision.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct TableEntry {
    pub table: &'static str,
    /// The schema objects this table is *required* to carry for the current
    /// scanner to be sound: the keys its row identity uses, and the keys or
    /// foreign keys its ownership joins assume.
    ///
    /// These are requirements, not observations. Each Step 4A audit run
    /// verifies every entry against the live PostgreSQL catalog of the database
    /// it is pointed at, and emits `SCHEMA_RELATIONSHIP_DRIFT` per mismatch. A
    /// declaration in this registry is therefore never, by itself, proof that
    /// any particular deployment satisfies it — only a run against that
    /// deployment can say so, and only for that deployment.
    pub schema_contract: &'static [SchemaObjectContract],
    /// How this table's example row identifier is built. Static, so no catalog
    /// value ever reaches query construction.
    pub row_identity: RowIdentity,
    /// For session-bearing tables: what the session reference means, and the
    /// evidence for it.
    pub session_semantics: Option<SessionSemantics>,
    pub class: TableClass,
    /// Exactly one, for everything except `SYSTEM_GLOBAL`.
    pub canonical_path: Option<OwnershipPath>,
    /// Paths that must agree with the canonical one. Disagreement is a
    /// blocking finding, never a fallback.
    pub secondary_paths: &'static [OwnershipPath],
    /// What agreement means for this table, in words.
    pub consistency: Option<&'static str>,
    pub tranche: Tranche,
    pub plan: MigrationPlan,
    /// For `SYSTEM_GLOBAL` only: the evidence that global scope is actually
    /// safe. Absent evidence is `GLOBAL_SCOPE_UNVERIFIED`, which blocks.
    pub global_scope_evidence: Option<&'static str>,
    /// Why this table is classified as it is, given that names alone do not
    /// decide it.
    pub rationale: &'static str,
}

/// How a table's example row identifier is built.
///
/// Declared here, never derived from the live catalog. The catalog's job is to
/// *verify* this declaration — that the columns exist, carry the expected type,
/// and together form the primary key — and to emit `SCHEMA_RELATIONSHIP_DRIFT`
/// and refuse to build the scanner query when they do not. A name returned by
/// the catalog is never interpolated into SQL, validated or otherwise: the
/// registry is the only source of identifiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct IdentityColumn {
    pub name: &'static str,
    pub kind: IdentityKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum IdentityKind {
    /// An opaque surrogate (UUID or integer). Emitted as-is: it is already
    /// meaningless outside the database, and an operator needs it to find the
    /// row.
    Surrogate,
    /// Caller-supplied text. Replaced by a domain-separated SHA-256 pseudonym
    /// so caller strings are not copied into a diagnostic artifact.
    CallerText,
}

impl IdentityKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Surrogate => "surrogate, emitted as-is",
            Self::CallerText => "caller text, pseudonymised",
        }
    }

    /// The exact `format_type` spellings this column may have.
    ///
    /// Compared by **equality**, never by substring. A substring test accepted
    /// every one of these as "text" or "uuid": `text[]`, `uuid[]`, `character
    /// varying(64)` (contains neither, but `varchar` domains named `*_text` do),
    /// and any domain whose name merely contains `text` or `uuid`. An array
    /// column in particular would make `concat_ws` produce a brace-wrapped
    /// literal that identifies no row, and a `NULL` element would silently
    /// vanish from the pseudonym.
    pub const fn allowed_types(self) -> &'static [&'static str] {
        match self {
            Self::Surrogate => &["uuid"],
            Self::CallerText => &["text"],
        }
    }
}

/// The columns forming a table's example identifier, in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct RowIdentity {
    pub columns: &'static [IdentityColumn],
}

impl RowIdentity {
    pub fn has_caller_text(&self) -> bool {
        self.columns
            .iter()
            .any(|c| c.kind == IdentityKind::CallerText)
    }
}

/// What a session reference *means* for a table, concluded from its actual
/// insert paths, update paths, read predicates and constraints — not from
/// whether the column happens to be NULL-able.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum SessionRole {
    /// Rows are addressed by their session; a missing session is a broken row.
    Canonical,
    /// The agent owns the row, and the session is context that must *agree* —
    /// a genuine secondary consistency path, checked and reported on mismatch.
    ///
    /// No table reaches it today. The five that once did turned out to have
    /// session references that can legitimately outrun their `sessions` row, so
    /// they are `ContextOnly` instead: context checked against nothing, because
    /// there is nothing sound to check it against. The distinction is worth
    /// keeping — a future table whose session row is written atomically with it
    /// would belong here, not there.
    #[allow(dead_code)]
    SecondaryToAgent,
    /// A memory owns the row; the session is context that must agree.
    ///
    /// Declared for completeness. No table in the current schema reaches it:
    /// every session-bearing table resolved to an agent, not to a memory.
    #[allow(dead_code)]
    SecondaryToMemoryLineage,
    /// The field was inspected and is deliberately **not** an ownership path.
    ///
    /// Distinct from recording no conclusion at all: absent semantics say
    /// *not investigated*, this says *investigated and non-authoritative*.
    ContextOnly,
}

impl SessionRole {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Canonical => "CANONICAL",
            Self::SecondaryToAgent => "SECONDARY_TO_AGENT",
            Self::SecondaryToMemoryLineage => "SECONDARY_TO_MEMORY_LINEAGE",
            Self::ContextOnly => "CONTEXT_ONLY",
        }
    }
}

/// The conclusion, with the evidence it rests on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct SessionSemantics {
    pub role: SessionRole,
    pub evidence: &'static str,
}

const SURROGATE_ID: RowIdentity = RowIdentity {
    columns: &[IdentityColumn {
        name: "id",
        kind: IdentityKind::Surrogate,
    }],
};

/// What kind of constraint `pg_constraint` should report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ConstraintKind {
    PrimaryKey,
    Unique,
    ForeignKey,
}

impl ConstraintKind {
    /// The `pg_constraint.contype` character the live constraint must report.
    pub const fn contype(self) -> &'static str {
        match self {
            Self::PrimaryKey => "p",
            Self::Unique => "u",
            Self::ForeignKey => "f",
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PrimaryKey => "PRIMARY KEY",
            Self::Unique => "UNIQUE",
            Self::ForeignKey => "FOREIGN KEY",
        }
    }
}

/// A schema object the *current* scanner relies on.
///
/// Two shapes rather than one, because a constraint and a standalone index
/// expose different validity properties, and conflating them would let a
/// partial unique index — which guarantees nothing outside its predicate —
/// satisfy a uniqueness requirement.
///
/// Verified through catalog OIDs and attribute numbers (`conkey`, `confkey`,
/// `indkey`), never by matching rendered SQL: `pg_get_constraintdef` output is
/// a presentation format, and a substring test over it is not a check.
///
/// These describe what exists **today**. Step 4B's planned constraints live in
/// [`MigrationPlan`] and are deliberately never required here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "object", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum SchemaObjectContract {
    Constraint {
        name: &'static str,
        kind: ConstraintKind,
        /// Ordered — `conkey` order is part of the guarantee.
        local_columns: &'static [&'static str],
        /// `None` for primary keys and unique constraints.
        referenced_table: Option<&'static str>,
        /// Ordered, matching `confkey`.
        referenced_columns: &'static [&'static str],
        /// Whether `convalidated` must be true. A `NOT VALID` foreign key
        /// enforces new rows only, so it does not back a join the scanner
        /// treats as authoritative.
        require_validated: bool,
    },
    UniqueIndex {
        name: &'static str,
        /// Ordered, matching `indkey`.
        columns: &'static [&'static str],
        require_unique: bool,
        require_valid: bool,
        require_ready: bool,
        /// A partial index guarantees uniqueness only over its predicate, so it
        /// cannot back a join that assumes at most one match.
        require_non_partial: bool,
        /// An expression index does not constrain the bare columns.
        require_no_expressions: bool,
    },
}

impl SchemaObjectContract {
    /// The object's name, used to look it up in `pg_constraint` / `pg_index`.
    ///
    /// A declared name is only ever *compared* against the catalog — it is
    /// never interpolated into generated SQL, so it needs no identifier
    /// validation and cannot reach a query string.
    pub const fn name(&self) -> &'static str {
        match self {
            Self::Constraint { name, .. } | Self::UniqueIndex { name, .. } => name,
        }
    }

    /// One line for the artifacts. Spells out the referenced columns and the
    /// strictness flags, because those are the parts a reader would otherwise
    /// have to take on trust.
    pub fn describe(&self) -> String {
        match self {
            Self::Constraint {
                name,
                kind,
                local_columns,
                referenced_table,
                referenced_columns,
                require_validated,
            } => {
                let mut out = format!("`{name}` {} ({})", kind.as_str(), local_columns.join(", "));
                if let Some(table) = referenced_table {
                    out.push_str(&format!(
                        " REFERENCES {table}({})",
                        referenced_columns.join(", ")
                    ));
                }
                if *require_validated {
                    out.push_str(", validated");
                }
                out
            }
            Self::UniqueIndex {
                name,
                columns,
                require_unique,
                require_valid,
                require_ready,
                require_non_partial,
                require_no_expressions,
            } => {
                let mut flags = Vec::new();
                if *require_unique {
                    flags.push("unique");
                }
                if *require_valid {
                    flags.push("valid");
                }
                if *require_ready {
                    flags.push("ready");
                }
                if *require_non_partial {
                    flags.push("non-partial");
                }
                if *require_no_expressions {
                    flags.push("no expressions");
                }
                format!(
                    "`{name}` INDEX ({}) - {}",
                    columns.join(", "),
                    flags.join(", ")
                )
            }
        }
    }
}

/// A standalone unique index, with the strict defaults.
const fn unique_index(
    name: &'static str,
    columns: &'static [&'static str],
) -> SchemaObjectContract {
    SchemaObjectContract::UniqueIndex {
        name,
        columns,
        require_unique: true,
        require_valid: true,
        require_ready: true,
        require_non_partial: true,
        require_no_expressions: true,
    }
}

const fn primary_key(
    name: &'static str,
    local_columns: &'static [&'static str],
) -> SchemaObjectContract {
    SchemaObjectContract::Constraint {
        name,
        kind: ConstraintKind::PrimaryKey,
        local_columns,
        referenced_table: None,
        referenced_columns: &[],
        require_validated: true,
    }
}

const fn unique_constraint(
    name: &'static str,
    local_columns: &'static [&'static str],
) -> SchemaObjectContract {
    SchemaObjectContract::Constraint {
        name,
        kind: ConstraintKind::Unique,
        local_columns,
        referenced_table: None,
        referenced_columns: &[],
        require_validated: true,
    }
}

const fn foreign_key(
    name: &'static str,
    local_columns: &'static [&'static str],
    referenced_table: &'static str,
    referenced_columns: &'static [&'static str],
) -> SchemaObjectContract {
    SchemaObjectContract::Constraint {
        name,
        kind: ConstraintKind::ForeignKey,
        local_columns,
        referenced_table: Some(referenced_table),
        referenced_columns,
        require_validated: true,
    }
}

// ── Shared plan fragments ───────────────────────────────────────────────────
// Most tables owe the same work, so the differences are what stand out.

const NULLABLE_THEN_TIGHTENED: &str =
    "added NULL-able; NOT NULL only in FINAL_CONSTRAINT_TIGHTENING, after backfill \
     verification reports zero blocking findings";
const NONE_REQUIRED: &[&str] = &[];

/// The reason codes that must be zero before any tenant-bearing table moves.
///
/// Per table, never unioned across a tranche. A union would hand every table the
/// strictest set any of its neighbours needs, and `audit_logs` is the case that
/// makes the difference concrete: agentless events are valid rows, so
/// `LEGACY_UNMAPPED` is absent from its set on purpose.
const OWNERSHIP_CODES: &[ReasonCode] = &[
    ReasonCode::LegacyUnmapped,
    ReasonCode::OrphanedAgentReference,
    ReasonCode::UnmappedAgent,
    ReasonCode::UnresolvableOwner,
    ReasonCode::NullOwnershipLink,
];

/// For `working_memory`, the one true `SESSION_CHILD`: its canonical path *is*
/// the session, so a missing session row is a real orphan.
const SESSION_CHILD_CODES: &[ReasonCode] = &[
    ReasonCode::LegacyUnmapped,
    ReasonCode::OrphanedAgentReference,
    ReasonCode::OrphanedSessionReference,
    ReasonCode::UnmappedAgent,
    ReasonCode::UnresolvableOwner,
    ReasonCode::NullOwnershipLink,
    ReasonCode::OwnershipPathDisagreement,
];

/// For `memories`, which has a secondary path but whose session is
/// `CONTEXT_ONLY`.
///
/// `ORPHANED_SESSION_REFERENCE` is deliberately **absent**: memories' canonical
/// path is `AGENT_TEXT` and no session path is registered, so the code cannot
/// fire for this table. A gate that can never fail is not a gate, and leaving it
/// in made the required-zero set look stricter than it was. Recorded in
/// [`super::plan::UNREACHABLE_REQUIRED_ZERO`] rather than silently dropped.
const MEMORIES_CODES: &[ReasonCode] = &[
    ReasonCode::LegacyUnmapped,
    ReasonCode::OrphanedAgentReference,
    ReasonCode::UnmappedAgent,
    ReasonCode::UnresolvableOwner,
    ReasonCode::NullOwnershipLink,
    ReasonCode::OwnershipPathDisagreement,
];

/// For the three `MEMORY_LINEAGE_CHILD` tables.
///
/// `ORPHANED_MEMORY_REFERENCE` is deliberately **absent**: every memory
/// reference in this schema is backed by a declared foreign key, so an orphan
/// cannot exist while the contract holds, and producing one means dropping that
/// key — which is `SCHEMA_RELATIONSHIP_DRIFT` and blocks by a different code.
/// The gate is still closed; it is closed by the code that can actually fire.
const LINEAGE_CODES: &[ReasonCode] = &[
    ReasonCode::LegacyUnmapped,
    ReasonCode::OrphanedAgentReference,
    ReasonCode::UnmappedAgent,
    ReasonCode::UnresolvableOwner,
    ReasonCode::NullOwnershipLink,
    ReasonCode::CrossTenantParentChild,
    ReasonCode::OwnershipPathDisagreement,
];

const WORKER_PATHS: &[&str] = &["rmk_worker", "extraction_worker", "hnsw_maintenance"];
const ARCHIVAL_PATHS: &[&str] = &["archival worker", "POST /memories", "GET /memories"];

/// The canonical legacy path, which most tables use.
const fn agent_text(column: &'static str, nullable: bool) -> OwnershipPath {
    OwnershipPath {
        label: "row.agent_id -> agents.agent_id -> agents.id -> agents.tenant_id",
        kind: PathKind::AgentText { column },
        nullable,
    }
}

const fn session_path(
    agent_column: &'static str,
    session_column: &'static str,
    nullable: bool,
) -> OwnershipPath {
    OwnershipPath {
        label: "(row.agent_id, row.session_id) -> sessions(agent_id, session_id) -> \
                agents.agent_id -> agents.id -> agents.tenant_id",
        kind: PathKind::Session {
            agent_column,
            session_column,
        },
        nullable,
    }
}

const fn memory_path(column: &'static str, nullable: bool) -> OwnershipPath {
    OwnershipPath {
        label: "row.memory_id -> memories.id -> memories.agent_id -> agents.agent_id -> \
                agents.tenant_id",
        kind: PathKind::Memory { column },
        nullable,
    }
}

/// A plan for a table whose only work is `agent_uuid` + `tenant_id` plus a
/// composite FK back to `agents`.
#[allow(clippy::too_many_arguments)]
const fn agent_child_plan(
    backfill_source: &'static str,
    validate: &'static str,
    lock_profile: &'static str,
    rollback_dependencies: &'static [&'static str],
    inactive_paths: &'static [&'static str],
) -> MigrationPlan {
    MigrationPlan {
        initial_nullability: NULLABLE_THEN_TIGHTENED,
        backfill_source,
        required_zero_codes: OWNERSHIP_CODES,
        validate_step: Some(validate),
        future_uniqueness: None,
        future_unique_columns: None,
        lock_profile,
        rollback_dependencies,
        inactive_paths,
    }
}

// ── The registry ────────────────────────────────────────────────────────────

/// Every application table, with its tenancy decision.
///
/// Ordered by table name so the rendered artifacts are stable regardless of how
/// this list is edited.
pub const REGISTRY: &[TableEntry] = &[
    TableEntry {
        table: "agent_tenancy_migrations",
        schema_contract: &[primary_key("agent_tenancy_migrations_pkey", &["id"])],
        row_identity: SURROGATE_ID,
        session_semantics: None,
        class: TableClass::SystemGlobal,
        canonical_path: None,
        secondary_paths: &[],
        consistency: None,
        tranche: Tranche::FinalConstraintTightening,
        plan: MigrationPlan {
            initial_nullability: "n/a — no ownership columns are added",
            backfill_source: "n/a — SYSTEM_GLOBAL tables are never backfilled with an \
                              inferred tenant",
            required_zero_codes: &[ReasonCode::GlobalScopeUnverified],
            validate_step: None,
            future_uniqueness: None,
            future_unique_columns: None,
            lock_profile: "none — not modified",
            rollback_dependencies: NONE_REQUIRED,
            inactive_paths: NONE_REQUIRED,
        },
        global_scope_evidence: Some(
            "Ledger of tenancy *migration runs*, written only by the step-1 backfill and read \
             only by `assert_multi_tenant_preconditions`. It is on no request path. Its \
             `legacy_tenant_id` is an input parameter of the run that produced the row, not \
             ownership of the row: migration 0028 constrains it with `(mode = 'single_tenant') = \
             (legacy_tenant_id IS NOT NULL)`, so in `mapped` mode it is NULL by construction. \
             Giving these rows a tenant would misrepresent an operator action as tenant data.",
        ),
        rationale: "System bookkeeping. Carries a tenant-shaped column, which is exactly why it \
                    needs stated evidence rather than an assumption.",
    },
    TableEntry {
        table: "agents",
        schema_contract: &[
            primary_key("agents_pkey", &["id"]),
            // Every AgentText path joins on this; without it a join can match
            // more than one agent and ownership becomes ambiguous.
            unique_constraint("agents_agent_id_key", &["agent_id"]),
            // Backs no scanner join, which is exactly why deriving this set
            // from join-reachability alone missed it. It backs the tenancy
            // invariant itself: an external identifier is unique *per tenant*,
            // not globally, so two tenants can each own an agent by the same
            // name without colliding.
            unique_constraint(
                "agents_tenant_id_external_agent_id_key",
                &["tenant_id", "external_agent_id"],
            ),
            // The composite target the grant foreign key resolves through.
            unique_constraint("agents_tenant_id_id_key", &["tenant_id", "id"]),
        ],
        row_identity: SURROGATE_ID,
        session_semantics: None,
        class: TableClass::TenantRoot,
        canonical_path: Some(OwnershipPath {
            label: "row.tenant_id (authoritative)",
            kind: PathKind::TenantColumn {
                column: "tenant_id",
            },
            nullable: true,
        }),
        secondary_paths: &[],
        consistency: Some(
            "`agents` is the authority: every other path in this registry terminates here. \
             `tenant_id` is still NULL-able because step 1 deliberately leaves unmapped agents \
             unreachable rather than inventing a tenant for them; every such row is reported as \
             UNMAPPED_AGENT.",
        ),
        tranche: Tranche::RootsAndDirectAgentChildren,
        plan: MigrationPlan {
            initial_nullability: "already present from migration 0028",
            backfill_source: "already backfilled by step 1 under an explicit \
                              LEGACY_MIGRATION_MODE",
            // A tenant root whose own tenant is NULL is itself an unmapped
            // agent, and it also emits NULL_OWNERSHIP_LINK / LEGACY_UNMAPPED.
            // Gating only on UNMAPPED_AGENT let tranche 1 read READY while its
            // planned SET NOT NULL could not possibly succeed.
            required_zero_codes: &[
                ReasonCode::LegacyUnmapped,
                ReasonCode::NullOwnershipLink,
                ReasonCode::UnmappedAgent,
            ],
            validate_step: Some(
                "SET NOT NULL on agents.tenant_id once UNMAPPED_AGENT reaches zero",
            ),
            future_uniqueness: Some(
                "Nothing is added. `agents` already carries UNIQUE (tenant_id, \
                 external_agent_id) from migration 0028, which is the tenant-scoped identity, \
                 and UNIQUE (tenant_id, id), which is the composite FK target every child \
                 references. `agents_agent_id_key` is *not* replaced by UNIQUE (tenant_id, \
                 agent_id): that would introduce a second tenant-scoped name for the same \
                 agent, leaving two identity keys to keep in agreement. The legacy global key \
                 is removed together with the legacy `agent_id` column in FUTURE STEP 7, once \
                 Step 5 endpoint enforcement and Step 6 legacy-key retirement have merged.",
            ),
            future_unique_columns: None,
            lock_profile: "SET NOT NULL takes ACCESS EXCLUSIVE and scans the table; on PG 16 a \
                           previously validated CHECK lets it skip the scan",
            rollback_dependencies: &["rollback/0028_agent_tenancy_identity_down.sql"],
            inactive_paths: &["POST /agents", "GET /agents"],
        },
        global_scope_evidence: None,
        rationale: "The tenant root: it holds `tenant_id` itself and is what every other \
                    ownership path resolves to.",
    },
    TableEntry {
        table: "amp_controller_state",
        schema_contract: &[primary_key("amp_controller_state_pkey", &["agent_id"])],
        row_identity: RowIdentity { columns: &[IdentityColumn { name: "agent_id", kind: IdentityKind::CallerText }] },
        session_semantics: None,
        class: TableClass::DirectAgentChild,
        canonical_path: Some(agent_text("agent_id", false)),
        secondary_paths: &[],
        consistency: None,
        tranche: Tranche::Operations,
        plan: agent_child_plan(
            "UPDATE amp_controller_state s SET agent_uuid = a.id, tenant_id = a.tenant_id \
             FROM agents a WHERE a.agent_id = s.agent_id",
            "VALIDATE CONSTRAINT after backfill",
            "primary key is the legacy TEXT `agent_id`; re-keying to `agent_uuid` is a table \
             rewrite and is deferred to FINAL_CONSTRAINT_TIGHTENING",
            &["tranche 1"],
            WORKER_PATHS,
        ),
        global_scope_evidence: None,
        rationale: "Reads as global controller state, but its primary key *is* `agent_id` and \
                    `rmk_worker` writes one row per agent. It is per-agent state — the name is \
                    the only thing about it that suggests otherwise, which is why the \
                    classification is taken from the key and the writers instead.",
    },
    TableEntry {
        table: "archival_batches",
        schema_contract: &[
            primary_key("archival_batches_pkey", &["id"]),
            foreign_key(
                "archival_batches_agent_id_fkey",
                &["agent_id"],
                "agents",
                &["agent_id"],
            ),
        ],
        row_identity: SURROGATE_ID,
        session_semantics: None,
        class: TableClass::DirectAgentChild,
        canonical_path: Some(agent_text("agent_id", false)),
        secondary_paths: &[],
        consistency: None,
        tranche: Tranche::RootsAndDirectAgentChildren,
        plan: agent_child_plan(
            "UPDATE archival_batches b SET agent_uuid = a.id, tenant_id = a.tenant_id \
             FROM agents a WHERE a.agent_id = b.agent_id",
            "VALIDATE CONSTRAINT after backfill",
            "ADD COLUMN NULL is metadata-only; the FK validation takes SHARE ROW EXCLUSIVE",
            &["memories (memories.archival_batch_id references this table)"],
            ARCHIVAL_PATHS,
        ),
        global_scope_evidence: None,
        rationale: "Has a real FK to `agents(agent_id)`, so its owner is already enforced. Must \
                    precede `memories`, which references it.",
    },
    TableEntry {
        table: "audit_logs",
        schema_contract: &[primary_key("audit_logs_pkey", &["id"])],
        row_identity: SURROGATE_ID,
        session_semantics: None,
        class: TableClass::DirectAgentChild,
        canonical_path: Some(agent_text("agent_id", true)),
        secondary_paths: &[],
        consistency: Some(
            "`agent_id` is NULL-able here and nowhere else among the direct children. A NULL is \
             not schema drift — some events genuinely have no agent — but it is unassignable, so \
             those rows are reported LEGACY_UNMAPPED and the table cannot take a NOT NULL tenant \
             until their disposition is decided.",
        ),
        tranche: Tranche::RootsAndDirectAgentChildren,
        plan: MigrationPlan {
            initial_nullability: "added NULL-able and **stays** NULL-able until the disposition \
                                  of agent-less audit rows is decided",
            backfill_source: "UPDATE audit_logs l SET agent_uuid = a.id, tenant_id = a.tenant_id \
                              FROM agents a WHERE a.agent_id = l.agent_id",
            // LEGACY_UNMAPPED is deliberately absent. audit_logs records
            // agentless events — startup, configuration change,
            // administrative action — which are legitimate rows with no
            // owning agent. Requiring zero unmapped rows here would demand
            // that the schema reject valid audit history, so its ownership
            // columns stay permanently NULL-able and this gate asks only that
            // the rows which *do* name an agent resolve correctly.
            required_zero_codes: &[
                ReasonCode::OrphanedAgentReference,
                ReasonCode::UnmappedAgent,
                ReasonCode::UnresolvableOwner,
            ],
            validate_step: Some("VALIDATE CONSTRAINT after backfill"),
            future_uniqueness: None,
            future_unique_columns: None,
            lock_profile: "append-mostly and potentially large; ADD COLUMN NULL is metadata-only",
            rollback_dependencies: &["tranche 1"],
            inactive_paths: &["all write paths that emit audit events"],
        },
        global_scope_evidence: None,
        rationale: "Agent-scoped when an agent is known. LEGACY_UNMAPPED exists as a row-level \
                    code precisely for tables like this one, which hold both assignable and \
                    unassignable rows.",
    },
    TableEntry {
        table: "co_access_edges",
        schema_contract: &[
            primary_key("co_access_edges_pkey", &["memory_a", "memory_b"]),
            foreign_key(
                "co_access_edges_memory_a_fkey",
                &["memory_a"],
                "memories",
                &["id"],
            ),
            foreign_key(
                "co_access_edges_memory_b_fkey",
                &["memory_b"],
                "memories",
                &["id"],
            ),
        ],
        row_identity: RowIdentity { columns: &[IdentityColumn { name: "memory_a", kind: IdentityKind::Surrogate }, IdentityColumn { name: "memory_b", kind: IdentityKind::Surrogate }] },
        session_semantics: None,
        class: TableClass::MemoryLineageChild,
        canonical_path: Some(memory_path("memory_a", false)),
        secondary_paths: &[memory_path("memory_b", false)],
        consistency: Some(
            "Both endpoints must resolve to the same tenant. This is the only table with no \
             agent column at all, so the two memory references are the entire ownership story — \
             and an edge spanning two tenants is precisely the cross-tenant link the audit \
             exists to find.",
        ),
        tranche: Tranche::LineageAndArchival,
        plan: MigrationPlan {
            initial_nullability: NULLABLE_THEN_TIGHTENED,
            backfill_source: "UPDATE co_access_edges e SET tenant_id = m.tenant_id FROM memories \
                              m WHERE m.id = e.memory_a, only where memory_a and memory_b agree",
            required_zero_codes: LINEAGE_CODES,
            validate_step: Some("VALIDATE both constraints after backfill"),
            future_uniqueness: None,
            future_unique_columns: None,
            lock_profile: "two FK validations, each SHARE ROW EXCLUSIVE on memories",
            rollback_dependencies: &["memories (tranche 3)"],
            inactive_paths: &["co-access edge maintenance", "retrieval scoring"],
        },
        global_scope_evidence: None,
        rationale: "No agent column exists. Both `memory_a` and `memory_b` are NOT NULL FKs to \
                    `memories`, so ownership is entirely lineage-derived.",
    },
    TableEntry {
        table: "cognitive_hypervisor_timeline",
        schema_contract: &[primary_key("cognitive_hypervisor_timeline_pkey", &["id"])],
        row_identity: SURROGATE_ID,
        session_semantics: Some(SessionSemantics { role: SessionRole::ContextOnly, evidence: "Read by `WHERE agent_id = $1` and `WHERE id = $1`; the hash chain is per-agent. No query selects by session." }),
        class: TableClass::DirectAgentChild,
        canonical_path: Some(agent_text("agent_id", false)),
        secondary_paths: &[],
        consistency: Some(
            "Measured, then deliberately excluded from ownership. `sessions` rows are created by \
             exactly one code path in the codebase, the working-memory upsert, and it runs \
             at the *end* of successful extraction. A pending job, a permanently failed job, \
             or a memory written before that upsert therefore legitimately references a \
             session that does not exist. Registering the session as an ownership or \
             consistency path would emit blocking ORPHANED_SESSION_REFERENCE findings on \
             entirely normal states, so it is recorded as context only rather than dropped \
             silently: CONTEXT_ONLY says *investigated and non-authoritative*, where an \
             absent conclusion would only say *not investigated*.",
        ),
        tranche: Tranche::Memories,
        plan: agent_child_plan(
            "UPDATE cognitive_hypervisor_timeline t SET agent_uuid = a.id, \
             tenant_id = a.tenant_id FROM agents a WHERE a.agent_id = t.agent_id",
            "VALIDATE CONSTRAINT after backfill",
            "append-only hash-chained timeline; ADD COLUMN NULL is metadata-only",
            &["sessions (tranche 2)"],
            &["POST /hypervisor/events", "GET /hypervisor/timeline"],
        ),
        global_scope_evidence: None,
        rationale: "The NOT NULL `agent_id` is the canonical path. `session_id` is CONTEXT_ONLY: \
                    it is not an ownership path and it is not a consistency path, because a row \
                    may legitimately outlive or precede its `sessions` row.",
    },
    TableEntry {
        table: "credential_agent_grants",
        schema_contract: &[
            primary_key(
                "credential_agent_grants_pkey",
                &["credential_id", "agent_uuid"],
            ),
            // The pair that forces one tenant across both parents. This table's
            // entire consistency claim rests on them.
            foreign_key(
                "credential_agent_grants_agent_fkey",
                &["tenant_id", "agent_uuid"],
                "agents",
                &["tenant_id", "id"],
            ),
            foreign_key(
                "credential_agent_grants_credential_fkey",
                &["credential_id", "tenant_id"],
                "credentials",
                &["id", "tenant_id"],
            ),
        ],
        row_identity: RowIdentity { columns: &[IdentityColumn { name: "credential_id", kind: IdentityKind::Surrogate }, IdentityColumn { name: "agent_uuid", kind: IdentityKind::Surrogate }] },
        session_semantics: None,
        class: TableClass::DirectAgentChild,
        canonical_path: Some(OwnershipPath {
            label: "row.agent_uuid -> agents.id -> agents.tenant_id",
            kind: PathKind::AgentUuid {
                column: "agent_uuid",
            },
            nullable: false,
        }),
        secondary_paths: &[OwnershipPath {
            label: "row.tenant_id (materialised, FK-enforced)",
            kind: PathKind::TenantColumn {
                column: "tenant_id",
            },
            nullable: false,
        }],
        consistency: Some(
            "The materialised `tenant_id` must equal the tenant reached through `agent_uuid`. \
             Migration 0030 already enforces this with composite foreign keys to both parents, \
             so a disagreement here would mean the constraint itself had drifted — which is why \
             the check is kept rather than assumed.",
        ),
        tranche: Tranche::RootsAndDirectAgentChildren,
        plan: MigrationPlan {
            initial_nullability: "already NOT NULL from migration 0030",
            backfill_source: "n/a — already tenant-scoped",
            required_zero_codes: &[
                ReasonCode::OwnershipPathDisagreement,
                ReasonCode::SchemaRelationshipDrift,
            ],
            validate_step: None,
            future_uniqueness: None,
            future_unique_columns: None,
            lock_profile: "none — not modified",
            rollback_dependencies: &[
                "rollback/0031_credential_agent_grants_hardening_down.sql",
                "rollback/0030_credential_agent_grants_down.sql",
            ],
            inactive_paths: NONE_REQUIRED,
        },
        global_scope_evidence: None,
        rationale: "The only table already keyed on the internal UUID. Its tenant is materialised \
                    but derived — the composite FK to `agents(tenant_id, id)` is what makes it \
                    authoritative — so it is a direct agent child, not a root.",
    },
    TableEntry {
        table: "credentials",
        schema_contract: &[
            primary_key("credentials_pkey", &["id"]),
            unique_constraint("credentials_id_tenant_id_key", &["id", "tenant_id"]),
        ],
        row_identity: SURROGATE_ID,
        session_semantics: None,
        class: TableClass::TenantGlobal,
        canonical_path: Some(OwnershipPath {
            label: "row.tenant_id (authoritative, NOT NULL)",
            kind: PathKind::TenantColumn {
                column: "tenant_id",
            },
            nullable: false,
        }),
        secondary_paths: &[],
        consistency: Some(
            "`tenant_id` is NOT NULL and is the credential's own authority. No agent parent \
             exists to cross-check it against: a `tenant_wide` credential reaches every agent in \
             the tenant, and an agent-restricted one reaches only those named in \
             `credential_agent_grants` — so the credential is owned by one tenant without being \
             owned by one agent.",
        ),
        tranche: Tranche::RootsAndDirectAgentChildren,
        plan: MigrationPlan {
            initial_nullability: "already NOT NULL from migration 0029",
            backfill_source: "n/a — created tenant-scoped",
            required_zero_codes: &[ReasonCode::SchemaRelationshipDrift],
            validate_step: None,
            future_uniqueness: None,
            future_unique_columns: None,
            lock_profile: "none — not modified",
            rollback_dependencies: NONE_REQUIRED,
            inactive_paths: NONE_REQUIRED,
        },
        global_scope_evidence: None,
        rationale: "TENANT_GLOBAL rather than TENANT_ROOT: it carries an authoritative tenant but \
                    is not the terminus other tables resolve to, and it is deliberately not bound \
                    to any one agent. TENANT_ROOT is reserved for `agents` and any future pure \
                    `tenants` table.",
    },
    TableEntry {
        table: "entities",
        schema_contract: &[primary_key("entities_pkey", &["id"])],
        row_identity: SURROGATE_ID,
        session_semantics: None,
        class: TableClass::DirectAgentChild,
        canonical_path: Some(agent_text("agent_id", false)),
        secondary_paths: &[],
        consistency: None,
        tranche: Tranche::RootsAndDirectAgentChildren,
        plan: MigrationPlan {
            initial_nullability: NULLABLE_THEN_TIGHTENED,
            backfill_source: "UPDATE entities e SET agent_uuid = a.id, tenant_id = a.tenant_id \
                              FROM agents a WHERE a.agent_id = e.agent_id",
            required_zero_codes: OWNERSHIP_CODES,
            validate_step: Some("VALIDATE CONSTRAINT after backfill"),
            future_uniqueness: Some(
                "`entities_agent_id_name_key` is UNIQUE (agent_id, name) and stays agent-scoped: \
                 an entity belongs to one agent, and widening it to the tenant would merge two \
                 agents' entity namespaces. Recorded so the decision is explicit rather than an \
                 omission.",
            ),
            future_unique_columns: None,
            lock_profile: "ADD COLUMN NULL is metadata-only; validation is SHARE ROW EXCLUSIVE",
            rollback_dependencies: &["memory_entity_links (tranche 4)"],
            inactive_paths: &["extraction_worker", "POST /entities"],
        },
        global_scope_evidence: None,
        rationale: "Per-agent extraction output, keyed by the legacy identifier.",
    },
    TableEntry {
        table: "extraction_jobs",
        schema_contract: &[primary_key("extraction_jobs_pkey", &["id"])],
        row_identity: SURROGATE_ID,
        session_semantics: Some(SessionSemantics { role: SessionRole::ContextOnly, evidence: "The worker claims jobs with an unfiltered queue scan over `extraction_jobs`, never by session; session_id is payload context carried through to the extracted memories." }),
        class: TableClass::DirectAgentChild,
        canonical_path: Some(agent_text("agent_id", false)),
        secondary_paths: &[],
        consistency: Some(
            "Measured, then deliberately excluded from ownership. `sessions` rows are created by \
             exactly one code path in the codebase, the working-memory upsert, and it runs \
             at the *end* of successful extraction. A pending job, a permanently failed job, \
             or a memory written before that upsert therefore legitimately references a \
             session that does not exist. Registering the session as an ownership or \
             consistency path would emit blocking ORPHANED_SESSION_REFERENCE findings on \
             entirely normal states, so it is recorded as context only rather than dropped \
             silently: CONTEXT_ONLY says *investigated and non-authoritative*, where an \
             absent conclusion would only say *not investigated*.",
        ),
        tranche: Tranche::Memories,
        plan: agent_child_plan(
            "UPDATE extraction_jobs j SET agent_uuid = a.id, tenant_id = a.tenant_id \
             FROM agents a WHERE a.agent_id = j.agent_id",
            "VALIDATE CONSTRAINT after backfill",
            "queue table with frequent updates; ADD COLUMN NULL is metadata-only",
            &["sessions (tranche 2)"],
            &["extraction_worker"],
        ),
        global_scope_evidence: None,
        rationale: "A work queue whose rows are owned by the agent that enqueued them; the \
                    session is optional context.",
    },
    TableEntry {
        table: "memories",
        schema_contract: &[
            primary_key("memories_pkey", &["id"]),
            foreign_key(
                "memories_archival_batch_id_fkey",
                &["archival_batch_id"],
                "archival_batches",
                &["id"],
            ),
        ],
        row_identity: SURROGATE_ID,
        session_semantics: Some(SessionSemantics { role: SessionRole::ContextOnly, evidence: "Every read predicate is `WHERE agent_id = ...` or `WHERE id = ...`; no query selects memories by session. Deletion is by agent or by id. The session records where a memory came from, not who owns it." }),
        class: TableClass::DirectAgentChild,
        canonical_path: Some(agent_text("agent_id", false)),
        secondary_paths: &[
            OwnershipPath {
                label: "row.archival_batch_id -> archival_batches.id -> agent_id -> tenant",
                kind: PathKind::ArchivalBatch {
                    column: "archival_batch_id",
                },
                nullable: true,
            },
        ],
        consistency: Some(
            "The archival batch, where present, must resolve to the same agent: an archived \
             memory whose batch belongs to a different agent is a blocking disagreement, not \
             a reason to prefer one path. The session is context only — `sessions` rows are \
             created solely by the working-memory upsert at the end of successful \
             extraction, so a memory written before it legitimately references a session \
             that does not yet exist.",
        ),
        tranche: Tranche::Memories,
        plan: MigrationPlan {
            initial_nullability: NULLABLE_THEN_TIGHTENED,
            backfill_source: "UPDATE memories m SET agent_uuid = a.id, tenant_id = a.tenant_id \
                              FROM agents a WHERE a.agent_id = m.agent_id",
            required_zero_codes: MEMORIES_CODES,
            validate_step: Some("VALIDATE CONSTRAINT after backfill"),
            future_uniqueness: Some(
                "Adds UNIQUE (id, tenant_id) — not a new restriction, since `id` is already the \
                 primary key, but the composite target every lineage child's FK needs in order to \
                 force one tenant across parent and child.",
            ),
            future_unique_columns: None,
            lock_profile: "the largest table, and the one carrying `embedding vector(1536)`; ADD \
                           COLUMN NULL is metadata-only, but the backfill should be batched and \
                           the FK added NOT VALID",
            rollback_dependencies: &[
                "memory_versions",
                "memory_entity_links",
                "co_access_edges",
                "memory_conflicts",
                "retrieval_feedback",
            ],
            inactive_paths: &[
                "POST /memories",
                "GET /memories",
                "archival worker",
                "rmk_worker",
            ],
        },
        global_scope_evidence: None,
        rationale: "Root of the memory lineage, but its own owner is the agent: `session_id` is \
                    NULL-able and `archival_batch_id` is NULL-able, so neither can be canonical.",
    },
    TableEntry {
        table: "memory_conflicts",
        schema_contract: &[
            primary_key("memory_conflicts_pkey", &["id"]),
            foreign_key(
                "memory_conflicts_memory_a_fkey",
                &["memory_a"],
                "memories",
                &["id"],
            ),
            foreign_key(
                "memory_conflicts_memory_b_fkey",
                &["memory_b"],
                "memories",
                &["id"],
            ),
        ],
        row_identity: SURROGATE_ID,
        session_semantics: None,
        class: TableClass::DirectAgentChild,
        canonical_path: Some(agent_text("agent_id", false)),
        secondary_paths: &[memory_path("memory_a", true), memory_path("memory_b", true)],
        consistency: Some(
            "Both memory references are NULL-able, so neither can be canonical. Where present, \
             each must resolve to the same tenant as the row's agent.",
        ),
        tranche: Tranche::LineageAndArchival,
        plan: agent_child_plan(
            "UPDATE memory_conflicts c SET agent_uuid = a.id, tenant_id = a.tenant_id \
             FROM agents a WHERE a.agent_id = c.agent_id",
            "VALIDATE all three after backfill",
            "small; three FK validations",
            &["memories (tranche 3)"],
            &["conflict detection"],
        ),
        global_scope_evidence: None,
        rationale: "Relates two memories, but its NOT NULL `agent_id` is the only reliable link; \
                    the memory references are nullable and therefore secondary.",
    },
    TableEntry {
        table: "memory_entity_links",
        schema_contract: &[
            primary_key("memory_entity_links_pkey", &["memory_id", "entity_id"]),
            foreign_key(
                "memory_entity_links_memory_id_fkey",
                &["memory_id"],
                "memories",
                &["id"],
            ),
            foreign_key(
                "memory_entity_links_entity_id_fkey",
                &["entity_id"],
                "entities",
                &["id"],
            ),
        ],
        row_identity: RowIdentity { columns: &[IdentityColumn { name: "memory_id", kind: IdentityKind::Surrogate }, IdentityColumn { name: "entity_id", kind: IdentityKind::Surrogate }] },
        session_semantics: None,
        class: TableClass::MemoryLineageChild,
        canonical_path: Some(memory_path("memory_id", false)),
        secondary_paths: &[
            agent_text("agent_id", false),
            OwnershipPath {
                label: "row.entity_id -> entities.id -> entities.agent_id -> tenant",
                kind: PathKind::Entity {
                    column: "entity_id",
                },
                nullable: false,
            },
        ],
        consistency: Some(
            "Three independent routes to a tenant — the memory, the denormalised agent, and the \
             entity — and all three must agree. This is the table where a silent fallback would \
             be most tempting and most wrong: a link whose memory and entity belong to different \
             tenants is a cross-tenant join, and picking either answer would launder it.",
        ),
        tranche: Tranche::LineageAndArchival,
        plan: MigrationPlan {
            initial_nullability: NULLABLE_THEN_TIGHTENED,
            backfill_source: "UPDATE memory_entity_links l SET tenant_id = m.tenant_id FROM \
                              memories m WHERE m.id = l.memory_id",
            required_zero_codes: LINEAGE_CODES,
            validate_step: Some("VALIDATE both after backfill"),
            future_uniqueness: None,
            future_unique_columns: None,
            lock_profile: "join table; two FK validations",
            rollback_dependencies: &["memories (tranche 3)", "entities (tranche 1)"],
            inactive_paths: &["extraction_worker"],
        },
        global_scope_evidence: None,
        rationale: "Primary key is `(memory_id, entity_id)`; both are NOT NULL FKs. The memory is \
                    canonical because it is also what the tenant column will be keyed to.",
    },
    TableEntry {
        table: "memory_graph",
        schema_contract: &[primary_key("memory_graph_pkey", &["id"])],
        row_identity: SURROGATE_ID,
        session_semantics: None,
        class: TableClass::DirectAgentChild,
        canonical_path: Some(agent_text("agent_id", false)),
        secondary_paths: &[],
        consistency: None,
        tranche: Tranche::RootsAndDirectAgentChildren,
        plan: agent_child_plan(
            "UPDATE memory_graph g SET agent_uuid = a.id, tenant_id = a.tenant_id \
             FROM agents a WHERE a.agent_id = g.agent_id",
            "VALIDATE CONSTRAINT after backfill",
            "ADD COLUMN NULL is metadata-only",
            &["tranche 1"],
            &["extraction_worker", "GET /graph"],
        ),
        global_scope_evidence: None,
        rationale: "Subject/predicate/object triples scoped to one agent. Despite the name it \
                    holds no FK to `memories`, so it is not lineage.",
    },
    TableEntry {
        table: "memory_retrieval_logs",
        schema_contract: &[primary_key("memory_retrieval_logs_pkey", &["id"])],
        row_identity: SURROGATE_ID,
        session_semantics: Some(SessionSemantics { role: SessionRole::ContextOnly, evidence: "Two insert paths exist and one omits session_id entirely (`INSERT INTO memory_retrieval_logs (agent_id, query_hash, injected_memory_ids)`), so a log line is well-formed without a session. No read predicate uses it." }),
        class: TableClass::DirectAgentChild,
        canonical_path: Some(agent_text("agent_id", false)),
        secondary_paths: &[],
        consistency: Some(
            "Measured, then deliberately excluded from ownership. `sessions` rows are created by \
             exactly one code path in the codebase, the working-memory upsert, and it runs \
             at the *end* of successful extraction. A pending job, a permanently failed job, \
             or a memory written before that upsert therefore legitimately references a \
             session that does not exist. Registering the session as an ownership or \
             consistency path would emit blocking ORPHANED_SESSION_REFERENCE findings on \
             entirely normal states, so it is recorded as context only rather than dropped \
             silently: CONTEXT_ONLY says *investigated and non-authoritative*, where an \
             absent conclusion would only say *not investigated*.",
        ),
        tranche: Tranche::Memories,
        plan: agent_child_plan(
            "UPDATE memory_retrieval_logs l SET agent_uuid = a.id, tenant_id = a.tenant_id \
             FROM agents a WHERE a.agent_id = l.agent_id",
            "VALIDATE CONSTRAINT after backfill",
            "append-heavy; also carries `query_text`, which the report must never echo",
            &["sessions (tranche 2)"],
            &["retrieval path", "GET /memories/search"],
        ),
        global_scope_evidence: None,
        rationale: "Per-agent retrieval telemetry. Its `candidate_memory_ids` arrays are not \
                    foreign keys and are deliberately not treated as an ownership path.",
    },
    TableEntry {
        table: "memory_versions",
        schema_contract: &[
            primary_key("memory_versions_pkey", &["id"]),
            foreign_key(
                "memory_versions_memory_id_fkey",
                &["memory_id"],
                "memories",
                &["id"],
            ),
        ],
        row_identity: SURROGATE_ID,
        session_semantics: None,
        class: TableClass::MemoryLineageChild,
        canonical_path: Some(memory_path("memory_id", false)),
        secondary_paths: &[agent_text("agent_id", false)],
        consistency: Some(
            "The denormalised `agent_id` must name the same agent the parent memory does. A \
             version that claims a different agent than its memory is a blocking disagreement.",
        ),
        tranche: Tranche::LineageAndArchival,
        plan: MigrationPlan {
            initial_nullability: NULLABLE_THEN_TIGHTENED,
            backfill_source: "UPDATE memory_versions v SET tenant_id = m.tenant_id FROM memories \
                              m WHERE m.id = v.memory_id",
            required_zero_codes: LINEAGE_CODES,
            validate_step: Some("VALIDATE CONSTRAINT after backfill"),
            future_uniqueness: Some(
                "`memory_versions_memory_id_version_number_key` stays scoped to the memory. \
                 Widening it to the tenant would be wrong: version numbers are per-memory.",
            ),
            future_unique_columns: None,
            lock_profile: "potentially the second-largest table; batch the backfill",
            rollback_dependencies: &["memories (tranche 3)"],
            inactive_paths: &["POST /memories", "time-travel read paths"],
        },
        global_scope_evidence: None,
        rationale: "`memory_id` is a NOT NULL FK to `memories`; the version's tenant is the \
                    memory's tenant by definition.",
    },
    TableEntry {
        table: "retrieval_feedback",
        schema_contract: &[
            primary_key("retrieval_feedback_pkey", &["id"]),
            foreign_key(
                "retrieval_feedback_memory_id_fkey",
                &["memory_id"],
                "memories",
                &["id"],
            ),
        ],
        row_identity: SURROGATE_ID,
        session_semantics: None,
        class: TableClass::DirectAgentChild,
        canonical_path: Some(agent_text("agent_id", false)),
        secondary_paths: &[memory_path("memory_id", true)],
        consistency: Some(
            "`memory_id` is `ON DELETE SET NULL`, so it is absent for feedback whose memory has \
             been deleted and cannot be canonical. Where present it must agree with the agent.",
        ),
        tranche: Tranche::LineageAndArchival,
        plan: agent_child_plan(
            "UPDATE retrieval_feedback f SET agent_uuid = a.id, tenant_id = a.tenant_id \
             FROM agents a WHERE a.agent_id = f.agent_id",
            "VALIDATE both after backfill",
            "small; the FK to memories must tolerate a NULL memory_id (MATCH SIMPLE)",
            &["memories (tranche 3)"],
            &["retrieval feedback endpoint", "rmk_worker"],
        ),
        global_scope_evidence: None,
        rationale: "Owned by the agent that gave the feedback; the memory reference survives the \
                    memory's deletion as NULL, which is exactly why it is secondary.",
    },
    TableEntry {
        table: "rmk_episodes",
        schema_contract: &[
            primary_key("rmk_episodes_pkey", &["id"]),
            foreign_key(
                "rmk_episodes_policy_id_fkey",
                &["policy_id"],
                "rmk_policies",
                &["id"],
            ),
        ],
        row_identity: SURROGATE_ID,
        session_semantics: Some(SessionSemantics { role: SessionRole::ContextOnly, evidence: "Read by `WHERE agent_id = $1 ORDER BY session_id`: the session is a grouping key *within* an agent's episodes, not an addressing key. The owner is the agent." }),
        class: TableClass::DirectAgentChild,
        canonical_path: Some(agent_text("agent_id", false)),
        secondary_paths: &[
            OwnershipPath {
                label: "row.policy_id -> rmk_policies.id -> rmk_policies.agent_id -> tenant",
                kind: PathKind::Policy {
                    column: "policy_id",
                },
                nullable: true,
            },
        ],
        consistency: Some(
            "An episode's policy, where set, must belong to the same agent. `policy_id` is \
             `ON DELETE SET NULL`, so it cannot be canonical.",
        ),
        tranche: Tranche::Operations,
        plan: agent_child_plan(
            "UPDATE rmk_episodes e SET agent_uuid = a.id, tenant_id = a.tenant_id \
             FROM agents a WHERE a.agent_id = e.agent_id",
            "VALIDATE both after backfill",
            "append-heavy training log",
            &["rmk_policies (tranche 1)", "sessions (tranche 2)"],
            WORKER_PATHS,
        ),
        global_scope_evidence: None,
        rationale: "Reinforcement episodes recorded per agent; the policy is a secondary link \
                    that may be NULL after a policy is deleted.",
    },
    TableEntry {
        table: "rmk_policies",
        schema_contract: &[primary_key("rmk_policies_pkey", &["id"])],
        row_identity: SURROGATE_ID,
        session_semantics: None,
        class: TableClass::DirectAgentChild,
        canonical_path: Some(agent_text("agent_id", false)),
        secondary_paths: &[],
        consistency: None,
        tranche: Tranche::RootsAndDirectAgentChildren,
        plan: MigrationPlan {
            initial_nullability: NULLABLE_THEN_TIGHTENED,
            backfill_source: "UPDATE rmk_policies p SET agent_uuid = a.id, tenant_id = \
                              a.tenant_id FROM agents a WHERE a.agent_id = p.agent_id",
            required_zero_codes: OWNERSHIP_CODES,
            validate_step: Some("VALIDATE CONSTRAINT after backfill"),
            future_uniqueness: Some("Adds UNIQUE (id, tenant_id) as the FK target for episodes."),
            future_unique_columns: None,
            lock_profile: "small",
            rollback_dependencies: &["rmk_episodes (tranche 5)"],
            inactive_paths: WORKER_PATHS,
        },
        global_scope_evidence: None,
        rationale: "One policy set per agent; must precede `rmk_episodes`, which references it.",
    },
    TableEntry {
        table: "sessions",
        schema_contract: &[
            primary_key("sessions_pkey", &["id"]),
            // A standalone unique *index*, not a constraint — it has no
            // pg_constraint row, so a constraint-only verifier would report
            // false drift on a healthy schema. Every session ownership path
            // joins on it, and a partial or invalid index would not guarantee
            // the single match that join assumes.
            unique_index("idx_sessions_agent_session", &["agent_id", "session_id"]),
            foreign_key(
                "sessions_agent_id_fkey",
                &["agent_id"],
                "agents",
                &["agent_id"],
            ),
        ],
        row_identity: SURROGATE_ID,
        session_semantics: None,
        class: TableClass::DirectAgentChild,
        canonical_path: Some(agent_text("agent_id", false)),
        secondary_paths: &[],
        consistency: Some(
            "A session's owner is its agent. This table is a direct agent child, not a session \
             child — it is the parent every session child resolves through.",
        ),
        tranche: Tranche::Sessions,
        plan: MigrationPlan {
            initial_nullability: NULLABLE_THEN_TIGHTENED,
            backfill_source: "UPDATE sessions s SET agent_uuid = a.id, tenant_id = a.tenant_id \
                              FROM agents a WHERE a.agent_id = s.agent_id",
            required_zero_codes: OWNERSHIP_CODES,
            validate_step: Some("VALIDATE CONSTRAINT after backfill"),
            future_uniqueness: Some(
                "Session identity stays AGENT-scoped. `idx_sessions_agent_session` is UNIQUE \
                 (agent_id, session_id) today and becomes UNIQUE (tenant_id, agent_uuid, \
                 session_id) as `sessions_tenant_agent_session_key`, which is the FK target \
                 `working_memory` references. A tenant-scoped UNIQUE (tenant_id, session_id) is \
                 explicitly **rejected**: `session_id` is caller-supplied TEXT, so two agents in \
                 one tenant sharing the string `default` is ordinary caller behaviour, and \
                 widening identity to the tenant would make that a collision. Because the new \
                 tuple is a superset of the current one, the change is a relaxation and cannot \
                 collide.",
            ),
            // Grouped by tenant, agent and session — the tuple the future key
            // actually covers. Expected to stay at zero while the current unique
            // index holds; it is a defensive pre-check, not a prediction.
            future_unique_columns: None,
            lock_profile: "ADD COLUMN NULL is metadata-only; a new UNIQUE index should be built \
                           CONCURRENTLY outside the migration transaction",
            rollback_dependencies: &[
                "working_memory",
                "memories",
                "memory_retrieval_logs",
                "extraction_jobs",
                "cognitive_hypervisor_timeline",
                "rmk_episodes",
            ],
            inactive_paths: &["POST /sessions", "every session-scoped write"],
        },
        global_scope_evidence: None,
        rationale: "Owned by its agent through a real FK to `agents(agent_id)`. Everything \
                    session-scoped resolves through `(agent_id, session_id)`, which is the only \
                    unique key on this table besides its surrogate primary key.",
    },
    TableEntry {
        table: "working_memory",
        schema_contract: &[
            primary_key("working_memory_pkey", &["id"]),
            unique_constraint(
                "working_memory_agent_id_session_id_key",
                &["agent_id", "session_id"],
            ),
        ],
        row_identity: SURROGATE_ID,
        session_semantics: Some(SessionSemantics { role: SessionRole::Canonical, evidence: "INSERT carries session_id; rows are deleted by `WHERE agent_id = $1 AND session_id = $2`, i.e. addressed *by* their session; UNIQUE (agent_id, session_id) is exactly the session's own unique key. The session is what identifies the row, so a missing one is a broken row rather than absent context." }),
        class: TableClass::SessionChild,
        canonical_path: Some(session_path("agent_id", "session_id", false)),
        secondary_paths: &[agent_text("agent_id", false)],
        consistency: Some(
            "The only table whose session reference is NOT NULL, and whose own unique key \
             `(agent_id, session_id)` is exactly the session's unique key. The denormalised \
             agent must match the session's agent. OPERATIONAL CAVEAT: `working_memory` and \
             its `sessions` row are written by two consecutive statements, not one atomic \
             unit - the upsert writes working memory first and the session second. A live \
             audit can therefore observe the brief state between them and report a session \
             orphan that is about to exist. The finding is deliberately left BLOCKING, because \
             a persistent orphan is a real inconsistency and weakening it would hide that, so \
             a migration-readiness scan must be run with extraction writers quiesced or \
             drained, or re-run once activity settles.",
        ),
        tranche: Tranche::Sessions,
        plan: MigrationPlan {
            initial_nullability: NULLABLE_THEN_TIGHTENED,
            backfill_source: "UPDATE working_memory w SET agent_uuid = s.agent_uuid, tenant_id = \
                              s.tenant_id FROM sessions s WHERE s.agent_id = w.agent_id AND \
                              s.session_id = w.session_id",
            required_zero_codes: SESSION_CHILD_CODES,
            validate_step: Some("VALIDATE CONSTRAINT after backfill"),
            future_uniqueness: Some(
                "`working_memory_agent_id_session_id_key` moves with `sessions` and stays \
                 agent-scoped: UNIQUE (tenant_id, agent_uuid, session_id). It also gains a real \
                 composite foreign key — (tenant_id, agent_uuid, session_id) REFERENCES \
                 sessions(tenant_id, agent_uuid, session_id) — so the session reference is \
                 enforced by the database rather than by convention. The two uniqueness rules \
                 must move together or they disagree about what identifies a session.",
            ),
            future_unique_columns: None,
            lock_profile: "one row per active session; small",
            rollback_dependencies: &["sessions (tranche 2)"],
            inactive_paths: &["every session-scoped write"],
        },
        global_scope_evidence: None,
        rationale: "The one genuine session child: `session_id` is NOT NULL and the row is \
                    meaningless without its session.",
    },
];

// ── Live discovery ──────────────────────────────────────────────────────────

/// A table as PostgreSQL actually reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DiscoveredTable {
    pub name: String,
    pub columns: Vec<DiscoveredColumn>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DiscoveredColumn {
    pub name: String,
    pub data_type: String,
    pub not_null: bool,
}

/// An object that exists but is deliberately not an application table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ExcludedObject {
    pub name: String,
    pub reason: &'static str,
}

/// What the live schema contains.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DiscoveredSchema {
    pub schema: String,
    pub application_tables: Vec<DiscoveredTable>,
    pub excluded: Vec<ExcludedObject>,
}

impl DiscoveredSchema {
    /// Consumed by the Step 4A tests and by Step 4B's migration tooling; the
    /// binary has no caller while the diagnostic surface stays unwired.
    #[allow(dead_code)]
    pub fn table_names(&self) -> Vec<&str> {
        self.application_tables
            .iter()
            .map(|t| t.name.as_str())
            .collect()
    }

    pub fn table(&self, name: &str) -> Option<&DiscoveredTable> {
        self.application_tables.iter().find(|t| t.name == name)
    }
}

impl DiscoveredTable {
    pub fn column(&self, name: &str) -> Option<&DiscoveredColumn> {
        self.columns.iter().find(|c| c.name == name)
    }
}

pub const EXCLUSION_SQLX_LEDGER: &str = "sqlx migration ledger";
pub const EXCLUSION_EXTENSION: &str = "owned by an installed extension";
pub const EXCLUSION_TEMPORARY: &str = "temporary table";

/// The schema the application lives in. Not configurable at Step 4A: the whole
/// credential subsystem pins `search_path` to `pg_catalog, public`, and an
/// audit that looked somewhere else would be auditing a different database than
/// the one the triggers defend.
pub const APPLICATION_SCHEMA: &str = "public";

/// Read the live catalogs and report every persistent application table.
///
/// Deliberately *not* derived from the migration files. Migration `0028` adds
/// `agents.tenant_id` with `ALTER TABLE`, and `0031` renames
/// `credential_agent_grants.agent_id` to `agent_uuid` — neither is visible in a
/// `CREATE TABLE` statement, so a migration-text scan reports a schema that has
/// not existed since step 1.
///
/// `relkind IN ('r','p')` covers ordinary and partitioned tables. Views,
/// materialised views, indexes, sequences and foreign tables are not tables and
/// are not reported as excluded objects either — they were never candidates.
pub async fn discover(
    conn: &mut sqlx::PgConnection,
    schema: &str,
) -> Result<DiscoveredSchema, sqlx::Error> {
    use sqlx::Row;

    let rows = sqlx::query(
        "SELECT c.relname::text AS name, \
                c.relpersistence = 't' AS is_temp, \
                EXISTS (SELECT 1 FROM pg_depend d \
                         WHERE d.classid = 'pg_class'::regclass \
                           AND d.objid = c.oid AND d.deptype = 'e') AS is_extension \
           FROM pg_class c \
           JOIN pg_namespace n ON n.oid = c.relnamespace \
          WHERE c.relkind IN ('r', 'p') AND n.nspname = $1 \
          ORDER BY c.relname",
    )
    .bind(schema)
    .fetch_all(&mut *conn)
    .await?;

    let mut application_tables = Vec::new();
    let mut excluded = Vec::new();

    for row in rows {
        let name: String = row.try_get("name")?;
        let is_temp: bool = row.try_get("is_temp")?;
        let is_extension: bool = row.try_get("is_extension")?;

        let reason = if name == "_sqlx_migrations" {
            Some(EXCLUSION_SQLX_LEDGER)
        } else if is_extension {
            Some(EXCLUSION_EXTENSION)
        } else if is_temp {
            Some(EXCLUSION_TEMPORARY)
        } else {
            None
        };

        match reason {
            Some(reason) => excluded.push(ExcludedObject { name, reason }),
            None => application_tables.push(DiscoveredTable {
                name,
                columns: Vec::new(),
            }),
        }
    }

    // Columns in one pass rather than a query per table: the audit runs inside
    // a single read-only snapshot, and a round trip per table would be
    // twenty-two of them for no additional truth.
    let column_rows = sqlx::query(
        "SELECT c.relname::text AS table_name, a.attname::text AS column_name, \
                format_type(a.atttypid, a.atttypmod) AS data_type, a.attnotnull AS not_null \
           FROM pg_class c \
           JOIN pg_namespace n ON n.oid = c.relnamespace \
           JOIN pg_attribute a ON a.attrelid = c.oid \
          WHERE c.relkind IN ('r', 'p') AND n.nspname = $1 \
            AND a.attnum > 0 AND NOT a.attisdropped \
          ORDER BY c.relname, a.attnum",
    )
    .bind(schema)
    .fetch_all(&mut *conn)
    .await?;

    for row in column_rows {
        let table_name: String = row.try_get("table_name")?;
        let Some(table) = application_tables.iter_mut().find(|t| t.name == table_name) else {
            continue;
        };
        table.columns.push(DiscoveredColumn {
            name: row.try_get("column_name")?,
            data_type: row.try_get("data_type")?,
            not_null: row.try_get("not_null")?,
        });
    }

    Ok(DiscoveredSchema {
        schema: schema.to_string(),
        application_tables,
        excluded,
    })
}

/// Look a table up in the registry.
/// Consumed by the Step 4A tests and by Step 4B's migration tooling; the
/// binary has no caller while the diagnostic surface stays unwired.
#[allow(dead_code)]
pub fn entry(table: &str) -> Option<&'static TableEntry> {
    REGISTRY.iter().find(|e| e.table == table)
}

/// How many registry entries name this table. More than one is
/// `MULTIPLE_CLASSIFICATIONS`.
pub fn entry_count(table: &str) -> usize {
    REGISTRY.iter().filter(|e| e.table == table).count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_registry_is_sorted_and_free_of_duplicates() {
        // Ordering is what makes the rendered artifacts stable, and a duplicate
        // is the condition MULTIPLE_CLASSIFICATIONS exists to catch — worth
        // failing without a database rather than only with one.
        let names: Vec<&str> = REGISTRY.iter().map(|e| e.table).collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        assert_eq!(names, sorted, "REGISTRY must be ordered by table name");

        let mut unique = sorted.clone();
        unique.dedup();
        assert_eq!(unique.len(), names.len(), "REGISTRY has a duplicate table");
    }

    #[test]
    fn every_non_system_global_table_has_exactly_one_canonical_path() {
        for e in REGISTRY {
            match e.class {
                TableClass::SystemGlobal => {
                    assert!(
                        e.canonical_path.is_none(),
                        "{}: SYSTEM_GLOBAL must not claim an ownership path",
                        e.table
                    );
                    assert!(
                        e.global_scope_evidence.is_some(),
                        "{}: SYSTEM_GLOBAL requires stated evidence",
                        e.table
                    );
                }
                _ => assert!(
                    e.canonical_path.is_some(),
                    "{}: needs exactly one canonical ownership path",
                    e.table
                ),
            }
        }
    }

    #[test]
    fn only_system_global_tables_carry_global_scope_evidence() {
        for e in REGISTRY {
            if e.class != TableClass::SystemGlobal {
                assert!(
                    e.global_scope_evidence.is_none(),
                    "{}: only SYSTEM_GLOBAL tables take global-scope evidence",
                    e.table
                );
            }
        }
    }

    #[test]
    fn a_secondary_path_never_duplicates_the_canonical_one() {
        // Two identical paths would "agree" trivially and the consistency check
        // would be worthless.
        for e in REGISTRY {
            let Some(canonical) = e.canonical_path else {
                continue;
            };
            for secondary in e.secondary_paths {
                assert_ne!(
                    canonical.kind, secondary.kind,
                    "{}: secondary path repeats the canonical one",
                    e.table
                );
            }
        }
    }

    #[test]
    fn every_table_with_secondary_paths_states_what_agreement_means() {
        // The rule the review insisted on: a secondary path is only acceptable
        // if the registry says what agreement means, so the scanner has
        // something to report a disagreement *against* rather than a reason to
        // prefer one path.
        for e in REGISTRY {
            if !e.secondary_paths.is_empty() {
                assert!(
                    e.consistency.is_some(),
                    "{}: has secondary paths but no consistency predicate",
                    e.table
                );
            }
        }
    }

    #[test]
    fn tenant_root_is_reserved_for_the_ownership_terminus() {
        // `credentials` also carries an authoritative NOT NULL tenant_id, so
        // the distinction is not "has a tenant column" — it is "is what other
        // tables resolve to". Only `agents` is.
        let roots: Vec<&str> = REGISTRY
            .iter()
            .filter(|e| e.class == TableClass::TenantRoot)
            .map(|e| e.table)
            .collect();
        assert_eq!(roots, vec!["agents"]);
    }
}
