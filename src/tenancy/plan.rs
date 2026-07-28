//! The typed Step 4B migration contract.
//!
//! Step 4A produced the ownership map. This module turns the *plan* half of that
//! map from prose into data: every column, index, unique target, foreign key and
//! constraint Step 4B intends to create is a typed value with an owning tranche,
//! and the human-readable rendering in the artifacts is generated from those
//! values rather than hand-written beside them.
//!
//! The reason is not tidiness. The Step 4A plan carried its future schema as
//! strings like `"FOREIGN KEY (tenant_id, agent_uuid) REFERENCES agents(tenant_id,
//! id)"`, and two foreign keys named a parent tuple that no planned unique target
//! covered — `archival_batches(id, tenant_id)` and `entities(id, tenant_id)`.
//! PostgreSQL rejects such a key outright, so both migrations would have failed on
//! first execution. Nothing could have caught it, because checking would have
//! meant parsing the prose. The invariants below now check it as data.
//!
//! ## What this module deliberately does not do
//!
//! It creates nothing. There is no DDL here, no migration file, and no backfill.
//! Step 4B-0 is the contract; Step 4B-1 onward executes against it.

use serde::Serialize;

use super::audit::ReasonCode;
use super::inventory::Tranche;

// ── The three-stage protocol ────────────────────────────────────────────────

/// Which stage of a tranche a piece of work belongs to.
///
/// A tranche is not one migration. Splitting it into three is what makes the
/// backfill safe to resume and impossible to skip: `VALIDATE CONSTRAINT` is the
/// step that turns "we believe every row is owned" into a database-enforced
/// fact, and running it in the same release as the `ADD COLUMN` would validate a
/// table nobody had backfilled yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Stage {
    /// Add nullable columns, install the bridge trigger, build indexes
    /// concurrently, and add constraints `NOT VALID` so that *new* writes are
    /// already constrained while historical rows are not yet.
    Prepare,
    /// Resumable bounded batches with a checkpoint, until the Step 4A audit
    /// reports zero for that table's required codes. Not a migration — an
    /// operational command that can be stopped and restarted.
    Backfill,
    /// `VALIDATE CONSTRAINT`, move every newly current object into the table's
    /// `schema_contract`, regenerate the artifacts, and re-run the audit.
    Finalize,
}

impl Stage {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Prepare => "PREPARE",
            Self::Backfill => "BACKFILL",
            Self::Finalize => "FINALIZE",
        }
    }

    pub const ALL: &'static [Stage] = &[Self::Prepare, Self::Backfill, Self::Finalize];
}

impl std::fmt::Display for Stage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// How a stage is executed, and what stops it running out of order.
///
/// `sqlx migrate run` applies every pending migration in one pass. If PREPARE
/// and FINALIZE were both plain migrations, a fresh deployment would run them
/// back to back and validate a constraint over rows no backfill had ever touched
/// — on an empty database that silently succeeds, which is the worst possible
/// outcome because it makes the gate look green.
///
/// FINALIZE therefore begins by asserting a recorded backfill completion for its
/// tranche, and raises if it is absent. The check lives in the migration itself
/// rather than in an external runner, so it holds however the migration is
/// invoked.
pub const FINALIZE_GUARD: &str = "FINALIZE migrations open with a guard that raises unless \
     agent_tenancy_migrations records a completed backfill checkpoint for the tranche. \
     `sqlx migrate run` therefore cannot advance PREPARE straight into FINALIZE: on a fresh \
     database the guard fires because no backfill ran, and on a live database it fires until \
     the operator's backfill command records completion.";

/// `CREATE INDEX CONCURRENTLY` cannot run inside a transaction block, and sqlx
/// wraps each migration in one by default.
pub const CONCURRENT_BUILD_MECHANISM: &str = "A migration containing CREATE INDEX CONCURRENTLY \
     must carry `-- no-transaction` as its first line so sqlx runs it unwrapped. A concurrent \
     build that fails leaves an INVALID index behind, which the Step 4A verifier already \
     reports as drift, so a failed build cannot be mistaken for a healthy one.";

// ── Lock profiles ───────────────────────────────────────────────────────────

/// The lock a planned operation actually takes.
///
/// Typed rather than prose because the Step 4A plan described `VALIDATE
/// CONSTRAINT` as taking `SHARE ROW EXCLUSIVE`, which is the lock `ADD
/// CONSTRAINT` takes. `VALIDATE` takes the weaker `SHARE UPDATE EXCLUSIVE`, and
/// the difference is the whole point of splitting them: the strong lock is held
/// only for the instant the constraint is declared, and the long scan runs under
/// a lock that still permits `INSERT`, `UPDATE` and `DELETE`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum LockProfile {
    /// Metadata-only since PostgreSQL 11 when the default is NULL or constant:
    /// catalog update, no table rewrite.
    AddColumnNullable,
    /// Runs outside a transaction. Two table scans, `SHARE UPDATE EXCLUSIVE`,
    /// concurrent reads and writes continue.
    CreateIndexConcurrently,
    /// `SHARE ROW EXCLUSIVE` on the child **and** on the referenced table.
    /// Blocks writes to both for the duration of the catalog change, which with
    /// `NOT VALID` is brief because no rows are examined.
    AddForeignKeyNotValid,
    /// `SHARE UPDATE EXCLUSIVE` on the child and `ROW SHARE` on the referenced
    /// table. Scans every row, but concurrent DML continues throughout.
    ValidateConstraint,
    /// `ACCESS EXCLUSIVE`. Scans the table unless an already-validated CHECK
    /// lets PostgreSQL 12+ skip the scan.
    SetNotNull,
    /// `ACCESS EXCLUSIVE` for the full rewrite. The table is unavailable.
    TableRewrite,
    /// `ACCESS EXCLUSIVE`, but brief: dropping a column is a catalog update.
    DropColumn,
}

impl LockProfile {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AddColumnNullable => "ADD COLUMN NULL",
            Self::CreateIndexConcurrently => "CREATE INDEX CONCURRENTLY",
            Self::AddForeignKeyNotValid => "ADD CONSTRAINT ... NOT VALID",
            Self::ValidateConstraint => "VALIDATE CONSTRAINT",
            Self::SetNotNull => "SET NOT NULL",
            Self::TableRewrite => "table rewrite",
            Self::DropColumn => "DROP COLUMN",
        }
    }

    /// The exact locks taken, on the operated table and on any referenced one.
    pub const fn locks(self) -> &'static str {
        match self {
            Self::AddColumnNullable => "ACCESS EXCLUSIVE, catalog-only — no rewrite, no scan",
            Self::CreateIndexConcurrently => {
                "SHARE UPDATE EXCLUSIVE; must run outside a transaction block"
            }
            Self::AddForeignKeyNotValid => {
                "SHARE ROW EXCLUSIVE on the child AND on the referenced table"
            }
            Self::ValidateConstraint => {
                "SHARE UPDATE EXCLUSIVE on the child, ROW SHARE on the referenced table"
            }
            Self::SetNotNull => "ACCESS EXCLUSIVE; full scan unless a validated CHECK permits skip",
            Self::TableRewrite => "ACCESS EXCLUSIVE for the whole rewrite",
            Self::DropColumn => "ACCESS EXCLUSIVE, brief — catalog update only",
        }
    }

    pub const ALL: &'static [LockProfile] = &[
        Self::AddColumnNullable,
        Self::CreateIndexConcurrently,
        Self::AddForeignKeyNotValid,
        Self::ValidateConstraint,
        Self::SetNotNull,
        Self::TableRewrite,
        Self::DropColumn,
    ];
}

// ── Column nullability ──────────────────────────────────────────────────────

/// What a planned column's nullability is now, and what it becomes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Nullability {
    /// Added NULL-able; tightened to NOT NULL only in FUTURE STEP 7, after the
    /// audit reports zero blocking findings for the table.
    NullableThenTightened,
    /// Added NULL-able and **stays** NULL-able. `audit_logs` records agentless
    /// events, which are legitimate rows with no owner: tightening this column
    /// would make the schema reject valid audit history.
    RemainsNullable,
}

