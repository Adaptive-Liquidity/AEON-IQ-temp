//! Tranche 1 BACKFILL behaviour, measured against a live database.
//!
//! Split from `tranche1_db_tests.rs` for the same reason that file was split
//! from `audit_db_tests.rs`: the no-database CI job skips DB-backed modules by
//! name in an explicit enumeration in `.github/workflows/ci.yml`, so a new
//! module has to be added there as well as here.
//!
//! Everything here writes rows *before* the bridges could have owned them and
//! then asks the engine to resolve them, because that is the only situation the
//! backfill exists for. A test that inserted through the live bridges and then
//! ran the backfill would measure nothing: the rows would already be settled.
//! The helpers below therefore disable the bridge triggers around their seed
//! inserts, which is exactly the state a database upgraded into PREPARE is in.

use sqlx::{AssertSqlSafe, PgPool, Row};
use uuid::Uuid;

use super::backfill::{
    abandon_tranche_backfill, run_tranche_backfill, targets_for, tranche_backfill_status,
    BackfillOptions, BackfillOutcome, LockCleanup, TrancheLock, MIN_BACKFILL_CONNECTIONS,
};
use super::inventory::Tranche;
use super::plan::CheckpointStatus;
use super::report;

const TENANT: &str = "11111111-1111-1111-1111-111111111111";
const TRANCHE: Tranche = Tranche::RootsAndDirectAgentChildren;

/// Mirrors `backfill::TRANCHE_BACKFILL_ADVISORY_LOCK_ID`, which is private.
/// `the_lock_id_these_tests_watch_is_the_one_the_engine_takes` fails if they
/// drift, so these tests cannot end up watching a lock nobody takes.
const TRANCHE_LOCK_ID: i64 = 0x4145_4F4E_5442;

fn options(batch_size: i64, max_batches: Option<u64>) -> BackfillOptions {
    BackfillOptions {
        batch_size,
        max_batches,
    }
}

async fn insert_agent(pool: &PgPool, agent_id: &str, tenant: Option<&str>) {
    let query = match tenant {
        Some(t) => sqlx::query(
            "INSERT INTO agents (agent_id, tenant_id, external_agent_id) \
             VALUES ($1, $2::uuid, $3)",
        )
        .bind(agent_id)
        .bind(t)
        .bind(format!("ext-{agent_id}")),
        None => sqlx::query("INSERT INTO agents (agent_id, external_agent_id) VALUES ($1, $2)")
            .bind(agent_id)
            .bind(format!("ext-{agent_id}")),
    };
    query.execute(pool).await.expect("insert agent");
}

/// Run `body` with the tranche-1 bridge triggers disabled.
///
/// This is how a row that predates PREPARE is simulated. `ALTER TABLE ...
/// DISABLE TRIGGER USER` is used rather than `session_replication_role`,
/// because the latter also disables foreign keys and would let the seed create
/// referential states the schema forbids — which would then be measuring the
/// backfill against a database that cannot exist.
async fn without_bridges<F, Fut>(pool: &PgPool, body: F)
where
    F: FnOnce(PgPool) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let targets = targets_for(TRANCHE).expect("targets derive");
    for target in &targets {
        sqlx::query(AssertSqlSafe(format!(
            "ALTER TABLE public.{} DISABLE TRIGGER USER",
            target.table
        )))
        .execute(pool)
        .await
        .expect("disable bridges");
    }

    body(pool.clone()).await;

    for target in &targets {
        sqlx::query(AssertSqlSafe(format!(
            "ALTER TABLE public.{} ENABLE TRIGGER USER",
            target.table
        )))
        .execute(pool)
        .await
        .expect("re-enable bridges");
    }
}

/// One INSERT per tranche-1 table, supplying only the legacy `agent_id` and
/// whatever else the table requires.
const SEED_STATEMENTS: &[&str] = &[
    "INSERT INTO archival_batches (agent_id, source_count, l3_count) VALUES ($1, 1, 1)",
    "INSERT INTO audit_logs (agent_id, event_type) VALUES ($1, 'seed')",
    "INSERT INTO entities (agent_id, name, entity_type) VALUES ($1, 'e', 'person')",
    "INSERT INTO memory_graph (agent_id, subject, predicate, object) VALUES ($1, 's', 'p', 'o')",
    "INSERT INTO rmk_policies (agent_id, pressure_a, pressure_b, kp, ki, \
     graph_bonus_weight, retrieval_threshold) VALUES ($1, 0, 0, 0, 0, 0, 0)",
];

/// Seed one row per tranche-1 table for `agent_id`.
async fn seed_row_everywhere(pool: &PgPool, agent_id: &str) {
    for sql in SEED_STATEMENTS {
        sqlx::query(*sql)
            .bind(agent_id)
            .execute(pool)
            .await
            .unwrap_or_else(|e| panic!("seed failed for {sql}: {e}"));
    }
}

async fn unowned_count(pool: &PgPool, table: &str) -> i64 {
    let row = sqlx::query(AssertSqlSafe(format!(
        "SELECT count(*)::bigint AS n FROM public.{table} \
          WHERE tenant_id IS NULL OR agent_uuid IS NULL"
    )))
    .fetch_one(pool)
    .await
    .expect("count unowned");
    row.try_get("n").expect("bigint")
}

async fn checkpoint_column<T>(pool: &PgPool, id: Uuid, column: &str) -> T
where
    T: for<'r> sqlx::Decode<'r, sqlx::Postgres> + sqlx::Type<sqlx::Postgres> + Send + Unpin,
{
    let row = sqlx::query(AssertSqlSafe(format!(
        "SELECT {column} AS v FROM tenancy_backfill_checkpoints WHERE id = $1"
    )))
    .bind(id)
    .fetch_one(pool)
    .await
    .expect("read checkpoint");
    row.try_get("v").expect("column decodes")
}

// ── The happy path ───────────────────────────────────────────────────────────

