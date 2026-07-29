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
     tenancy_backfill_checkpoints records a COMPLETED checkpoint for *this* tranche against the \
     current contract digest with blocking_count = 0. `sqlx migrate run` therefore cannot advance \
     PREPARE straight into FINALIZE: on a fresh database the guard fires because no backfill ran, \
     and on a live database it fires until the operator's backfill command records completion. \
     agent_tenancy_migrations cannot serve as that evidence — it has no tranche, no digest, no \
     cursor and no status column — so Step 4B-1 must create the checkpoint table before the first \
     backfill runs.";

/// `CREATE INDEX CONCURRENTLY` cannot run inside a transaction block, and sqlx
/// wraps each migration in one by default.
pub const CONCURRENT_BUILD_MECHANISM: &str = "A migration containing CREATE INDEX CONCURRENTLY \
     must carry `-- no-transaction` as its first line so sqlx runs it unwrapped. A concurrent \
     build that fails leaves an INVALID index behind, which the Step 4A verifier already \
     reports as drift, so a failed build cannot be mistaken for a healthy one.";

// ── Backfill checkpoint evidence ────────────────────────────────────────────

/// What a checkpoint row is worth as evidence.
///
/// `Completed` is the only status FINALIZE accepts, and it is not a free-text
/// label: the guard compares it, so it has to be a value rather than a string
/// some future writer can spell differently.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum CheckpointStatus {
    /// Batches are still running. The cursor is meaningful; the completion is
    /// not.
    InProgress,
    /// Every batch for the tranche finished and the audit reported zero blocking
    /// findings for it. This is the only status FINALIZE accepts.
    Completed,
    /// Superseded — the contract digest moved, or an operator restarted the
    /// tranche. Retained for history and never treated as completion.
    Abandoned,
}

impl CheckpointStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InProgress => "IN_PROGRESS",
            Self::Completed => "COMPLETED",
            Self::Abandoned => "ABANDONED",
        }
    }

    pub const ALL: &'static [CheckpointStatus] =
        &[Self::InProgress, Self::Completed, Self::Abandoned];
}

/// Why a checkpoint column exists, so the invariant can check the table covers
/// every category the protocol depends on rather than counting columns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum CheckpointColumnRole {
    Identity,
    /// Which tranche this checkpoint is evidence for.
    Tranche,
    /// The contract the backfill ran against. A checkpoint recorded against a
    /// superseded plan is not evidence for the current one.
    ContractDigest,
    Status,
    /// Where a stopped backfill resumes. Without it BACKFILL is restartable only
    /// by starting over.
    ResumableCursor,
    RowCount,
    /// Blocking audit findings outstanding for the tranche. FINALIZE requires
    /// zero.
    BlockingCount,
    Timestamp,
}

/// One column of a table Step 4B-1 must create.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct PlannedTableColumn {
    pub name: &'static str,
    pub sql_type: &'static str,
    pub nullable: bool,
    pub role: CheckpointColumnRole,
}

const fn ckpt(
    name: &'static str,
    sql_type: &'static str,
    nullable: bool,
    role: CheckpointColumnRole,
) -> PlannedTableColumn {
    PlannedTableColumn {
        name,
        sql_type,
        nullable,
        role,
    }
}

/// A table Step 4B intends to create outright, as opposed to a column added to
/// an existing one.
///
/// There is exactly one, and it exists because the protocol asserts against
/// evidence that has nowhere to live today.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct PlannedTable {
    pub name: &'static str,
    pub purpose: &'static str,
    pub columns: &'static [PlannedTableColumn],
    /// The tuple on which at most one authoritative completion may exist.
    pub unique_completion: &'static [&'static str],
    /// The status the uniqueness is *scoped to*, making it a partial unique
    /// index rather than a plain constraint.
    ///
    /// The scope is load-bearing and cannot be left implicit. A plain
    /// `UNIQUE (tranche, contract_digest)` would reject the second row an
    /// operator creates when restarting a tranche against the same digest —
    /// but [`CheckpointStatus::Abandoned`] exists precisely so that the
    /// superseded attempt is *retained* rather than overwritten. Only
    /// `WHERE status = 'COMPLETED'` gives "at most one authoritative
    /// completion" while leaving the history alone.
    pub unique_completion_when: CheckpointStatus,
    pub creating_tranche: Tranche,
    /// What has to land in the same change, so the new table cannot be created
    /// outside the contract that describes it.
    pub joins_contract: &'static str,
}

/// The per-tranche checkpoint table.
///
/// `agent_tenancy_migrations` cannot serve this purpose. Measured against a
/// pg16 with all 31 migrations applied, its columns are exactly `id`, `mode`,
/// `legacy_tenant_id`, `agents_total`, `agents_assigned`, `agents_unmapped` and
/// `applied_at`. There is no tranche, no digest, no cursor and no status — so it
/// can record *that* a one-shot agent assignment happened, but not *which*
/// tranche completed, *against which contract*, or *where to resume*. A FINALIZE
/// guard reading it would assert against the wrong evidence and pass.
pub const TENANCY_BACKFILL_CHECKPOINTS: PlannedTable = PlannedTable {
    name: "tenancy_backfill_checkpoints",
    purpose: "Per-tranche, per-contract backfill evidence. BACKFILL writes progress here and \
              FINALIZE refuses to proceed without a COMPLETED row for its own tranche against \
              the current contract digest with zero blocking findings.",
    columns: &[
        ckpt("id", "UUID", false, CheckpointColumnRole::Identity),
        ckpt("tranche", "TEXT", false, CheckpointColumnRole::Tranche),
        ckpt(
            "contract_digest",
            "TEXT",
            false,
            CheckpointColumnRole::ContractDigest,
        ),
        ckpt("status", "TEXT", false, CheckpointColumnRole::Status),
        // NULL once the tranche completes: there is nothing left to resume.
        ckpt(
            "resume_cursor",
            "TEXT",
            true,
            CheckpointColumnRole::ResumableCursor,
        ),
        ckpt(
            "rows_total",
            "BIGINT",
            false,
            CheckpointColumnRole::RowCount,
        ),
        ckpt(
            "rows_backfilled",
            "BIGINT",
            false,
            CheckpointColumnRole::RowCount,
        ),
        ckpt(
            "blocking_count",
            "BIGINT",
            false,
            CheckpointColumnRole::BlockingCount,
        ),
        ckpt(
            "started_at",
            "TIMESTAMPTZ",
            false,
            CheckpointColumnRole::Timestamp,
        ),
        ckpt(
            "updated_at",
            "TIMESTAMPTZ",
            false,
            CheckpointColumnRole::Timestamp,
        ),
        ckpt(
            "completed_at",
            "TIMESTAMPTZ",
            true,
            CheckpointColumnRole::Timestamp,
        ),
    ],
    unique_completion: &["tranche", "contract_digest"],
    unique_completion_when: CheckpointStatus::Completed,
    creating_tranche: Tranche::RootsAndDirectAgentChildren,
    joins_contract: "Created in the PREPARE of the first tranche, before any backfill runs. The \
                     same change registers it in the Step 4A inventory REGISTRY and the \
                     current-schema contract, so the table cannot exist as an unregistered \
                     side-table the verifier reports as drift.",
};

pub const PLANNED_TABLES: &[PlannedTable] = &[TENANCY_BACKFILL_CHECKPOINTS];

/// What FINALIZE checks before it validates anything.
///
/// Typed rather than prose because the guard is the whole reason PREPARE cannot
/// run straight into FINALIZE, and "a completed backfill checkpoint" left three
/// questions unanswered: completed for *which* tranche, against *which*
/// contract, and with how many blocking findings outstanding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct FinalizePrecondition {
    pub required_status: CheckpointStatus,
    /// The checkpoint's tranche must equal the finalizing tranche exactly. A
    /// later tranche's completion is not evidence for an earlier one, and a
    /// tranche-agnostic "any completion" check is how FINALIZE would validate a
    /// table nobody backfilled.
    pub tranche_must_match_exactly: bool,
    /// The checkpoint's digest must equal the current contract digest. A
    /// backfill that ran against a superseded plan proves nothing about this
    /// one.
    pub digest_must_match_current: bool,
    /// Outstanding blocking findings permitted at FINALIZE.
    pub max_blocking_count: i64,
}

pub const FINALIZE_PRECONDITION: FinalizePrecondition = FinalizePrecondition {
    required_status: CheckpointStatus::Completed,
    tranche_must_match_exactly: true,
    digest_must_match_current: true,
    max_blocking_count: 0,
};

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
    /// `ACCESS EXCLUSIVE` on the table alone, and brief: `ADD CONSTRAINT ...
    /// USING INDEX` adopts an index that already exists rather than building
    /// one, so it examines no rows and names no parent.
    ///
    /// Separate from [`Self::CreateIndexConcurrently`] because a unique target
    /// is not one operation but two, and they differ on every axis that matters
    /// operationally. The concurrent build takes the weak
    /// `SHARE UPDATE EXCLUSIVE`, runs outside a transaction block, and is the
    /// slow half; the attach takes the strongest lock PostgreSQL has, runs in
    /// milliseconds, and is the half that actually blocks readers. Reporting
    /// only the build would tell an operator planning a maintenance window that
    /// the strong lock never happens.
    ///
    /// No row scan applies here specifically because these targets are `UNIQUE`
    /// rather than `PRIMARY KEY`: `USING INDEX` scans only when it has to mark
    /// columns `NOT NULL`, and the ownership columns stay NULL-able for the
    /// whole of the transition.
    AddUniqueUsingIndex,
    /// `SHARE ROW EXCLUSIVE` on the child **and** on the referenced table.
    /// Blocks writes to both for the duration of the catalog change, which with
    /// `NOT VALID` is brief because no rows are examined.
    AddForeignKeyNotValid,
    /// `ACCESS EXCLUSIVE` on the table alone, and brief: a CHECK constraint
    /// added `NOT VALID` examines no rows and names no parent.
    ///
    /// Separate from [`Self::AddForeignKeyNotValid`] because the two differ on
    /// both axes that matter operationally. A CHECK takes the *stronger* lock —
    /// `ACCESS EXCLUSIVE` blocks reads, `SHARE ROW EXCLUSIVE` does not — but
    /// takes it on one table instead of two. Describing a CHECK with the
    /// foreign-key profile would promise a lock that permits concurrent reads
    /// and would name a referenced table that does not exist.
    AddCheckNotValid,
    /// `SHARE UPDATE EXCLUSIVE` on the child and `ROW SHARE` on the referenced
    /// table. Scans every row, but concurrent DML continues throughout.
    ///
    /// Foreign keys only — the referenced-table half is what makes it specific
    /// to them. See [`Self::ValidateCheckConstraint`].
    ValidateConstraint,
    /// `SHARE UPDATE EXCLUSIVE` on the table alone. Scans every row, concurrent
    /// DML continues, and no parent is involved because a CHECK has none.
    ValidateCheckConstraint,
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
            Self::AddUniqueUsingIndex => "ADD CONSTRAINT ... USING INDEX",
            Self::AddForeignKeyNotValid => "ADD CONSTRAINT ... FOREIGN KEY ... NOT VALID",
            Self::AddCheckNotValid => "ADD CONSTRAINT ... CHECK ... NOT VALID",
            Self::ValidateConstraint => "VALIDATE CONSTRAINT (foreign key)",
            Self::ValidateCheckConstraint => "VALIDATE CONSTRAINT (CHECK)",
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
            Self::AddUniqueUsingIndex => {
                "ACCESS EXCLUSIVE on the table alone, brief — adopts an already-built index, so \
                 no rows are scanned and no parent is locked alongside it"
            }
            Self::AddForeignKeyNotValid => {
                "SHARE ROW EXCLUSIVE on the child AND on the referenced table"
            }
            Self::AddCheckNotValid => {
                "ACCESS EXCLUSIVE on the table alone, brief — no row scan, and no parent is \
                 locked alongside it"
            }
            Self::ValidateConstraint => {
                "SHARE UPDATE EXCLUSIVE on the child, ROW SHARE on the referenced table"
            }
            Self::ValidateCheckConstraint => {
                "SHARE UPDATE EXCLUSIVE on the table alone — a CHECK has no parent to lock"
            }
            Self::SetNotNull => "ACCESS EXCLUSIVE; full scan unless a validated CHECK permits skip",
            Self::TableRewrite => "ACCESS EXCLUSIVE for the whole rewrite",
            Self::DropColumn => "ACCESS EXCLUSIVE, brief — catalog update only",
        }
    }

    pub const ALL: &'static [LockProfile] = &[
        Self::AddColumnNullable,
        Self::CreateIndexConcurrently,
        Self::AddUniqueUsingIndex,
        Self::AddForeignKeyNotValid,
        Self::AddCheckNotValid,
        Self::ValidateConstraint,
        Self::ValidateCheckConstraint,
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