impl Nullability {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NullableThenTightened => "NULL-able now; NOT NULL in FUTURE STEP 7",
            Self::RemainsNullable => "NULL-able, and stays NULL-able",
        }
    }
}

/// Whether a unique target already exists or has to be created.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum TargetStatus {
    /// Present in the live schema today and verified by the Step 4A audit.
    AlreadyCurrent,
    /// Created by Step 4B in its owning tranche.
    CreatedByStep4b,
}

/// The kind of a planned non-key constraint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum PlannedConstraintKind {
    /// A `CHECK (col IS NOT NULL) NOT VALID`, validated separately, so the later
    /// `SET NOT NULL` can skip its full scan.
    NotNullPrecheck,
}

// ── The typed planned object ────────────────────────────────────────────────

/// One schema object Step 4B intends to create.
///
/// Every variant records the same facts, so an invariant can reason across all
/// of them without knowing which kind it is holding: which table it belongs to,
/// its expected name, its ordered local columns, what it references, whether it
/// is unique, its nullability, the tranche that creates it, the tranche that
/// validates it, whether `NOT VALID` is permitted, and whether the build must be
/// concurrent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "planned", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum PlannedObject {
    Column {
        table: &'static str,
        name: &'static str,
        sql_type: &'static str,
        nullability: Nullability,
        creating_tranche: Tranche,
    },
    Index {
        table: &'static str,
        name: &'static str,
        columns: &'static [&'static str],
        unique: bool,
        creating_tranche: Tranche,
    },
    /// A unique constraint or index that exists *because* a planned foreign key
    /// needs something to point at. PostgreSQL requires the referenced columns
    /// of a foreign key to be covered by a unique constraint or index; without
    /// one the `ADD CONSTRAINT` fails outright.
    UniqueTarget {
        table: &'static str,
        name: &'static str,
        columns: &'static [&'static str],
        status: TargetStatus,
        creating_tranche: Tranche,
    },
    ForeignKey {
        table: &'static str,
        name: &'static str,
        local_columns: &'static [&'static str],
        referenced_table: &'static str,
        referenced_columns: &'static [&'static str],
        creating_tranche: Tranche,
        validating_tranche: Tranche,
        /// True when at least one local column is NULL-able. Under the default
        /// `MATCH SIMPLE`, a row with any NULL in the key is **not** checked
        /// against the parent at all — so the foreign key is not evidence that
        /// those rows are owned. The audit is.
        unenforced_when_null: bool,
    },
    Constraint {
        table: &'static str,
        name: &'static str,
        kind: PlannedConstraintKind,
        columns: &'static [&'static str],
        creating_tranche: Tranche,
        validating_tranche: Tranche,
    },
}

impl PlannedObject {
    pub const fn table(&self) -> &'static str {
        match self {
            Self::Column { table, .. }
            | Self::Index { table, .. }
            | Self::UniqueTarget { table, .. }
            | Self::ForeignKey { table, .. }
            | Self::Constraint { table, .. } => table,
        }
    }

    pub const fn name(&self) -> &'static str {
        match self {
            Self::Column { name, .. }
            | Self::Index { name, .. }
            | Self::UniqueTarget { name, .. }
            | Self::ForeignKey { name, .. }
            | Self::Constraint { name, .. } => name,
        }
    }

    /// Ordered local columns. Order is part of the guarantee: a unique target on
    /// `(a, b)` does not satisfy a foreign key referencing `(b, a)`.
    pub const fn local_columns(&self) -> &'static [&'static str] {
        match self {
            Self::Column { .. } => &[],
            Self::Index { columns, .. }
            | Self::UniqueTarget { columns, .. }
            | Self::Constraint { columns, .. } => columns,
            Self::ForeignKey { local_columns, .. } => local_columns,
        }
    }

    /// The referenced table and its ordered columns, where one applies.
    pub const fn referenced(&self) -> Option<(&'static str, &'static [&'static str])> {
        match self {
            Self::ForeignKey {
                referenced_table,
                referenced_columns,
                ..
            } => Some((referenced_table, referenced_columns)),
            _ => None,
        }
    }

    pub const fn is_unique(&self) -> bool {
        match self {
            Self::Index { unique, .. } => *unique,
            Self::UniqueTarget { .. } => true,
            _ => false,
        }
    }

    pub const fn nullability(&self) -> Option<Nullability> {
        match self {
            Self::Column { nullability, .. } => Some(*nullability),
            _ => None,
        }
    }

    pub const fn creating_tranche(&self) -> Tranche {
        match self {
            Self::Column {
                creating_tranche, ..
            }
            | Self::Index {
                creating_tranche, ..
            }
            | Self::UniqueTarget {
                creating_tranche, ..
            }
            | Self::ForeignKey {
                creating_tranche, ..
            }
            | Self::Constraint {
                creating_tranche, ..
            } => *creating_tranche,
        }
    }

    /// The tranche whose FINALIZE stage validates this object. Equal to the
    /// creating tranche for everything that is not deliberately deferred.
    pub const fn validating_tranche(&self) -> Tranche {
        match self {
            Self::ForeignKey {
                validating_tranche, ..
            }
            | Self::Constraint {
                validating_tranche, ..
            } => *validating_tranche,
            _ => self.creating_tranche(),
        }
    }

    /// Whether this object may be added `NOT VALID` and validated later.
    ///
    /// Only foreign keys and CHECK constraints support it. An index cannot be
    /// "not valid" in this sense — a failed concurrent build leaves an INVALID
    /// index, which is a fault, not a planned intermediate state.
    pub const fn not_valid_permitted(&self) -> bool {
        matches!(self, Self::ForeignKey { .. } | Self::Constraint { .. })
    }

    /// Whether the build must run outside a transaction block.
    pub const fn concurrent_build_required(&self) -> bool {
        matches!(self, Self::Index { .. } | Self::UniqueTarget { .. })
    }

    /// The lock this object's creation takes.
    pub const fn creation_lock(&self) -> LockProfile {
        match self {
            Self::Column { .. } => LockProfile::AddColumnNullable,
            // A unique target is built CONCURRENTLY and attached with
            // `ADD CONSTRAINT ... USING INDEX`, so the strong lock is held only
            // for the attach rather than for the whole build.
            Self::Index { .. } | Self::UniqueTarget { .. } => LockProfile::CreateIndexConcurrently,
            Self::ForeignKey { .. } | Self::Constraint { .. } => LockProfile::AddForeignKeyNotValid,
        }
    }

    /// The lock this object's validation takes, where it has one.
    pub const fn validation_lock(&self) -> Option<LockProfile> {
        if self.not_valid_permitted() {
            Some(LockProfile::ValidateConstraint)
        } else {
            None
        }
    }

    /// One line for the artifacts, generated from the typed fields.
    ///
    /// The rendering exists so a reader does not have to read Rust, but it is
    /// derived — there is no second copy of this information to drift from.
    pub fn describe(&self) -> String {
        match self {
            Self::Column {
                table,
                name,
                sql_type,
                nullability,
                ..
            } => format!(
                "ALTER TABLE {table} ADD COLUMN {name} {sql_type} NULL — {}",
                nullability.as_str()
            ),
            Self::Index {
                table,
                name,
                columns,
                unique,
                ..
            } => format!(
                "CREATE {}INDEX CONCURRENTLY {name} ON {table} ({})",
                if *unique { "UNIQUE " } else { "" },
                columns.join(", ")
            ),
            Self::UniqueTarget {
                table,
                name,
                columns,
                status,
                ..
            } => format!(
                "{table} UNIQUE ({}) AS `{name}` — {}",
                columns.join(", "),
                match status {
                    TargetStatus::AlreadyCurrent => "already current",
                    TargetStatus::CreatedByStep4b => "created by Step 4B as an FK target",
                }
            ),
            Self::ForeignKey {
                table,
                name,
                local_columns,
                referenced_table,
                referenced_columns,
                unenforced_when_null,
                ..
            } => format!(
                "ALTER TABLE {table} ADD CONSTRAINT {name} FOREIGN KEY ({}) REFERENCES \
                 {referenced_table} ({}) NOT VALID{}",
                local_columns.join(", "),
                referenced_columns.join(", "),
                if *unenforced_when_null {
                    " — MATCH SIMPLE: rows with a NULL key component are not checked"
                } else {
                    ""
                }
            ),
            Self::Constraint {
                table,
                name,
                columns,
                ..
            } => format!(
                "ALTER TABLE {table} ADD CONSTRAINT {name} CHECK ({} IS NOT NULL) NOT VALID",
                columns.join(" IS NOT NULL AND ")
            ),
        }
    }
}

