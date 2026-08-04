//! Tranche BACKFILL: the second of the three Step 4B stages.
//!
//! PREPARE (migration `0032` and the index migrations after it) added the
//! ownership columns NULL-able and attached a bridge trigger to every table
//! that received them, so **new** rows have been owned since the moment PREPARE
//! applied. This module owns the other half: walking the rows that already
//! existed and resolving their ownership from the same authority the bridge
//! uses.
//!
//! FINALIZE is deliberately not here. It is a separate stage with a separate
//! migration, and [`plan::FINALIZE_PRECONDITION`] is the contract between them:
//! it accepts a `COMPLETED` checkpoint for the finalizing tranche whose
//! `contract_digest` equals the current one and whose `blocking_count` is zero.
//! Everything this module does is in service of producing a checkpoint row that
//! is either honestly `COMPLETED` or honestly not.
//!
//! ## Why the engine is a library and the CLI is a caller
//!
//! An operator runs this from a subcommand today and a management endpoint will
//! run it later. Those are two front doors onto one piece of behaviour, and the
//! behaviour is the part with the invariants — batching, cursor persistence,
//! reconciliation, the refusal to complete over a dirty audit. Putting it in the
//! command handler would mean the endpoint either re-implements it or shells
//! out, and a re-implementation is where the second front door gets a subtly
//! different completion rule.
//!
//! ## Three properties are load-bearing
//!
//! * **The bridge is the final authority on every write this module makes.**
//!   The backfill's `UPDATE` sets `agent_uuid`/`tenant_id` from `agents`, and
//!   then the `BEFORE UPDATE` bridge re-reads `agents` and overwrites both with
//!   its own resolution. The backfill therefore *cannot* write a value that
//!   disagrees with `agents`, even if this module's SQL were wrong, and it
//!   cannot resurrect ownership for a row whose agent has since stopped
//!   resolving. That is why no `agreement` predicate appears here: for tranche 1
//!   there is exactly one authority, and [`plan::BACKFILL_AUTHORITY`] carries no
//!   entry for any of these five tables precisely because there is no second
//!   path to agree with.
//!
//! * **Progress is measured by cursor advance, never by rows affected.** A row
//!   whose agent resolves to an agent with a NULL `tenant_id` is written, stays
//!   NULL, and is still not settled. Counting rows affected would either spin on
//!   it forever or read it as done. The scan advances past it and the audit
//!   reports it.
//!
//! * **Completion is reconciled, not accumulated.** The intermediate
//!   `rows_total` / `rows_backfilled` on an `IN_PROGRESS` row are progress
//!   indicators: they are exact as of the counts taken at run start and drift
//!   under concurrent inserts. Before any `COMPLETED` is written both are
//!   recounted from the live tables, which is what makes the
//!   `tenancy_backfill_checkpoints_completed_accounting_ck` CHECK
//!   (`rows_backfilled = rows_total`) a statement about the database rather than
//!   about this module's arithmetic.
//!
//! ## The identifier boundary
//!
//! Two queries here are built at runtime, because the table and its legacy
//! identifier column vary per target. They observe the same boundary
//! `super::audit` documents and for the same reason: every interpolated
//! identifier is a `&'static str` from [`inventory::REGISTRY`], each is
//! validated against `[a-z_][a-z0-9_]*` and quoted by
//! [`audit::SqlIdentifier`], and a value that fails validation is rejected
//! before any query string exists. Nothing from a row, a catalog or an operator
//! is ever interpolated — the resume cursor and every count and key are bound
//! parameters. `AssertSqlSafe` is what sqlx 0.9 requires to accept a non-static
//! string; it asserts nothing this module has not already enforced.

use anyhow::{anyhow, bail, Context, Result};
use sqlx::{AssertSqlSafe, PgPool, Row};
use uuid::Uuid;

use super::audit::{self, SqlIdentifier};
use super::inventory::{self, PathKind, Tranche};
use super::plan::{self, CheckpointStatus, TransitionalWrite};
use super::report;

/// Serialises concurrent tranche backfills.
///
/// Distinct from `BACKFILL_ADVISORY_LOCK_ID` in the parent module, which guards
/// the Step 1 `agents.tenant_id` backfill. They protect different tables and
/// different checkpoints; sharing an id would make either one block the other
/// for no reason.
const TRANCHE_BACKFILL_ADVISORY_LOCK_ID: i64 = 0x4145_4F4E_5442; // "AEONTB"

/// The largest batch a caller may ask for.
///
/// Not a tuning ceiling — a bound on how long one `UPDATE` can hold row locks
/// against the live write path. The bridges are attached to tables the
/// application writes to, so an unbounded batch is a lock-duration decision
/// disguised as a parameter.
pub const MAX_BATCH_SIZE: i64 = 50_000;

pub const DEFAULT_BATCH_SIZE: i64 = 1_000;

/// Connections a backfill needs before it can make any progress at all.
///
/// One is held for the whole run by the session advisory lock and never issues
/// another statement; every batch, every checkpoint write and the final
/// reconciliation run on a second. With a pool of one, `acquire` takes the only
/// connection and the first query after it waits out `DB_ACQUIRE_TIMEOUT_SECS`
/// before failing with a timeout that says nothing about the cause.
///
/// Enforced in the engine rather than only in the CLI. The CLI is one caller of
/// three — a management endpoint is coming — and a check that lives in the
/// command handler is one the endpoint would have to remember to repeat.
pub const MIN_BACKFILL_CONNECTIONS: u32 = 2;

// ── Targets ──────────────────────────────────────────────────────────────────

/// One table a tranche's backfill has to walk.
///
/// Derived from the registry rather than hand-listed. A second list of the five
/// tranche-1 tables would be a second place for the set to drift from the
/// tenancy decisions, and the drift would be silent: a table dropped from this
/// list backfills nothing, reconciles against nothing, and completes clean.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackfillTarget {
    pub table: &'static str,
    /// The legacy identifier column the ownership path starts from.
    pub agent_column: &'static str,
    /// Whether a row naming no agent is a legitimate, permanently unowned row.
    ///
    /// True for `audit_logs` alone. Its bridge is a
    /// [`TransitionalWrite::ConditionalAgentBridge`], its canonical ownership
    /// path is nullable, and `LEGACY_UNMAPPED` is deliberately absent from its
    /// `required_zero_codes`. Those are three independent statements of one
    /// fact, and [`targets_for`] refuses to build a target whose three
    /// statements disagree rather than picking one.
    pub agentless_permitted: bool,
}

impl BackfillTarget {
    /// The predicate that says a row has reached its final ownership state.
    ///
    /// For a table with a mandatory agent that is simply "both ownership
    /// columns are populated". For `audit_logs` it also admits the agentless
    /// row — but only in the exact shape its bridge writes, all three columns
    /// NULL together. A row naming no agent while carrying a tenant is not a
    /// settled agentless row, it is a contradiction, and admitting it here
    /// would let the reconcile count it towards a completion.
    fn settled_predicate(&self, alias: &str, agent: &SqlIdentifier) -> String {
        let owned =
            format!("({alias}.\"tenant_id\" IS NOT NULL AND {alias}.\"agent_uuid\" IS NOT NULL)");
        if self.agentless_permitted {
            format!(
                "(({alias}.{agent} IS NULL AND {alias}.\"tenant_id\" IS NULL \
                  AND {alias}.\"agent_uuid\" IS NULL) OR {owned})",
                agent = agent.as_sql(),
            )
        } else {
            owned
        }
    }
}

/// Every table in `tranche` whose rows this stage has to resolve, in registry
/// order.
///
/// A table is a target when it received ownership columns — which is what
/// having a [`TransitionalWrite`] means — and not otherwise. `agents` is the
/// root and owns itself; `credentials` and `credential_agent_grants` are already
/// tenant-scoped; `tenancy_backfill_checkpoints` describes the migration rather
/// than an owner. None of them declare a bridge, so none of them appear.
pub fn targets_for(tranche: Tranche) -> Result<Vec<BackfillTarget>> {
    let mut targets = Vec::new();

    for entry in inventory::REGISTRY {
        if entry.tranche != tranche {
            continue;
        }
        let Some(bridge) = plan::transition_for(entry.table) else {
            continue;
        };

        let path = entry.canonical_path.ok_or_else(|| {
            anyhow!(
                "table {} declares a transitional bridge but no canonical ownership path, so \
                 there is nothing to backfill it from",
                entry.table
            )
        })?;

        // This module resolves ownership from `agents` using the row's own
        // legacy identifier and knows how to do nothing else. A bridge that
        // resolves through a parent belongs to a later tranche and needs that
        // parent's agreement predicate, so refuse rather than walk it with the
        // wrong authority.
        let PathKind::AgentText { column } = path.kind else {
            bail!(
                "table {} resolves ownership via {:?}, which this stage does not implement; \
                 tranche {} is the direct-agent-child tranche",
                entry.table,
                path.kind,
                tranche,
            );
        };
        if bridge.resolves_through_a_parent() {
            bail!(
                "table {} declares a {} but a direct agent ownership path; write-time and \
                 backfill-time disagree about who owns its rows",
                entry.table,
                bridge.as_str(),
            );
        }

        // The three independent statements of "agentless rows are legitimate
        // here". They are checked against each other rather than trusted
        // individually: each one is a place an edit could quietly turn a
        // conditional table into an unconditional one, and driving `audit_logs`
        // to zero unmapped rows would demand the schema reject valid audit
        // history.
        let conditional_bridge = matches!(bridge, TransitionalWrite::ConditionalAgentBridge { .. });
        let unmapped_is_required_zero = entry
            .plan
            .required_zero_codes
            .contains(&audit::ReasonCode::LegacyUnmapped);

        if conditional_bridge != path.nullable || conditional_bridge == unmapped_is_required_zero {
            bail!(
                "table {} disagrees with itself about whether an agentless row is legitimate: \
                 bridge is conditional = {}, canonical path is nullable = {}, LEGACY_UNMAPPED is \
                 a required-zero code = {}. All three describe one decision and must agree.",
                entry.table,
                conditional_bridge,
                path.nullable,
                unmapped_is_required_zero,
            );
        }

        targets.push(BackfillTarget {
            table: entry.table,
            agent_column: column,
            agentless_permitted: conditional_bridge,
        });
    }

    if targets.is_empty() {
        bail!("tranche {tranche} has no tables carrying ownership columns, so it has no backfill");
    }
    Ok(targets)
}