#[sqlx::test]
async fn a_clean_tranche_settles_every_row_and_completes(pool: PgPool) {
    insert_agent(&pool, "agent-a", Some(TENANT)).await;
    without_bridges(&pool, |p| async move {
        seed_row_everywhere(&p, "agent-a").await;
    })
    .await;

    for target in targets_for(TRANCHE).unwrap() {
        assert_eq!(
            unowned_count(&pool, target.table).await,
            1,
            "{} should start unowned, or the test is measuring nothing",
            target.table
        );
    }

    let report = run_tranche_backfill(&pool, TRANCHE, options(10, None))
        .await
        .expect("backfill runs");

    assert_eq!(report.outcome, BackfillOutcome::Completed, "{report:?}");
    assert_eq!(report.status, CheckpointStatus::Completed);
    assert_eq!(report.blocking_count, 0, "{:?}", report.blocking_reasons);
    assert_eq!(report.rows_backfilled, report.rows_total);
    assert_eq!(report.rows_total, 5, "one row per tranche-1 table");
    assert_eq!(report.rows_settled_this_run, 5);
    assert!(report.is_finalizable(), "{report:?}");

    for target in targets_for(TRANCHE).unwrap() {
        assert_eq!(
            unowned_count(&pool, target.table).await,
            0,
            "{} still has an unowned row after a COMPLETED backfill",
            target.table
        );
    }

    // The stored row is what FINALIZE reads, so assert against it and not only
    // against the in-memory report.
    let cursor: Option<String> =
        checkpoint_column(&pool, report.checkpoint_id, "resume_cursor").await;
    assert_eq!(cursor, None, "a completed checkpoint has nothing to resume");
    let completed_at: Option<chrono::DateTime<chrono::Utc>> =
        checkpoint_column(&pool, report.checkpoint_id, "completed_at").await;
    assert!(
        completed_at.is_some(),
        "COMPLETED must carry a completion time"
    );
    let digest: String = checkpoint_column(&pool, report.checkpoint_id, "contract_digest").await;
    assert_eq!(
        digest,
        report::inventory_digest(),
        "the checkpoint must record the digest FINALIZE will compare against"
    );
}

#[sqlx::test]
async fn re_running_a_completed_tranche_does_nothing(pool: PgPool) {
    insert_agent(&pool, "agent-a", Some(TENANT)).await;
    without_bridges(&pool, |p| async move {
        seed_row_everywhere(&p, "agent-a").await;
    })
    .await;

    let first = run_tranche_backfill(&pool, TRANCHE, options(10, None))
        .await
        .expect("first run");
    assert_eq!(first.outcome, BackfillOutcome::Completed);

    let second = run_tranche_backfill(&pool, TRANCHE, options(10, None))
        .await
        .expect("second run");
    assert_eq!(second.outcome, BackfillOutcome::AlreadyCompleted);
    assert_eq!(
        second.checkpoint_id, first.checkpoint_id,
        "a re-run must report the existing completion, not raise a second one"
    );
    assert_eq!(second.batches_executed, 0);

    let count: i64 = sqlx::query("SELECT count(*)::bigint AS n FROM tenancy_backfill_checkpoints")
        .fetch_one(&pool)
        .await
        .unwrap()
        .try_get("n")
        .unwrap();
    assert_eq!(count, 1, "a no-op re-run must not add a checkpoint row");
}

// ── Resumability ─────────────────────────────────────────────────────────────

#[sqlx::test]
async fn a_bounded_run_pauses_persists_its_cursor_and_resumes_from_it(pool: PgPool) {
    insert_agent(&pool, "agent-a", Some(TENANT)).await;
    // Twelve entities so a batch of two cannot finish the table in one run.
    without_bridges(&pool, |p| async move {
        for _ in 0..12 {
            sqlx::query("INSERT INTO entities (agent_id, name, entity_type) VALUES ($1, $2, 'x')")
                .bind("agent-a")
                .bind(Uuid::new_v4().to_string())
                .execute(&p)
                .await
                .expect("seed entity");
        }
    })
    .await;

    let paused = run_tranche_backfill(&pool, TRANCHE, options(2, Some(1)))
        .await
        .expect("bounded run");
    assert_eq!(paused.outcome, BackfillOutcome::Paused, "{paused:?}");
    assert_eq!(paused.status, CheckpointStatus::InProgress);
    assert_eq!(paused.batches_executed, 1);
    assert_eq!(paused.rows_settled_this_run, 2);

    let cursor = paused
        .resume_cursor
        .clone()
        .expect("a paused run has a cursor");
    assert!(
        cursor.starts_with("entities|"),
        "the cursor must name the table it stopped in: {cursor}"
    );
    let stored: Option<String> =
        checkpoint_column(&pool, paused.checkpoint_id, "resume_cursor").await;
    assert_eq!(
        stored.as_deref(),
        Some(cursor.as_str()),
        "the cursor must be persisted, not merely returned"
    );
    assert_eq!(
        unowned_count(&pool, "entities").await,
        10,
        "exactly one batch of two should have been settled"
    );

    // Resume. The engine must pick up from the stored cursor and finish.
    let done = run_tranche_backfill(&pool, TRANCHE, options(2, None))
        .await
        .expect("resumed run");
    assert_eq!(done.outcome, BackfillOutcome::Completed, "{done:?}");
    assert_eq!(
        done.checkpoint_id, paused.checkpoint_id,
        "resuming must continue the same checkpoint, not open a new one"
    );
    assert_eq!(
        done.rows_settled_this_run, 10,
        "the resumed run must settle only the ten rows the paused run left, which is what \
         proves it started from the cursor rather than from the beginning"
    );
    assert_eq!(unowned_count(&pool, "entities").await, 0);
    assert_eq!(done.rows_total, done.rows_backfilled);
}