// ── The transitional write strategy ─────────────────────────────────────────

/// How a table's new ownership columns stay populated between PREPARE and
/// FUTURE STEP 7.
///
/// Quiescing writers during the backfill is necessary but **not sufficient**.
/// The backfill covers rows that exist when it runs; the question this type
/// answers is what happens to row number one written *after* it finishes, by a
/// writer that knows nothing about the new columns. Without an answer the
/// backfill converges on zero and then immediately diverges again, and the `SET
/// NOT NULL` in step 7 fails against rows created in the interval.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "strategy", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum TransitionalWrite {
    /// A `BEFORE INSERT OR UPDATE` trigger that resolves the ownership columns
    /// from `agents` whenever they arrive NULL, and raises when they arrive
    /// contradicting the legacy identifier.
    BridgeTrigger {
        trigger: &'static str,
        function: &'static str,
    },
    /// Ownership may legitimately be absent, so no bridge is installed.
    OwnershipMayRemainNull { reason: &'static str },
}

impl TransitionalWrite {
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::BridgeTrigger { .. } => "DATABASE_BRIDGE_TRIGGER",
            Self::OwnershipMayRemainNull { .. } => "OWNERSHIP_MAY_REMAIN_NULL",
        }
    }
}

/// Why the bridge is a trigger rather than application dual-write.
///
/// Both were considered. Dual-write was rejected on a property, not a
/// preference: it holds only while the version that implements it is the version
/// that is running.
pub const TRANSITIONAL_WRITE_RATIONALE: &str = "A database bridge trigger is chosen over \
     exhaustive application dual-write. Dual-write requires enumerating every writer and \
     keeping that enumeration exhaustive; it is defeated by a rollback to the previous \
     release, by a second service, by a maintenance script, and by any psql session. Each of \
     those silently reintroduces NULL ownership on new rows, and the failure surfaces later as \
     a SET NOT NULL that cannot succeed. The trigger is attached to the table, so it holds for \
     every writer including ones nobody enumerated, and it survives an application rollback \
     because it is schema rather than code. It is installed in PREPARE, before any backfill, \
     so no window exists in which new rows are unowned.";

/// The property the bridge has to have, stated so a test can check it.
pub const TRANSITIONAL_WRITE_GUARANTEE: &str = "After historical backfill completes, a resumed \
     legacy writer cannot create a row with NULL or contradictory ownership. The BEFORE trigger \
     fires on INSERT and on UPDATE: when agent_uuid or tenant_id arrives NULL it resolves both \
     from agents using the row's legacy agent_id; when they arrive non-NULL and disagree with \
     what agents says for that agent_id, it raises. A legacy writer that supplies only agent_id \
     therefore produces a fully owned row, and a writer that supplies a wrong tenant is \
     rejected rather than silently believed. The one remaining case that yields NULL is an \
     agent whose own tenant_id is NULL, which is exactly UNMAPPED_AGENT and is already a \
     blocking finding.";

// ── Per-table transitional strategy ─────────────────────────────────────────

const fn bridge(trigger: &'static str, function: &'static str) -> TransitionalWrite {
    TransitionalWrite::BridgeTrigger { trigger, function }
}

/// Every table receiving ownership columns, and how its new writes stay owned.
pub const TRANSITION: &[(&str, TransitionalWrite)] = &[
    // ── Tranche 1 ──
    (
        "archival_batches",
        bridge(
            "trg_archival_batches_tenancy_bridge",
            "fn_archival_batches_tenancy_bridge",
        ),
    ),
    (
        "audit_logs",
        TransitionalWrite::OwnershipMayRemainNull {
            reason: "audit_logs records agentless events — startup, configuration change, \
                     administrative action — which are valid rows with no owning agent. A bridge \
                     that forced ownership would either invent an owner or reject the event, and \
                     losing audit history is a worse outcome than an unowned audit row. Its \
                     ownership columns stay NULL-able permanently, and LEGACY_UNMAPPED is \
                     deliberately absent from its required-zero set.",
        },
    ),
    (
        "entities",
        bridge("trg_entities_tenancy_bridge", "fn_entities_tenancy_bridge"),
    ),
    (
        "memory_graph",
        bridge(
            "trg_memory_graph_tenancy_bridge",
            "fn_memory_graph_tenancy_bridge",
        ),
    ),
    (
        "rmk_policies",
        bridge(
            "trg_rmk_policies_tenancy_bridge",
            "fn_rmk_policies_tenancy_bridge",
        ),
    ),
    // ── Tranche 2 ──
    (
        "sessions",
        bridge("trg_sessions_tenancy_bridge", "fn_sessions_tenancy_bridge"),
    ),
    (
        "working_memory",
        bridge(
            "trg_working_memory_tenancy_bridge",
            "fn_working_memory_tenancy_bridge",
        ),
    ),
    // ── Tranche 3 ──
    (
        "memories",
        bridge("trg_memories_tenancy_bridge", "fn_memories_tenancy_bridge"),
    ),
    (
        "extraction_jobs",
        bridge(
            "trg_extraction_jobs_tenancy_bridge",
            "fn_extraction_jobs_tenancy_bridge",
        ),
    ),
    (
        "memory_retrieval_logs",
        bridge(
            "trg_memory_retrieval_logs_tenancy_bridge",
            "fn_memory_retrieval_logs_tenancy_bridge",
        ),
    ),
    (
        "cognitive_hypervisor_timeline",
        bridge("trg_cht_tenancy_bridge", "fn_cht_tenancy_bridge"),
    ),
    // ── Tranche 4 ──
    (
        "memory_versions",
        bridge(
            "trg_memory_versions_tenancy_bridge",
            "fn_memory_versions_tenancy_bridge",
        ),
    ),
    (
        "memory_entity_links",
        bridge(
            "trg_memory_entity_links_tenancy_bridge",
            "fn_memory_entity_links_tenancy_bridge",
        ),
    ),
    (
        "co_access_edges",
        bridge(
            "trg_co_access_edges_tenancy_bridge",
            "fn_co_access_edges_tenancy_bridge",
        ),
    ),
    (
        "memory_conflicts",
        bridge(
            "trg_memory_conflicts_tenancy_bridge",
            "fn_memory_conflicts_tenancy_bridge",
        ),
    ),
    (
        "retrieval_feedback",
        bridge(
            "trg_retrieval_feedback_tenancy_bridge",
            "fn_retrieval_feedback_tenancy_bridge",
        ),
    ),
    // ── Tranche 5 ──
    (
        "amp_controller_state",
        bridge(
            "trg_amp_controller_state_tenancy_bridge",
            "fn_amp_controller_state_tenancy_bridge",
        ),
    ),
    (
        "rmk_episodes",
        bridge(
            "trg_rmk_episodes_tenancy_bridge",
            "fn_rmk_episodes_tenancy_bridge",
        ),
    ),
];

/// The transitional strategy for one table, if it receives ownership columns.
pub fn transition_for(table: &str) -> Option<TransitionalWrite> {
    TRANSITION
        .iter()
        .find(|(name, _)| *name == table)
        .map(|(_, strategy)| *strategy)
}

// ── Planned-object constructors ─────────────────────────────────────────────

const AGENT_UUID: &str = "agent_uuid";
const TENANT_ID: &str = "tenant_id";

const fn col(table: &'static str, name: &'static str, tranche: Tranche) -> PlannedObject {
    PlannedObject::Column {
        table,
        name,
        sql_type: "UUID",
        nullability: Nullability::NullableThenTightened,
        creating_tranche: tranche,
    }
}

const fn col_permanent(table: &'static str, name: &'static str, tranche: Tranche) -> PlannedObject {
    PlannedObject::Column {
        table,
        name,
        sql_type: "UUID",
        nullability: Nullability::RemainsNullable,
        creating_tranche: tranche,
    }
}