// ── The resume cursor ────────────────────────────────────────────────────────

/// Where a paused run stopped.
///
/// A single `TEXT` column has to carry a position across five tables, so the
/// cursor names the table as well as the key. Encoded as `table|uuid`, with an
/// empty key meaning "this table, from the beginning". The separator cannot
/// appear in either half: table names are validated snake_case and the key is a
/// UUID.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResumeCursor {
    pub table: String,
    pub after_id: Option<Uuid>,
}

impl ResumeCursor {
    fn encode(&self) -> String {
        match self.after_id {
            Some(id) => format!("{}|{}", self.table, id),
            None => format!("{}|", self.table),
        }
    }

    /// Parse a stored cursor, rejecting anything that does not name a table this
    /// tranche actually walks.
    ///
    /// A cursor naming an unknown table is not recoverable by guessing: resuming
    /// from the start would re-walk settled rows harmlessly, but it would also
    /// mean the persisted position was silently discarded, and the whole point
    /// of the column is that the position survives. Refuse and let an operator
    /// abandon the checkpoint.
    fn decode(raw: &str, targets: &[BackfillTarget]) -> Result<Self> {
        let (table, key) = raw
            .split_once('|')
            .ok_or_else(|| anyhow!("resume cursor {raw:?} is malformed: expected `table|uuid`"))?;
        if !targets.iter().any(|t| t.table == table) {
            bail!(
                "resume cursor {raw:?} names table {table:?}, which is not one of this tranche's \
                 backfill targets; the checkpoint does not belong to this tranche"
            );
        }
        let after_id = if key.is_empty() {
            None
        } else {
            Some(
                Uuid::parse_str(key)
                    .with_context(|| format!("resume cursor {raw:?} carries a malformed key"))?,
            )
        };
        Ok(Self {
            table: table.to_string(),
            after_id,
        })
    }
}

// ── Options and results ──────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackfillOptions {
    pub batch_size: i64,
    /// Stop after this many batches and leave the checkpoint `IN_PROGRESS`.
    ///
    /// The mechanism by which a run is resumable is exercised rather than
    /// merely available: an operator can bound a maintenance window with it, and
    /// the tests use it to stop mid-table and prove the next run picks up from
    /// the persisted cursor instead of from the beginning.
    pub max_batches: Option<u64>,
    /// How long final reconciliation waits for its table locks before giving up.
    ///
    /// The window holds `SHARE` on six tables, so every `INSERT`, `UPDATE` and
    /// `DELETE` against them queues behind it. Waiting for the locks is the
    /// cheap half; a wait with no bound is a backfill that parks itself behind
    /// somebody's long transaction and takes the write path down with it.
    pub reconcile_lock_timeout_secs: u64,
    /// How long any one statement inside the window may run.
    ///
    /// This is the bound that matters for the *outage*, because the locks are
    /// held while the counts and the full-registry audit run. Generous by
    /// default -- the audit is a whole-schema scan and a large deployment should
    /// not have the command fail out from under it -- but bounded, so a
    /// pathological plan cannot hold the write path indefinitely.
    pub reconcile_statement_timeout_secs: u64,
}

/// Conservative: long enough that a healthy database finishes comfortably,
/// short enough that a stuck window is measured in seconds, not shifts.
pub const DEFAULT_RECONCILE_LOCK_TIMEOUT_SECS: u64 = 30;
pub const DEFAULT_RECONCILE_STATEMENT_TIMEOUT_SECS: u64 = 300;

impl Default for BackfillOptions {
    fn default() -> Self {
        Self {
            batch_size: DEFAULT_BATCH_SIZE,
            max_batches: None,
            reconcile_lock_timeout_secs: DEFAULT_RECONCILE_LOCK_TIMEOUT_SECS,
            reconcile_statement_timeout_secs: DEFAULT_RECONCILE_STATEMENT_TIMEOUT_SECS,
        }
    }
}

impl BackfillOptions {
    fn validate(&self) -> Result<()> {
        if self.batch_size <= 0 {
            bail!("batch_size must be positive, got {}", self.batch_size);
        }
        if self.batch_size > MAX_BATCH_SIZE {
            bail!(
                "batch_size {} exceeds the {MAX_BATCH_SIZE} ceiling, which bounds how long one \
                 UPDATE holds row locks against the live write path",
                self.batch_size
            );
        }
        if self.max_batches == Some(0) {
            bail!("max_batches must be positive when set; 0 would do nothing and report progress");
        }
        // Zero disables the timeout in PostgreSQL rather than making it instant,
        // so accepting it here would quietly restore the unbounded window these
        // settings exist to prevent.
        if self.reconcile_lock_timeout_secs == 0 {
            bail!(
                "reconcile_lock_timeout_secs must be positive; PostgreSQL reads 0 as \"no \
                 timeout\", which is the unbounded lock wait this setting exists to bound"
            );
        }
        if self.reconcile_statement_timeout_secs == 0 {
            bail!(
                "reconcile_statement_timeout_secs must be positive; PostgreSQL reads 0 as \"no \
                 timeout\", which is the unbounded window this setting exists to bound"
            );
        }
        Ok(())
    }
}

/// What a run concluded. Every variant is a real, distinguishable state — none
/// of them is an error dressed as a result, and none is a success dressed as a
/// caveat.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackfillOutcome {
    /// The tranche was already `COMPLETED` at the current digest. Nothing ran.
    AlreadyCompleted,
    /// Every row settled and the audit reported nothing blocking. The checkpoint
    /// is now `COMPLETED` and FINALIZE will accept it.
    Completed,
    /// The scan finished but the tranche is not clean: rows remain unsettled, or
    /// the audit still reports blocking findings against it. The checkpoint stays
    /// `IN_PROGRESS` and records why.
    Blocked,
    /// `max_batches` was reached with work left. The cursor is persisted.
    Paused,
}

impl BackfillOutcome {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AlreadyCompleted => "ALREADY_COMPLETED",
            Self::Completed => "COMPLETED",
            Self::Blocked => "BLOCKED",
            Self::Paused => "PAUSED",
        }
    }
}

/// The result of one run, and the state of the checkpoint it leaves behind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrancheBackfillReport {
    pub tranche: Tranche,
    pub contract_digest: String,
    pub checkpoint_id: Uuid,
    pub status: CheckpointStatus,
    pub outcome: BackfillOutcome,
    pub rows_total: i64,
    pub rows_backfilled: i64,
    pub blocking_count: i64,
    /// Blocking reasons as the audit itself phrased them, so an operator does
    /// not have to re-run the audit to learn what stopped the completion.
    pub blocking_reasons: Vec<String>,
    pub resume_cursor: Option<String>,
    pub batches_executed: u64,
    /// Rows this run moved from unsettled to settled.
    pub rows_settled_this_run: i64,
}

impl TrancheBackfillReport {
    /// Whether FINALIZE would accept this tranche now.
    pub fn is_finalizable(&self) -> bool {
        self.status == plan::FINALIZE_PRECONDITION.required_status
            && self.blocking_count <= plan::FINALIZE_PRECONDITION.max_blocking_count
            && self.rows_backfilled == self.rows_total
    }
}

/// One checkpoint row, as stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckpointRow {
    pub id: Uuid,
    pub tranche: String,
    pub contract_digest: String,
    pub status: String,
    pub resume_cursor: Option<String>,
    pub rows_total: i64,
    pub rows_backfilled: i64,
    pub blocking_count: i64,
}

// ── Entry points ─────────────────────────────────────────────────────────────

/// Refuse a pool that cannot serve a backfill, before anything is acquired.
///
/// The failure this prevents is not a crash but a hang: the run would sit on
/// `DB_ACQUIRE_TIMEOUT_SECS` and then report a pool timeout, which reads as a
/// busy database rather than as a configuration error the operator can fix in
/// one variable.
fn assert_pool_can_serve_a_backfill(pool: &PgPool) -> Result<()> {
    let available = pool.options().get_max_connections();
    if available < MIN_BACKFILL_CONNECTIONS {
        bail!(
            "a tranche backfill needs at least {MIN_BACKFILL_CONNECTIONS} database connections \
             but this pool allows {available}: one is held for the whole run by the session \
             advisory lock that stops two backfills interleaving, and the batches need another. \
             Raise DB_MAX_CONNECTIONS to {MIN_BACKFILL_CONNECTIONS} or more."
        );
    }
    Ok(())
}