#[sqlx::test]
async fn a_pause_that_lands_on_a_table_boundary_resumes_into_the_next_table(pool: PgPool) {
    insert_agent(&pool, "agent-a", Some(TENANT)).await;
    without_bridges(&pool, |p| async move {
        seed_row_everywhere(&p, "agent-a").await;
    })
    .await;

    // One batch, large enough to exhaust `archival_batches` (a single row) in
    // one step. The run stops with the cursor sitting at the end of the first
    // table, which is the case where "resume where you stopped" and "resume
    // with the next table" have to agree.
    let paused = run_tranche_backfill(&pool, TRANCHE, options(10, Some(1)))
        .await
        .expect("bounded run");
    assert_eq!(paused.outcome, BackfillOutcome::Paused);
    assert!(
        paused
            .resume_cursor
            .as_deref()
            .is_some_and(|c| c.starts_with("archival_batches|")),
        "{:?}",
        paused.resume_cursor
    );
    assert_eq!(unowned_count(&pool, "archival_batches").await, 0);
    assert_eq!(unowned_count(&pool, "entities").await, 1);

    let done = run_tranche_backfill(&pool, TRANCHE, options(10, None))
        .await
        .expect("resumed run");
    assert_eq!(done.outcome, BackfillOutcome::Completed, "{done:?}");
    assert_eq!(
        done.rows_settled_this_run, 4,
        "the four tables after `archival_batches` are what remained"
    );
}

// ── Blocking, and the refusal to complete over it ────────────────────────────

#[sqlx::test]
async fn an_unmapped_agent_blocks_completion_instead_of_being_guessed_at(pool: PgPool) {
    // An agent with no tenant. Its rows are resolvable to an agent and
    // unresolvable to a tenant, which is `LEGACY_UNMAPPED`.
    insert_agent(&pool, "agent-unmapped", None).await;
    without_bridges(&pool, |p| async move {
        sqlx::query(
            "INSERT INTO entities (agent_id, name, entity_type) VALUES ($1, 'e', 'person')",
        )
        .bind("agent-unmapped")
        .execute(&p)
        .await
        .expect("seed entity");
    })
    .await;

    let report = run_tranche_backfill(&pool, TRANCHE, options(10, None))
        .await
        .expect("backfill runs");

    assert_eq!(report.outcome, BackfillOutcome::Blocked, "{report:?}");
    assert_eq!(
        report.status,
        CheckpointStatus::InProgress,
        "a blocked tranche stays open; it has not finished"
    );
    assert!(report.blocking_count > 0);
    assert!(
        report
            .blocking_reasons
            .iter()
            .any(|r| r.contains("entities") && r.contains("LEGACY_UNMAPPED")),
        "the reasons must name the table and the code: {:?}",
        report.blocking_reasons
    );
    assert!(report.rows_backfilled < report.rows_total);
    assert!(!report.is_finalizable(), "FINALIZE must not accept this");

    // The row was not guessed at.
    let tenant: Option<Uuid> = sqlx::query("SELECT tenant_id FROM entities LIMIT 1")
        .fetch_one(&pool)
        .await
        .unwrap()
        .try_get("tenant_id")
        .unwrap();
    assert_eq!(tenant, None, "an unmapped agent must not acquire a tenant");

    // And no COMPLETED row exists at any digest.
    let completed: i64 = sqlx::query(
        "SELECT count(*)::bigint AS n FROM tenancy_backfill_checkpoints WHERE status = 'COMPLETED'",
    )
    .fetch_one(&pool)
    .await
    .unwrap()
    .try_get("n")
    .unwrap();
    assert_eq!(completed, 0);
}

#[sqlx::test]
async fn a_blocked_tranche_completes_once_the_blocker_is_resolved(pool: PgPool) {
    insert_agent(&pool, "agent-unmapped", None).await;
    without_bridges(&pool, |p| async move {
        sqlx::query(
            "INSERT INTO entities (agent_id, name, entity_type) VALUES ($1, 'e', 'person')",
        )
        .bind("agent-unmapped")
        .execute(&p)
        .await
        .expect("seed entity");
    })
    .await;

    let blocked = run_tranche_backfill(&pool, TRANCHE, options(10, None))
        .await
        .expect("first run");
    assert_eq!(blocked.outcome, BackfillOutcome::Blocked);

    // The operator's decision, made outside the backfill: this agent belongs to
    // this tenant.
    sqlx::query("UPDATE agents SET tenant_id = $1::uuid WHERE agent_id = 'agent-unmapped'")
        .bind(TENANT)
        .execute(&pool)
        .await
        .expect("assign tenant");

    let done = run_tranche_backfill(&pool, TRANCHE, options(10, None))
        .await
        .expect("second run");
    assert_eq!(done.outcome, BackfillOutcome::Completed, "{done:?}");
    assert_eq!(
        done.checkpoint_id, blocked.checkpoint_id,
        "the blocked checkpoint is the one that completes; a blocker is not a new attempt"
    );
    assert!(done.is_finalizable());
    assert_eq!(unowned_count(&pool, "entities").await, 0);
}

// ── The conditional table ────────────────────────────────────────────────────

#[sqlx::test]
async fn agentless_audit_rows_stay_null_and_do_not_block_completion(pool: PgPool) {
    insert_agent(&pool, "agent-a", Some(TENANT)).await;
    without_bridges(&pool, |p| async move {
        // Three agentless audit rows — startup, configuration, administrative
        // action — alongside one that names a resolvable agent.
        for action in ["startup", "config_change", "admin_action"] {
            sqlx::query("INSERT INTO audit_logs (event_type) VALUES ($1)")
                .bind(action)
                .execute(&p)
                .await
                .expect("seed agentless audit row");
        }
        sqlx::query("INSERT INTO audit_logs (agent_id, event_type) VALUES ($1, 'agent_action')")
            .bind("agent-a")
            .execute(&p)
            .await
            .expect("seed owned audit row");
    })
    .await;

    let report = run_tranche_backfill(&pool, TRANCHE, options(10, None))
        .await
        .expect("backfill runs");

    assert_eq!(
        report.outcome,
        BackfillOutcome::Completed,
        "agentless audit rows are legitimate and must not block the tranche: {:?}",
        report.blocking_reasons
    );
    assert_eq!(
        report.rows_total, 4,
        "all four audit rows count towards the tranche"
    );
    assert_eq!(
        report.rows_backfilled, 4,
        "an agentless row is settled by staying unowned; requiring otherwise would demand the \
         schema reject valid audit history"
    );

    let agentless_owned: i64 = sqlx::query(
        "SELECT count(*)::bigint AS n FROM audit_logs \
          WHERE agent_id IS NULL AND (tenant_id IS NOT NULL OR agent_uuid IS NOT NULL)",
    )
    .fetch_one(&pool)
    .await
    .unwrap()
    .try_get("n")
    .unwrap();
    assert_eq!(
        agentless_owned, 0,
        "an agentless audit row must not acquire an owner"
    );

    let named_unowned = unowned_count(&pool, "audit_logs").await;
    assert_eq!(
        named_unowned, 3,
        "exactly the three agentless rows remain NULL; the row naming an agent was resolved"
    );
}

