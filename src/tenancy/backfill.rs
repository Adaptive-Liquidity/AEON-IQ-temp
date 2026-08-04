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
/// Step 4B-2 wires its CLI subcommand in the next increment and a management
/// endpoint in a later one, so this entry point has no production caller yet
/// and the tests are its only consumer. Narrow allowance on the entry points
/// rather than the module, so anything that becomes *genuinely* unreachable
/// still shows up.
#[allow(dead_code)]
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
}

impl Default for BackfillOptions {
    fn default() -> Self {
        Self {
            batch_size: DEFAULT_BATCH_SIZE,
            max_batches: None,
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
    #[allow(dead_code)]
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

/// Run (or resume) the backfill for one tranche.
///
/// Idempotent at every granularity: a re-run over a `COMPLETED` tranche does
/// nothing, a re-run over a partially-done one resumes from the persisted
/// cursor, and the `UPDATE` itself writes values the bridge would write anyway,
/// so re-walking a settled row is a no-op rather than a second assignment.
/// Step 4B-2 wires its CLI subcommand in the next increment and a management
/// endpoint in a later one, so this entry point has no production caller yet
/// and the tests are its only consumer. Narrow allowance on the entry points
/// rather than the module, so anything that becomes *genuinely* unreachable
/// still shows up.
#[allow(dead_code)]
pub async fn run_tranche_backfill(
    pool: &PgPool,
    tranche: Tranche,
    options: BackfillOptions,
) -> Result<TrancheBackfillReport> {
    options.validate()?;
    let targets = targets_for(tranche)?;

    // Held for the whole run rather than per transaction: the point is that two
    // runs cannot interleave their cursor writes, and a per-transaction lock
    // would let them alternate batches and leave a cursor that describes
    // neither. `try` rather than a wait, so a second operator is told what is
    // happening instead of hanging on a lock with no message.
    let mut lock = TrancheLock::acquire(pool).await?;
    let result = run_locked(pool, tranche, options, &targets).await;
    lock.release().await?;
    result
}

/// Retire a checkpoint an operator has decided not to finish.
///
/// The only transition out of `IN_PROGRESS` other than completion. The cursor is
/// deliberately left in place: an abandoned checkpoint is history, and how far
/// it got is the interesting part of that history. `completed_at` stays NULL,
/// which `tenancy_backfill_checkpoints_completed_shape_ck` requires and which
/// keeps an abandoned row from ever reading as evidence of completion.
/// Step 4B-2 wires its CLI subcommand in the next increment and a management
/// endpoint in a later one, so this entry point has no production caller yet
/// and the tests are its only consumer. Narrow allowance on the entry points
/// rather than the module, so anything that becomes *genuinely* unreachable
/// still shows up.
#[allow(dead_code)]
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
/// Step 4B-2 wires its CLI subcommand in the next increment and a management
/// endpoint in a later one, so this entry point has no production caller yet
/// and the tests are its only consumer. Narrow allowance on the entry points
/// rather than the module, so anything that becomes *genuinely* unreachable
/// still shows up.
#[allow(dead_code)]
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
        batches_executed,
        settled_this_run,
    )
    .await
}

/// Reconcile the accounting against the live tables, consult the audit, and
/// complete only if both agree the tranche is done.
async fn finish(
    pool: &PgPool,
    tranche: Tranche,
    digest: String,
    checkpoint_id: Uuid,
    targets: &[BackfillTarget],
    batches_executed: u64,
    settled_this_run: i64,
) -> Result<TrancheBackfillReport> {
    let counts = count_rows(pool, targets).await?;

    // The authoritative verdict. Derived from the audit's own tranche readiness
    // rather than recomputed from `findings`, so `blocking_count == 0` and
    // `ready == true` cannot come apart: they are the same list. Re-deriving it
    // here would be a second definition of "blocking for this tranche", and the
    // second definition is the one that eventually disagrees.
    let audited = audit::run(pool, None)
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

    let mut tx = pool.begin().await?;
    let status = if clean {
        sqlx::query(
            "UPDATE tenancy_backfill_checkpoints \
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
        CheckpointStatus::Completed
    } else {
        // Stays IN_PROGRESS, and keeps a cursor of NULL: the scan did reach the
        // end, so there is no position to resume from. What is left is not more
        // scanning but a decision about rows the backfill is not permitted to
        // guess at.
        sqlx::query(
            "UPDATE tenancy_backfill_checkpoints \
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
        CheckpointStatus::InProgress
    };
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

    // `last_id` is taken as the batch's own last row rather than with `max()`:
    // PostgreSQL ships no `max(uuid)` aggregate (measured on pg16 — `function
    // max(uuid) does not exist`), and every tranche-1 key is a UUID. Reading it
    // back off the ordered, limited CTE is also the more direct statement of
    // what the cursor means: the last key this batch scanned.
    let sql = format!(
        "WITH batch AS ( \
             SELECT x.\"id\" \
               FROM public.{table} x \
              WHERE ($1::uuid IS NULL OR x.\"id\" > $1::uuid) \
                AND NOT {settled} \
              ORDER BY x.\"id\" \
              LIMIT $2 \
         ), \
         updated AS ( \
             UPDATE public.{table} AS x \
                SET \"agent_uuid\" = a.\"id\", \"tenant_id\" = a.\"tenant_id\" \
               FROM public.agents a \
              WHERE x.\"id\" IN (SELECT \"id\" FROM batch) \
                AND a.\"agent_id\" = x.{agent} \
          RETURNING ({settled}) AS is_settled \
         ) \
         SELECT (SELECT count(*) FROM batch)::bigint AS scanned, \
                (SELECT count(*) FROM updated WHERE is_settled)::bigint AS settled, \
                (SELECT \"id\" FROM batch ORDER BY \"id\" DESC LIMIT 1) AS last_id",
        table = table.as_sql(),
        agent = agent.as_sql(),
    );

    let row = sqlx::query(AssertSqlSafe(sql))
        .bind(after_id)
        .bind(batch_size)
        .fetch_one(pool)
        .await
        .with_context(|| format!("backfilling a batch of {}", target.table))?;

    Ok(BatchResult {
        scanned: row.try_get("scanned")?,
        settled: row.try_get("settled")?,
        last_id: row.try_get("last_id")?,
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
            .fetch_one(&mut *tx)
            .await
            .with_context(|| format!("counting {}", target.table))?;
        total += row.try_get::<i64, _>("total")?;
        settled += row.try_get::<i64, _>("settled")?;
    }

    tx.commit().await?;
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
/// them. Released explicitly: a session lock survives the connection going back
/// to the pool, so dropping this without unlocking would leave the tranche
/// locked until that connection is recycled.
struct TrancheLock {
    conn: sqlx::pool::PoolConnection<sqlx::Postgres>,
    released: bool,
}

impl TrancheLock {
    async fn acquire(pool: &PgPool) -> Result<Self> {
        let mut conn = pool.acquire().await?;
        let row = sqlx::query("SELECT pg_try_advisory_lock($1) AS acquired")
            .bind(TRANCHE_BACKFILL_ADVISORY_LOCK_ID)
            .fetch_one(&mut *conn)
            .await?;
        let acquired: bool = row.try_get("acquired")?;
        if !acquired {
            bail!(
                "another tenancy tranche backfill is already running against this database; \
                 concurrent runs would interleave their cursor writes and leave a checkpoint \
                 that describes neither"
            );
        }
        Ok(Self {
            conn,
            released: false,
        })
    }

    async fn release(&mut self) -> Result<()> {
        if self.released {
            return Ok(());
        }
        self.released = true;
        sqlx::query("SELECT pg_advisory_unlock($1)")
            .bind(TRANCHE_BACKFILL_ADVISORY_LOCK_ID)
            .execute(&mut *self.conn)
            .await
            .context("releasing the tranche backfill advisory lock")?;
        Ok(())
    }
}

impl Drop for TrancheLock {
    fn drop(&mut self) {
        if !self.released {
            // Reached only when the run panicked or returned without going
            // through `release`. Nothing async can run here, so the lock stays
            // held until the connection is closed or recycled. Say so, loudly:
            // a silently stuck lock looks exactly like a hung backfill.
            tracing::error!(
                "tenancy tranche backfill lock dropped without release; it is held until this \
                 connection is recycled"
            );
        }
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
            max_batches: None
        }
        .validate()
        .is_err());
        assert!(BackfillOptions {
            batch_size: MAX_BATCH_SIZE + 1,
            max_batches: None
        }
        .validate()
        .is_err());
        assert!(BackfillOptions {
            batch_size: DEFAULT_BATCH_SIZE,
            max_batches: Some(0)
        }
        .validate()
        .is_err());
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