/// Run (or resume) the backfill for one tranche.
///
/// Idempotent at every granularity: a re-run over a `COMPLETED` tranche does
/// nothing, a re-run over a partially-done one resumes from the persisted
/// cursor, and the `UPDATE` itself writes values the bridge would write anyway,
/// so re-walking a settled row is a no-op rather than a second assignment.
pub async fn run_tranche_backfill(
    pool: &PgPool,
    tranche: Tranche,
    options: BackfillOptions,
) -> Result<TrancheBackfillReport> {
    options.validate()?;
    assert_pool_can_serve_a_backfill(pool)?;
    let targets = targets_for(tranche)?;

    // Held for the whole run rather than per transaction: the point is that two
    // runs cannot interleave their cursor writes, and a per-transaction lock
    // would let them alternate batches and leave a cursor that describes
    // neither. `try` rather than a wait, so a second operator is told what is
    // happening instead of hanging on a lock with no message.
    let lock = TrancheLock::acquire(pool).await?;
    let result = run_locked(pool, tranche, options, &targets).await;
    let cleanup = lock.release().await;
    resolve_run_outcome(result, cleanup)
}

/// Retire a checkpoint an operator has decided not to finish.
///
/// The only transition out of `IN_PROGRESS` other than completion. The cursor is
/// deliberately left in place: an abandoned checkpoint is history, and how far
/// it got is the interesting part of that history. `completed_at` stays NULL,
/// which `tenancy_backfill_checkpoints_completed_shape_ck` requires and which
/// keeps an abandoned row from ever reading as evidence of completion.
pub async fn abandon_tranche_backfill(
    pool: &PgPool,
    tranche: Tranche,
    reason: &str,
) -> Result<Option<CheckpointRow>> {
    if reason.trim().is_empty() {
        bail!(
            "abandoning a checkpoint requires a reason; an unexplained ABANDONED row is history \
             nobody can read"
        );
    }

    let mut tx = pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(TRANCHE_BACKFILL_ADVISORY_LOCK_ID)
        .execute(&mut *tx)
        .await?;

    let existing = in_progress_checkpoint(&mut tx, tranche).await?;
    let Some(row) = existing else {
        tx.commit().await?;
        return Ok(None);
    };

    let updated = sqlx::query(
        "UPDATE tenancy_backfill_checkpoints \
            SET status = $1, updated_at = now() \
          WHERE id = $2 \
      RETURNING id, tranche, contract_digest, status, resume_cursor, \
                rows_total, rows_backfilled, blocking_count",
    )
    .bind(CheckpointStatus::Abandoned.as_str())
    .bind(row.id)
    .fetch_one(&mut *tx)
    .await
    .context("abandoning the checkpoint")?;

    let out = checkpoint_from_row(&updated)?;
    tx.commit().await?;

    tracing::warn!(
        tranche = %tranche,
        checkpoint = %out.id,
        digest = %out.contract_digest,
        cursor = ?out.resume_cursor,
        reason,
        "tenancy tranche backfill checkpoint abandoned",
    );
    Ok(Some(out))
}

/// The checkpoint a tranche is currently sitting on, if any.
///
/// Prefers a completion at the current digest — that is the state FINALIZE
/// cares about — and falls back to an in-progress row.
pub async fn tranche_backfill_status(
    pool: &PgPool,
    tranche: Tranche,
) -> Result<Option<CheckpointRow>> {
    let digest = report::inventory_digest();
    let mut tx = pool.begin().await?;
    if let Some(done) = completed_checkpoint(&mut tx, tranche, &digest).await? {
        tx.commit().await?;
        return Ok(Some(done));
    }
    let current = in_progress_checkpoint(&mut tx, tranche).await?;
    tx.commit().await?;
    Ok(current)
}

// ── The run ──────────────────────────────────────────────────────────────────

async fn run_locked(
    pool: &PgPool,
    tranche: Tranche,
    options: BackfillOptions,
    targets: &[BackfillTarget],
) -> Result<TrancheBackfillReport> {
    let digest = report::inventory_digest();

    let checkpoint = open_checkpoint(pool, tranche, &digest, targets).await?;
    let Some(mut state) = checkpoint else {
        // Already completed at this digest. Report the stored row rather than a
        // synthesised one, so what the caller sees is what FINALIZE will read.
        let mut tx = pool.begin().await?;
        let row = completed_checkpoint(&mut tx, tranche, &digest)
            .await?
            .ok_or_else(|| anyhow!("completed checkpoint vanished between reads"))?;
        tx.commit().await?;
        return Ok(TrancheBackfillReport {
            tranche,
            contract_digest: row.contract_digest.clone(),
            checkpoint_id: row.id,
            status: CheckpointStatus::Completed,
            outcome: BackfillOutcome::AlreadyCompleted,
            rows_total: row.rows_total,
            rows_backfilled: row.rows_backfilled,
            blocking_count: row.blocking_count,
            blocking_reasons: Vec::new(),
            resume_cursor: row.resume_cursor,
            batches_executed: 0,
            rows_settled_this_run: 0,
        });
    };

    // Where to pick up. A fresh checkpoint starts at the first target; a resumed
    // one starts where it stopped.
    let mut cursor = match state.resume_cursor.as_deref() {
        Some(raw) => ResumeCursor::decode(raw, targets)?,
        None => ResumeCursor {
            table: targets[0].table.to_string(),
            after_id: None,
        },
    };

    let start_index = targets
        .iter()
        .position(|t| t.table == cursor.table)
        .ok_or_else(|| anyhow!("resume cursor names a table not in this tranche's targets"))?;

    let mut batches_executed: u64 = 0;
    let mut settled_this_run: i64 = 0;
    let mut paused = false;

    'targets: for (index, target) in targets.iter().enumerate().skip(start_index) {
        // Only the target the cursor names inherits its key; every later target
        // starts from the beginning.
        let mut after_id = if index == start_index {
            cursor.after_id
        } else {
            None
        };

        loop {
            if let Some(limit) = options.max_batches {
                if batches_executed >= limit {
                    paused = true;
                    break 'targets;
                }
            }

            let batch = run_batch(pool, target, after_id, options.batch_size).await?;
            if batch.scanned == 0 {
                // This target has no unsettled rows past the cursor. Move on.
                break;
            }

            batches_executed += 1;
            settled_this_run += batch.settled;
            after_id = batch.last_id;

            cursor = ResumeCursor {
                table: target.table.to_string(),
                after_id,
            };
            let claimed = state.rows_backfilled.saturating_add(batch.settled);
            state.rows_backfilled = persist_progress(pool, state.id, &cursor, claimed).await?;

            // A batch that scanned fewer rows than it asked for has reached the
            // end of the table. Checked on the scan count, not on the settled
            // count: a short batch of entirely unsettleable rows is still the
            // end of the table, and a full batch that settled nothing is not.
            if batch.scanned < options.batch_size {
                break;
            }
        }
    }

    if paused {
        return Ok(TrancheBackfillReport {
            tranche,
            contract_digest: digest,
            checkpoint_id: state.id,
            status: CheckpointStatus::InProgress,
            outcome: BackfillOutcome::Paused,
            rows_total: state.rows_total,
            rows_backfilled: state.rows_backfilled,
            blocking_count: state.blocking_count,
            blocking_reasons: Vec::new(),
            resume_cursor: Some(cursor.encode()),
            batches_executed,
            rows_settled_this_run: settled_this_run,
        });
    }

    finish(
        pool,
        tranche,
        digest,
        state.id,
        targets,
        options,
        batches_executed,
        settled_this_run,
    )
    .await
}

/// The order the reconciliation window takes its table locks in.
///
/// **Not** the registry's alphabetical order, and the difference is a deadlock.
/// `memory::store::delete_agent` is a live multi-table write path that takes
/// `ROW EXCLUSIVE` on `entities`, `memory_graph`, `audit_logs` and
/// `rmk_policies` in that order, then on `agents` and — by `ON DELETE CASCADE`
/// from that same statement — `archival_batches`. Alphabetical order put
/// `audit_logs` before `entities`, so reconciliation could hold `audit_logs`
/// and wait for `entities` while a deletion held `entities` and waited for
/// `audit_logs`. PostgreSQL breaks that by aborting one of them, which means
/// either an operator's agent deletion or the backfill dies for no reason
/// either of them can see.
///
/// This list is therefore `delete_agent`'s order, and
/// `the_lock_order_matches_the_live_deletion_path` parses `store.rs` to fail if
/// the two ever drift apart. Locking in a compatible order does not make the two
/// stop waiting on each other — one still waits — but a wait that resolves is a
/// different thing from a cycle that cannot.
pub(super) const RECONCILIATION_LOCK_ORDER: &[&str] = &[
    "entities",
    "memory_graph",
    "audit_logs",
    "rmk_policies",
    // `agents` and then `archival_batches`: `delete_agent` reaches both in its
    // final `DELETE FROM agents`, which takes the parent's lock before the
    // cascade takes the child's.
    "agents",
    "archival_batches",
];