#[sqlx::test]
async fn an_audit_row_naming_an_unresolvable_agent_still_blocks(pool: PgPool) {
    // The conditional bridge tolerates *agentless* rows. It does not tolerate a
    // row that names an agent nobody can resolve — `ORPHANED_AGENT_REFERENCE`
    // is in `audit_logs`'s required-zero set even though `LEGACY_UNMAPPED` is
    // not, and this is what tells the two cases apart.
    without_bridges(&pool, |p| async move {
        sqlx::query("INSERT INTO audit_logs (agent_id, event_type) VALUES ($1, 'orphan')")
            .bind("agent-that-does-not-exist")
            .execute(&p)
            .await
            .expect("seed orphan audit row");
    })
    .await;

    let report = run_tranche_backfill(&pool, TRANCHE, options(10, None))
        .await
        .expect("backfill runs");

    assert_eq!(report.outcome, BackfillOutcome::Blocked, "{report:?}");
    assert!(
        report
            .blocking_reasons
            .iter()
            .any(|r| r.contains("audit_logs")),
        "the orphan must be reported against audit_logs: {:?}",
        report.blocking_reasons
    );
}

// ── Checkpoint lifecycle ─────────────────────────────────────────────────────

#[sqlx::test]
async fn abandoning_a_checkpoint_retires_it_without_completing_it(pool: PgPool) {
    insert_agent(&pool, "agent-a", Some(TENANT)).await;
    without_bridges(&pool, |p| async move {
        for _ in 0..6 {
            sqlx::query("INSERT INTO entities (agent_id, name, entity_type) VALUES ($1, $2, 'x')")
                .bind("agent-a")
                .bind(Uuid::new_v4().to_string())
                .execute(&p)
                .await
                .expect("seed entity");
        }
    })
    .await;

    let paused = run_tranche_backfill(&pool, TRANCHE, options(2, Some(1)))
        .await
        .expect("bounded run");
    assert_eq!(paused.outcome, BackfillOutcome::Paused);

    let abandoned = abandon_tranche_backfill(&pool, TRANCHE, "superseded by a corrected plan")
        .await
        .expect("abandon succeeds")
        .expect("there was an open checkpoint");

    assert_eq!(abandoned.id, paused.checkpoint_id);
    assert_eq!(abandoned.status, CheckpointStatus::Abandoned.as_str());
    assert!(
        abandoned.resume_cursor.is_some(),
        "how far an abandoned run got is the interesting part of its history"
    );

    let completed_at: Option<chrono::DateTime<chrono::Utc>> =
        checkpoint_column(&pool, abandoned.id, "completed_at").await;
    assert_eq!(
        completed_at, None,
        "an ABANDONED row must never carry a completion time"
    );

    // A second abandon has nothing open to retire.
    let again = abandon_tranche_backfill(&pool, TRANCHE, "again")
        .await
        .expect("abandon is safe to repeat");
    assert!(again.is_none());

    // And a fresh run opens a new checkpoint rather than resuming the abandoned
    // one.
    let fresh = run_tranche_backfill(&pool, TRANCHE, options(10, None))
        .await
        .expect("fresh run");
    assert_ne!(fresh.checkpoint_id, abandoned.id);
    assert_eq!(fresh.outcome, BackfillOutcome::Completed, "{fresh:?}");
}

#[sqlx::test]
async fn abandoning_requires_a_reason(pool: PgPool) {
    let err = abandon_tranche_backfill(&pool, TRANCHE, "   ")
        .await
        .expect_err("an unexplained abandonment is refused");
    assert!(err.to_string().contains("reason"), "{err}");
}

#[sqlx::test]
async fn a_run_whose_checkpoint_is_retired_mid_flight_stops_rather_than_writing_on(pool: PgPool) {
    // The advisory lock makes this hard to reach through the engine's own API,
    // which is exactly why it is worth asserting: a run that kept batching
    // against a retired checkpoint would be writing progress nothing will ever
    // read, and the failure would be silent. Reached here by retiring the row
    // out from under a paused run and resuming.
    insert_agent(&pool, "agent-a", Some(TENANT)).await;
    without_bridges(&pool, |p| async move {
        for _ in 0..8 {
            sqlx::query("INSERT INTO entities (agent_id, name, entity_type) VALUES ($1, $2, 'x')")
                .bind("agent-a")
                .bind(Uuid::new_v4().to_string())
                .execute(&p)
                .await
                .expect("seed entity");
        }
    })
    .await;

    let paused = run_tranche_backfill(&pool, TRANCHE, options(2, Some(1)))
        .await
        .expect("bounded run");
    assert_eq!(paused.outcome, BackfillOutcome::Paused);

    // Retire it directly, simulating the row changing status between the resume
    // read and the next batch's write.
    sqlx::query("UPDATE tenancy_backfill_checkpoints SET status = 'ABANDONED' WHERE id = $1")
        .bind(paused.checkpoint_id)
        .execute(&pool)
        .await
        .expect("retire the checkpoint");

    // With no open checkpoint the next run opens a fresh one and finishes the
    // remaining rows, rather than continuing to write against the retired row.
    let fresh = run_tranche_backfill(&pool, TRANCHE, options(2, None))
        .await
        .expect("fresh run");
    assert_ne!(
        fresh.checkpoint_id, paused.checkpoint_id,
        "a retired checkpoint must not be resumed"
    );
    assert_eq!(fresh.outcome, BackfillOutcome::Completed, "{fresh:?}");
    assert_eq!(unowned_count(&pool, "entities").await, 0);

    // The retired row keeps its own accounting and never acquires a completion.
    let retired_status: String = checkpoint_column(&pool, paused.checkpoint_id, "status").await;
    assert_eq!(retired_status, CheckpointStatus::Abandoned.as_str());
    let retired_completed_at: Option<chrono::DateTime<chrono::Utc>> =
        checkpoint_column(&pool, paused.checkpoint_id, "completed_at").await;
    assert_eq!(retired_completed_at, None);
}