/// Columns the live schema already permits to be NULL, as `(table, column)`.
///
/// Keyed on the pair and never on the name alone, because the same name has
/// different nullability on different tables: `memory_id` is NOT NULL on
/// `memory_versions` and `memory_entity_links` but NULL-able on
/// `retrieval_feedback`; `memory_a`/`memory_b` are NOT NULL on
/// `co_access_edges` and NULL-able on `memory_conflicts`. A name-keyed list got
/// this wrong in both directions at once — it marked keys unenforced that are
/// fully enforced, and left keys enforced that `MATCH SIMPLE` silently skips.
///
/// Measured against a pg16 with all 31 migrations applied.
pub const PRE_EXISTING_NULLABLE: &[(&str, &str)] = &[
    ("memories", "archival_batch_id"),
    ("memory_conflicts", "memory_a"),
    ("memory_conflicts", "memory_b"),
    ("retrieval_feedback", "memory_id"),
    ("rmk_episodes", "policy_id"),
];

/// How a foreign key treats rows whose local key contains a NULL.
///
/// Typed so a consumer can branch on it. Previously the only statement of this
/// was a clause appended to the rendered description — so anything that needed
/// to know had to substring-match English prose, which is exactly the kind of
/// second, drifting copy this module exists to remove.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum MatchSemantics {
    /// No local column can be NULL, so the constraint checks every row and the
    /// key really is evidence that they are owned.
    AllRowsChecked,
    /// The default `MATCH SIMPLE`: a row with *any* NULL in the key is not
    /// checked against the parent at all. The key is therefore not evidence of
    /// ownership for those rows — the audit is.
    NullKeyRowsUnchecked,
}

/// Which of a planned key's two lifetimes a nullability question is about.
///
/// A foreign key on ownership columns is unenforced for un-backfilled rows
/// while its tranche runs, and fully enforced once STEP 7 tightens the columns.
/// Both are true; they are true at different times.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EnforcementWindow {
    /// PREPARE through BACKFILL, while ownership columns may still be NULL.
    DuringTransition,
    /// After FUTURE STEP 7's `SET NOT NULL`.
    AfterTightening,
}

impl MatchSemantics {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AllRowsChecked => "ALL_ROWS_CHECKED",
            Self::NullKeyRowsUnchecked => "MATCH_SIMPLE_NULL_KEY_ROWS_UNCHECKED",
        }
    }
}

/// Whether a unique target already exists or has to be created.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum TargetStatus {
    /// Present in the live schema today and verified by the Step 4A audit.
    ///
    /// This is an operational statement, not a note. Nothing creates it: it has
    /// no creating tranche, takes no creation lock, and must never reach a
    /// PREPARE worklist. It stays in the plan solely so that a foreign key
    /// depending on it can prove its parent tuple is covered — a dependency
    /// prerequisite, not a unit of work. Emitting `CREATE UNIQUE INDEX
    /// CONCURRENTLY` for it would fail against the index that already exists.
    AlreadyCurrent,
    /// Created by Step 4B in its owning tranche.
    CreatedByStep4b,
}

impl TargetStatus {
    /// Whether Step 4B has to build this, or the live schema already provides
    /// it.
    pub const fn requires_creation(self) -> bool {
        matches!(self, Self::CreatedByStep4b)
    }
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

    /// Whether Step 4B has to build this object at all.
    ///
    /// False only for a unique target the live schema already provides.
    pub const fn requires_creation(&self) -> bool {
        match self {
            Self::UniqueTarget { status, .. } => status.requires_creation(),
            _ => true,
        }
    }

    /// The tranche this object is declared under, whether or not anything is
    /// built for it.
    ///
    /// Used for grouping and for dependency ordering. Distinct from
    /// [`Self::creating_tranche`], which is `None` when nothing is created —
    /// keeping the two apart is what stops an already-current target being
    /// scheduled as work simply because it is filed under a tranche.
    pub const fn declared_in(&self) -> Tranche {
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

    /// The tranche that creates this object, or `None` when the live schema
    /// already provides it and there is nothing to create.
    pub const fn creating_tranche(&self) -> Option<Tranche> {
        if self.requires_creation() {
            Some(self.declared_in())
        } else {
            None
        }
    }

    /// The tranche whose FINALIZE stage validates this object. Equal to the
    /// creating tranche for everything that is not deliberately deferred, and
    /// `None` for an object nothing creates — there is nothing to validate.
    pub const fn validating_tranche(&self) -> Option<Tranche> {
        match self {
            Self::ForeignKey {
                validating_tranche, ..
            }
            | Self::Constraint {
                validating_tranche, ..
            } => Some(*validating_tranche),
            _ => self.creating_tranche(),
        }
    }

    /// Whether a NULL in this object's local key leaves rows unchecked.
    ///
    /// `None` for anything that is not a foreign key — the question does not
    /// apply.
    pub const fn unenforced_when_null(&self) -> Option<bool> {
        match self {
            Self::ForeignKey {
                unenforced_when_null,
                ..
            } => Some(*unenforced_when_null),
            _ => None,
        }
    }

    /// The `MATCH SIMPLE` consequence, as a value rather than a clause appended
    /// to rendered prose.
    pub const fn match_semantics(&self) -> Option<MatchSemantics> {
        match self.unenforced_when_null() {
            Some(true) => Some(MatchSemantics::NullKeyRowsUnchecked),
            Some(false) => Some(MatchSemantics::AllRowsChecked),
            None => None,
        }
    }

    /// Which of this object's local columns can actually contain NULL.
    ///
    /// Two sources: a column the live schema already permits to be NULL, and a
    /// column Step 4B plans as permanently NULL-able. A `NullableThenTightened`
    /// column does not count — it is NULL only during the transition, and
    /// FUTURE STEP 7 closes it.
    pub fn nullable_local_columns(&self) -> Vec<&'static str> {
        self.nullable_local_columns_in(EnforcementWindow::AfterTightening)
    }