const fn idx(
    table: &'static str,
    name: &'static str,
    columns: &'static [&'static str],
    tranche: Tranche,
) -> PlannedObject {
    PlannedObject::Index {
        table,
        name,
        columns,
        unique: false,
        creating_tranche: tranche,
    }
}

const fn target(
    table: &'static str,
    name: &'static str,
    columns: &'static [&'static str],
    tranche: Tranche,
) -> PlannedObject {
    PlannedObject::UniqueTarget {
        table,
        name,
        columns,
        status: TargetStatus::CreatedByStep4b,
        creating_tranche: tranche,
    }
}

const fn fk(
    table: &'static str,
    name: &'static str,
    local_columns: &'static [&'static str],
    referenced_table: &'static str,
    referenced_columns: &'static [&'static str],
    tranche: Tranche,
) -> PlannedObject {
    PlannedObject::ForeignKey {
        table,
        name,
        local_columns,
        referenced_table,
        referenced_columns,
        creating_tranche: tranche,
        validating_tranche: tranche,
        unenforced_when_null: false,
    }
}

/// A foreign key whose local key contains a NULL-able column, so `MATCH SIMPLE`
/// leaves those rows unchecked.
const fn fk_nullable(
    table: &'static str,
    name: &'static str,
    local_columns: &'static [&'static str],
    referenced_table: &'static str,
    referenced_columns: &'static [&'static str],
    tranche: Tranche,
) -> PlannedObject {
    PlannedObject::ForeignKey {
        table,
        name,
        local_columns,
        referenced_table,
        referenced_columns,
        creating_tranche: tranche,
        validating_tranche: tranche,
        unenforced_when_null: true,
    }
}

const AGENT_KEY: &[&str] = &[TENANT_ID, AGENT_UUID];
const AGENTS_TARGET: &[&str] = &[TENANT_ID, "id"];
const TENANT_AGENT_IDX: &[&str] = &[TENANT_ID, AGENT_UUID];
const TENANT_ONLY_IDX: &[&str] = &[TENANT_ID];
const ID_TENANT: &[&str] = &["id", TENANT_ID];
const SESSION_IDENTITY: &[&str] = &[TENANT_ID, AGENT_UUID, "session_id"];

const T1: Tranche = Tranche::RootsAndDirectAgentChildren;
const T2: Tranche = Tranche::Sessions;
const T3: Tranche = Tranche::Memories;
const T4: Tranche = Tranche::LineageAndArchival;
const T5: Tranche = Tranche::Operations;
const T7: Tranche = Tranche::FinalConstraintTightening;

// ── The plan ────────────────────────────────────────────────────────────────