#[sqlx::test]
async fn a_checkpoint_raised_against_a_superseded_digest_is_refused(pool: PgPool) {
    insert_agent(&pool, "agent-a", Some(TENANT)).await;
    without_bridges(&pool, |p| async move {
        seed_row_everywhere(&p, "agent-a").await;
    })
    .await;

    // An open checkpoint from a plan that has since moved.
    sqlx::query(
        "INSERT INTO tenancy_backfill_checkpoints \
             (tranche, contract_digest, status, resume_cursor, rows_total, rows_backfilled) \
         VALUES ($1, $2, 'IN_PROGRESS', 'entities|', 5, 0)",
    )
    .bind(TRANCHE.as_str())
    .bind("sha256:0000000000000000000000000000000000000000000000000000000000000000")
    .execute(&pool)
    .await
    .expect("seed stale checkpoint");

    let err = run_tranche_backfill(&pool, TRANCHE, options(10, None))
        .await
        .expect_err("a superseded checkpoint must be refused");
    let message = err.to_string();
    assert!(
        message.contains("digest") && message.contains("Abandon"),
        "the refusal must explain the digest mismatch and the way out: {message}"
    );

    // Nothing was written under the stale checkpoint.
    assert_eq!(unowned_count(&pool, "entities").await, 1);
}

#[sqlx::test]
async fn two_open_checkpoints_are_an_ambiguity_rather_than_a_race_to_resume(pool: PgPool) {
    let digest = report::inventory_digest();
    for _ in 0..2 {
        sqlx::query(
            "INSERT INTO tenancy_backfill_checkpoints \
                 (tranche, contract_digest, status, rows_total, rows_backfilled) \
             VALUES ($1, $2, 'IN_PROGRESS', 0, 0)",
        )
        .bind(TRANCHE.as_str())
        .bind(&digest)
        .execute(&pool)
        .await
        .expect("seed checkpoint");
    }

    let err = run_tranche_backfill(&pool, TRANCHE, options(10, None))
        .await
        .expect_err("two open checkpoints must be refused");
    assert!(
        err.to_string().contains("open checkpoints"),
        "the refusal must say what is ambiguous: {err}"
    );
}

#[sqlx::test]
async fn a_corrupt_resume_cursor_fails_before_any_batch_runs(pool: PgPool) {
    insert_agent(&pool, "agent-a", Some(TENANT)).await;
    without_bridges(&pool, |p| async move {
        seed_row_everywhere(&p, "agent-a").await;
    })
    .await;

    sqlx::query(
        "INSERT INTO tenancy_backfill_checkpoints \
             (tranche, contract_digest, status, resume_cursor, rows_total, rows_backfilled) \
         VALUES ($1, $2, 'IN_PROGRESS', 'sessions|', 5, 0)",
    )
    .bind(TRANCHE.as_str())
    .bind(report::inventory_digest())
    .execute(&pool)
    .await
    .expect("seed checkpoint with a foreign cursor");

    let err = run_tranche_backfill(&pool, TRANCHE, options(10, None))
        .await
        .expect_err("a cursor naming another tranche's table must be refused");
    assert!(err.to_string().contains("sessions"), "{err}");

    // Refused before doing anything, rather than after a partial walk.
    assert_eq!(unowned_count(&pool, "entities").await, 1);
}

#[sqlx::test]
async fn status_reports_the_completion_that_finalize_will_read(pool: PgPool) {
    insert_agent(&pool, "agent-a", Some(TENANT)).await;
    without_bridges(&pool, |p| async move {
        seed_row_everywhere(&p, "agent-a").await;
    })
    .await;

    assert!(
        tranche_backfill_status(&pool, TRANCHE)
            .await
            .expect("status")
            .is_none(),
        "no checkpoint exists before the first run"
    );

    let report = run_tranche_backfill(&pool, TRANCHE, options(10, None))
        .await
        .expect("backfill runs");

    let status = tranche_backfill_status(&pool, TRANCHE)
        .await
        .expect("status")
        .expect("a checkpoint exists now");
    assert_eq!(status.id, report.checkpoint_id);
    assert_eq!(status.status, CheckpointStatus::Completed.as_str());
    assert_eq!(status.contract_digest, report::inventory_digest());
    assert_eq!(status.blocking_count, 0);
    assert_eq!(status.rows_backfilled, status.rows_total);
}

// ── Idempotency and the bridge's authority ───────────────────────────────────

#[sqlx::test]
async fn a_settled_row_is_never_revisited(pool: PgPool) {
    insert_agent(&pool, "agent-a", Some(TENANT)).await;
    without_bridges(&pool, |p| async move {
        for _ in 0..4 {
            sqlx::query("INSERT INTO entities (agent_id, name, entity_type) VALUES ($1, $2, 'x')")
                .bind("agent-a")
                .bind(Uuid::new_v4().to_string())
                .execute(&p)
                .await
                .expect("seed entity");
        }
    })
    .await;

    let first = run_tranche_backfill(&pool, TRANCHE, options(2, Some(1)))
        .await
        .expect("bounded run");
    assert_eq!(first.rows_settled_this_run, 2);

    let second = run_tranche_backfill(&pool, TRANCHE, options(2, None))
        .await
        .expect("resumed run");
    assert_eq!(
        second.rows_settled_this_run, 2,
        "the two rows the first run settled must not be counted again; the scan is restricted \
         to unsettled rows, which is what makes a re-run cost only what still needs doing"
    );
    assert_eq!(second.outcome, BackfillOutcome::Completed);
}