    /// The same question asked of the transition, where the answer differs.
    ///
    /// Between PREPARE and FUTURE STEP 7 a `NullableThenTightened` column is
    /// genuinely NULL — that is the entire point of the backfill — so
    /// `MATCH SIMPLE` skips those rows too. A key that is fully enforced in the
    /// end state can therefore be enforcing nothing while the tranche is still
    /// running, which is exactly why the audit rather than the foreign key is
    /// the gate. Modelled separately rather than folded into the end-state
    /// answer, because the two are different facts and a consumer needs to know
    /// which one it is reading.
    pub fn transition_nullable_local_columns(&self) -> Vec<&'static str> {
        self.nullable_local_columns_in(EnforcementWindow::DuringTransition)
    }

    fn nullable_local_columns_in(&self, window: EnforcementWindow) -> Vec<&'static str> {
        let table = self.table();
        self.local_columns()
            .iter()
            .copied()
            .filter(|column| {
                if PRE_EXISTING_NULLABLE.contains(&(table, column)) {
                    return true;
                }
                PLANNED_OBJECTS.iter().any(|p| {
                    p.table() == table
                        && p.name() == *column
                        && match p.nullability() {
                            Some(Nullability::RemainsNullable) => true,
                            // NULL only until its tranche's backfill completes.
                            Some(Nullability::NullableThenTightened) => {
                                window == EnforcementWindow::DuringTransition
                            }
                            _ => false,
                        }
                })
            })
            .collect()
    }

    /// The `MATCH SIMPLE` consequence during PREPARE/BACKFILL, before FUTURE
    /// STEP 7 tightens the ownership columns.
    pub fn transition_match_semantics(&self) -> Option<MatchSemantics> {
        self.unenforced_when_null()?;
        Some(if self.transition_nullable_local_columns().is_empty() {
            MatchSemantics::AllRowsChecked
        } else {
            MatchSemantics::NullKeyRowsUnchecked
        })
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
    ///
    /// An already-current target is not built, so it dictates nothing about any
    /// migration file's `-- no-transaction` header.
    pub const fn concurrent_build_required(&self) -> bool {
        self.requires_creation() && matches!(self, Self::Index { .. } | Self::UniqueTarget { .. })
    }

    /// The lock this object's creation takes, or `None` when nothing is created.
    pub const fn creation_lock(&self) -> Option<LockProfile> {
        if !self.requires_creation() {
            return None;
        }
        Some(self.creation_lock_kind())
    }

    const fn creation_lock_kind(&self) -> LockProfile {
        match self {
            Self::Column { .. } => LockProfile::AddColumnNullable,
            // Both are built CONCURRENTLY. A unique target additionally has an
            // attach phase, which is reported by `attachment_lock()` rather
            // than folded in here.
            Self::Index { .. } | Self::UniqueTarget { .. } => LockProfile::CreateIndexConcurrently,
            Self::ForeignKey { .. } => LockProfile::AddForeignKeyNotValid,
            // A CHECK names no parent, so it locks one table rather than two —
            // but it locks it more strongly.
            Self::Constraint { .. } => LockProfile::AddCheckNotValid,
        }
    }

    /// The lock taken when a concurrently-built unique index is adopted as a
    /// constraint, for the objects that have such a phase at all.
    ///
    /// This is a second accessor rather than [`Self::creation_lock`] returning
    /// an ordered sequence, and the reason is that the two phases are not
    /// interchangeable list elements. `concurrent_build_required()` keys the
    /// migration file's `-- no-transaction` header off the *build*, so a
    /// sequence would have to be read positionally — "element zero decides the
    /// header" — which is exactly the kind of unwritten convention this module
    /// exists to replace with data. Naming the phase makes it queryable and
    /// renders it into the artifacts, which is what was actually missing.
    ///
    /// `None` for every other kind of object, and `None` for an already-current
    /// target: nothing is built, so nothing is attached.
    pub const fn attachment_lock(&self) -> Option<LockProfile> {
        if !self.requires_creation() {
            return None;
        }
        match self {
            Self::UniqueTarget { .. } => Some(LockProfile::AddUniqueUsingIndex),
            _ => None,
        }
    }

    /// The lock this object's validation takes, where it has one.
    pub const fn validation_lock(&self) -> Option<LockProfile> {
        match self {
            Self::ForeignKey { .. } => Some(LockProfile::ValidateConstraint),
            // A CHECK has no parent, so its validation locks one table. Reusing
            // the foreign-key profile would promise a `ROW SHARE` on a
            // referenced table that does not exist — the same conflation on the
            // validation side that `AddCheckNotValid` fixes on the creation
            // side.
            Self::Constraint { .. } => Some(LockProfile::ValidateCheckConstraint),
            _ => None,
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
/// What a conditional bridge does with one incoming row.
///
/// "Ownership may remain NULL" was one answer to four different questions. It
/// described agentless events correctly and everything else wrongly: a row that
/// names a resolvable agent should be owned, a row that names an agent nothing
/// can resolve should keep NULL ownership so the audit reports it, and a row
/// that asserts a tenant contradicting `agents` should be refused rather than
/// believed. Collapsing those into one blanket meant the third case looked
/// identical to the first, which is precisely the case the audit exists to
/// surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ConditionalOwnership {
    /// The row names no agent at all. Legitimate: startup, configuration
    /// change, administrative action. Ownership stays NULL and nothing is
    /// reported, because there is nothing wrong.
    AgentlessAllowed,
    /// The row names an agent that resolves to a tenant. The bridge populates
    /// both ownership columns from `agents` and verifies them against it.
    ResolvedAndVerified,
    /// The row names an agent that does not resolve — no such agent, or an
    /// agent whose own `tenant_id` is NULL. The row is written with NULL
    /// ownership rather than rejected, so the audit reports
    /// `ORPHANED_AGENT_REFERENCE` or `UNMAPPED_AGENT` against it. Losing audit
    /// history is a worse outcome than an unowned audit row, and a silently
    /// dropped event is worse than both.
    PreservedUnresolved,
    /// The row supplies ownership that contradicts what `agents` says for its
    /// own agent. Refused. A contradicted audit row is not evidence of
    /// anything, and accepting it would let a caller write history under
    /// another tenant.
    ContradictionRejected,
}

impl ConditionalOwnership {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AgentlessAllowed => "AGENTLESS_ALLOWED",
            Self::ResolvedAndVerified => "RESOLVED_AND_VERIFIED",
            Self::PreservedUnresolved => "PRESERVED_UNRESOLVED",
            Self::ContradictionRejected => "CONTRADICTION_REJECTED",
        }
    }

    /// Every state a conditional bridge has to decide between. A bridge that
    /// declares fewer has left a case undefined, and an undefined case in a
    /// `BEFORE` trigger resolves to "write whatever arrived".
    pub const ALL: &'static [ConditionalOwnership] = &[
        Self::AgentlessAllowed,
        Self::ResolvedAndVerified,
        Self::PreservedUnresolved,
        Self::ContradictionRejected,
    ];
}

/// How a table's new ownership columns stay populated, and from which authority.
///
/// The original model had one bridge shape and assumed `agents` was always the
/// answer. It is not: `working_memory` resolves through `sessions`,
/// `memory_versions` and `co_access_edges` through `memories`, and five tables
/// have more than one parent that must agree first. A single agents-shaped
/// bridge on those tables would resolve from whichever parent it happened to
/// join — which is exactly how a cross-tenant link becomes permanent, and it is
/// the same failure the backfill's `agreement` predicate exists to prevent.
///
/// Each variant therefore names its authority, and an invariant checks that the
/// authority matches the table's [`BackfillAuthority`]. Write-time and
/// backfill-time must not be able to disagree about who owns a row.
// The shared `Bridge` suffix is the point rather than an accident: every
// variant *is* a bridge, and what distinguishes them is which authority they
// resolve ownership from. Dropping the suffix would leave `Agent`, `Session`
// and `Memory` as variants of a type whose name says nothing about bridging.
#[allow(clippy::enum_variant_names)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "strategy", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum TransitionalWrite {
    /// Ownership resolves from `agents` using the row's own legacy `agent_id`,
    /// which is mandatory on this table. Raises on contradiction.
    AgentBridge {
        trigger: &'static str,
        function: &'static str,
    },
    /// Ownership resolves from `agents`, but the agent reference is optional,
    /// so the bridge decides between outcomes rather than having only one.
    ///
    /// The outcomes are [`ConditionalOwnership::ALL`] and are not carried as a
    /// field: a per-bridge list would let a future conditional bridge declare
    /// three of the four and compile, leaving the fourth case to resolve as
    /// "write whatever arrived".
    ConditionalAgentBridge {
        trigger: &'static str,
        function: &'static str,
        reason: &'static str,
    },
    /// Ownership resolves through `sessions`, not through `agents` directly.
    SessionBridge {
        trigger: &'static str,
        function: &'static str,
    },
    /// Ownership resolves through `memories`. Where the row references two
    /// memories, both must agree before anything is written.
    MemoryBridge {
        trigger: &'static str,
        function: &'static str,
    },
    /// More than one authority can answer "who owns this row", so the bridge
    /// writes only when they agree and refuses when they do not. It never picks
    /// a side.
    MultiPathBridge {
        trigger: &'static str,
        function: &'static str,
        parents: &'static [&'static str],
    },
}

impl TransitionalWrite {
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::AgentBridge { .. } => "AGENT_BRIDGE_TRIGGER",
            Self::ConditionalAgentBridge { .. } => "CONDITIONAL_AGENT_BRIDGE_TRIGGER",
            Self::SessionBridge { .. } => "SESSION_BRIDGE_TRIGGER",
            Self::MemoryBridge { .. } => "MEMORY_BRIDGE_TRIGGER",
            Self::MultiPathBridge { .. } => "MULTI_PATH_BRIDGE_TRIGGER",
        }
    }

    pub const fn trigger(&self) -> &'static str {
        match self {
            Self::AgentBridge { trigger, .. }
            | Self::ConditionalAgentBridge { trigger, .. }
            | Self::SessionBridge { trigger, .. }
            | Self::MemoryBridge { trigger, .. }
            | Self::MultiPathBridge { trigger, .. } => trigger,
        }
    }

    pub const fn function(&self) -> &'static str {
        match self {
            Self::AgentBridge { function, .. }
            | Self::ConditionalAgentBridge { function, .. }
            | Self::SessionBridge { function, .. }
            | Self::MemoryBridge { function, .. }
            | Self::MultiPathBridge { function, .. } => function,
        }
    }

    /// Whether this bridge resolves ownership from somewhere other than the
    /// row's own agent — in which case the table must also declare a
    /// [`BackfillAuthority`], and the two must agree.
    pub const fn resolves_through_a_parent(&self) -> bool {
        matches!(
            self,
            Self::SessionBridge { .. } | Self::MemoryBridge { .. } | Self::MultiPathBridge { .. }
        )
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

const fn agent_bridge(trigger: &'static str, function: &'static str) -> TransitionalWrite {
    TransitionalWrite::AgentBridge { trigger, function }
}

const fn session_bridge(trigger: &'static str, function: &'static str) -> TransitionalWrite {
    TransitionalWrite::SessionBridge { trigger, function }
}

const fn memory_bridge(trigger: &'static str, function: &'static str) -> TransitionalWrite {
    TransitionalWrite::MemoryBridge { trigger, function }
}

const fn multi_path_bridge(
    trigger: &'static str,
    function: &'static str,
    parents: &'static [&'static str],
) -> TransitionalWrite {
    TransitionalWrite::MultiPathBridge {
        trigger,
        function,
        parents,
    }
}