/// Take the reconciliation window's table locks, in one fixed order.
///
/// `SHARE` is the weakest mode that does what the window needs: it conflicts
/// with `ROW EXCLUSIVE`, which is what `INSERT`, `UPDATE` and `DELETE` take, and
/// not with `ACCESS SHARE`, so concurrent readers are untouched. Nothing here
/// writes to these tables — the batches finished before `finish` was called — so
/// a stronger mode would only widen the outage without excluding anything more.
///
/// `agents` is locked as well as the targets. It is the authority every one of
/// them resolves through, so a row that stops being settled because its agent's
/// `tenant_id` changed mid-window is the same race in a different table.
///
/// The order is fixed and total — `agents`, then the targets in registry order —
/// because two sessions taking these locks in different orders is the textbook
/// deadlock. One `LOCK` statement listing them acquires in the order written.
async fn lock_reconciliation_window(
    conn: &mut sqlx::PgConnection,
    targets: &[BackfillTarget],
) -> Result<()> {
    // The order is the constant's, but the *set* is still checked against the
    // tranche, so a target added to the registry without a place in the order
    // fails here rather than silently going unlocked -- which would leave
    // exactly the window this whole path exists to close.
    let expected: std::collections::BTreeSet<&str> = targets
        .iter()
        .map(|t| t.table)
        .chain(std::iter::once("agents"))
        .collect();
    let ordered: std::collections::BTreeSet<&str> =
        RECONCILIATION_LOCK_ORDER.iter().copied().collect();
    if expected != ordered {
        bail!(
            "RECONCILIATION_LOCK_ORDER covers {ordered:?} but this tranche needs {expected:?}; a \
             table with no place in the order would go unlocked during reconciliation"
        );
    }

    let mut relations = Vec::with_capacity(RECONCILIATION_LOCK_ORDER.len());
    for table in RECONCILIATION_LOCK_ORDER {
        relations.push(format!("public.{}", SqlIdentifier::new(table)?.as_sql()));
    }

    let sql = format!("LOCK TABLE {} IN SHARE MODE", relations.join(", "));
    sqlx::query(AssertSqlSafe(sql))
        .execute(conn)
        .await
        .map_err(|e| lock_window_error(e, "acquiring the reconciliation locks"))?;
    Ok(())
}

/// Turn a timeout inside the reconciliation window into something an operator
/// can act on.
///
/// `lock_timeout` surfaces as SQLSTATE 55P03 and `statement_timeout` as 57014.
/// Both mean "the window was abandoned", and both matter because the reflex on
/// seeing a failed backfill is to re-run it -- which is the right thing here and
/// the wrong thing for most other failures, so the message says so.
fn lock_window_error(error: sqlx::Error, doing: &str) -> anyhow::Error {
    let code = error
        .as_database_error()
        .and_then(|e| e.code())
        .unwrap_or_default()
        .to_string();

    let hint = match code.as_str() {
        "55P03" => Some(
            "the reconciliation lock timeout expired: another transaction holds a conflicting \
             lock on one of the tranche's tables",
        ),
        "57014" => Some(
            "the reconciliation statement timeout expired: counting and auditing the tranche \
             took longer than the configured budget",
        ),
        _ => None,
    };

    match hint {
        Some(hint) => anyhow!(error).context(format!(
            "{doing}: {hint}. Nothing was committed and the checkpoint is still IN_PROGRESS, so \
             re-running the backfill resumes from where it stopped. Retry in a quieter window, \
             or raise the timeout."
        )),
        None => anyhow!(error).context(doing.to_string()),
    }
}

/// Reconcile the accounting against the live tables, consult the audit, and
/// complete only if both agree the tranche is done.
///
/// ## Why this is one transaction
///
/// It used to be three: `count_rows` committed its own snapshot, `audit::run`
/// opened and committed a second, and only then did a third transaction write
/// the checkpoint. On a live database that leaves two windows, and a row
/// inserted in either one is invisible to the completion that follows it. The
/// bridge accepts a row naming an unknown agent with NULL ownership, so it
/// appears in neither the counts nor the blocking reasons, and a checkpoint
/// claiming a clean tranche is persisted over it. That checkpoint then satisfies
/// the FINALIZE guard, and FINALIZE validates a `NOT NULL` constraint against a
/// table holding a NULL.
///
/// So: one transaction, opened here and owned here. It locks every table the
/// verdict depends on, counts inside the lock, audits inside the same
/// transaction via [`audit::run_within`], writes the checkpoint, and commits
/// only once all three agree. Anything that fails leaves the transaction
/// uncommitted, which is the difference between "no completion was recorded" and
/// "a completion was recorded for a state that no longer holds".
///
/// The isolation level is the default. It is the locks, not the snapshot
/// semantics, that make the window exclusive — `REPEATABLE READ` would add a
/// serialization-failure mode on the checkpoint row (deliberately *not* locked,
/// so the row stays reachable) while excluding nothing the locks do not already
/// exclude.
#[allow(clippy::too_many_arguments)]
async fn finish(
    pool: &PgPool,
    tranche: Tranche,
    digest: String,
    checkpoint_id: Uuid,
    targets: &[BackfillTarget],
    options: BackfillOptions,
    batches_executed: u64,
    settled_this_run: i64,
) -> Result<TrancheBackfillReport> {
    let mut tx = pool.begin().await?;

    // Transaction-local, and only here. The batches deliberately run without
    // either bound: they take ordinary row locks, they are already bounded by
    // `batch_size`, and a batch that fails a timeout would strand the run
    // mid-scan for no gain. This window is the one that holds table locks.
    //
    // `set_config(.., true)` rather than an interpolated `SET LOCAL`: the value
    // is a bind parameter, so no number this function computed is ever spliced
    // into SQL text.
    for (setting, seconds) in [
        ("lock_timeout", options.reconcile_lock_timeout_secs),
        (
            "statement_timeout",
            options.reconcile_statement_timeout_secs,
        ),
    ] {
        sqlx::query("SELECT set_config($1, $2, true)")
            .bind(setting)
            .bind(format!("{seconds}s"))
            .execute(&mut *tx)
            .await
            .with_context(|| format!("setting {setting} for the reconciliation window"))?;
    }

    lock_reconciliation_window(&mut tx, targets).await?;

    let counts = count_rows_within(&mut tx, targets).await.map_err(|e| {
        match e.downcast::<sqlx::Error>() {
            Ok(db) => {
                lock_window_error(db, "counting the tranche inside the reconciliation window")
            }
            Err(other) => other,
        }
    })?;

    // The authoritative verdict. Derived from the audit's own tranche readiness
    // rather than recomputed from `findings`, so `blocking_count == 0` and
    // `ready == true` cannot come apart: they are the same list. Re-deriving it
    // here would be a second definition of "blocking for this tranche", and the
    // second definition is the one that eventually disagrees.
    //
    // `run_within` rather than `run`: the verdict has to describe the same
    // locked, counted state the checkpoint is about to claim. It also applies
    // `SET LOCAL search_path = pg_catalog, public` for the rest of this
    // transaction, which is why the statements below are schema-qualified.
    let audited = audit::run_within(&mut tx, None)
        .await
        .context("running the tenancy audit to establish the tranche's blocking count")?;
    let readiness = audited
        .tranche_readiness
        .iter()
        .find(|r| r.tranche == tranche)
        .ok_or_else(|| anyhow!("audit reported no readiness entry for tranche {tranche}"))?;
    let blocking_reasons = readiness.blocking_reasons.clone();
    let blocking_count = blocking_reasons.len() as i64;

    let accounting_balances = counts.settled == counts.total;
    let clean =
        accounting_balances && blocking_count <= plan::FINALIZE_PRECONDITION.max_blocking_count;

    let (status, written) = if clean {
        let done = sqlx::query(
            "UPDATE public.tenancy_backfill_checkpoints \
                SET status = $1, resume_cursor = NULL, completed_at = now(), updated_at = now(), \
                    rows_total = $2, rows_backfilled = $3, blocking_count = $4 \
              WHERE id = $5 AND status = $6",
        )
        .bind(CheckpointStatus::Completed.as_str())
        .bind(counts.total)
        .bind(counts.settled)
        .bind(blocking_count)
        .bind(checkpoint_id)
        .bind(CheckpointStatus::InProgress.as_str())
        .execute(&mut *tx)
        .await
        .context(
            "completing the checkpoint; a CHECK violation here means the reconciled accounting \
             did not actually balance",
        )?;
        (CheckpointStatus::Completed, done.rows_affected())
    } else {
        // Stays IN_PROGRESS, and keeps a cursor of NULL: the scan did reach the
        // end, so there is no position to resume from. What is left is not more
        // scanning but a decision about rows the backfill is not permitted to
        // guess at.
        let done = sqlx::query(
            "UPDATE public.tenancy_backfill_checkpoints \
                SET resume_cursor = NULL, updated_at = now(), \
                    rows_total = $1, rows_backfilled = $2, blocking_count = $3 \
              WHERE id = $4 AND status = $5",
        )
        .bind(counts.total)
        .bind(counts.settled)
        .bind(blocking_count)
        .bind(checkpoint_id)
        .bind(CheckpointStatus::InProgress.as_str())
        .execute(&mut *tx)
        .await
        .context("recording the blocked checkpoint")?;
        (CheckpointStatus::InProgress, done.rows_affected())
    };

    // The `AND status = 'IN_PROGRESS'` guard above is only half a guard while
    // nothing reads the row count back. Without this, a checkpoint that stopped
    // being `IN_PROGRESS` between `open_checkpoint` and here matches zero rows
    // and the function still returns `Completed` — so `is_finalizable()` reports
    // true for a checkpoint carrying no `COMPLETED` row and no `completed_at`,
    // and the report describes a database state that was never written.
    if written != 1 {
        bail!(
            "checkpoint {checkpoint_id} is no longer IN_PROGRESS ({written} rows matched the \
             guarded update, expected 1); it was completed or abandoned while this run was \
             reconciling. Nothing was committed and no completion is being reported."
        );
    }

    tx.commit().await?;

    Ok(TrancheBackfillReport {
        tranche,
        contract_digest: digest,
        checkpoint_id,
        status,
        outcome: if clean {
            BackfillOutcome::Completed
        } else {
            BackfillOutcome::Blocked
        },
        rows_total: counts.total,
        rows_backfilled: counts.settled,
        blocking_count,
        blocking_reasons,
        resume_cursor: None,
        batches_executed,
        rows_settled_this_run: settled_this_run,
    })
}