#[sqlx::test]
async fn the_backfill_cannot_write_a_tenant_that_disagrees_with_agents(pool: PgPool) {
    // The bridge is a BEFORE UPDATE trigger, so it re-resolves ownership on
    // every row the backfill touches. A row seeded with a hostile tenant is
    // therefore corrected rather than preserved — the backfill's own SQL is not
    // the only thing standing between a forged tenant and a completion.
    insert_agent(&pool, "agent-a", Some(TENANT)).await;
    let hostile = "33333333-3333-3333-3333-333333333333";
    without_bridges(&pool, |p| async move {
        sqlx::query(
            "INSERT INTO entities (agent_id, name, entity_type, tenant_id) \
             VALUES ($1, 'e', 'person', $2::uuid)",
        )
        .bind("agent-a")
        .bind(hostile)
        .execute(&p)
        .await
        .expect("seed forged row");
    })
    .await;

    let report = run_tranche_backfill(&pool, TRANCHE, options(10, None))
        .await
        .expect("backfill runs");
    assert_eq!(report.outcome, BackfillOutcome::Completed, "{report:?}");

    let tenant: Uuid = sqlx::query("SELECT tenant_id FROM entities LIMIT 1")
        .fetch_one(&pool)
        .await
        .unwrap()
        .try_get("tenant_id")
        .unwrap();
    assert_eq!(
        tenant.to_string(),
        TENANT,
        "the forged tenant must be overwritten with what `agents` says"
    );
}

#[sqlx::test]
async fn rows_written_through_the_bridges_need_no_backfill(pool: PgPool) {
    // The complement of every other test here: a row inserted normally is
    // already owned, so the backfill finds nothing to do and still completes.
    insert_agent(&pool, "agent-a", Some(TENANT)).await;
    seed_row_everywhere(&pool, "agent-a").await;

    for target in targets_for(TRANCHE).unwrap() {
        assert_eq!(
            unowned_count(&pool, target.table).await,
            0,
            "{} should already be owned by its bridge",
            target.table
        );
    }

    let report = run_tranche_backfill(&pool, TRANCHE, options(10, None))
        .await
        .expect("backfill runs");
    assert_eq!(report.outcome, BackfillOutcome::Completed, "{report:?}");
    assert_eq!(
        report.rows_settled_this_run, 0,
        "there was nothing to settle"
    );
    assert_eq!(report.batches_executed, 0);
    assert_eq!(report.rows_backfilled, report.rows_total);
}

#[sqlx::test]
async fn an_empty_database_completes_the_tranche(pool: PgPool) {
    let report = run_tranche_backfill(&pool, TRANCHE, options(10, None))
        .await
        .expect("backfill runs");
    assert_eq!(report.outcome, BackfillOutcome::Completed, "{report:?}");
    assert_eq!(report.rows_total, 0);
    assert_eq!(report.rows_backfilled, 0);
    assert!(report.is_finalizable());
}

// ── Atomic final reconciliation ──────────────────────────────────────────────
//
// `finish` used to reconcile across three separately-committed snapshots: the
// counts, then the audit, then the checkpoint write. Each gap was a window in
// which a row could be inserted that the completion would never see, and the
// bridge writes such a row with NULL ownership rather than refusing it -- so a
// checkpoint claiming a clean tranche could be persisted over a table holding a
// NULL, and FINALIZE would then validate a NOT NULL constraint against it.
//
// The tests below drive that window deliberately rather than hoping to hit it.