/// Every table receiving ownership columns, and how its new writes stay owned.
pub const TRANSITION: &[(&str, TransitionalWrite)] = &[
    // ── Tranche 1 ──
    (
        "archival_batches",
        agent_bridge(
            "trg_archival_batches_tenancy_bridge",
            "fn_archival_batches_tenancy_bridge",
        ),
    ),
    (
        "audit_logs",
        TransitionalWrite::ConditionalAgentBridge {
            trigger: "trg_audit_logs_tenancy_bridge",
            function: "fn_audit_logs_tenancy_bridge",
            reason: "audit_logs records agentless events — startup, configuration change, \
                     administrative action — which are valid rows with no owning agent, so its \
                     ownership columns stay NULL-able permanently and LEGACY_UNMAPPED is \
                     deliberately absent from its required-zero set. That is not a reason to \
                     install no bridge. Declining to resolve ownership at all also declines it \
                     for the rows that *do* name a resolvable agent, which then stay NULL for no \
                     reason and are indistinguishable from genuinely agentless ones. The bridge \
                     is therefore conditional rather than absent: it owns what it can resolve, \
                     leaves what it cannot so the audit reports it, and refuses a row that \
                     contradicts agents outright.",
        },
    ),
    (
        "entities",
        agent_bridge("trg_entities_tenancy_bridge", "fn_entities_tenancy_bridge"),
    ),
    (
        "memory_graph",
        agent_bridge(
            "trg_memory_graph_tenancy_bridge",
            "fn_memory_graph_tenancy_bridge",
        ),
    ),
    (
        "rmk_policies",
        agent_bridge(
            "trg_rmk_policies_tenancy_bridge",
            "fn_rmk_policies_tenancy_bridge",
        ),
    ),
    // ── Tranche 2 ──
    (
        "sessions",
        agent_bridge("trg_sessions_tenancy_bridge", "fn_sessions_tenancy_bridge"),
    ),
    (
        "working_memory",
        session_bridge(
            "trg_working_memory_tenancy_bridge",
            "fn_working_memory_tenancy_bridge",
        ),
    ),
    // ── Tranche 3 ──
    (
        "memories",
        multi_path_bridge(
            "trg_memories_tenancy_bridge",
            "fn_memories_tenancy_bridge",
            &["agents", "archival_batches"],
        ),
    ),
    (
        "extraction_jobs",
        agent_bridge(
            "trg_extraction_jobs_tenancy_bridge",
            "fn_extraction_jobs_tenancy_bridge",
        ),
    ),
    (
        "memory_retrieval_logs",
        agent_bridge(
            "trg_memory_retrieval_logs_tenancy_bridge",
            "fn_memory_retrieval_logs_tenancy_bridge",
        ),
    ),
    (
        "cognitive_hypervisor_timeline",
        agent_bridge("trg_cht_tenancy_bridge", "fn_cht_tenancy_bridge"),
    ),
    // ── Tranche 4 ──
    (
        "memory_versions",
        memory_bridge(
            "trg_memory_versions_tenancy_bridge",
            "fn_memory_versions_tenancy_bridge",
        ),
    ),
    (
        "memory_entity_links",
        multi_path_bridge(
            "trg_memory_entity_links_tenancy_bridge",
            "fn_memory_entity_links_tenancy_bridge",
            &["memories", "entities", "agents"],
        ),
    ),
    (
        "co_access_edges",
        memory_bridge(
            "trg_co_access_edges_tenancy_bridge",
            "fn_co_access_edges_tenancy_bridge",
        ),
    ),
    (
        "memory_conflicts",
        multi_path_bridge(
            "trg_memory_conflicts_tenancy_bridge",
            "fn_memory_conflicts_tenancy_bridge",
            &["agents", "memories"],
        ),
    ),
    (
        "retrieval_feedback",
        multi_path_bridge(
            "trg_retrieval_feedback_tenancy_bridge",
            "fn_retrieval_feedback_tenancy_bridge",
            &["agents", "memories"],
        ),
    ),
    // ── Tranche 5 ──
    (
        "amp_controller_state",
        agent_bridge(
            "trg_amp_controller_state_tenancy_bridge",
            "fn_amp_controller_state_tenancy_bridge",
        ),
    ),
    (
        "rmk_episodes",
        multi_path_bridge(
            "trg_rmk_episodes_tenancy_bridge",
            "fn_rmk_episodes_tenancy_bridge",
            &["agents", "rmk_policies"],
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

/// Every planned object declared under one tranche, including any the live
/// schema already provides.
pub fn planned_in(tranche: Tranche) -> Vec<&'static PlannedObject> {
    PLANNED_OBJECTS
        .iter()
        .filter(|o| o.declared_in() == tranche)
        .collect()
}

/// The objects a tranche's PREPARE actually has to build.
///
/// Distinct from [`planned_in`] because an already-current unique target is
/// declared under a tranche but created by nothing. Handing the full list to a
/// migration generator would emit `CREATE UNIQUE INDEX CONCURRENTLY` for an
/// index that already exists, which fails.
pub fn prepare_worklist(tranche: Tranche) -> Vec<&'static PlannedObject> {
    PLANNED_OBJECTS
        .iter()
        .filter(|o| o.creating_tranche() == Some(tranche))
        .collect()
}

// ── Compatibility prerequisites ─────────────────────────────────────────────

/// A code change that must be deployed, and its incompatible predecessors
/// drained, before a planned object may be created at all.
///
/// `NOT VALID` is commonly read as "the constraint is not in force yet". It does
/// not mean that. It means *existing rows are not scanned*; every `INSERT` and
/// `UPDATE` after the constraint lands is checked in full. A foreign key added
/// `NOT VALID` in PREPARE is therefore live against new writes from the instant
/// the catalog change commits — so a writer that inserts the child before the
/// parent starts failing at PREPARE, not at FINALIZE and not at FUTURE STEP 7.
///
/// This is the one place where the contract constrains application code rather
/// than schema, which is why it is recorded as data: PREPARE for the owning
/// tranche cannot be scheduled while the prerequisite is outstanding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct CompatibilityPrerequisite {
    /// The planned object that cannot be created until this is satisfied.
    pub planned_object: &'static str,
    /// The writer that is incompatible today, and what breaks.
    pub blocker: &'static str,
    /// The order the writer must use, parent first.
    pub required_write_order: &'static [&'static str],
    /// Both writes must land in one transaction. Correct ordering alone is not
    /// enough: between two autocommit statements a concurrent reader sees a
    /// session with no working memory, and a crash between them leaves the pair
    /// half-written with no rollback.
    pub single_transaction: bool,
    /// The stage by which compatible writers must already be serving. `Prepare`
    /// means deployed *before* PREPARE runs, not during it.
    pub writers_deployed_before: Stage,
    /// Old binaries keep the old order, so deploying the fix is not sufficient
    /// while a previous release is still serving traffic.
    pub drain_incompatible_binaries: bool,
    /// What a rollback costs once the constraint exists.
    pub rollback: &'static str,
}

/// Prerequisites outstanding at ff16d96.
///
/// Measured, not assumed: `upsert_working_memory` in `src/memory/store.rs`
/// issues its `working_memory` INSERT first and mirrors into `sessions`
/// second, as two separate statements executed on the pool rather than inside
/// one transaction. Step 4B-0 does not change that code — it records that
/// Step 4B-1 cannot schedule tranche 2's PREPARE until it has.
pub const COMPATIBILITY_PREREQUISITES: &[CompatibilityPrerequisite] =
    &[CompatibilityPrerequisite {
        planned_object: "working_memory_session_fkey",
        blocker: "upsert_working_memory inserts into working_memory first and mirrors into \
                  sessions second, as two separate autocommit statements on the pool. Once \
                  working_memory_session_fkey exists the first statement references a sessions \
                  row the second has not written yet, so the first turn of every new session \
                  fails. NOT VALID does not defer this: it exempts historical rows, never new \
                  ones.",
        required_write_order: &["sessions", "working_memory"],
        single_transaction: true,
        writers_deployed_before: Stage::Prepare,
        drain_incompatible_binaries: true,
        rollback: "Once the foreign key exists, rolling the application back to a release with \
                   the old write order reintroduces the failure. A rollback therefore requires \
                   either dropping working_memory_session_fkey or keeping a bridge that \
                   materialises the parent session row before the child write. Rolling back the \
                   binary alone is not a recovery path.",
    }];

/// The prerequisite blocking one planned object, if it has one.
pub fn compatibility_prerequisite_for(object: &str) -> Option<&'static CompatibilityPrerequisite> {
    COMPATIBILITY_PREREQUISITES
        .iter()
        .find(|p| p.planned_object == object)
}

/// Whether a tranche's PREPARE may be scheduled, or what blocks it.
///
/// The answer is a value rather than a comment so that a scheduler cannot start
/// a tranche whose prerequisites are outstanding simply by not knowing about
/// them.
pub fn prepare_blockers(tranche: Tranche) -> Vec<&'static CompatibilityPrerequisite> {
    PLANNED_OBJECTS
        .iter()
        .filter(|o| o.creating_tranche() == Some(tranche))
        .filter_map(|o| compatibility_prerequisite_for(o.name()))
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

// ── Uniqueness transitions ──────────────────────────────────────────────────

/// How a planned unique tuple relates to the one it replaces.
///
/// `future_unique_columns` is `None` on every table, so the pre-check query
/// never runs. That is correct today and explains nothing: it records the
/// conclusion without the reasoning that produced it. The reasoning is that
/// only one of the three shapes below can turn two rows the schema currently
/// permits into a collision. Naming all three makes the claim checkable, and
/// makes it impossible to add a narrowing tuple later without also adding the
/// probe that would have caught the collision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum UniquenessTransition {
    /// The new tuple is already implied by a constraint that exists. Nothing
    /// changes, so nothing can collide.
    AlreadyImplied,
    /// The new tuple distinguishes at least as finely as the old one. Rows that
    /// are distinct today stay distinct, so no collision is possible.
    Relaxation,
    /// The new tuple distinguishes *less* finely. Rows the schema permits today
    /// may collide under it, so a probe must prove they do not before the
    /// constraint is created. This is the only shape that can fail.
    Narrowing,
}

impl UniquenessTransition {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AlreadyImplied => "ALREADY_IMPLIED",
            Self::Relaxation => "RELAXATION",
            Self::Narrowing => "NARROWING",
        }
    }

    /// Whether this shape can produce a collision that does not exist today.
    pub const fn can_collide(self) -> bool {
        matches!(self, Self::Narrowing)
    }

    /// All three shapes. `Narrowing` has no instance in the current contract —
    /// that is the finding, not an omission — so the taxonomy is published in
    /// full rather than inferred from the entries that happen to exist.
    pub const ALL: &'static [UniquenessTransition] =
        &[Self::AlreadyImplied, Self::Relaxation, Self::Narrowing];
}

/// A uniqueness change together with the evidence its shape requires.
///
/// The probe is carried *inside* `Narrowing` rather than beside it as an
/// `Option`. With two independent fields, `Narrowing` with no probe is
/// representable, and the first contributor to add a real narrowing tuple by
/// copying an existing entry — every one of which has no probe, because none of
/// them narrows — would ship a compiling plan that skips the only check that
/// can catch the collision. Here that pairing is unconstructable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "transition", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum UniquenessChange {
    AlreadyImplied,
    Relaxation,
    /// Deliberately has no instance in the current contract — that is the
    /// finding, not an oversight, and
    /// `the_current_contract_plans_no_narrowing` asserts it. The variant exists
    /// so the shape is nameable and so the first real narrowing tuple cannot be
    /// written without its probe.
    #[allow(dead_code)]
    Narrowing {
        /// The query that proves no rows collide, run before the constraint is
        /// created.
        collision_probe: &'static str,
    },
}

impl UniquenessChange {
    /// The shape alone, for grouping and display.
    pub const fn shape(&self) -> UniquenessTransition {
        match self {
            Self::AlreadyImplied => UniquenessTransition::AlreadyImplied,
            Self::Relaxation => UniquenessTransition::Relaxation,
            Self::Narrowing { .. } => UniquenessTransition::Narrowing,
        }
    }

    pub const fn collision_probe(&self) -> Option<&'static str> {
        match self {
            Self::Narrowing { collision_probe } => Some(collision_probe),
            _ => None,
        }
    }
}

/// One table's uniqueness change, classified.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct PlannedUniquenessTransition {
    pub table: &'static str,
    pub from: &'static [&'static str],
    pub to: &'static [&'static str],
    pub change: UniquenessChange,
    pub reason: &'static str,
}