/// Every object Step 4B intends to create, as data.
///
/// This is the single source of truth. The artifacts render from it, the
/// invariants check it, and there is no parallel prose copy to disagree with it.
pub const PLANNED_OBJECTS: &[PlannedObject] = &[
    // ══ Tranche 1 — roots and direct agent children ══
    //
    // `agents` owes no columns: `tenant_id` arrived in migration 0028 and
    // `agents_tenant_id_id_key` already exists. It is recorded as the
    // already-current target that fifteen foreign keys depend on.
    PlannedObject::UniqueTarget {
        table: "agents",
        name: "agents_tenant_id_id_key",
        columns: AGENTS_TARGET,
        status: TargetStatus::AlreadyCurrent,
        creating_tranche: T1,
    },
    col("archival_batches", AGENT_UUID, T1),
    col("archival_batches", TENANT_ID, T1),
    idx(
        "archival_batches",
        "idx_archival_batches_tenant",
        TENANT_AGENT_IDX,
        T1,
    ),
    // Required by `memories.archival_batch_id`. Absent from the Step 4A plan;
    // without it that foreign key cannot be created at all.
    target(
        "archival_batches",
        "archival_batches_id_tenant_id_key",
        ID_TENANT,
        T1,
    ),
    fk(
        "archival_batches",
        "archival_batches_tenant_agent_fkey",
        AGENT_KEY,
        "agents",
        AGENTS_TARGET,
        T1,
    ),
    // audit_logs keeps NULL-able ownership: agentless events are valid rows.
    col_permanent("audit_logs", AGENT_UUID, T1),
    col_permanent("audit_logs", TENANT_ID, T1),
    idx("audit_logs", "idx_audit_logs_tenant", TENANT_AGENT_IDX, T1),
    fk_nullable(
        "audit_logs",
        "audit_logs_tenant_agent_fkey",
        AGENT_KEY,
        "agents",
        AGENTS_TARGET,
        T1,
    ),
    col("entities", AGENT_UUID, T1),
    col("entities", TENANT_ID, T1),
    idx("entities", "idx_entities_tenant", TENANT_AGENT_IDX, T1),
    // Required by `memory_entity_links.entity_id`. Also absent from the Step 4A
    // plan.
    target("entities", "entities_id_tenant_id_key", ID_TENANT, T1),
    fk(
        "entities",
        "entities_tenant_agent_fkey",
        AGENT_KEY,
        "agents",
        AGENTS_TARGET,
        T1,
    ),
    col("memory_graph", AGENT_UUID, T1),
    col("memory_graph", TENANT_ID, T1),
    idx(
        "memory_graph",
        "idx_memory_graph_tenant",
        TENANT_AGENT_IDX,
        T1,
    ),
    fk(
        "memory_graph",
        "memory_graph_tenant_agent_fkey",
        AGENT_KEY,
        "agents",
        AGENTS_TARGET,
        T1,
    ),
    col("rmk_policies", AGENT_UUID, T1),
    col("rmk_policies", TENANT_ID, T1),
    idx(
        "rmk_policies",
        "idx_rmk_policies_tenant",
        TENANT_AGENT_IDX,
        T1,
    ),
    target(
        "rmk_policies",
        "rmk_policies_id_tenant_id_key",
        ID_TENANT,
        T1,
    ),
    fk(
        "rmk_policies",
        "rmk_policies_tenant_agent_fkey",
        AGENT_KEY,
        "agents",
        AGENTS_TARGET,
        T1,
    ),
    // ══ Tranche 2 — sessions ══
    //
    // Session identity stays AGENT-scoped. A tenant-scoped
    // `UNIQUE (tenant_id, session_id)` would collide whenever two agents in one
    // tenant happen to use the same caller-supplied session string, which is a
    // normal thing for callers to do.
    col("sessions", AGENT_UUID, T2),
    col("sessions", TENANT_ID, T2),
    idx("sessions", "idx_sessions_tenant", TENANT_AGENT_IDX, T2),
    target(
        "sessions",
        "sessions_tenant_agent_session_key",
        SESSION_IDENTITY,
        T2,
    ),
    fk(
        "sessions",
        "sessions_tenant_agent_fkey",
        AGENT_KEY,
        "agents",
        AGENTS_TARGET,
        T2,
    ),
    col("working_memory", AGENT_UUID, T2),
    col("working_memory", TENANT_ID, T2),
    idx(
        "working_memory",
        "idx_working_memory_tenant",
        TENANT_AGENT_IDX,
        T2,
    ),
    fk(
        "working_memory",
        "working_memory_tenant_agent_fkey",
        AGENT_KEY,
        "agents",
        AGENTS_TARGET,
        T2,
    ),
    // working_memory is the only true SESSION_CHILD, so its session reference
    // becomes a real composite foreign key rather than a convention.
    fk(
        "working_memory",
        "working_memory_session_fkey",
        SESSION_IDENTITY,
        "sessions",
        SESSION_IDENTITY,
        T2,
    ),
    // ══ Tranche 3 — memories ══
    col("memories", AGENT_UUID, T3),
    col("memories", TENANT_ID, T3),
    idx("memories", "idx_memories_tenant", TENANT_AGENT_IDX, T3),
    target("memories", "memories_id_tenant_id_key", ID_TENANT, T3),
    fk(
        "memories",
        "memories_tenant_agent_fkey",
        AGENT_KEY,
        "agents",
        AGENTS_TARGET,
        T3,
    ),
    fk_nullable(
        "memories",
        "memories_archival_batch_tenant_fkey",
        &["archival_batch_id", TENANT_ID],
        "archival_batches",
        ID_TENANT,
        T3,
    ),
    col("extraction_jobs", AGENT_UUID, T3),
    col("extraction_jobs", TENANT_ID, T3),
    idx(
        "extraction_jobs",
        "idx_extraction_jobs_tenant",
        TENANT_AGENT_IDX,
        T3,
    ),
    fk(
        "extraction_jobs",
        "extraction_jobs_tenant_agent_fkey",
        AGENT_KEY,
        "agents",
        AGENTS_TARGET,
        T3,
    ),
    col("memory_retrieval_logs", AGENT_UUID, T3),
    col("memory_retrieval_logs", TENANT_ID, T3),
    idx(
        "memory_retrieval_logs",
        "idx_memory_retrieval_logs_tenant",
        TENANT_AGENT_IDX,
        T3,
    ),
    fk(
        "memory_retrieval_logs",
        "memory_retrieval_logs_tenant_agent_fkey",
        AGENT_KEY,
        "agents",
        AGENTS_TARGET,
        T3,
    ),
    col("cognitive_hypervisor_timeline", AGENT_UUID, T3),
    col("cognitive_hypervisor_timeline", TENANT_ID, T3),
    idx(
        "cognitive_hypervisor_timeline",
        "idx_cht_tenant",
        TENANT_AGENT_IDX,
        T3,
    ),
    fk(
        "cognitive_hypervisor_timeline",
        "cht_tenant_agent_fkey",
        AGENT_KEY,
        "agents",
        AGENTS_TARGET,
        T3,
    ),
    // ══ Tranche 4 — lineage and archival ══
    //
    // The three MEMORY_LINEAGE_CHILD tables take `tenant_id` only: they carry no
    // agent column, and ownership is entirely lineage-derived.
    col("memory_versions", TENANT_ID, T4),
    idx(
        "memory_versions",
        "idx_memory_versions_tenant",
        TENANT_ONLY_IDX,
        T4,
    ),
    fk(
        "memory_versions",
        "memory_versions_memory_tenant_fkey",
        &["memory_id", TENANT_ID],
        "memories",
        ID_TENANT,
        T4,
    ),
    col("memory_entity_links", TENANT_ID, T4),
    idx(
        "memory_entity_links",
        "idx_memory_entity_links_tenant",
        TENANT_ONLY_IDX,
        T4,
    ),
    fk(
        "memory_entity_links",
        "memory_entity_links_memory_tenant_fkey",
        &["memory_id", TENANT_ID],
        "memories",
        ID_TENANT,
        T4,
    ),
    fk(
        "memory_entity_links",
        "memory_entity_links_entity_tenant_fkey",
        &["entity_id", TENANT_ID],
        "entities",
        ID_TENANT,
        T4,
    ),
    col("co_access_edges", TENANT_ID, T4),
    idx(
        "co_access_edges",
        "idx_co_access_edges_tenant",
        TENANT_ONLY_IDX,
        T4,
    ),
    fk(
        "co_access_edges",
        "co_access_edges_memory_a_tenant_fkey",
        &["memory_a", TENANT_ID],
        "memories",
        ID_TENANT,
        T4,
    ),
    fk(
        "co_access_edges",
        "co_access_edges_memory_b_tenant_fkey",
        &["memory_b", TENANT_ID],
        "memories",
        ID_TENANT,
        T4,
    ),
    col("memory_conflicts", AGENT_UUID, T4),
    col("memory_conflicts", TENANT_ID, T4),
    idx(
        "memory_conflicts",
        "idx_memory_conflicts_tenant",
        TENANT_AGENT_IDX,
        T4,
    ),
    fk(
        "memory_conflicts",
        "memory_conflicts_tenant_agent_fkey",
        AGENT_KEY,
        "agents",
        AGENTS_TARGET,
        T4,
    ),
    fk_nullable(
        "memory_conflicts",
        "memory_conflicts_memory_a_tenant_fkey",
        &["memory_a", TENANT_ID],
        "memories",
        ID_TENANT,
        T4,
    ),
    fk_nullable(
        "memory_conflicts",
        "memory_conflicts_memory_b_tenant_fkey",
        &["memory_b", TENANT_ID],
        "memories",
        ID_TENANT,
        T4,
    ),
    col("retrieval_feedback", AGENT_UUID, T4),
    col("retrieval_feedback", TENANT_ID, T4),
    idx(
        "retrieval_feedback",
        "idx_retrieval_feedback_tenant",
        TENANT_AGENT_IDX,
        T4,
    ),
    fk(
        "retrieval_feedback",
        "retrieval_feedback_tenant_agent_fkey",
        AGENT_KEY,
        "agents",
        AGENTS_TARGET,
        T4,
    ),
    fk_nullable(
        "retrieval_feedback",
        "retrieval_feedback_memory_tenant_fkey",
        &["memory_id", TENANT_ID],
        "memories",
        ID_TENANT,
        T4,
    ),
    // ══ Tranche 5 — operations ══
    col("amp_controller_state", AGENT_UUID, T5),
    col("amp_controller_state", TENANT_ID, T5),
    idx(
        "amp_controller_state",
        "idx_amp_controller_state_tenant",
        TENANT_AGENT_IDX,
        T5,
    ),
    fk(
        "amp_controller_state",
        "amp_controller_state_tenant_agent_fkey",
        AGENT_KEY,
        "agents",
        AGENTS_TARGET,
        T5,
    ),
    col("rmk_episodes", AGENT_UUID, T5),
    col("rmk_episodes", TENANT_ID, T5),
    idx(
        "rmk_episodes",
        "idx_rmk_episodes_tenant",
        TENANT_AGENT_IDX,
        T5,
    ),
    fk(
        "rmk_episodes",
        "rmk_episodes_tenant_agent_fkey",
        AGENT_KEY,
        "agents",
        AGENTS_TARGET,
        T5,
    ),
    fk_nullable(
        "rmk_episodes",
        "rmk_episodes_policy_tenant_fkey",
        &["policy_id", TENANT_ID],
        "rmk_policies",
        ID_TENANT,
        T5,
    ),
    // ══ FUTURE STEP 7 — not part of Step 4B ══
    //
    // Declared so the dependency graph is complete and so the NOT NULL
    // pre-check has an owner. Step 7 cannot begin until Step 5 endpoint
    // enforcement and Step 6 legacy-key retirement are merged.
    PlannedObject::Constraint {
        table: "agents",
        name: "agents_tenant_id_not_null_chk",
        kind: PlannedConstraintKind::NotNullPrecheck,
        columns: &[TENANT_ID],
        creating_tranche: T7,
        validating_tranche: T7,
    },
];

/// Every planned object for one table, in declaration order.
pub fn planned_for(table: &str) -> Vec<&'static PlannedObject> {
    PLANNED_OBJECTS
        .iter()
        .filter(|o| o.table() == table)
        .collect()
}

/// Every planned object created by one tranche.
pub fn planned_in(tranche: Tranche) -> Vec<&'static PlannedObject> {
    PLANNED_OBJECTS
        .iter()
        .filter(|o| o.creating_tranche() == tranche)
        .collect()
}

// ── Backfill authority ──────────────────────────────────────────────────────

/// How one table's ownership columns are populated, and what must agree first.
///
/// `agreement` is the part Step 4A's prose left implicit. A table with secondary
/// ownership paths has more than one answer to "who owns this row", and the
/// backfill must refuse to write when they disagree rather than picking one.
/// Copying `tenant_id` from the first available parent is precisely how a
/// cross-tenant link becomes permanent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct BackfillAuthority {
    pub table: &'static str,
    /// The authoritative source, as a `FROM` clause.
    pub source: &'static str,
    /// The predicate every candidate row must satisfy before it is written.
    pub agreement: Option<&'static str>,
    /// What happens to rows that fail `agreement`.
    pub on_disagreement: &'static str,
}

/// Rows that disagree are never written, never guessed, and always reported.
const LEAVE_AND_REPORT: &str = "left NULL and reported; the audit's blocking finding is the \
     output, and no side is picked";