/// Wait until PostgreSQL shows an ungranted `ShareLock` request on `table`.
///
/// This is what makes the interleaving tests deterministic instead of timing
/// races: it is direct evidence that the run has reached `finish`, taken the
/// reconciliation lock's place in the queue, and is blocked there -- so anything
/// committed after this returns is committed strictly inside the window.
async fn await_blocked_share_lock(pool: &PgPool, table: &str) {
    // ~10s: generous for a loaded CI runner to batch a handful of rows and
    // reach `finish`, short enough that a regression fails promptly.
    for _ in 0..400 {
        let waiting: i64 = sqlx::query_scalar(
            "SELECT count(*)::bigint \
               FROM pg_locks l \
               JOIN pg_class c ON c.oid = l.relation \
              WHERE c.relname = $1 AND l.mode = 'ShareLock' AND NOT l.granted",
        )
        .bind(table)
        .fetch_one(pool)
        .await
        .expect("inspect pg_locks");
        if waiting > 0 {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    panic!(
        "no session ever blocked waiting for a SHARE lock on {table}; the final reconciliation \
         is not excluding concurrent writes"
    );
}

#[sqlx::test]
async fn a_blocker_committed_before_reconciliation_prevents_completion(pool: PgPool) {
    insert_agent(&pool, "agent-a", Some(TENANT)).await;
    without_bridges(&pool, |p| async move {
        for _ in 0..4 {
            sqlx::query("INSERT INTO entities (agent_id, name, entity_type) VALUES ($1, $2, 'x')")
                .bind("agent-a")
                .bind(Uuid::new_v4().to_string())
                .execute(&p)
                .await
                .expect("seed entity");
        }
    })
    .await;

    // Stop with work left, so the completing run's reconciliation happens after
    // the blocker below rather than before it.
    let paused = run_tranche_backfill(&pool, TRANCHE, options(2, Some(1)))
        .await
        .expect("bounded run");
    assert_eq!(paused.outcome, BackfillOutcome::Paused);

    // A row the bridge itself cannot own: the agent does not exist, so ownership
    // is written NULL and the row is an ORPHANED_AGENT_REFERENCE.
    sqlx::query("INSERT INTO entities (agent_id, name, entity_type) VALUES ($1, 'blocker', 'x')")
        .bind("agent-that-does-not-exist")
        .execute(&pool)
        .await
        .expect("commit the blocker");

    let done = run_tranche_backfill(&pool, TRANCHE, options(10, None))
        .await
        .expect("resumed run");

    assert_eq!(
        done.outcome,
        BackfillOutcome::Blocked,
        "a blocker committed before reconciliation must be seen by it: {done:?}"
    );
    assert!(!done.is_finalizable());
    assert!(
        done.blocking_reasons.iter().any(|r| r.contains("entities")),
        "{:?}",
        done.blocking_reasons
    );

    let completed: i64 = sqlx::query_scalar(
        "SELECT count(*)::bigint FROM tenancy_backfill_checkpoints WHERE status = 'COMPLETED'",
    )
    .fetch_one(&pool)
    .await
    .expect("count completions");
    assert_eq!(
        completed, 0,
        "no completion may exist over a blocked tranche"
    );
}

#[sqlx::test]
async fn a_write_cannot_slip_between_the_audit_and_the_checkpoint(pool: PgPool) {
    insert_agent(&pool, "agent-a", Some(TENANT)).await;
    without_bridges(&pool, |p| async move {
        sqlx::query("INSERT INTO entities (agent_id, name, entity_type) VALUES ($1, 'e', 'x')")
            .bind("agent-a")
            .execute(&p)
            .await
            .expect("seed entity");
    })
    .await;

    // Connection B holds an uncommitted INSERT, so it owns ROW EXCLUSIVE on
    // `entities`. The backfill's batches do not conflict with that -- they touch
    // different rows -- but `finish`'s SHARE lock does, so the run proceeds to
    // reconciliation and stops there.
    let mut blocker = pool.acquire().await.expect("second connection");
    sqlx::query("BEGIN").execute(&mut *blocker).await.unwrap();
    sqlx::query("INSERT INTO entities (agent_id, name, entity_type) VALUES ($1, 'late', 'x')")
        .bind("agent-that-does-not-exist")
        .execute(&mut *blocker)
        .await
        .expect("uncommitted insert");

    let runner = pool.clone();
    let run =
        tokio::spawn(
            async move { run_tranche_backfill(&runner, TRANCHE, options(10, None)).await },
        );

    // Deterministic: the run is now inside `finish`, waiting for the lock.
    await_blocked_share_lock(&pool, "entities").await;
    assert!(
        !run.is_finished(),
        "the run must be blocked, not past the lock"
    );

    // Commit strictly inside the window the old code left open.
    sqlx::query("COMMIT").execute(&mut *blocker).await.unwrap();
    drop(blocker);

    let done = run
        .await
        .expect("run task joins")
        .expect("run returns a report");

    assert_eq!(
        done.outcome,
        BackfillOutcome::Blocked,
        "the row committed during the window must be inside the verdict, not after it: {done:?}"
    );
    assert_eq!(
        done.rows_total, 2,
        "the late row must be counted, not missed by a snapshot taken before it"
    );
    assert!(!done.is_finalizable());

    let completed: i64 = sqlx::query_scalar(
        "SELECT count(*)::bigint FROM tenancy_backfill_checkpoints WHERE status = 'COMPLETED'",
    )
    .fetch_one(&pool)
    .await
    .expect("count completions");
    assert_eq!(completed, 0);
}

#[sqlx::test]
async fn no_completion_is_reported_when_the_guarded_update_writes_nothing(pool: PgPool) {
    insert_agent(&pool, "agent-a", Some(TENANT)).await;
    without_bridges(&pool, |p| async move {
        sqlx::query("INSERT INTO entities (agent_id, name, entity_type) VALUES ($1, 'e', 'x')")
            .bind("agent-a")
            .execute(&p)
            .await
            .expect("seed entity");
    })
    .await;

    // Same mechanism as above, used to open a window in the *checkpoint* rather
    // than in the data: hold the run at the reconciliation lock, retire its
    // checkpoint underneath it, then let it through.
    let mut blocker = pool.acquire().await.expect("second connection");
    sqlx::query("BEGIN").execute(&mut *blocker).await.unwrap();
    sqlx::query("INSERT INTO entities (agent_id, name, entity_type) VALUES ($1, 'x', 'x')")
        .bind("agent-a")
        .execute(&mut *blocker)
        .await
        .expect("uncommitted insert");

    let runner = pool.clone();
    let run =
        tokio::spawn(
            async move { run_tranche_backfill(&runner, TRANCHE, options(10, None)).await },
        );

    await_blocked_share_lock(&pool, "entities").await;

    // The checkpoint table is deliberately not locked, so this succeeds.
    let retired = sqlx::query(
        "UPDATE tenancy_backfill_checkpoints SET status = 'ABANDONED' WHERE status = 'IN_PROGRESS'",
    )
    .execute(&pool)
    .await
    .expect("retire the checkpoint");
    assert_eq!(retired.rows_affected(), 1, "there was one open checkpoint");

    sqlx::query("COMMIT").execute(&mut *blocker).await.unwrap();
    drop(blocker);

    let err = run
        .await
        .expect("run task joins")
        .expect_err("a completion that wrote no row must not be reported");
    let message = err.to_string();
    assert!(
        message.contains("no longer IN_PROGRESS") && message.contains("0 rows"),
        "the refusal must say the guarded update matched nothing: {message}"
    );

    // And nothing was committed: the retired checkpoint is untouched.
    let completed: i64 = sqlx::query_scalar(
        "SELECT count(*)::bigint FROM tenancy_backfill_checkpoints \
          WHERE status = 'COMPLETED' OR completed_at IS NOT NULL",
    )
    .fetch_one(&pool)
    .await
    .expect("count completions");
    assert_eq!(
        completed, 0,
        "a rolled-back finish must leave no completion"
    );
}

// ── Pool sizing ──────────────────────────────────────────────────────────────

#[sqlx::test]
async fn a_single_connection_pool_is_refused_by_the_engine(pool: PgPool) {
    // The CLI checks this too, but the engine is where it has to hold: a future
    // management endpoint is a second caller, and a check that lives only in the
    // command handler is one the endpoint would have to remember to repeat.
    // Without it the run acquires the pool's only connection for the advisory
    // lock and then deadlocks against itself until the acquire timeout expires.
    let connect_options = (*pool.connect_options()).clone();
    let single = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(std::time::Duration::from_secs(2))
        .connect_with(connect_options)
        .await
        .expect("one-connection pool");

    let started = std::time::Instant::now();
    let err = run_tranche_backfill(&single, TRANCHE, options(10, None))
        .await
        .expect_err("a pool of one cannot run a backfill");
    let message = err.to_string();

    assert!(
        message.contains("DB_MAX_CONNECTIONS") && message.contains("advisory lock"),
        "the refusal must name the variable and the reason: {message}"
    );
    assert!(
        started.elapsed() < std::time::Duration::from_secs(2),
        "the refusal must be immediate, not an acquire timeout"
    );
    const { assert!(MIN_BACKFILL_CONNECTIONS >= 2) };

    single.close().await;
}

// ── Advisory-lock cleanup ────────────────────────────────────────────────────

/// Whether *this database* holds the tranche backfill advisory lock.
///
/// The `database` predicate is load-bearing, not tidiness. An advisory lock's
/// key space is per-database, so two databases in one cluster can hold the same
/// id at once -- and `#[sqlx::test]` gives every test its own database inside a
/// shared cluster. Without the filter this reads every concurrent test's lock as
/// its own, which is how the first version of these tests both failed spuriously
/// and, worse, terminated other tests' backends.
async fn tranche_lock_is_held(pool: &PgPool) -> bool {
    sqlx::query_scalar::<_, i64>(
        "SELECT count(*)::bigint FROM pg_locks \
          WHERE locktype = 'advisory' AND granted \
            AND database = (SELECT oid FROM pg_database WHERE datname = current_database()) \
            AND ((classid::bigint << 32) | objid::bigint) = $1",
    )
    .bind(TRANCHE_LOCK_ID)
    .fetch_one(pool)
    .await
    .expect("inspect advisory locks")
        > 0
}

#[sqlx::test]
async fn a_completed_run_leaves_no_advisory_lock_behind(pool: PgPool) {
    insert_agent(&pool, "agent-a", Some(TENANT)).await;
    without_bridges(&pool, |p| async move {
        seed_row_everywhere(&p, "agent-a").await;
    })
    .await;

    assert!(!tranche_lock_is_held(&pool).await, "nothing holds it yet");

    let done = run_tranche_backfill(&pool, TRANCHE, options(10, None))
        .await
        .expect("backfill runs");
    assert_eq!(done.outcome, BackfillOutcome::Completed);

    assert!(
        !tranche_lock_is_held(&pool).await,
        "the session lock must be released, not carried back into the pool on a recycled \
         connection where nothing can ever release it"
    );

    // The observable consequence of a leak: acquire refuses. Proving it still
    // works is what says the release was real rather than merely attempted.
    let again = run_tranche_backfill(&pool, TRANCHE, options(10, None))
        .await
        .expect("a later run can still take the lock");
    assert_eq!(again.outcome, BackfillOutcome::AlreadyCompleted);
}

#[sqlx::test]
async fn an_unlock_that_cannot_be_confirmed_discards_the_connection(pool: PgPool) {
    // Induces a real unlock failure rather than asserting about one: the
    // lock-holding backend is terminated, so `pg_advisory_unlock` cannot run on
    // it. The old lifecycle set `released = true` before awaiting that query, so
    // this path returned a connection to the pool marked clean; the rewritten
    // one has to report that it could not confirm the unlock and take the
    // connection out of circulation instead.
    let lock = TrancheLock::acquire(&pool).await.expect("acquire the lock");
    assert!(tranche_lock_is_held(&pool).await, "the lock is held");

    // Scoped to this test's own database for the reason `tranche_lock_is_held`
    // documents: an unscoped terminate would kill the backends of every other
    // test running concurrently in the same cluster.
    let killed: i64 = sqlx::query_scalar(
        "SELECT count(*)::bigint FROM (\
             SELECT pg_terminate_backend(l.pid) \
               FROM pg_locks l \
              WHERE l.locktype = 'advisory' AND l.granted \
                AND l.database = (SELECT oid FROM pg_database WHERE datname = current_database()) \
                AND ((l.classid::bigint << 32) | l.objid::bigint) = $1 \
                AND l.pid <> pg_backend_pid()\
         ) t",
    )
    .bind(TRANCHE_LOCK_ID)
    .fetch_one(&pool)
    .await
    .expect("terminate the lock holder");
    assert_eq!(
        killed, 1,
        "exactly one backend in this database held the lock"
    );

    let cleanup = lock.release().await;
    assert!(
        !matches!(cleanup, LockCleanup::Unlocked),
        "an unlock issued on a terminated backend must not be reported as confirmed: {cleanup:?}"
    );
    assert!(
        cleanup.is_guaranteed(),
        "closing the connection still proves the lock is gone: {cleanup:?}"
    );

    // The terminated session took the lock with it, and the pool is usable.
    assert!(!tranche_lock_is_held(&pool).await);
    insert_agent(&pool, "agent-a", Some(TENANT)).await;
    let done = run_tranche_backfill(&pool, TRANCHE, options(10, None))
        .await
        .expect("the pool is not poisoned");
    assert_eq!(done.outcome, BackfillOutcome::Completed);
}

#[sqlx::test]
async fn a_second_run_is_refused_while_the_first_holds_the_lock(pool: PgPool) {
    let lock = TrancheLock::acquire(&pool).await.expect("acquire");

    let err = run_tranche_backfill(&pool, TRANCHE, options(10, None))
        .await
        .expect_err("the lock is held");
    assert!(
        err.to_string().contains("already running"),
        "the refusal must say why: {err}"
    );

    assert!(matches!(lock.release().await, LockCleanup::Unlocked));
    assert!(!tranche_lock_is_held(&pool).await);
}

#[sqlx::test]
async fn the_lock_id_these_tests_watch_is_the_one_the_engine_takes(pool: PgPool) {
    // `TRANCHE_LOCK_ID` is a copy of a private constant. A copy that drifts
    // would make every lock assertion above vacuously true -- watching an id
    // nothing ever takes reports "no lock held" forever. Rather than widen the
    // engine's constant to `pub(super)` for this, hold the real lock and check
    // that the id these tests watch is the one that lights up.
    let lock = TrancheLock::acquire(&pool).await.expect("acquire");
    assert!(
        tranche_lock_is_held(&pool).await,
        "the engine's lock id and TRANCHE_LOCK_ID have drifted apart"
    );
    assert!(matches!(lock.release().await, LockCleanup::Unlocked));
}