// ── Batches ──────────────────────────────────────────────────────────────────

struct BatchResult {
    /// Unsettled rows the batch looked at. Zero means the target is exhausted.
    scanned: i64,
    /// Of those, how many are settled now.
    settled: i64,
    /// The highest key scanned, which becomes the next cursor.
    last_id: Option<Uuid>,
}

/// One bounded keyset step over a single table.
///
/// The scan is restricted to *unsettled* rows, which makes the whole run
/// idempotent for free: a settled row is never revisited, so a resumed or
/// repeated run costs only the rows that still need something. Rows that cannot
/// be settled — an agent that does not resolve, or one whose own `tenant_id` is
/// NULL — stay in the scanned set, which is why the cursor advances on the key
/// rather than on whether anything was written.
async fn run_batch(
    pool: &PgPool,
    target: &BackfillTarget,
    after_id: Option<Uuid>,
    batch_size: i64,
) -> Result<BatchResult> {
    // Registry constants, not catalog values — but routed through the validator
    // anyway, so "no identifier reaches SQL unvalidated" is a property of the
    // code rather than of the current contents of the registry.
    let table = SqlIdentifier::new(target.table)?;
    let agent = SqlIdentifier::new(target.agent_column)?;
    let settled = target.settled_predicate("x", &agent);

    // One transaction, three statements, in an order that is the whole point.
    //
    // A single `UPDATE` was a deadlock against `memory::store::delete_agent`.
    // Migration 0041 gives every tranche-1 table a composite foreign key to
    // `agents`, and a `NOT VALID` key still enforces on update — so writing
    // `agent_uuid`/`tenant_id` fires an FK check that takes `FOR KEY SHARE` on
    // the parent row. The batch therefore locked a child row and *then* reached
    // for the parent, while `delete_agent` takes the parent `FOR UPDATE` first
    // (`store.rs:110`) and then deletes the children. Child-then-parent against
    // parent-then-child is a cycle, and PostgreSQL breaks it by aborting one
    // side: either an operator's agent deletion or this backfill.
    //
    // Locking the parents first makes the two paths agree on direction. When
    // `delete_agent` holds the agent, this blocks at step 2 holding no child
    // rows at all, so there is nothing for it to wait on and it completes; when
    // this holds the agent, the deletion waits for a batch that is about to
    // commit. A wait either way — but a wait that resolves, rather than a cycle
    // that cannot.
    //
    // Deliberately no `lock_timeout` here: the batches take ordinary row locks,
    // are already bounded by `batch_size`, and a timeout would strand a run
    // mid-scan. The bounded window is final reconciliation, which holds *table*
    // locks.
    let mut tx = pool.begin().await?;

    // 1. Choose the batch. Reads only, so no lock is taken and no ordering
    //    obligation is incurred yet.
    let pick = format!(
        "SELECT x.\"id\" AS id, x.{agent} AS agent_id \
           FROM public.{table} x \
          WHERE ($1::uuid IS NULL OR x.\"id\" > $1::uuid) \
            AND NOT {settled} \
          ORDER BY x.\"id\" \
          LIMIT $2",
        table = table.as_sql(),
        agent = agent.as_sql(),
    );
    let picked = sqlx::query(AssertSqlSafe(pick))
        .bind(after_id)
        .bind(batch_size)
        .fetch_all(&mut *tx)
        .await
        .with_context(|| format!("selecting a batch of {}", target.table))?;

    if picked.is_empty() {
        tx.commit().await?;
        return Ok(BatchResult {
            scanned: 0,
            settled: 0,
            last_id: None,
        });
    }

    let ids: Vec<Uuid> = picked
        .iter()
        .map(|r| r.try_get::<Uuid, _>("id"))
        .collect::<Result<_, _>>()?;
    // `last_id` is the batch's own last row rather than a `max()`: PostgreSQL
    // ships no `max(uuid)` aggregate (measured on pg16 — `function max(uuid)
    // does not exist`), and the rows are already in key order.
    let last_id = ids.last().copied();

    // The agents this batch touches. `agent_id` is NULL-able on `audit_logs`,
    // and an agentless row has no parent to lock.
    let mut agent_ids: Vec<String> = picked
        .iter()
        .filter_map(|r| r.try_get::<Option<String>, _>("agent_id").transpose())
        .collect::<Result<_, _>>()?;
    agent_ids.sort_unstable();
    agent_ids.dedup();

    // 2. Lock the parents, before any child row is touched.
    //
    // `FOR KEY SHARE` is exactly what the foreign-key check would take, so this
    // adds no contention the `UPDATE` was not already going to create — it only
    // moves it earlier, to a point where this transaction holds nothing.
    // `ORDER BY` makes the acquisition order deterministic, so two batches can
    // never take the same pair of agents in opposite orders.
    if !agent_ids.is_empty() {
        sqlx::query(
            "SELECT 1 FROM public.agents WHERE agent_id = ANY($1) \
              ORDER BY agent_id FOR KEY SHARE",
        )
        .bind(&agent_ids)
        .fetch_all(&mut *tx)
        .await
        .with_context(|| {
            format!(
                "locking the parent agents for a batch of {} before updating it",
                target.table
            )
        })?;
    }

    // 3. Now the children.
    let update = format!(
        "UPDATE public.{table} AS x \
            SET \"agent_uuid\" = a.\"id\", \"tenant_id\" = a.\"tenant_id\" \
           FROM public.agents a \
          WHERE x.\"id\" = ANY($1) \
            AND a.\"agent_id\" = x.{agent} \
      RETURNING ({settled}) AS is_settled",
        table = table.as_sql(),
        agent = agent.as_sql(),
    );
    let updated = sqlx::query(AssertSqlSafe(update))
        .bind(&ids)
        .fetch_all(&mut *tx)
        .await
        .with_context(|| format!("backfilling a batch of {}", target.table))?;

    let settled_now = updated
        .iter()
        .map(|r| r.try_get::<bool, _>("is_settled"))
        .collect::<Result<Vec<bool>, _>>()?
        .into_iter()
        .filter(|s| *s)
        .count() as i64;

    tx.commit().await?;

    Ok(BatchResult {
        scanned: ids.len() as i64,
        settled: settled_now,
        last_id,
    })
}

struct Counts {
    total: i64,
    settled: i64,
}