impl PlannedUniquenessTransition {
    pub const fn shape(&self) -> UniquenessTransition {
        self.change.shape()
    }

    pub const fn collision_probe(&self) -> Option<&'static str> {
        self.change.collision_probe()
    }
}

pub const UNIQUENESS_TRANSITIONS: &[PlannedUniquenessTransition] = &[
    PlannedUniquenessTransition {
        table: "sessions",
        from: &["agent_id", "session_id"],
        to: SESSION_IDENTITY,
        change: UniquenessChange::Relaxation,
        reason: "agent_uuid is a one-for-one re-identification of the legacy agent_id, and \
                 tenant_id is added rather than substituted for anything. Two rows distinguished \
                 by (agent_id, session_id) remain distinguished by (tenant_id, agent_uuid, \
                 session_id), so nothing legal today can collide under the new tuple.",
    },
    PlannedUniquenessTransition {
        table: "agents",
        from: &[TENANT_ID, "external_agent_id"],
        to: &[TENANT_ID, "external_agent_id"],
        change: UniquenessChange::AlreadyImplied,
        reason: "UNIQUE (tenant_id, external_agent_id) already exists. Step 4B adds no uniqueness \
                 to agents, so the tuple is unchanged.",
    },
];

/// The uniqueness transition planned for one table, if any.
pub fn uniqueness_transition_for(table: &str) -> Option<&'static PlannedUniquenessTransition> {
    UNIQUENESS_TRANSITIONS.iter().find(|t| t.table == table)
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
        table: "co_access_edges",
        code: ReasonCode::OrphanedAgentReference,
        reason: "co_access_edges is the only table in the schema with no agent column. The audit \
                 derives an orphan code from the path kind, and both of this table's paths — \
                 memory_a and memory_b — are Memory paths, so a missing parent raises \
                 ORPHANED_MEMORY_REFERENCE and never ORPHANED_AGENT_REFERENCE. Requiring it to be \
                 zero was requiring zero of something that cannot be produced. LEGACY_UNMAPPED and \
                 UNMAPPED_AGENT are *not* removed with it: those come off the canonical chain \
                 memory_a -> memories.agent_id -> agents.tenant_id and remain reachable \
                 transitively, so they stay in the required-zero set.",
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

    /// The parent tables a backfill authority actually consults.
    ///
    /// Derived from the authority's own SQL rather than restated beside it, and
    /// matched against the registry's table names so the vocabulary is closed —
    /// an arbitrary substring search would happily "find" a parent in a column
    /// name. Returning a set makes the comparison against a bridge's declared
    /// parents exact in both directions, rather than a one-way containment
    /// check that cannot notice a parent the bridge forgot.
    fn authority_parents(authority: &BackfillAuthority) -> BTreeSet<&'static str> {
        let sql = format!("{} {}", authority.source, authority.agreement.unwrap_or(""));
        super::super::inventory::REGISTRY
            .iter()
            .map(|entry| entry.table)
            .filter(|table| *table != authority.table && sql.contains(table))
            .collect()
    }

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
            let creating = object
                .creating_tranche()
                .expect("a referencing object is always created by a tranche");
            // A target the live schema already provides is available from the
            // start, so it can never be the later half of a dependency. This is
            // the one thing an AlreadyCurrent target is retained for.
            let Some(target_tranche) = target.creating_tranche() else {
                continue;
            };
            assert!(
                target_tranche <= creating,
                "`{}` is created in {creating} but its target `{}` is not created until \
                 {target_tranche}",
                object.name(),
                target.name()
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
            // An object nothing creates cannot use a column too early: it is
            // already in the live schema, and so are the columns it names.
            let Some(used_at) = object.creating_tranche() else {
                continue;
            };
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
                let owner_tranche = owner
                    .creating_tranche()
                    .expect("a planned column is always created by a tranche");
                assert!(
                    owner_tranche <= used_at,
                    "`{}` uses `{column}`, which is not created until {owner_tranche}",
                    object.name()
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
                Some(TransitionalWrite::ConditionalAgentBridge { .. })
            ),
            "audit_logs must declare a conditional bridge: its ownership may remain NULL, but \
             only for the rows that genuinely have no owner"
        );
    }

    /// `audit_logs` must decide all four cases, not one.
    ///
    /// A blanket "ownership may remain NULL" answered the agentless case and
    /// silently gave the same answer to three others — including a row that
    /// contradicts `agents`, which must be refused rather than stored.
    #[test]
    fn audit_logs_declares_all_four_conditional_states() {
        let Some(TransitionalWrite::ConditionalAgentBridge { reason, .. }) =
            transition_for("audit_logs")
        else {
            panic!("audit_logs must declare a conditional bridge");
        };
        // The four states are not a per-bridge list any more — the variant
        // means all of them — so what is left to check is that the taxonomy
        // itself stays complete and duplicate-free.
        let distinct: BTreeSet<ConditionalOwnership> =
            ConditionalOwnership::ALL.iter().copied().collect();
        assert_eq!(
            distinct.len(),
            ConditionalOwnership::ALL.len(),
            "the conditional-ownership taxonomy lists a state twice"
        );
        // The two outcomes that are easiest to conflate: a row that cannot be
        // resolved is kept so the audit reports it, a row that contradicts is
        // refused. Collapsing either into the agentless case is the original
        // defect.
        for required in [
            ConditionalOwnership::AgentlessAllowed,
            ConditionalOwnership::ResolvedAndVerified,
            ConditionalOwnership::PreservedUnresolved,
            ConditionalOwnership::ContradictionRejected,
        ] {
            assert!(
                distinct.contains(&required),
                "{required:?} is missing from the taxonomy; an undefined case in a BEFORE trigger \
                 resolves to writing whatever arrived"
            );
        }
        assert!(
            reason.contains("conditional rather than absent"),
            "the reason must say why a bridge is installed at all"
        );
    }

    /// A bridge that resolves through a parent must agree with the backfill.
    ///
    /// Write-time and backfill-time cannot be allowed to disagree about who owns
    /// a row: if the trigger resolves from `agents` while the backfill resolves
    /// through `sessions`, the same row gets two different owners depending on
    /// which one reached it first.
    #[test]
    fn bridges_are_authority_aware_and_match_the_backfill() {
        for (table, strategy) in TRANSITION {
            let authority = backfill_authority_for(table);
            // The parents each strategy claims to resolve through. `None` means
            // the bridge resolves straight from the row's own agent and must
            // declare no backfill authority at all.
            let claimed: Option<BTreeSet<&str>> = match strategy {
                TransitionalWrite::AgentBridge { .. }
                | TransitionalWrite::ConditionalAgentBridge { .. } => None,
                TransitionalWrite::SessionBridge { .. } => Some(BTreeSet::from(["sessions"])),
                TransitionalWrite::MemoryBridge { .. } => Some(BTreeSet::from(["memories"])),
                TransitionalWrite::MultiPathBridge { parents, .. } => {
                    Some(parents.iter().copied().collect())
                }
            };

            let Some(claimed) = claimed else {
                assert!(
                    authority.is_none(),
                    "`{table}` resolves straight from agents at write time but declares a \
                     backfill authority; the two would disagree"
                );
                continue;
            };

            let authority = authority.unwrap_or_else(|| {
                panic!("`{table}` resolves through a parent but declares no backfill authority")
            });
            assert!(
                authority.agreement.is_some(),
                "`{table}` resolves through a parent but its backfill has no agreement predicate"
            );
            if let TransitionalWrite::MultiPathBridge { parents, .. } = strategy {
                assert!(
                    parents.len() >= 2,
                    "`{table}` declares a multi-path bridge over fewer than two parents"
                );
                assert_eq!(
                    claimed.len(),
                    parents.len(),
                    "`{table}` lists a parent twice"
                );
            }

            // Exact, in both directions, for *every* parent-resolving strategy
            // — not only the multi-path one. A containment check cannot notice
            // a parent the bridge forgot: if a single-parent authority later
            // grew a second source, SessionBridge and MemoryBridge would still
            // pass while resolving from the smaller set, and write-time and
            // backfill-time would assign different owners to the same row.
            assert_eq!(
                claimed,
                authority_parents(authority),
                "`{table}`'s bridge and its backfill authority disagree about which parents are \
                 consulted"
            );
        }
    }

    /// No bridge may pick a parent when its sources disagree.
    ///
    /// This is the property the whole authority-aware model exists for. A bridge
    /// that resolves from whichever parent it joined first turns a cross-tenant
    /// link into a permanent one, and does it at write time where no backfill
    /// audit will ever see it.
    #[test]
    fn no_bridge_picks_a_side_when_parents_disagree() {
        for (table, strategy) in TRANSITION {
            if !strategy.resolves_through_a_parent() {
                continue;
            }
            let authority =
                backfill_authority_for(table).expect("checked by bridges_are_authority_aware");
            assert!(
                authority.agreement.is_some(),
                "`{table}` resolves through a parent with no agreement predicate"
            );
            let on_disagreement = authority.on_disagreement;
            assert!(
                on_disagreement.contains("left NULL") && on_disagreement.contains("reported"),
                "`{table}` must leave disagreeing rows NULL and report them, never choose: {}",
                on_disagreement
            );
        }
    }

    /// Trigger and function names follow the schema's convention, so a bridge
    /// cannot be half-declared.
    #[test]
    fn every_bridge_names_a_trigger_and_a_function() {
        for (table, strategy) in TRANSITION {
            assert!(
                strategy.trigger().starts_with("trg_"),
                "`{table}` names a trigger that breaks the convention: {}",
                strategy.trigger()
            );
            assert!(
                strategy.function().starts_with("fn_"),
                "`{table}` names a function that breaks the convention: {}",
                strategy.function()
            );
        }
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
        // The measured pairs now live in `PRE_EXISTING_NULLABLE` at module
        // level, so consumers get the same data the invariant checks.
        for object in PLANNED_OBJECTS {
            let Some(unenforced_when_null) = object.unenforced_when_null() else {
                continue;
            };
            let nullable = object.nullable_local_columns();
            assert_eq!(
                unenforced_when_null,
                !nullable.is_empty(),
                "`{}` disagrees with the live schema about whether its key can contain NULL \
                 (nullable local columns: {nullable:?}); MATCH SIMPLE leaves such rows unchecked, \
                 so the flag decides whether this key is evidence of ownership",
                object.name()
            );
            // The typed semantics and the flag are two views of one fact and
            // may never disagree.
            let expected = if unenforced_when_null {
                MatchSemantics::NullKeyRowsUnchecked
            } else {
                MatchSemantics::AllRowsChecked
            };
            assert_eq!(object.match_semantics(), Some(expected));
        }
        // Every measured pair must name a table the registry knows, or the
        // list has drifted from the schema it claims to describe.
        for (table, _) in PRE_EXISTING_NULLABLE {
            assert!(
                super::super::inventory::entry(table).is_some(),
                "`{table}` is recorded as having a NULL-able column but is not a registered table"
            );
        }
        // A non-foreign-key object has no MATCH semantics to report.
        for object in PLANNED_OBJECTS {
            if object.referenced().is_none() {
                assert_eq!(object.match_semantics(), None);
                assert_eq!(object.unenforced_when_null(), None);
                assert_eq!(object.transition_match_semantics(), None);
            }
        }
    }

    /// The transition window is strictly weaker than the end state.
    ///
    /// While a tranche runs, its ownership columns are genuinely NULL — that is
    /// what the backfill is for — so `MATCH SIMPLE` skips those rows too. A key
    /// recorded as fully enforced can therefore be enforcing nothing until
    /// FUTURE STEP 7 tightens the columns, which is precisely why the audit and
    /// not the foreign key is the gate. Both facts are published; neither may
    /// be mistaken for the other.
    #[test]
    fn transition_semantics_are_never_stronger_than_the_end_state() {
        let mut weaker_during_transition = 0usize;
        for object in PLANNED_OBJECTS {
            if object.unenforced_when_null().is_none() {
                continue;
            }
            let end_state = object.nullable_local_columns();
            let during = object.transition_nullable_local_columns();
            for column in &end_state {
                assert!(
                    during.contains(column),
                    "`{}` claims `{column}` is NULL-able in the end state but not during the \
                     transition; nothing tightens a column *into* being NULL-able",
                    object.name()
                );
            }
            if during.len() > end_state.len() {
                weaker_during_transition += 1;
                assert_eq!(
                    object.transition_match_semantics(),
                    Some(MatchSemantics::NullKeyRowsUnchecked),
                    "`{}` has transitional NULL-able columns, so MATCH SIMPLE skips those rows \
                     while the tranche runs",
                    object.name()
                );
            }
        }
        // The distinction has to be load-bearing for something, or publishing
        // it is noise. Every ownership foreign key is unenforced until its
        // backfill completes.
        assert!(
            weaker_during_transition > 0,
            "no planned key is weaker during the transition; if that is genuinely true, the \
             transition fields are redundant and should go"
        );
    }

    /// An already-current target is a dependency prerequisite, not a unit of
    /// work.
    ///
    /// `AlreadyCurrent` used to be a label with no operational consequence: the
    /// object still reported a creating tranche and a concurrent-build lock, so
    /// anything generating DDL from a tranche's object list would emit `CREATE
    /// UNIQUE INDEX CONCURRENTLY agents_tenant_id_id_key` — against the index
    /// that already exists, which fails. It has to be excluded from the
    /// worklist while staying in the plan, because fifteen foreign keys rely on
    /// it to prove their parent tuple is covered.
    #[test]
    fn already_current_targets_are_never_scheduled_as_work() {
        let target = PLANNED_OBJECTS
            .iter()
            .find(|o| o.name() == "agents_tenant_id_id_key")
            .expect("agents declares the target its dependants rely on");

        assert!(!target.requires_creation());
        assert_eq!(target.creating_tranche(), None, "nothing creates it");
        assert_eq!(target.creation_lock(), None, "nothing locks it");
        assert_eq!(target.validating_tranche(), None, "nothing validates it");
        assert!(
            !target.concurrent_build_required(),
            "nothing is built, so it dictates no `-- no-transaction` header"
        );
        assert_eq!(
            target.attachment_lock(),
            None,
            "nothing is built, so nothing is attached"
        );

        // Excluded from every PREPARE worklist, in every tranche.
        for tranche in Tranche::ALL {
            assert!(
                !prepare_worklist(*tranche)
                    .iter()
                    .any(|o| o.name() == target.name()),
                "`{}` reached {}'s PREPARE worklist; generating DDL for it would fail against \
                 the object already in the schema",
                target.name(),
                tranche
            );
        }

        // Retained, though — and retained *for* something. If nothing depended
        // on it, keeping it would be dead weight rather than a prerequisite.
        assert!(
            planned_in(target.declared_in())
                .iter()
                .any(|o| o.name() == target.name()),
            "the target must stay in the plan so dependants can find it"
        );
        let dependants = PLANNED_OBJECTS
            .iter()
            .filter(|o| o.referenced() == Some(("agents", target.local_columns())))
            .count();
        assert!(
            dependants > 0,
            "`{}` is retained as a dependency prerequisite, but nothing depends on it",
            target.name()
        );

        // Status and behaviour may never disagree.
        for object in PLANNED_OBJECTS {
            let PlannedObject::UniqueTarget { status, .. } = object else {
                continue;
            };
            assert_eq!(
                object.creating_tranche().is_some(),
                status.requires_creation(),
                "`{}` disagrees with its own status about whether it is created",
                object.name()
            );
            assert_eq!(
                object.creation_lock().is_some(),
                status.requires_creation(),
                "`{}` disagrees with its own status about whether it takes a lock",
                object.name()
            );
        }

        // Everything else in the plan is real work.
        for object in PLANNED_OBJECTS {
            if object.name() == target.name() {
                continue;
            }
            assert!(
                object.requires_creation(),
                "`{}` is not created by anything; if that is deliberate, it needs a status that \
                 says so",
                object.name()
            );
        }
    }

    /// Nothing in Step 4B may plan a rewrite or a blocking scan. Those belong to
    /// FUTURE STEP 7.
    #[test]
    fn step_4b_plans_no_rewrite_and_no_blocking_scan() {
        for object in PLANNED_OBJECTS {
            // Nothing is created, so nothing is locked.
            let Some(lock) = object.creation_lock() else {
                continue;
            };
            if object.creating_tranche() == Some(Tranche::FinalConstraintTightening) {
                continue;
            }
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
        // Both VALIDATE profiles take SHARE UPDATE EXCLUSIVE on the table being
        // validated; they differ only in whether a parent is locked alongside
        // it, which a CHECK does not have.
        assert!(LockProfile::ValidateCheckConstraint
            .locks()
            .contains("SHARE UPDATE EXCLUSIVE"));
        for object in PLANNED_OBJECTS {
            let Some(validation) = object.validation_lock() else {
                assert!(
                    !object.not_valid_permitted(),
                    "`{}` permits NOT VALID but declares no validation lock",
                    object.name()
                );
                continue;
            };
            assert!(
                object.not_valid_permitted(),
                "`{}` declares a validation lock but cannot be added NOT VALID",
                object.name()
            );
            assert!(
                matches!(
                    validation,
                    LockProfile::ValidateConstraint | LockProfile::ValidateCheckConstraint
                ) && validation.locks().contains("SHARE UPDATE EXCLUSIVE"),
                "`{}` permits NOT VALID so it must validate under a weaker lock, not {}",
                object.name(),
                validation.as_str()
            );
        }
    }

    /// A CHECK added `NOT VALID` is not a foreign key and may not borrow its
    /// lock profile.
    ///
    /// The two differ on both axes an operator plans around. The foreign-key
    /// profile promises `SHARE ROW EXCLUSIVE`, which still permits reads, and
    /// names a referenced table that is locked alongside the child. A CHECK
    /// takes `ACCESS EXCLUSIVE` — briefly, and scanning nothing — on one table
    /// and no other. Describing a CHECK with the foreign-key profile understates
    /// the lock and invents a second table.
    #[test]
    fn check_constraints_declare_their_own_lock_profile() {
        let check = LockProfile::AddCheckNotValid.locks();
        assert!(
            check.contains("ACCESS EXCLUSIVE"),
            "a CHECK takes the stronger lock: {check}"
        );
        assert!(
            !check.contains("referenced table"),
            "a CHECK names no parent, so no referenced table is locked: {check}"
        );
        assert!(
            check.contains("no row scan"),
            "NOT VALID is what makes it brief: {check}"
        );
        assert!(LockProfile::ALL.contains(&LockProfile::AddCheckNotValid));

        for object in PLANNED_OBJECTS {
            match object {
                PlannedObject::Constraint { .. } => assert_eq!(
                    object.creation_lock(),
                    Some(LockProfile::AddCheckNotValid),
                    "`{}` is a CHECK and must declare the CHECK profile",
                    object.name()
                ),
                PlannedObject::ForeignKey { .. } => assert_eq!(
                    object.creation_lock(),
                    Some(LockProfile::AddForeignKeyNotValid),
                    "`{}` is a foreign key and must declare the foreign-key profile",
                    object.name()
                ),
                _ => {}
            }
            // Whichever of the two it is, validation stays separate and weaker
            // — and a CHECK validates under its own profile, because it has no
            // parent to take ROW SHARE on.
            if object.not_valid_permitted() {
                let expected = match object {
                    PlannedObject::Constraint { .. } => LockProfile::ValidateCheckConstraint,
                    _ => LockProfile::ValidateConstraint,
                };
                assert_eq!(object.validation_lock(), Some(expected));
                assert_ne!(
                    object.creation_lock(),
                    object.validation_lock(),
                    "`{}` must not conflate declaring a constraint with validating it",
                    object.name()
                );
            }
        }
        // A CHECK's validation may not claim a referenced table.
        let check = LockProfile::ValidateCheckConstraint.locks();
        assert!(check.contains("SHARE UPDATE EXCLUSIVE"));
        assert!(
            !check.contains("ROW SHARE"),
            "a CHECK has no parent, so its validation locks no second table: {check}"
        );
        assert!(LockProfile::ALL.contains(&LockProfile::ValidateCheckConstraint));
    }

    /// No table's prose may claim that *validation* takes the creation lock.
    ///
    /// Three registry entries said "the FK validation takes SHARE ROW
    /// EXCLUSIVE". That is the lock `ADD CONSTRAINT ... NOT VALID` takes, held
    /// briefly while nothing is scanned; `VALIDATE CONSTRAINT` takes the weaker
    /// `SHARE UPDATE EXCLUSIVE` on the child with `ROW SHARE` on the parent, and
    /// the difference is the entire reason the two are split. `SHARE ROW
    /// EXCLUSIVE` conflicts with `ROW EXCLUSIVE`, so the old wording promised
    /// that the long scan blocks ordinary writes — the opposite of what the
    /// design achieves, and the same class of error the typed
    /// [`LockProfile`] exists to prevent. Checked here so the prose cannot
    /// drift back while the type stays right.
    #[test]
    fn no_lock_profile_prose_confuses_validation_with_creation() {
        for entry in super::super::inventory::REGISTRY {
            let prose = entry.plan.lock_profile;
            let lowered = prose.to_ascii_lowercase();
            if !lowered.contains("validat") {
                continue;
            }
            // A sentence that mentions validation and names SHARE ROW
            // EXCLUSIVE must also name the weaker lock validation really takes,
            // or it is describing the creation lock as if it were validation's.
            if lowered.contains("share row exclusive") {
                assert!(
                    lowered.contains("share update exclusive"),
                    "`{}` describes validation as taking SHARE ROW EXCLUSIVE without naming the \
                     weaker lock VALIDATE actually takes; that overstates the impact and \
                     contradicts LockProfile::ValidateConstraint: {prose:?}",
                    entry.table
                );
            }
        }
        // The typed profiles remain the authority the prose is checked against.
        assert!(LockProfile::ValidateConstraint
            .locks()
            .contains("SHARE UPDATE EXCLUSIVE"));
        assert!(LockProfile::AddForeignKeyNotValid
            .locks()
            .contains("SHARE ROW EXCLUSIVE"));
    }

    /// A concurrent build must be flagged, because it dictates the migration
    /// file's `-- no-transaction` header.
    #[test]
    fn concurrently_built_objects_are_flagged() {
        for object in PLANNED_OBJECTS {
            let concurrent = object.concurrent_build_required();
            // An index-shaped object needs a concurrent build only if it is
            // built at all: an already-current target dictates no migration
            // file's header, because it has no migration file.
            let is_built_index = matches!(
                object,
                PlannedObject::Index { .. } | PlannedObject::UniqueTarget { .. }
            ) && object.requires_creation();
            assert_eq!(
                concurrent,
                is_built_index,
                "`{}` disagrees with itself about needing a concurrent build",
                object.name()
            );
        }
        assert!(CONCURRENT_BUILD_MECHANISM.contains("-- no-transaction"));
    }

    /// A built unique target is two operations, and both are now published.
    ///
    /// The attach phase was real before this test existed — it was described in
    /// a comment inside `creation_lock_kind` — but a comment is not in the
    /// artifacts. An operator sizing a maintenance window from the rendered
    /// contract saw `SHARE UPDATE EXCLUSIVE` alone and would conclude no reader
    /// is ever blocked, which is the opposite of what the attach does.
    #[test]
    fn a_built_unique_target_publishes_both_phases_of_its_build() {
        let mut built_targets = 0;
        for object in PLANNED_OBJECTS {
            let expected =
                matches!(object, PlannedObject::UniqueTarget { .. }) && object.requires_creation();
            assert_eq!(
                object.attachment_lock().is_some(),
                expected,
                "`{}` disagrees with itself about having an attach phase",
                object.name()
            );

            if !expected {
                continue;
            }
            built_targets += 1;

            // Both halves, and the right way round: the slow concurrent build
            // first, the brief strong lock second.
            assert_eq!(
                object.creation_lock(),
                Some(LockProfile::CreateIndexConcurrently),
                "`{}` must still report the concurrent build as its creation lock",
                object.name()
            );
            assert_eq!(
                object.attachment_lock(),
                Some(LockProfile::AddUniqueUsingIndex),
                "`{}` must report the attach as a distinct lock",
                object.name()
            );
        }

        // If no target were built, the accessor would be vacuous and the
        // assertions above would pass without proving anything.
        assert!(
            built_targets > 0,
            "no unique target is built, so nothing exercises the attach phase"
        );
    }

    /// The attach profile must not inherit the wording of the phase it is
    /// distinguished from, or splitting them changes nothing a reader can see.
    #[test]
    fn the_attach_profile_names_one_table_and_no_scan() {
        let locks = LockProfile::AddUniqueUsingIndex.locks();
        assert!(
            locks.contains("ACCESS EXCLUSIVE"),
            "the attach takes the strongest lock; that is the whole reason to publish it"
        );
        for borrowed in ["SHARE UPDATE EXCLUSIVE", "ROW SHARE", "referenced table"] {
            assert!(
                !locks.contains(borrowed),
                "the attach profile borrowed `{borrowed}` from another operation — it locks one \
                 table, names no parent, and is not the concurrent build"
            );
        }
        assert!(LockProfile::ALL.contains(&LockProfile::AddUniqueUsingIndex));
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

    /// A narrowing tuple must carry the probe that would catch its collisions;
    /// the other two shapes must not pretend to.
    #[test]
    fn narrowing_transitions_declare_a_collision_probe() {
        for transition in UNIQUENESS_TRANSITIONS {
            assert!(
                !transition.reason.is_empty(),
                "`{}` classifies its uniqueness transition with no reason",
                transition.table
            );
            // Probe-or-not is now decided by the type: `Narrowing` carries its
            // probe, and the other two shapes have nowhere to put one. What is
            // still checkable is that the accessor agrees with the shape, and
            // that a probe, where required, is not blank.
            match transition.change {
                UniquenessChange::AlreadyImplied => {
                    assert_eq!(transition.collision_probe(), None);
                    assert_eq!(
                        transition.from, transition.to,
                        "`{}` claims its tuple is already implied, but the tuple changes",
                        transition.table
                    );
                }
                UniquenessChange::Relaxation => {
                    assert_eq!(transition.collision_probe(), None);
                    assert!(
                        transition.to.len() >= transition.from.len(),
                        "`{}` claims a relaxation while dropping columns from the tuple; a \
                         shorter tuple distinguishes less finely, which is a narrowing",
                        transition.table
                    );
                }
                UniquenessChange::Narrowing { collision_probe } => {
                    assert!(
                        !collision_probe.trim().is_empty(),
                        "`{}` narrows its uniqueness tuple and declares a blank collision probe",
                        transition.table
                    );
                }
            }
            assert_eq!(
                transition.change.shape().can_collide(),
                transition.collision_probe().is_some(),
                "`{}` disagrees with itself about whether a collision is possible",
                transition.table
            );
        }
    }

    /// The current contract plans no narrowing anywhere.
    ///
    /// This is what makes `future_unique_columns = None` a conclusion rather
    /// than an omission, and it is why the pre-check query never runs. The
    /// reason code stays in the catalogue regardless: a future narrowing tuple
    /// makes it fire again.
    #[test]
    fn the_current_contract_plans_no_narrowing() {
        let narrowing: Vec<&str> = UNIQUENESS_TRANSITIONS
            .iter()
            .filter(|t| t.shape().can_collide())
            .map(|t| t.table)
            .collect();
        assert!(
            narrowing.is_empty(),
            "{narrowing:?} narrow a uniqueness tuple, so future_unique_columns can no longer be \
             None for them and the pre-check has to run"
        );
        for entry in super::super::inventory::REGISTRY {
            assert!(
                entry.plan.future_unique_columns.is_none(),
                "`{}` declares future_unique_columns while no table narrows; either the pre-check \
                 has nothing to check, or a narrowing transition is missing from the register",
                entry.table
            );
        }
        assert!(
            ReasonCode::ALL.contains(&ReasonCode::FutureTenantUniquenessCollision),
            "the reason code is a stable contract and stays in the catalogue even while \
             unreachable"
        );

        // The classified `to` tuple must be the one the plan actually creates,
        // or the classification is about a constraint nobody is building.
        let sessions = uniqueness_transition_for("sessions").expect("sessions changes its tuple");
        let target = PLANNED_OBJECTS
            .iter()
            .find(|o| o.name() == "sessions_tenant_agent_session_key")
            .expect("sessions declares its identity target");
        assert_eq!(
            target.local_columns(),
            sessions.to,
            "the uniqueness transition describes a tuple the plan does not create"
        );
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

    /// A table may only require zero `ORPHANED_AGENT_REFERENCE` if it has a
    /// path that can produce one.
    ///
    /// Stated structurally rather than as a list, so it holds for tables added
    /// later. The audit derives the orphan code from the path kind: only an
    /// `AgentText` or `AgentUuid` path yields `ORPHANED_AGENT_REFERENCE`. A
    /// table with neither — `co_access_edges` is the only one today — was
    /// requiring zero of something structurally impossible, which reads as a
    /// gate and is not one.
    #[test]
    fn orphaned_agent_reference_is_gated_only_where_an_agent_path_exists() {
        use super::super::inventory::PathKind;
        for entry in super::super::inventory::REGISTRY {
            let has_agent_path = entry
                .canonical_path
                .iter()
                .chain(entry.secondary_paths.iter())
                .any(|path| {
                    matches!(
                        path.kind,
                        PathKind::AgentText { .. } | PathKind::AgentUuid { .. }
                    )
                });
            let gates_it = entry
                .plan
                .required_zero_codes
                .contains(&ReasonCode::OrphanedAgentReference);
            assert!(
                has_agent_path || !gates_it,
                "`{}` requires zero ORPHANED_AGENT_REFERENCE but has no AgentText or AgentUuid \
                 path; the audit can never produce that code for it, so the gate cannot fail",
                entry.table
            );
        }
        // The one table this applies to today must be recorded, so the removal
        // is documented rather than merely done.
        assert!(
            UNREACHABLE_REQUIRED_ZERO
                .iter()
                .any(|u| u.table == "co_access_edges"
                    && u.code == ReasonCode::OrphanedAgentReference),
            "co_access_edges' removal of ORPHANED_AGENT_REFERENCE must be recorded with a reason"
        );
        // …and the two codes that remain reachable through its memory chain
        // must not have been dropped along with it.
        let entry = super::super::inventory::entry("co_access_edges").expect("registered");
        for still_reachable in [ReasonCode::LegacyUnmapped, ReasonCode::UnmappedAgent] {
            assert!(
                entry.plan.required_zero_codes.contains(&still_reachable),
                "`co_access_edges` dropped {still_reachable}, which is still reachable \
                 transitively through memory_a -> memories.agent_id -> agents.tenant_id"
            );
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
        assert!(FINALIZE_GUARD.contains("tenancy_backfill_checkpoints"));
        assert!(FINALIZE_GUARD.contains("sqlx migrate run"));
        assert_eq!(Stage::ALL.len(), 3);
        assert_eq!(Stage::Prepare.to_string(), "PREPARE");
    }

    /// The guard may not name `agent_tenancy_migrations` as its evidence.
    ///
    /// That table has no tranche, no digest, no cursor and no status column, so
    /// a guard reading it cannot distinguish "tranche 3 completed against the
    /// current contract" from "somebody once ran the agent assignment".
    ///
    /// This test checks only that the contract *says* so. The claim about the
    /// live schema is a claim about a database, so it is proven against one —
    /// see `agent_tenancy_migrations_cannot_carry_checkpoint_evidence` in
    /// `audit_db_tests`. A hardcoded column list here would keep passing
    /// forever if a migration added the very column it denies.
    #[test]
    fn the_finalize_guard_does_not_rely_on_agent_tenancy_migrations() {
        assert!(
            FINALIZE_GUARD.contains("agent_tenancy_migrations cannot serve"),
            "the guard must say why the existing table is not the evidence, or a later reader \
             will point it back at the wrong table"
        );
        for absent in ["tranche", "digest", "cursor", "status"] {
            assert!(
                FINALIZE_GUARD.contains(absent),
                "the guard must name `{absent}` as something the existing table lacks, so the \
                 reason is specific rather than an assertion of inadequacy"
            );
        }
    }

    /// The checkpoint table must cover every category the protocol asserts on.
    ///
    /// Checking roles rather than column names means renaming a column cannot
    /// silently remove the thing FINALIZE depends on.
    #[test]
    fn the_checkpoint_table_covers_every_required_role() {
        let table = TENANCY_BACKFILL_CHECKPOINTS;
        for required in [
            CheckpointColumnRole::Tranche,
            CheckpointColumnRole::ContractDigest,
            CheckpointColumnRole::Status,
            CheckpointColumnRole::ResumableCursor,
            CheckpointColumnRole::RowCount,
            CheckpointColumnRole::BlockingCount,
            CheckpointColumnRole::Timestamp,
        ] {
            assert!(
                table.columns.iter().any(|c| c.role == required),
                "`{}` declares no column for {required:?}; the protocol asserts on it",
                table.name
            );
        }
        // The tuple that makes a completion authoritative must be columns the
        // table actually has, or the unique index cannot be created.
        for column in table.unique_completion {
            assert!(
                table.columns.iter().any(|c| c.name == *column),
                "`{}` keys its authoritative completion on `{column}`, which it does not declare",
                table.name
            );
        }
        // Neither half of the completion key may be NULL-able: a NULL in a
        // unique tuple does not collide, so two NULL-digest completions would
        // both be accepted.
        for column in table.columns {
            if table.unique_completion.contains(&column.name) {
                assert!(
                    !column.nullable,
                    "`{}` is part of the authoritative-completion key and must be NOT NULL; NULLs \
                     do not collide, so duplicates would be admitted",
                    column.name
                );
            }
        }
        // Pinned, not merely well-formed. A generic "the named columns exist"
        // check would pass just as happily for `["tranche"]` alone, which is a
        // different and wrong guarantee: it would make a completion unique per
        // tranche regardless of which contract it was recorded against.
        assert_eq!(
            table.unique_completion,
            &["tranche", "contract_digest"],
            "authoritative completion must be keyed on the tranche *and* the contract it ran \
             against"
        );
        // …and scoped to COMPLETED. A plain UNIQUE would reject the row an
        // operator writes when restarting a tranche against the same digest,
        // which is exactly what ABANDONED exists to preserve.
        assert_eq!(
            table.unique_completion_when,
            CheckpointStatus::Completed,
            "the uniqueness must be partial on COMPLETED, or retained ABANDONED history collides \
             with the live attempt"
        );
        assert_eq!(
            table.unique_completion_when, FINALIZE_PRECONDITION.required_status,
            "the status the uniqueness is scoped to and the status FINALIZE accepts must be the \
             same one, or the index guarantees uniqueness of something the guard does not read"
        );
        // Roles the ruling states in the plural must actually be plural: one
        // row count cannot express progress, and one timestamp cannot express
        // both when a tranche started and when it finished.
        for (role, least) in [
            (CheckpointColumnRole::RowCount, 2),
            (CheckpointColumnRole::Timestamp, 2),
        ] {
            let found = table.columns.iter().filter(|c| c.role == role).count();
            assert!(
                found >= least,
                "`{}` declares {found} column(s) for {role:?}; at least {least} are needed",
                table.name
            );
        }
    }

    /// FINALIZE's precondition must be exact on all four axes.
    ///
    /// Any one of them relaxed reintroduces the failure the three-stage split
    /// exists to prevent: validating a constraint over rows nobody backfilled.
    #[test]
    fn the_finalize_precondition_is_exact() {
        // Checked as one value rather than four assertions, so relaxing any
        // single axis fails here with the whole expected shape in the output.
        assert_eq!(
            FINALIZE_PRECONDITION,
            FinalizePrecondition {
                required_status: CheckpointStatus::Completed,
                tranche_must_match_exactly: true,
                digest_must_match_current: true,
                max_blocking_count: 0,
            },
            "relaxing any axis of the FINALIZE precondition reintroduces validating a constraint \
             over rows nobody backfilled"
        );
        assert_eq!(CheckpointStatus::Completed.as_str(), "COMPLETED");
        // Exactly one status may satisfy the guard. If a second ever reads as
        // "done", the guard stops being a gate.
        let accepted = CheckpointStatus::ALL
            .iter()
            .filter(|s| **s == FINALIZE_PRECONDITION.required_status)
            .count();
        assert_eq!(
            accepted, 1,
            "exactly one checkpoint status may satisfy FINALIZE"
        );
    }

    /// The checkpoint table has to exist before the first backfill records
    /// anything, and it may not appear as an unregistered side-table.
    #[test]
    fn the_checkpoint_table_is_created_before_any_backfill_needs_it() {
        let earliest = PLANNED_OBJECTS
            .iter()
            .map(|o| o.declared_in())
            .min()
            .expect("the plan is not empty");
        for table in PLANNED_TABLES {
            assert!(
                table.creating_tranche <= earliest,
                "`{}` is created in {} but a backfill needs it from {earliest} onward",
                table.name,
                table.creating_tranche
            );
            assert!(
                table.joins_contract.contains("REGISTRY"),
                "`{}` must join the Step 4A registry in the same change, or the verifier reports \
                 it as drift",
                table.name
            );
        }
    }

    /// The chosen transitional strategy must state the property it guarantees,
    /// not merely name a mechanism.
    #[test]
    fn the_transitional_strategy_states_its_guarantee() {
        assert!(TRANSITIONAL_WRITE_RATIONALE.contains("dual-write"));
        assert!(TRANSITIONAL_WRITE_GUARANTEE.contains("resumed legacy writer"));
        assert!(TRANSITIONAL_WRITE_GUARANTEE.contains("UNMAPPED_AGENT"));
    }

    /// Every compatibility prerequisite names a planned object that exists, and
    /// orders the parent before the child.
    ///
    /// A prerequisite naming an object the plan does not create would be a
    /// blocker nothing can ever clear; one that ordered the child first would
    /// re-specify the bug it exists to prevent.
    #[test]
    fn compatibility_prerequisites_order_the_parent_before_the_child() {
        for prerequisite in COMPATIBILITY_PREREQUISITES {
            let object = PLANNED_OBJECTS
                .iter()
                .find(|o| o.name() == prerequisite.planned_object)
                .unwrap_or_else(|| {
                    panic!(
                        "`{}` is a prerequisite for `{}`, which the plan never creates",
                        prerequisite.planned_object, prerequisite.planned_object
                    )
                });
            let (parent, _) = object
                .referenced()
                .expect("a compatibility prerequisite only makes sense for a referencing object");
            let child = object.table();
            let order = prerequisite.required_write_order;
            let parent_at = order.iter().position(|t| *t == parent);
            let child_at = order.iter().position(|t| *t == child);
            let (Some(parent_at), Some(child_at)) = (parent_at, child_at) else {
                panic!(
                    "`{}` must order both `{parent}` and `{child}`; it orders {order:?}",
                    prerequisite.planned_object
                );
            };
            assert!(
                parent_at < child_at,
                "`{}` writes `{child}` before `{parent}`; the referenced row must exist first, or \
                 the very first write after the key lands fails",
                prerequisite.planned_object
            );
        }
    }

    /// A prerequisite must demand a transaction, a drain, and a rollback path.
    ///
    /// Ordering alone is not sufficient. Between two autocommit statements a
    /// reader sees a session with no working memory and a crash leaves the pair
    /// half-written; a deploy alone is not sufficient while an older binary is
    /// still serving the old order; and once the key exists, rolling the code
    /// back is not by itself a recovery path.
    #[test]
    fn compatibility_prerequisites_demand_transaction_drain_and_rollback() {
        for prerequisite in COMPATIBILITY_PREREQUISITES {
            assert!(
                prerequisite.single_transaction,
                "`{}` must require one transaction",
                prerequisite.planned_object
            );
            assert!(
                prerequisite.drain_incompatible_binaries,
                "`{}` must require incompatible binaries to be drained; deploying the fix while \
                 an older release still serves traffic leaves the old order running",
                prerequisite.planned_object
            );
            assert_eq!(
                prerequisite.writers_deployed_before,
                Stage::Prepare,
                "`{}` must have compatible writers serving before PREPARE, because NOT VALID \
                 constrains new writes from the moment the catalog change commits",
                prerequisite.planned_object
            );
            // Both alternatives, not either. The recovery path is "drop the
            // key *or* keep a bridge"; losing one of them from the text leaves
            // an operator with a single option presented as the only one.
            let rollback = prerequisite.rollback;
            for alternative in ["dropping", "bridge"] {
                assert!(
                    rollback.contains(alternative),
                    "`{}` must state both rollback alternatives — removing the key and retaining \
                     a bridge — but does not mention `{alternative}`: {rollback:?}",
                    prerequisite.planned_object
                );
            }
        }
    }

    /// PREPARE for the tranche that creates `working_memory_session_fkey` is
    /// blocked while the prerequisite stands.
    ///
    /// This is the proof the ruling asked for: the blocker is reachable from the
    /// tranche, so a scheduler consulting the plan cannot start that PREPARE
    /// without seeing it.
    #[test]
    fn prepare_cannot_be_scheduled_while_a_prerequisite_is_outstanding() {
        let fkey = PLANNED_OBJECTS
            .iter()
            .find(|o| o.name() == "working_memory_session_fkey")
            .expect("working_memory must reference sessions");
        let creating = fkey
            .creating_tranche()
            .expect("a foreign key is always created by a tranche");
        let blockers = prepare_blockers(creating);
        assert!(
            blockers
                .iter()
                .any(|b| b.planned_object == "working_memory_session_fkey"),
            "{creating} creates working_memory_session_fkey, so its PREPARE must report the \
             writer prerequisite as a blocker"
        );
        // The prerequisite must name the writer it is about, so that clearing it
        // is a specific piece of work rather than a judgement call.
        let prerequisite =
            compatibility_prerequisite_for("working_memory_session_fkey").expect("declared above");
        assert!(
            prerequisite.blocker.contains("upsert_working_memory"),
            "the prerequisite must name the incompatible writer"
        );
        assert!(
            prerequisite.blocker.contains("NOT VALID"),
            "the prerequisite must say why NOT VALID does not defer the problem"
        );
        // Tranches with no outstanding prerequisite must not acquire one by
        // accident — an empty list is the normal case and has to stay meaningful.
        assert!(
            prepare_blockers(Tranche::Memories).is_empty(),
            "tranche 3 has no recorded writer prerequisite; if one is added, record why"
        );
    }
}