pub const BACKFILL_AUTHORITY: &[BackfillAuthority] = &[
    BackfillAuthority {
        table: "memory_entity_links",
        source: "memories m ON m.id = l.memory_id, entities e ON e.id = l.entity_id, \
                 agents a ON a.agent_id = l.agent_id",
        agreement: Some(
            "m.tenant_id IS NOT NULL AND m.tenant_id = e.tenant_id AND m.tenant_id = a.tenant_id",
        ),
        on_disagreement: LEAVE_AND_REPORT,
    },
    BackfillAuthority {
        table: "co_access_edges",
        source: "memories ma ON ma.id = e.memory_a, memories mb ON mb.id = e.memory_b",
        agreement: Some("ma.tenant_id IS NOT NULL AND ma.tenant_id = mb.tenant_id"),
        on_disagreement: LEAVE_AND_REPORT,
    },
    BackfillAuthority {
        table: "memory_versions",
        source: "memories m ON m.id = v.memory_id",
        agreement: Some("m.tenant_id IS NOT NULL"),
        on_disagreement: LEAVE_AND_REPORT,
    },
    BackfillAuthority {
        table: "memory_conflicts",
        source: "agents a ON a.agent_id = c.agent_id",
        agreement: Some(
            "a.tenant_id IS NOT NULL \
             AND (c.memory_a IS NULL OR EXISTS (SELECT 1 FROM memories m \
             WHERE m.id = c.memory_a AND m.tenant_id = a.tenant_id)) \
             AND (c.memory_b IS NULL OR EXISTS (SELECT 1 FROM memories m \
             WHERE m.id = c.memory_b AND m.tenant_id = a.tenant_id))",
        ),
        on_disagreement: LEAVE_AND_REPORT,
    },
    BackfillAuthority {
        table: "retrieval_feedback",
        source: "agents a ON a.agent_id = f.agent_id",
        agreement: Some(
            "a.tenant_id IS NOT NULL \
             AND (f.memory_id IS NULL OR EXISTS (SELECT 1 FROM memories m \
             WHERE m.id = f.memory_id AND m.tenant_id = a.tenant_id))",
        ),
        on_disagreement: LEAVE_AND_REPORT,
    },
    BackfillAuthority {
        table: "memories",
        source: "agents a ON a.agent_id = m.agent_id",
        // The archival batch is a secondary path. Where it is present it must
        // resolve to the same tenant as the agent; where it is NULL the path
        // simply does not apply to the row.
        agreement: Some(
            "a.tenant_id IS NOT NULL \
             AND (m.archival_batch_id IS NULL OR EXISTS (SELECT 1 FROM archival_batches b \
             WHERE b.id = m.archival_batch_id AND b.tenant_id = a.tenant_id))",
        ),
        on_disagreement: LEAVE_AND_REPORT,
    },
    BackfillAuthority {
        table: "rmk_episodes",
        source: "agents a ON a.agent_id = e.agent_id",
        // The policy is a secondary path with the same nullable shape.
        agreement: Some(
            "a.tenant_id IS NOT NULL \
             AND (e.policy_id IS NULL OR EXISTS (SELECT 1 FROM rmk_policies p \
             WHERE p.id = e.policy_id AND p.tenant_id = a.tenant_id))",
        ),
        on_disagreement: LEAVE_AND_REPORT,
    },
    BackfillAuthority {
        table: "working_memory",
        source: "sessions s ON s.agent_id = w.agent_id AND s.session_id = w.session_id",
        agreement: Some("s.tenant_id IS NOT NULL AND s.agent_uuid IS NOT NULL"),
        on_disagreement: "left NULL and reported; sessions must be fully backfilled first, so a \
                          NULL here means either the session row does not exist yet or its own \
                          backfill has not run",
    },
];

/// The agreement contract for one table, if it has one.
pub fn backfill_authority_for(table: &str) -> Option<&'static BackfillAuthority> {
    BACKFILL_AUTHORITY.iter().find(|b| b.table == table)
}

// ── Reason codes that are structurally unreachable ──────────────────────────

/// A required-zero code that cannot currently fire, with the reason.
///
/// Recorded rather than silently dropped: the enum values are a stable contract,
/// and a code that is unreachable *today* becomes reachable the moment the
/// structure suppressing it changes. Writing down why is the difference between
/// a gate that is satisfied and a gate that was never tested.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct UnreachableCode {
    pub table: &'static str,
    pub code: ReasonCode,
    pub reason: &'static str,
}

pub const UNREACHABLE_REQUIRED_ZERO: &[UnreachableCode] = &[
    UnreachableCode {
        table: "memories",
        code: ReasonCode::OrphanedSessionReference,
        reason: "memories' canonical path is AGENT_TEXT and its session is CONTEXT_ONLY, so no \
                 session path is registered and the code cannot fire for this table. Removed \
                 from its required-zero set: a gate that can never fail is not a gate.",
    },
    UnreachableCode {
        table: "co_access_edges",
        code: ReasonCode::OrphanedMemoryReference,
        reason: "every memory reference is backed by a declared foreign key, so an orphan cannot \
                 exist while the contract holds; producing one means dropping that key, which is \
                 SCHEMA_RELATIONSHIP_DRIFT and blocks by a different code.",
    },
    UnreachableCode {
        table: "memory_entity_links",
        code: ReasonCode::OrphanedMemoryReference,
        reason: "same structure as co_access_edges: FK-backed, so the orphan count is withheld \
                 under drift and the drift is what blocks.",
    },
    UnreachableCode {
        table: "memory_versions",
        code: ReasonCode::OrphanedMemoryReference,
        reason: "same structure as co_access_edges: FK-backed, so the orphan count is withheld \
                 under drift and the drift is what blocks.",
    },
    UnreachableCode {
        table: "agents",
        code: ReasonCode::FutureTenantUniquenessCollision,
        reason: "a consequence of keeping session identity agent-scoped. No table in the plan \
                 narrows a uniqueness rule: `sessions` moves from UNIQUE (agent_id, session_id) \
                 to UNIQUE (tenant_id, agent_uuid, session_id), which is a superset and cannot \
                 collide, and `agents` adds nothing because UNIQUE (tenant_id, \
                 external_agent_id) already exists. With no narrowing planned anywhere, the \
                 pre-check has nothing to pre-check, so `future_unique_columns` is None on every \
                 table and the query no longer runs. The reason code stays in the catalogue \
                 because a future narrowing tuple would make it fire again.",
    },
];