/// Recount both sides of the accounting from the live tables.
///
/// Every target is counted inside one snapshot, so the totals describe a single
/// consistent state of the database rather than five states a few milliseconds
/// apart. Without that, a row inserted between two counts appears in `total` and
/// not in `settled`, and the tranche fails to complete for a reason that is not
/// real.
async fn count_rows(pool: &PgPool, targets: &[BackfillTarget]) -> Result<Counts> {
    let mut tx = pool
        .begin_with("BEGIN TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
        .await?;
    let counts = count_rows_within(&mut tx, targets).await?;
    tx.commit().await?;
    Ok(counts)
}

/// The counting itself, on a connection the caller owns.
///
/// Split out for [`finish`], which must count inside the same transaction that
/// holds the table locks, runs the audit and writes the checkpoint. Counting on
/// the pool there would open a second transaction and re-introduce exactly the
/// gap the locks exist to close.
///
/// The consistency of the counts is the *caller's* to establish, and the two
/// callers establish it differently: [`count_rows`] wraps this in a
/// `REPEATABLE READ READ ONLY` snapshot because it only ever reads, while
/// `finish` holds `SHARE` on every table so nothing can write during the window.
/// Either way all five targets are counted against one state of the database
/// rather than five states a few milliseconds apart.
async fn count_rows_within(
    conn: &mut sqlx::PgConnection,
    targets: &[BackfillTarget],
) -> Result<Counts> {
    let mut total = 0i64;
    let mut settled = 0i64;
    for target in targets {
        let table = SqlIdentifier::new(target.table)?;
        let agent = SqlIdentifier::new(target.agent_column)?;
        let predicate = target.settled_predicate("x", &agent);
        let sql = format!(
            "SELECT count(*)::bigint AS total, \
                    count(*) FILTER (WHERE {predicate})::bigint AS settled \
               FROM public.{table} x",
            table = table.as_sql(),
        );
        let row = sqlx::query(AssertSqlSafe(sql))
            .fetch_one(&mut *conn)
            .await
            .with_context(|| format!("counting {}", target.table))?;
        total += row.try_get::<i64, _>("total")?;
        settled += row.try_get::<i64, _>("settled")?;
    }
    Ok(Counts { total, settled })
}

// ── Checkpoint lifecycle ─────────────────────────────────────────────────────

struct CheckpointState {
    id: Uuid,
    resume_cursor: Option<String>,
    rows_total: i64,
    rows_backfilled: i64,
    blocking_count: i64,
}

/// Find or create the checkpoint this run works against.
///
/// `Ok(None)` means the tranche is already `COMPLETED` at the current digest and
/// there is nothing to do.
async fn open_checkpoint(
    pool: &PgPool,
    tranche: Tranche,
    digest: &str,
    targets: &[BackfillTarget],
) -> Result<Option<CheckpointState>> {
    let mut tx = pool.begin().await?;

    if completed_checkpoint(&mut tx, tranche, digest)
        .await?
        .is_some()
    {
        tx.commit().await?;
        return Ok(None);
    }

    // More than one open checkpoint is an ambiguity, not a state to pick a
    // winner from: two runs would resume from two different cursors and both
    // would reconcile against the same tables. Refuse and make an operator say
    // which one is real.
    let open: Vec<CheckpointRow> = sqlx::query(
        "SELECT id, tranche, contract_digest, status, resume_cursor, \
                rows_total, rows_backfilled, blocking_count \
           FROM tenancy_backfill_checkpoints \
          WHERE tranche = $1 AND status = $2 \
          ORDER BY started_at DESC, id DESC",
    )
    .bind(tranche.as_str())
    .bind(CheckpointStatus::InProgress.as_str())
    .fetch_all(&mut *tx)
    .await?
    .iter()
    .map(checkpoint_from_row)
    .collect::<Result<_>>()?;

    if open.len() > 1 {
        bail!(
            "tranche {tranche} has {} open checkpoints ({}); abandon all but the one that is \
             real before resuming",
            open.len(),
            open.iter()
                .map(|c| c.id.to_string())
                .collect::<Vec<_>>()
                .join(", "),
        );
    }

    if let Some(existing) = open.into_iter().next() {
        // A checkpoint raised against a superseded contract proves nothing about
        // the current one — which is exactly what
        // `FINALIZE_PRECONDITION.digest_must_match_current` says — so continuing
        // it would build evidence for a plan nobody is running.
        if existing.contract_digest != digest {
            bail!(
                "tranche {tranche} has an open checkpoint {} raised against contract digest {}, \
                 but the current digest is {}. The migration plan moved underneath it. Abandon \
                 the checkpoint and start again.",
                existing.id,
                existing.contract_digest,
                digest,
            );
        }
        // Validate the stored cursor before any batch runs, so a corrupt cursor
        // fails before it can be interpreted as a position.
        if let Some(raw) = existing.resume_cursor.as_deref() {
            ResumeCursor::decode(raw, targets)?;
        }
        tx.commit().await?;
        return Ok(Some(CheckpointState {
            id: existing.id,
            resume_cursor: existing.resume_cursor,
            rows_total: existing.rows_total,
            rows_backfilled: existing.rows_backfilled,
            blocking_count: existing.blocking_count,
        }));
    }

    tx.commit().await?;

    // Counted outside the insert transaction and in its own read-only snapshot.
    // These are opening figures for an operator watching progress; the numbers
    // a completion rests on are recounted in `finish`.
    let counts = count_rows(pool, targets).await?;

    let mut tx = pool.begin().await?;
    let row = sqlx::query(
        "INSERT INTO tenancy_backfill_checkpoints \
             (tranche, contract_digest, status, resume_cursor, rows_total, rows_backfilled) \
         VALUES ($1, $2, $3, NULL, $4, $5) \
      RETURNING id",
    )
    .bind(tranche.as_str())
    .bind(digest)
    .bind(CheckpointStatus::InProgress.as_str())
    .bind(counts.total)
    .bind(counts.settled)
    .fetch_one(&mut *tx)
    .await
    .context("opening a backfill checkpoint")?;
    let id: Uuid = row.try_get("id")?;
    tx.commit().await?;

    tracing::info!(
        tranche = %tranche,
        checkpoint = %id,
        digest,
        rows_total = counts.total,
        rows_settled = counts.settled,
        "opened tenancy tranche backfill checkpoint",
    );

    Ok(Some(CheckpointState {
        id,
        resume_cursor: None,
        rows_total: counts.total,
        rows_backfilled: counts.settled,
        blocking_count: 0,
    }))
}

/// Write the cursor and the running count, and return the count as stored.
///
/// The `LEAST` is not defensive noise. `rows_total` was counted when the
/// checkpoint opened, and rows inserted since then can be scanned and settled by
/// this run, so the running count really can overtake it — and
/// `tenancy_backfill_checkpoints_counts_ck` (`rows_backfilled <= rows_total`)
/// would reject the write rather than record a slightly stale total.
///
/// The clamped value is read back and returned so the caller's counter stays
/// equal to the row it describes. A report that disagrees with the checkpoint it
/// is reporting on is worse than an approximate number, because the checkpoint
/// is the thing FINALIZE reads.
async fn persist_progress(
    pool: &PgPool,
    checkpoint_id: Uuid,
    cursor: &ResumeCursor,
    rows_backfilled: i64,
) -> Result<i64> {
    let row = sqlx::query(
        "UPDATE tenancy_backfill_checkpoints \
            SET resume_cursor = $1, \
                rows_backfilled = LEAST($2, rows_total), \
                updated_at = now() \
          WHERE id = $3 AND status = $4 \
      RETURNING rows_backfilled",
    )
    .bind(cursor.encode())
    .bind(rows_backfilled)
    .bind(checkpoint_id)
    .bind(CheckpointStatus::InProgress.as_str())
    .fetch_optional(pool)
    .await
    .context("persisting the resume cursor")?;

    // No row means the checkpoint stopped being `IN_PROGRESS` underneath this
    // run — completed or abandoned by someone else. The advisory lock makes that
    // hard to reach, which is exactly why it must not be swallowed: a run that
    // kept batching against a retired checkpoint would be writing progress
    // nothing will ever read.
    let row = row.ok_or_else(|| {
        anyhow!(
            "checkpoint {checkpoint_id} is no longer IN_PROGRESS; it was completed or abandoned \
             while this run was batching"
        )
    })?;
    Ok(row.try_get("rows_backfilled")?)
}

async fn completed_checkpoint(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    tranche: Tranche,
    digest: &str,
) -> Result<Option<CheckpointRow>> {
    let row = sqlx::query(
        "SELECT id, tranche, contract_digest, status, resume_cursor, \
                rows_total, rows_backfilled, blocking_count \
           FROM tenancy_backfill_checkpoints \
          WHERE tranche = $1 AND contract_digest = $2 AND status = $3",
    )
    .bind(tranche.as_str())
    .bind(digest)
    .bind(CheckpointStatus::Completed.as_str())
    .fetch_optional(&mut **tx)
    .await?;
    row.as_ref().map(checkpoint_from_row).transpose()
}

async fn in_progress_checkpoint(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    tranche: Tranche,
) -> Result<Option<CheckpointRow>> {
    let row = sqlx::query(
        "SELECT id, tranche, contract_digest, status, resume_cursor, \
                rows_total, rows_backfilled, blocking_count \
           FROM tenancy_backfill_checkpoints \
          WHERE tranche = $1 AND status = $2 \
          ORDER BY started_at DESC, id DESC \
          LIMIT 1",
    )
    .bind(tranche.as_str())
    .bind(CheckpointStatus::InProgress.as_str())
    .fetch_optional(&mut **tx)
    .await?;
    row.as_ref().map(checkpoint_from_row).transpose()
}

fn checkpoint_from_row(row: &sqlx::postgres::PgRow) -> Result<CheckpointRow> {
    Ok(CheckpointRow {
        id: row.try_get("id")?,
        tranche: row.try_get("tranche")?,
        contract_digest: row.try_get("contract_digest")?,
        status: row.try_get("status")?,
        resume_cursor: row.try_get("resume_cursor")?,
        rows_total: row.try_get("rows_total")?,
        rows_backfilled: row.try_get("rows_backfilled")?,
        blocking_count: row.try_get("blocking_count")?,
    })
}

// ── The run lock ─────────────────────────────────────────────────────────────

/// A session-level advisory lock held on a dedicated connection.
///
/// Session-level rather than transaction-level because the run spans many
/// transactions and the thing being protected is the sequence, not any one of
/// them.
///
/// A session-level lock outlives the transaction that took it and is released
/// only by `pg_advisory_unlock` or by the session ending. A `PoolConnection`
/// returning to the pool does **not** end its session, so a connection handed
/// back while still holding this lock poisons the pool: the lock is held by
/// nobody in particular, `pg_try_advisory_lock` fails for every later run, and
/// the backfill is permanently unavailable until the process restarts. Every
/// path out of this type therefore ends in one of two proofs — the unlock
/// returned `true`, or the connection was closed — and nothing else counts.
pub(super) struct TrancheLock {
    /// `None` once ownership has been handed to `release`. Kept as an `Option`
    /// so `Drop` can tell "already dealt with" from "leaked by a panic" and take
    /// the connection out of the pool in the second case.
    conn: Option<sqlx::pool::PoolConnection<sqlx::Postgres>>,
}

/// How a lock's cleanup ended. Not an alias for `Result`: two of the three are
/// success, and the difference between them matters to the log even though it
/// does not matter to the caller's return value.
#[derive(Debug)]
pub(super) enum LockCleanup {
    /// `pg_advisory_unlock` returned `true`. The lock is gone and the connection
    /// is clean, so it goes back to the pool.
    Unlocked,
    /// The unlock could not be confirmed, so the connection was closed instead.
    /// Ending the session releases every session-level lock it held, which makes
    /// this an equally good proof — bought by discarding a connection.
    ConnectionClosed { why: String },
    /// Neither proof could be obtained. The lock may still be held.
    Leaked { why: String },
}

impl LockCleanup {
    /// Whether the session lock is provably gone.
    pub(super) fn is_guaranteed(&self) -> bool {
        !matches!(self, Self::Leaked { .. })
    }

    fn why(&self) -> Option<&str> {
        match self {
            Self::Unlocked => None,
            Self::ConnectionClosed { why } | Self::Leaked { why } => Some(why),
        }
    }
}

impl TrancheLock {
    pub(super) async fn acquire(pool: &PgPool) -> Result<Self> {
        let conn = pool.acquire().await?;

        // The guard is built *before* the lock query, and that ordering is the
        // point -- the same lesson as `release`, at the other end of the
        // lifecycle.
        //
        // While the connection is a bare local `PoolConnection`, a cancellation
        // between PostgreSQL evaluating `pg_try_advisory_lock` and the result
        // reaching this client drops that local, and dropping one *returns it to
        // the pool* with the session lock held. No `TrancheLock` exists yet, so
        // nothing runs the detach-and-close cleanup. Worse than a plain leak:
        // session advisory locks are counted, so a later checkout of that same
        // session re-enters the lock rather than blocking on it, and a single
        // unlock then leaves it held.
        //
        // Constructing the guard first means any cancellation from here on runs
        // `Drop`, which detaches and closes.
        let mut guard = Self { conn: Some(conn) };

        let acquired: bool = {
            let held = guard.conn.as_mut().expect("just constructed with Some");
            sqlx::query_scalar("SELECT pg_try_advisory_lock($1)")
                .bind(TRANCHE_BACKFILL_ADVISORY_LOCK_ID)
                .fetch_one(&mut **held)
                .await
                // On a query error the lock's state is unknown, so `guard` drops
                // here and burns the connection rather than risk returning a
                // holder to the pool.
                .context("asking PostgreSQL for the tranche backfill advisory lock")?
        };

        if !acquired {
            // `false` is PostgreSQL stating this session does *not* hold the
            // lock. That is a proof, so the connection is clean and goes back to
            // the pool -- taking it out of the guard is what stops `Drop` from
            // needlessly burning a connection on every contended run.
            drop(guard.conn.take());
            bail!(
                "another tenancy tranche backfill is already running against this database; \
                 concurrent runs would interleave their cursor writes and leave a checkpoint \
                 that describes neither"
            );
        }

        Ok(guard)
    }

    /// Give the lock up, and say what was actually proved.
    ///
    /// Consumes `self` so there is no "released" flag to get out of step with
    /// reality. The previous version set that flag *before* awaiting the unlock,
    /// so a failed unlock left a connection marked clean and returned it to the
    /// pool still holding the lock — the exact poisoning this type exists to
    /// prevent, reached by the error path rather than the happy one.
    ///
    /// `pg_advisory_unlock` returns a boolean rather than raising, and `false`
    /// means "this session did not hold that lock". That is not a success: it
    /// means the state is not what this type believed, so it is treated the same
    /// as a failed query.
    pub(super) async fn release(mut self) -> LockCleanup {
        // The connection stays inside `self` across the unlock await, and that
        // is the whole point of this shape.
        //
        // Taking it out first -- into a local `PoolConnection` -- looks
        // equivalent and is not. If the future is cancelled while
        // `pg_advisory_unlock` is in flight (a caller aborting the task, a
        // future management request being cancelled), the local is dropped and
        // a `PoolConnection` drop *returns it to the pool*, still holding the
        // lock; `Drop` below then sees `None` and cannot rescue it. Leaving it
        // in `self` means cancellation runs `Drop` with the connection still
        // present, which detaches and closes it. Same poisoning this type
        // exists to prevent, reached by cancellation rather than by error.
        let Some(conn) = self.conn.as_mut() else {
            // Unreachable while `release` consumes `self`, and cheap to state.
            return LockCleanup::Unlocked;
        };

        let unlock_error = match sqlx::query_scalar::<_, bool>("SELECT pg_advisory_unlock($1)")
            .bind(TRANCHE_BACKFILL_ADVISORY_LOCK_ID)
            .fetch_one(&mut **conn)
            .await
        {
            Ok(true) => {
                // Proven released. Only now is handing the connection back
                // safe, so only now is it taken out of the guard.
                drop(self.conn.take());
                return LockCleanup::Unlocked;
            }
            Ok(false) => "pg_advisory_unlock returned false: this session did not hold the \
                          tranche backfill lock, so the lock state is not what this run assumed"
                .to_string(),
            Err(e) => format!("pg_advisory_unlock failed: {e}"),
        };

        // Unlock unproven, so the connection must not go back to the pool.
        //
        // `detach` first and synchronously: it removes the connection from the
        // pool's accounting with no await in between, so from here on no
        // cancellation can hand it to another borrower. Only then is the close
        // awaited -- and if *that* is cancelled, dropping a detached connection
        // closes it rather than returning it.
        let raw = self
            .conn
            .take()
            .expect("still held: `as_mut` above succeeded and nothing took it since")
            .detach();

        match sqlx::Connection::close(raw).await {
            Ok(()) => LockCleanup::ConnectionClosed { why: unlock_error },
            // The connection is detached either way, so the pool is not
            // poisoned; what is unproven is whether the backend has gone yet,
            // and until it does the lock may still be held.
            Err(close) => LockCleanup::Leaked {
                why: format!(
                    "{unlock_error}; closing the connection also failed: {close}. The connection \
                     was detached from the pool, so no later borrower can inherit it"
                ),
            },
        }
    }
}

impl Drop for TrancheLock {
    fn drop(&mut self) {
        if let Some(conn) = self.conn.take() {
            // Reached only when the run panicked or unwound past `release`.
            // Nothing async can run here, so the unlock cannot be issued — but
            // the connection can still be kept out of the pool. `detach` removes
            // it from the pool's accounting, and dropping the detached
            // connection closes it, ending the session and with it the lock.
            //
            // Returning it to the pool instead would hand the next borrower a
            // session still holding the tranche lock, and every subsequent
            // backfill would refuse to start with a message about another run
            // that is not running.
            drop(conn.detach());
            tracing::error!(
                "tenancy tranche backfill lock dropped without release; its connection has been \
                 detached and closed rather than returned to the pool"
            );
        }
    }
}

/// Decide what a run returns, given its result and what cleanup could prove.
///
/// Separated from [`run_tranche_backfill`] because the interesting part is the
/// six-way decision and not the plumbing, and because the two failure axes are
/// independent: the backfill can succeed or fail, and cleanup can prove the lock
/// is gone or not.
///
/// The rule is that a report is returned only when the lock is provably gone.
/// CodeRabbit suggested logging any release failure and returning the report
/// regardless, which is right about the first half — a committed completion must
/// not be hidden behind a cleanup error — and wrong about the second: a report
/// returned over a still-held session lock reads as "this succeeded, carry on",
/// while the next run and every run after it refuses to start. When the lock
/// cannot be accounted for, the caller is told both things.
fn resolve_run_outcome(
    result: Result<TrancheBackfillReport>,
    cleanup: LockCleanup,
) -> Result<TrancheBackfillReport> {
    if let Some(why) = cleanup.why() {
        if cleanup.is_guaranteed() {
            tracing::warn!(
                reason = %why,
                "tenancy tranche backfill lock could not be released normally; its connection \
                 was closed instead, which released the lock",
            );
        } else {
            tracing::error!(
                reason = %why,
                "tenancy tranche backfill lock may still be held; further runs will refuse to \
                 start until the holding session ends",
            );
        }
    }

    match (result, cleanup) {
        (result, LockCleanup::Unlocked) | (result, LockCleanup::ConnectionClosed { .. }) => result,

        // The backfill's own error is the more actionable of the two, so it
        // leads; the leak is attached rather than replacing it.
        (Err(run), LockCleanup::Leaked { why }) => Err(run.context(format!(
            "the tranche backfill lock was also left held: {why}"
        ))),

        // The work committed and the report is true, but returning it alone
        // would let an operator walk away from a database that cannot run
        // another backfill. Both facts, in one error.
        (Ok(report), LockCleanup::Leaked { why }) => Err(anyhow!(
            "the backfill COMMITTED and its checkpoint is real — tranche {tranche}, checkpoint \
             {checkpoint}, outcome {outcome}, {backfilled}/{total} rows settled, blocking count \
             {blocking} — but the advisory lock could not be released and may still be held, so \
             later runs will refuse to start until the holding session ends. Do not re-run the \
             backfill to \"fix\" this; check the checkpoint and clear the stale session. Cause: \
             {why}",
            tranche = report.tranche,
            checkpoint = report.checkpoint_id,
            outcome = report.outcome.as_str(),
            backfilled = report.rows_backfilled,
            total = report.rows_total,
            blocking = report.blocking_count,
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tranche_one_targets_are_the_five_bridged_tables() {
        let targets = targets_for(Tranche::RootsAndDirectAgentChildren).expect("targets derive");
        let names: Vec<&str> = targets.iter().map(|t| t.table).collect();
        assert_eq!(
            names,
            vec![
                "archival_batches",
                "audit_logs",
                "entities",
                "memory_graph",
                "rmk_policies",
            ],
            "the tranche-1 backfill set is derived from the registry; `agents` owns itself, \
             `credentials` and `credential_agent_grants` are already tenant-scoped, and \
             `tenancy_backfill_checkpoints` describes the migration rather than an owner"
        );
    }

    #[test]
    fn audit_logs_is_the_only_conditional_target() {
        let targets = targets_for(Tranche::RootsAndDirectAgentChildren).expect("targets derive");
        for target in &targets {
            assert_eq!(
                target.agentless_permitted,
                target.table == "audit_logs",
                "{} disagrees about whether an agentless row is legitimate",
                target.table
            );
            assert_eq!(target.agent_column, "agent_id");
        }
    }

    #[test]
    fn the_conditional_target_admits_the_agentless_row_and_the_owned_one() {
        let targets = targets_for(Tranche::RootsAndDirectAgentChildren).expect("targets derive");
        let agent = SqlIdentifier::new("agent_id").expect("valid identifier");

        let audit_logs = targets.iter().find(|t| t.table == "audit_logs").unwrap();
        let conditional = audit_logs.settled_predicate("x", &agent);
        assert!(
            conditional.contains("\"agent_id\" IS NULL"),
            "audit_logs must settle its agentless rows: {conditional}"
        );

        let entities = targets.iter().find(|t| t.table == "entities").unwrap();
        let unconditional = entities.settled_predicate("x", &agent);
        assert!(
            !unconditional.contains("\"agent_id\" IS NULL"),
            "a mandatory-agent table has no agentless settled state: {unconditional}"
        );
    }

    #[test]
    fn a_cursor_round_trips_through_its_encoding() {
        let targets = targets_for(Tranche::RootsAndDirectAgentChildren).expect("targets derive");
        let id = Uuid::parse_str("11111111-2222-3333-4444-555555555555").unwrap();

        for cursor in [
            ResumeCursor {
                table: "entities".into(),
                after_id: Some(id),
            },
            ResumeCursor {
                table: "audit_logs".into(),
                after_id: None,
            },
        ] {
            let encoded = cursor.encode();
            let decoded = ResumeCursor::decode(&encoded, &targets).expect("decodes");
            assert_eq!(decoded, cursor, "cursor {encoded} did not round-trip");
        }
    }

    #[test]
    fn a_cursor_naming_a_foreign_table_is_refused_not_reset() {
        let targets = targets_for(Tranche::RootsAndDirectAgentChildren).expect("targets derive");
        let err = ResumeCursor::decode("sessions|", &targets).expect_err("must refuse");
        assert!(
            err.to_string().contains("sessions"),
            "the refusal must name the offending table: {err}"
        );

        let err = ResumeCursor::decode("entities|not-a-uuid", &targets).expect_err("must refuse");
        assert!(err.to_string().contains("malformed key"), "{err}");

        let err = ResumeCursor::decode("entities", &targets).expect_err("must refuse");
        assert!(err.to_string().contains("malformed"), "{err}");
    }

    #[test]
    fn batch_options_are_bounded_on_both_sides() {
        assert!(BackfillOptions::default().validate().is_ok());
        assert!(BackfillOptions {
            batch_size: 0,
            ..BackfillOptions::default()
        }
        .validate()
        .is_err());
        assert!(BackfillOptions {
            batch_size: MAX_BATCH_SIZE + 1,
            ..BackfillOptions::default()
        }
        .validate()
        .is_err());
        assert!(BackfillOptions {
            batch_size: DEFAULT_BATCH_SIZE,
            max_batches: Some(0),
            ..BackfillOptions::default()
        }
        .validate()
        .is_err());

        // Zero disables the timeout in PostgreSQL, so it must be refused too.
        assert!(BackfillOptions {
            reconcile_lock_timeout_secs: 0,
            ..BackfillOptions::default()
        }
        .validate()
        .is_err());
        assert!(BackfillOptions {
            reconcile_statement_timeout_secs: 0,
            ..BackfillOptions::default()
        }
        .validate()
        .is_err());
    }

    fn sample_report() -> TrancheBackfillReport {
        TrancheBackfillReport {
            tranche: Tranche::RootsAndDirectAgentChildren,
            contract_digest: "sha256:abc".into(),
            checkpoint_id: Uuid::nil(),
            status: CheckpointStatus::Completed,
            outcome: BackfillOutcome::Completed,
            rows_total: 5,
            rows_backfilled: 5,
            blocking_count: 0,
            blocking_reasons: Vec::new(),
            resume_cursor: None,
            batches_executed: 1,
            rows_settled_this_run: 5,
        }
    }

    #[test]
    fn a_confirmed_unlock_lets_a_successful_run_report_normally() {
        let out = resolve_run_outcome(Ok(sample_report()), LockCleanup::Unlocked)
            .expect("a clean run with a clean unlock reports");
        assert_eq!(out.outcome, BackfillOutcome::Completed);
    }

    #[test]
    fn closing_the_connection_is_as_good_a_proof_as_unlocking() {
        // Ending the session releases every session-level lock it held, so the
        // guarantee holds and the committed work is still reported. Paying one
        // connection for that is the point of the discard path.
        let out = resolve_run_outcome(
            Ok(sample_report()),
            LockCleanup::ConnectionClosed {
                why: "unlock failed".into(),
            },
        )
        .expect("a closed connection still guarantees the lock is gone");
        assert_eq!(out.outcome, BackfillOutcome::Completed);
    }

    #[test]
    fn a_leaked_lock_turns_a_successful_run_into_a_combined_error() {
        // The alternative — CodeRabbit's suggestion of logging and returning the
        // report — reads as "this succeeded, carry on" while every later run
        // refuses to start. Both facts have to reach the operator.
        let err = resolve_run_outcome(
            Ok(sample_report()),
            LockCleanup::Leaked {
                why: "connection wedged".into(),
            },
        )
        .expect_err("an unaccounted-for lock is not a clean success");
        let message = err.to_string();

        assert!(
            message.contains("COMMITTED") && message.contains("checkpoint"),
            "the committed result must survive the error: {message}"
        );
        assert!(
            message.contains("connection wedged"),
            "the cleanup cause must survive too: {message}"
        );
        assert!(
            message.contains("Do not re-run"),
            "an operator needs to be told what NOT to do: {message}"
        );
    }

    #[test]
    fn a_failed_run_keeps_its_own_error_whatever_cleanup_did() {
        for cleanup in [
            LockCleanup::Unlocked,
            LockCleanup::ConnectionClosed { why: "x".into() },
        ] {
            let err = resolve_run_outcome(Err(anyhow!("the backfill itself failed")), cleanup)
                .expect_err("a failed run stays failed");
            assert!(err.to_string().contains("the backfill itself failed"));
        }

        // With a leak, the run's own error still leads -- it is the more
        // actionable of the two -- and the leak is attached rather than
        // replacing it.
        let err = resolve_run_outcome(
            Err(anyhow!("the backfill itself failed")),
            LockCleanup::Leaked {
                why: "wedged".into(),
            },
        )
        .expect_err("a failed run stays failed");
        let chain = format!("{err:#}");
        assert!(chain.contains("the backfill itself failed"), "{chain}");
        assert!(chain.contains("wedged"), "{chain}");
    }

    #[test]
    fn only_a_leak_is_an_unguaranteed_cleanup() {
        assert!(LockCleanup::Unlocked.is_guaranteed());
        assert!(LockCleanup::ConnectionClosed { why: "x".into() }.is_guaranteed());
        assert!(!LockCleanup::Leaked { why: "x".into() }.is_guaranteed());
    }

    #[test]
    fn later_tranches_are_refused_rather_than_walked_with_the_wrong_authority() {
        // Tranche 2 holds `working_memory`, whose ownership resolves through
        // `sessions`. Walking it with an agents-shaped UPDATE would resolve
        // ownership from the wrong parent, which is the failure the
        // `BackfillAuthority` agreement predicate exists to prevent.
        let err = targets_for(Tranche::Sessions).expect_err("tranche 2 is not implemented here");
        let message = err.to_string();
        assert!(
            message.contains("working_memory") || message.contains("does not implement"),
            "the refusal must say why: {message}"
        );
    }
}