// ── Invariants ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod invariants {
    use super::*;
    use std::collections::BTreeSet;

    fn unique_target_for(parent: &str, referenced: &[&str]) -> Option<&'static PlannedObject> {
        PLANNED_OBJECTS.iter().find(|candidate| {
            matches!(candidate, PlannedObject::UniqueTarget { .. })
                && candidate.table() == parent
                && candidate.local_columns() == referenced
        })
    }

    /// Every planned foreign key must reference a tuple that some unique target
    /// covers, in the same order.
    ///
    /// This is the check the Step 4A plan could not make. `memories` planned a
    /// key to `archival_batches(id, tenant_id)` and `memory_entity_links` one to
    /// `entities(id, tenant_id)`, and neither parent declared a matching unique
    /// target. PostgreSQL rejects such a key with "there is no unique constraint
    /// matching given keys", so both migrations would have failed on first
    /// execution.
    #[test]
    fn every_planned_foreign_key_has_a_matching_unique_target() {
        for object in PLANNED_OBJECTS {
            let Some((parent, referenced)) = object.referenced() else {
                continue;
            };
            assert!(
                unique_target_for(parent, referenced).is_some(),
                "`{}` references {parent}({}) but no unique target covers that tuple in that \
                 order; PostgreSQL would reject the constraint",
                object.name(),
                referenced.join(", ")
            );
        }
    }

    /// A foreign key may not depend on a target its own tranche has not reached.
    #[test]
    fn no_foreign_key_depends_on_a_later_tranche() {
        for object in PLANNED_OBJECTS {
            let Some((parent, referenced)) = object.referenced() else {
                continue;
            };
            let target = unique_target_for(parent, referenced)
                .expect("covered by every_planned_foreign_key_has_a_matching_unique_target");
            assert!(
                target.creating_tranche() <= object.creating_tranche(),
                "`{}` is created in {} but its target `{}` is not created until {}",
                object.name(),
                object.creating_tranche(),
                target.name(),
                target.creating_tranche()
            );
        }
    }

    /// Every planned object has exactly one owning tranche — no object may be
    /// declared twice.
    #[test]
    fn every_planned_object_has_one_owner_tranche() {
        let mut seen: BTreeSet<(&str, &str)> = BTreeSet::new();
        for object in PLANNED_OBJECTS {
            assert!(
                seen.insert((object.table(), object.name())),
                "`{}` on `{}` is declared more than once",
                object.name(),
                object.table()
            );
        }
    }

    /// Every ownership column an index, target or key names must be created by
    /// a tranche no later than the one that uses it.
    #[test]
    fn planned_columns_are_created_before_they_are_used() {
        let mut created: BTreeSet<(&str, &str)> = BTreeSet::new();
        for object in PLANNED_OBJECTS {
            if let PlannedObject::Column { table, name, .. } = object {
                created.insert((table, name));
            }
        }
        for object in PLANNED_OBJECTS {
            if matches!(object, PlannedObject::Column { .. }) {
                continue;
            }
            for column in object.local_columns() {
                let is_ownership_column = matches!(*column, "agent_uuid" | "tenant_id");
                if !is_ownership_column {
                    continue; // pre-existing column, not Step 4B's to create
                }
                // `agents.tenant_id` predates Step 4B (migration 0028).
                if object.table() == "agents" {
                    continue;
                }
                let owner = PLANNED_OBJECTS
                    .iter()
                    .find(|c| {
                        matches!(c, PlannedObject::Column { .. })
                            && c.table() == object.table()
                            && c.name() == *column
                    })
                    .unwrap_or_else(|| {
                        panic!(
                            "`{}` on `{}` names `{column}`, which Step 4B never creates",
                            object.name(),
                            object.table()
                        )
                    });
                assert!(
                    owner.creating_tranche() <= object.creating_tranche(),
                    "`{}` uses `{column}`, which is not created until {}",
                    object.name(),
                    owner.creating_tranche()
                );
            }
        }
    }

    /// Every table receiving ownership columns declares how new writes stay
    /// owned. "Quiesce during backfill" is not a strategy — it says nothing
    /// about the first row written after the backfill finishes.
    #[test]
    fn every_table_receiving_columns_declares_a_transitional_strategy() {
        let mut receiving: BTreeSet<&str> = BTreeSet::new();
        for object in PLANNED_OBJECTS {
            if let PlannedObject::Column { table, .. } = object {
                receiving.insert(table);
            }
        }
        for table in &receiving {
            assert!(
                transition_for(table).is_some(),
                "`{table}` receives ownership columns but declares no transitional write strategy"
            );
        }
        for (table, _) in TRANSITION {
            assert!(
                receiving.contains(table),
                "`{table}` declares a transitional strategy but receives no ownership columns"
            );
        }
    }

    /// Only `audit_logs` may leave ownership permanently NULL.
    #[test]
    fn permanently_nullable_ownership_is_confined_to_audit_logs() {
        for object in PLANNED_OBJECTS {
            if object.nullability() == Some(Nullability::RemainsNullable) {
                assert_eq!(
                    object.table(),
                    "audit_logs",
                    "`{}` plans permanently NULL-able ownership; only agentless audit events \
                     justify that",
                    object.table()
                );
            }
        }
        assert!(
            matches!(
                transition_for("audit_logs"),
                Some(TransitionalWrite::OwnershipMayRemainNull { .. })
            ),
            "audit_logs must declare that its ownership may remain NULL"
        );
    }

    /// Session identity stays agent-scoped. A tenant-scoped session key would
    /// collide whenever two agents in one tenant use the same caller-supplied
    /// session string.
    #[test]
    fn session_identity_is_agent_scoped_never_tenant_only() {
        let target = PLANNED_OBJECTS
            .iter()
            .find(|o| o.name() == "sessions_tenant_agent_session_key")
            .expect("sessions must declare its identity target");
        assert_eq!(
            target.local_columns(),
            &["tenant_id", "agent_uuid", "session_id"],
            "session identity must include the agent"
        );
        for object in PLANNED_OBJECTS {
            let cols = object.local_columns();
            let tenant_session_only =
                cols == ["tenant_id", "session_id"] || cols == ["session_id", "tenant_id"];
            assert!(
                !tenant_session_only,
                "`{}` plans a tenant-scoped session key, which two agents in one tenant can \
                 collide on",
                object.name()
            );
        }
        // working_memory's session reference must move with it.
        let wm = PLANNED_OBJECTS
            .iter()
            .find(|o| o.name() == "working_memory_session_fkey")
            .expect("working_memory must reference sessions");
        assert_eq!(wm.referenced(), Some(("sessions", target.local_columns())));
    }

    /// Every table with more than one ownership path declares what must agree
    /// before its backfill writes anything.
    #[test]
    fn multi_path_tables_declare_an_agreement_predicate() {
        for entry in super::super::inventory::REGISTRY {
            if entry.secondary_paths.is_empty() {
                continue;
            }
            let receives = PLANNED_OBJECTS
                .iter()
                .any(|o| matches!(o, PlannedObject::Column { .. }) && o.table() == entry.table);
            if !receives {
                continue;
            }
            let authority = backfill_authority_for(entry.table).unwrap_or_else(|| {
                panic!(
                    "`{}` has {} secondary ownership path(s) but declares no backfill agreement \
                     predicate; copying from one parent is how a cross-tenant link becomes \
                     permanent",
                    entry.table,
                    entry.secondary_paths.len()
                )
            });
            assert!(
                authority.agreement.is_some(),
                "`{}` declares a backfill authority with no agreement predicate",
                entry.table
            );
        }
    }

    /// `memory_entity_links` must check all three of its paths, not just the
    /// memory it happens to be keyed on first.
    #[test]
    fn memory_entity_links_requires_all_three_paths_to_agree() {
        let authority = backfill_authority_for("memory_entity_links").expect("declared above");
        let predicate = authority.agreement.expect("has an agreement predicate");
        for expected in ["m.tenant_id", "e.tenant_id", "a.tenant_id"] {
            assert!(
                predicate.contains(expected),
                "the memory, entity and legacy-agent paths must all agree; `{expected}` is absent \
                 from {predicate:?}"
            );
        }
    }

    /// Both memories behind a co-access edge must agree.
    #[test]
    fn co_access_edges_requires_both_memories_to_agree() {
        let authority = backfill_authority_for("co_access_edges").expect("declared above");
        let predicate = authority.agreement.expect("has an agreement predicate");
        assert!(
            predicate.contains("ma.tenant_id = mb.tenant_id"),
            "both memories must agree: {predicate:?}"
        );
    }

    /// A foreign key whose local key can contain NULL must say so, because
    /// `MATCH SIMPLE` leaves those rows unchecked and the key is then not
    /// evidence that they are owned.
    #[test]
    fn nullable_foreign_keys_are_marked_unenforced() {
        // Nullability is a property of (table, column), never of a column name.
        // `memory_id` is NOT NULL on memory_versions and memory_entity_links but
        // NULL-able on retrieval_feedback; `memory_a`/`memory_b` are NOT NULL on
        // co_access_edges and NULL-able on memory_conflicts. Measured against a
        // pg16 with all 31 migrations applied — a name-keyed list got this wrong
        // in both directions.
        const PRE_EXISTING_NULLABLE: &[(&str, &str)] = &[
            ("memories", "archival_batch_id"),
            ("memory_conflicts", "memory_a"),
            ("memory_conflicts", "memory_b"),
            ("retrieval_feedback", "memory_id"),
            ("rmk_episodes", "policy_id"),
        ];
        for object in PLANNED_OBJECTS {
            let PlannedObject::ForeignKey {
                table,
                local_columns,
                unenforced_when_null,
                ..
            } = object
            else {
                continue;
            };
            // Two sources of NULL. A pre-existing column the schema already
            // allows to be NULL, and a column Step 4B plans as permanently
            // NULL-able — `audit_logs`, whose agentless events are valid rows
            // with no owner. A NullableThenTightened column does not count: it
            // is NULL only during the transition, and FUTURE STEP 7 closes it.
            let has_nullable = local_columns.iter().any(|c| {
                PRE_EXISTING_NULLABLE.contains(&(table, c))
                    || PLANNED_OBJECTS.iter().any(|p| {
                        p.table() == *table
                            && p.name() == *c
                            && p.nullability() == Some(Nullability::RemainsNullable)
                    })
            });
            assert_eq!(
                *unenforced_when_null,
                has_nullable,
                "`{}` disagrees with the live schema about whether its key can contain NULL; \
                 MATCH SIMPLE leaves such rows unchecked, so the flag decides whether this key \
                 is evidence of ownership",
                object.name()
            );
        }
    }

    /// Nothing in Step 4B may plan a rewrite or a blocking scan. Those belong to
    /// FUTURE STEP 7.
    #[test]
    fn step_4b_plans_no_rewrite_and_no_blocking_scan() {
        for object in PLANNED_OBJECTS {
            if object.creating_tranche() == Tranche::FinalConstraintTightening {
                continue;
            }
            let lock = object.creation_lock();
            assert!(
                !matches!(
                    lock,
                    LockProfile::TableRewrite | LockProfile::SetNotNull | LockProfile::DropColumn
                ),
                "`{}` plans {} in a Step 4B tranche; that belongs to FUTURE STEP 7",
                object.name(),
                lock.as_str()
            );
        }
    }

    /// `VALIDATE CONSTRAINT` takes a weaker lock than `ADD CONSTRAINT`. Getting
    /// this backwards is what made the Step 4A plan describe validation as
    /// blocking writes when it does not.
    #[test]
    fn validation_locks_are_weaker_than_creation_locks() {
        assert!(LockProfile::ValidateConstraint
            .locks()
            .contains("SHARE UPDATE EXCLUSIVE"));
        assert!(LockProfile::ValidateConstraint
            .locks()
            .contains("ROW SHARE"));
        assert!(LockProfile::AddForeignKeyNotValid
            .locks()
            .contains("SHARE ROW EXCLUSIVE"));
        assert!(LockProfile::AddForeignKeyNotValid
            .locks()
            .contains("referenced table"));
        for object in PLANNED_OBJECTS {
            if object.not_valid_permitted() {
                assert_eq!(
                    object.validation_lock(),
                    Some(LockProfile::ValidateConstraint),
                    "`{}` permits NOT VALID so it must validate under the weaker lock",
                    object.name()
                );
            } else {
                assert_eq!(object.validation_lock(), None);
            }
        }
    }

    /// A concurrent build must be flagged, because it dictates the migration
    /// file's `-- no-transaction` header.
    #[test]
    fn concurrently_built_objects_are_flagged() {
        for object in PLANNED_OBJECTS {
            let concurrent = object.concurrent_build_required();
            let is_index = matches!(
                object,
                PlannedObject::Index { .. } | PlannedObject::UniqueTarget { .. }
            );
            assert_eq!(
                concurrent,
                is_index,
                "`{}` disagrees with itself about needing a concurrent build",
                object.name()
            );
        }
        assert!(CONCURRENT_BUILD_MECHANISM.contains("-- no-transaction"));
    }

    /// Human-readable rendering is generated, so every object must produce one
    /// and it must name the object or its table.
    #[test]
    fn every_planned_object_renders_from_typed_data() {
        for object in PLANNED_OBJECTS {
            let rendered = object.describe();
            assert!(
                rendered.contains(object.name()) || rendered.contains(object.table()),
                "rendering for `{}` names neither the object nor its table: {rendered}",
                object.name()
            );
        }
    }

    /// The unreachable-code register may only name real tables, and must give a
    /// reason for every entry.
    #[test]
    fn unreachable_codes_are_recorded_with_a_reason() {
        for unreachable in UNREACHABLE_REQUIRED_ZERO {
            assert!(
                !unreachable.reason.is_empty(),
                "`{}`/{} is marked unreachable with no reason",
                unreachable.table,
                unreachable.code
            );
            assert!(
                super::super::inventory::entry(unreachable.table).is_some(),
                "`{}` is not a registered table",
                unreachable.table
            );
        }
    }

    /// A code recorded as unreachable must actually be gone from that table's
    /// required-zero set, and every code still in a required-zero set must not
    /// be recorded as unreachable.
    ///
    /// This is the invariant that keeps the register honest in both directions.
    /// Without it, "recorded as unreachable" and "removed from the gate" could
    /// drift apart, and a gate would silently look stricter than it is again.
    #[test]
    fn recorded_unreachable_codes_are_absent_from_their_required_zero_sets() {
        for unreachable in UNREACHABLE_REQUIRED_ZERO {
            let entry = super::super::inventory::entry(unreachable.table)
                .expect("checked by unreachable_codes_are_recorded_with_a_reason");
            assert!(
                !entry.plan.required_zero_codes.contains(&unreachable.code),
                "`{}` still lists {} as required-zero, but it is recorded as structurally \
                 unreachable — the gate cannot fail, so it is not a gate",
                unreachable.table,
                unreachable.code
            );
        }
        for entry in super::super::inventory::REGISTRY {
            for code in entry.plan.required_zero_codes {
                let recorded = UNREACHABLE_REQUIRED_ZERO
                    .iter()
                    .any(|u| u.table == entry.table && u.code == *code);
                assert!(
                    !recorded,
                    "`{}` requires {} to be zero, but that pair is recorded as unreachable",
                    entry.table, code
                );
            }
        }
    }

    /// `audit_logs` must never acquire LEGACY_UNMAPPED. Agentless events are
    /// valid rows, and a tranche-wide union of required-zero codes is exactly
    /// how that gate would get reintroduced.
    #[test]
    fn audit_logs_never_requires_zero_legacy_unmapped() {
        let entry = super::super::inventory::entry("audit_logs").expect("registered");
        assert!(
            !entry
                .plan
                .required_zero_codes
                .contains(&ReasonCode::LegacyUnmapped),
            "audit_logs records agentless events; requiring zero unmapped rows would demand that \
             the schema reject valid audit history"
        );
        assert!(
            entry
                .plan
                .required_zero_codes
                .contains(&ReasonCode::UnmappedAgent),
            "…but rows that *do* name an agent must still resolve"
        );
    }

    /// Required-zero sets stay per table. If any two tables in one tranche have
    /// identical sets purely because someone unioned them, this catches the
    /// shape: `audit_logs` must differ from its tranche-1 neighbours.
    #[test]
    fn required_zero_sets_are_not_unioned_across_a_tranche() {
        let audit_logs = super::super::inventory::entry("audit_logs").expect("registered");
        let entities = super::super::inventory::entry("entities").expect("registered");
        assert_eq!(audit_logs.tranche, entities.tranche);
        assert_ne!(
            audit_logs.plan.required_zero_codes, entities.plan.required_zero_codes,
            "two tables in one tranche have identical required-zero sets; a union would produce \
             exactly this, and it would silently tighten audit_logs"
        );
    }

    /// The three-stage protocol has to be able to refuse. A FINALIZE that runs
    /// straight after PREPARE would validate a constraint over rows nobody had
    /// backfilled.
    #[test]
    fn the_finalize_guard_names_its_evidence() {
        assert!(FINALIZE_GUARD.contains("agent_tenancy_migrations"));
        assert!(FINALIZE_GUARD.contains("sqlx migrate run"));
        assert_eq!(Stage::ALL.len(), 3);
        assert_eq!(Stage::Prepare.to_string(), "PREPARE");
    }

    /// The chosen transitional strategy must state the property it guarantees,
    /// not merely name a mechanism.
    #[test]
    fn the_transitional_strategy_states_its_guarantee() {
        assert!(TRANSITIONAL_WRITE_RATIONALE.contains("dual-write"));
        assert!(TRANSITIONAL_WRITE_GUARANTEE.contains("resumed legacy writer"));
        assert!(TRANSITIONAL_WRITE_GUARANTEE.contains("UNMAPPED_AGENT"));
    }
}
