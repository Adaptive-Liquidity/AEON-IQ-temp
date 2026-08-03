//! Tranche 1 PREPARE behaviour, measured against a live database.
//!
//! Split from `audit_db_tests.rs` for the same reason that file was split from
//! `audit.rs`: the no-database CI job skips DB-backed modules and the
//! integration job runs `cargo test` unfiltered. That skip list is an explicit
//! enumeration in `.github/workflows/ci.yml`, not a wildcard — adding a module
//! here without adding it there turns the no-database job red, which is exactly
//! what happened when this file was introduced.
//!
//! These tests exist because tranche 1 installs five `BEFORE INSERT OR UPDATE`
//! triggers that are live from the moment the migration applies. Everything
//! else in this PR is inert until a backfill runs; the bridges are not. A
//! passing suite that never writes through them would say nothing about them.
//!
//! The bridges are five independently written plpgsql bodies, not one shared
//! function, so every behavioural test here runs against all five tables rather
//! than one representative. A typo confined to `fn_memory_graph_tenancy_bridge`
//! is exactly the defect a single-table test would miss.

use sqlx::{PgPool, Row};
use uuid::Uuid;

const TENANT: &str = "11111111-1111-1111-1111-111111111111";
const OTHER_TENANT: &str = "22222222-2222-2222-2222-222222222222";

/// Every table tranche 1 attaches an agent bridge to, with an INSERT that
/// supplies only the legacy `agent_id` and whatever else the table requires.
///
/// `audit_logs` is deliberately absent: its bridge is conditional and gets its
/// own four-state test below.
const AGENT_BRIDGE_TABLES: &[(&str, &str)] = &[
    (
        "archival_batches",
        "INSERT INTO archival_batches (agent_id, source_count, l3_count) VALUES ($1, 1, 1)",
    ),
    (
        "entities",
        "INSERT INTO entities (agent_id, name, entity_type) VALUES ($1, 'e', 'person')",
    ),
    (
        "memory_graph",
        "INSERT INTO memory_graph (agent_id, subject, predicate, object) \
         VALUES ($1, 's', 'p', 'o')",
    ),
    (
        "rmk_policies",
        "INSERT INTO rmk_policies (agent_id, pressure_a, pressure_b, kp, ki, \
         graph_bonus_weight, retrieval_threshold) VALUES ($1, 0, 0, 0, 0, 0, 0)",
    ),
];

async fn insert_agent(pool: &PgPool, agent_id: &str, tenant: Option<&str>) {
    match tenant {
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
    }
    .execute(pool)
    .await
    .unwrap();
}

/// The one ownership pair on a table, as `(agent_uuid, tenant_id)`.
async fn ownership(pool: &PgPool, table: &str) -> (Option<Uuid>, Option<Uuid>) {
    let row = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT agent_uuid, tenant_id FROM {table} ORDER BY 1 NULLS LAST LIMIT 1"
    )))
    .fetch_one(pool)
    .await
    .unwrap();
    (row.get("agent_uuid"), row.get("tenant_id"))
}

async fn agent_uuid_of(pool: &PgPool, agent_id: &str) -> Uuid {
    sqlx::query_scalar("SELECT id FROM agents WHERE agent_id = $1")
        .bind(agent_id)
        .fetch_one(pool)
        .await
        .unwrap()
}

// ── The bridge resolves ownership a legacy writer never supplies ────────────

/// A writer that knows nothing about tenancy produces a fully owned row.
///
/// Asserted by value against `agents`, not merely non-NULL: a bridge that wrote
/// a random UUID would satisfy an `IS NOT NULL` check and be wrong.
#[sqlx::test(migrations = "./migrations")]
async fn every_agent_bridge_resolves_ownership_from_the_legacy_agent_id(pool: PgPool) {
    insert_agent(&pool, "mapped", Some(TENANT)).await;
    let expected_uuid = agent_uuid_of(&pool, "mapped").await;
    let expected_tenant = Uuid::parse_str(TENANT).unwrap();

    for (table, insert) in AGENT_BRIDGE_TABLES {
        sqlx::query(*insert)
            .bind("mapped")
            .execute(&pool)
            .await
            .unwrap_or_else(|e| panic!("{table}: legacy-shaped insert must still succeed: {e}"));

        let (uuid, tenant) = ownership(&pool, table).await;
        assert_eq!(
            uuid,
            Some(expected_uuid),
            "{table}: agent_uuid must equal agents.id for this agent"
        );
        assert_eq!(
            tenant,
            Some(expected_tenant),
            "{table}: tenant_id must equal agents.tenant_id for this agent"
        );
    }
}

/// The trigger is `BEFORE INSERT OR UPDATE`, and the UPDATE half is a distinct
/// path: the value the bridge compares against is the row's own prior
/// resolution rather than anything a caller supplied.
#[sqlx::test(migrations = "./migrations")]
async fn every_agent_bridge_also_fires_on_update(pool: PgPool) {
    insert_agent(&pool, "mapped", Some(TENANT)).await;
    let expected_tenant = Uuid::parse_str(TENANT).unwrap();

    for (table, insert) in AGENT_BRIDGE_TABLES {
        sqlx::query(*insert)
            .bind("mapped")
            .execute(&pool)
            .await
            .unwrap();
        // Blank the ownership behind the trigger's back, then touch the row.
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "UPDATE {table} SET agent_uuid = NULL, tenant_id = NULL"
        )))
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "UPDATE {table} SET agent_id = agent_id"
        )))
        .execute(&pool)
        .await
        .unwrap();

        let (_, tenant) = ownership(&pool, table).await;
        assert_eq!(
            tenant,
            Some(expected_tenant),
            "{table}: UPDATE must re-resolve ownership, not leave it NULL"
        );
    }
}

// ── The bridge refuses to be told who owns a row ────────────────────────────

/// Regression guard for a measured cross-tenant injection.
///
/// Before the fix the bridges only *filled* NULL ownership. Naming an agent
/// that does not exist and supplying any `tenant_id` got the row accepted
/// carrying that tenant: neither contradiction check can fire when the resolved
/// side is NULL, and the composite foreign key is skipped entirely because
/// `MATCH SIMPLE` ignores a key with a NULL component. The row then appeared in
/// that tenant's scans. Only `archival_batches` was protected, incidentally, by
/// an `agent_id` foreign key from migration 0006.
#[sqlx::test(migrations = "./migrations")]
async fn a_nonexistent_agent_cannot_tag_a_row_with_someone_elses_tenant(pool: PgPool) {
    insert_agent(&pool, "victim", Some(TENANT)).await;

    for (table, _) in AGENT_BRIDGE_TABLES {
        // archival_batches has its own agent_id FK and rejects outright, which
        // is a correct-but-different defence; the rest must be defended by the
        // bridge itself.
        if *table == "archival_batches" {
            continue;
        }
        let forged = match *table {
            "entities" => {
                "INSERT INTO entities (agent_id, name, entity_type, tenant_id) \
                 VALUES ('ghost', 'e', 'person', $1::uuid)"
            }
            "memory_graph" => {
                "INSERT INTO memory_graph (agent_id, subject, predicate, object, tenant_id) \
                 VALUES ('ghost', 's', 'p', 'o', $1::uuid)"
            }
            _ => {
                "INSERT INTO rmk_policies (agent_id, pressure_a, pressure_b, kp, ki, \
                 graph_bonus_weight, retrieval_threshold, tenant_id) \
                 VALUES ('ghost', 0, 0, 0, 0, 0, 0, $1::uuid)"
            }
        };
        sqlx::query(forged)
            .bind(TENANT)
            .execute(&pool)
            .await
            .unwrap();

        let visible: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT count(*) FROM {table} WHERE tenant_id = $1::uuid"
        )))
        .bind(TENANT)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            visible, 0,
            "{table}: a row naming a nonexistent agent must not be visible to tenant {TENANT}"
        );
    }
}

/// An agent that exists but has no tenant yet is `UNMAPPED_AGENT`. Its rows are
/// written and left unowned for the audit to report — not rejected, and not
/// silently given the tenant a caller asked for.
#[sqlx::test(migrations = "./migrations")]
async fn an_unmapped_agent_leaves_ownership_null_and_ignores_a_supplied_tenant(pool: PgPool) {
    insert_agent(&pool, "unmapped", None).await;

    sqlx::query(
        "INSERT INTO entities (agent_id, name, entity_type, tenant_id) \
         VALUES ('unmapped', 'e', 'person', $1::uuid)",
    )
    .bind(TENANT)
    .execute(&pool)
    .await
    .unwrap();

    let (uuid, tenant) = ownership(&pool, "entities").await;
    assert!(uuid.is_some(), "the agent resolves, so agent_uuid is known");
    assert_eq!(
        tenant, None,
        "an unmapped agent's row must stay unowned rather than take a caller-supplied tenant"
    );
}

/// When the agent *does* resolve and the caller disagrees, the write is refused
/// rather than corrected: silently overwriting would accept a write the caller
/// believed was going somewhere else.
#[sqlx::test(migrations = "./migrations")]
async fn every_agent_bridge_rejects_ownership_that_contradicts_agents(pool: PgPool) {
    insert_agent(&pool, "mapped", Some(TENANT)).await;

    for (table, _) in AGENT_BRIDGE_TABLES {
        let contradictory = match *table {
            "archival_batches" => {
                "INSERT INTO archival_batches (agent_id, source_count, l3_count, tenant_id) \
                 VALUES ('mapped', 1, 1, $1::uuid)"
            }
            "entities" => {
                "INSERT INTO entities (agent_id, name, entity_type, tenant_id) \
                 VALUES ('mapped', 'e', 'person', $1::uuid)"
            }
            "memory_graph" => {
                "INSERT INTO memory_graph (agent_id, subject, predicate, object, tenant_id) \
                 VALUES ('mapped', 's', 'p', 'o', $1::uuid)"
            }
            _ => {
                "INSERT INTO rmk_policies (agent_id, pressure_a, pressure_b, kp, ki, \
                 graph_bonus_weight, retrieval_threshold, tenant_id) \
                 VALUES ('mapped', 0, 0, 0, 0, 0, 0, $1::uuid)"
            }
        };
        let error = sqlx::query(contradictory)
            .bind(OTHER_TENANT)
            .execute(&pool)
            .await
            .unwrap_err();

        // Which refusal fired matters. "some error occurred" would also be
        // satisfied by a NOT NULL violation in the fixture.
        let db = error.as_database_error().expect("a database error");
        assert_eq!(
            db.code().as_deref(),
            Some("23514"),
            "{table}: must be check_violation from the bridge, got {error}"
        );
        assert!(
            db.message().contains("disagrees with agents"),
            "{table}: unexpected message: {}",
            db.message()
        );
        // And the resolved tenant must not appear in the message a caller reads.
        assert!(
            !db.message().contains(TENANT),
            "{table}: the message discloses the resolved tenant: {}",
            db.message()
        );
    }
}

// ── audit_logs: all four conditional states ─────────────────────────────────

/// `audit_logs` is the one table whose ownership stays permanently NULL-able,
/// because agentless events are legitimate. Its bridge therefore has to decide
/// four cases rather than two, and a bridge that declares fewer has left one
/// undefined — which in a `BEFORE` trigger resolves to "write whatever arrived".
///
/// All four are asserted here, including the two that look like no-ops.
#[sqlx::test(migrations = "./migrations")]
async fn the_audit_logs_bridge_decides_all_four_conditional_states(pool: PgPool) {
    insert_agent(&pool, "mapped", Some(TENANT)).await;
    insert_agent(&pool, "unmapped", None).await;
    let mapped_uuid = agent_uuid_of(&pool, "mapped").await;

    // AGENTLESS_ALLOWED — no agent named, nothing wrong, nothing reported.
    sqlx::query("INSERT INTO audit_logs (agent_id, event_type) VALUES (NULL, 'startup')")
        .execute(&pool)
        .await
        .unwrap();
    let row =
        sqlx::query("SELECT agent_uuid, tenant_id FROM audit_logs WHERE event_type='startup'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(row.get::<Option<Uuid>, _>("agent_uuid").is_none());
    assert!(row.get::<Option<Uuid>, _>("tenant_id").is_none());

    // AGENTLESS_ALLOWED, adversarial: ownership supplied on a row that names no
    // agent is dropped rather than trusted. `agents` says nothing about such a
    // row, so accepting it would let a caller write history under any tenant.
    sqlx::query(
        "INSERT INTO audit_logs (agent_id, event_type, tenant_id) \
         VALUES (NULL, 'forged', $1::uuid)",
    )
    .bind(TENANT)
    .execute(&pool)
    .await
    .unwrap();
    let forged: Option<Uuid> =
        sqlx::query_scalar("SELECT tenant_id FROM audit_logs WHERE event_type='forged'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(forged, None, "an agentless row must not carry a tenant");

    // RESOLVED_AND_VERIFIED.
    sqlx::query("INSERT INTO audit_logs (agent_id, event_type) VALUES ('mapped', 'resolved')")
        .execute(&pool)
        .await
        .unwrap();
    let row =
        sqlx::query("SELECT agent_uuid, tenant_id FROM audit_logs WHERE event_type='resolved'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(row.get::<Option<Uuid>, _>("agent_uuid"), Some(mapped_uuid));
    assert_eq!(
        row.get::<Option<Uuid>, _>("tenant_id"),
        Some(Uuid::parse_str(TENANT).unwrap())
    );

    // PRESERVED_UNRESOLVED, both shapes: an agent that does not exist, and an
    // agent that exists but is itself unmapped. The audit reports different
    // codes for these, so conflating them into one case would hide a bug in
    // whichever branch went untested.
    for (agent, event) in [("ghost", "orphaned"), ("unmapped", "unmapped")] {
        sqlx::query("INSERT INTO audit_logs (agent_id, event_type) VALUES ($1, $2)")
            .bind(agent)
            .bind(event)
            .execute(&pool)
            .await
            .unwrap_or_else(|e| panic!("{event}: the row must be preserved, not rejected: {e}"));
        let tenant: Option<Uuid> =
            sqlx::query_scalar("SELECT tenant_id FROM audit_logs WHERE event_type = $1")
                .bind(event)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            tenant, None,
            "{event}: ownership must be left for the audit"
        );
    }

    // CONTRADICTION_REJECTED.
    let error = sqlx::query(
        "INSERT INTO audit_logs (agent_id, event_type, tenant_id) \
         VALUES ('mapped', 'bad', $1::uuid)",
    )
    .bind(OTHER_TENANT)
    .execute(&pool)
    .await
    .unwrap_err();
    assert_eq!(
        error.as_database_error().and_then(|e| e.code()).as_deref(),
        Some("23514"),
        "{error}"
    );
}

// ── A failed concurrent build must not be adopted as a constraint ───────────

/// The guard at the head of 0041 exists because `CREATE INDEX CONCURRENTLY
/// IF NOT EXISTS` silently skips an index that already exists *and is INVALID*.
/// Without the guard, a re-run reports success over a broken object and 0041
/// adopts a unique index that enforces nothing.
///
/// A real concurrent build cannot be made to fail on demand inside a test, so
/// the INVALID state is set directly in the catalog. That is the state a failed
/// build leaves behind.
#[sqlx::test(migrations = "./migrations")]
async fn the_invalid_index_guard_refuses_to_adopt_a_broken_build(pool: PgPool) {
    sqlx::query(
        "UPDATE pg_index SET indisvalid = false \
         WHERE indexrelid = 'public.idx_entities_tenant'::regclass",
    )
    .execute(&pool)
    .await
    .expect("the test role must be able to mark an index invalid");

    let error = sqlx::raw_sql(CONSTRAINTS_UP)
        .execute(&pool)
        .await
        .unwrap_err();
    let db = error.as_database_error().expect("a database error");
    assert_eq!(
        db.code().as_deref(),
        Some("55000"),
        "must be object_not_in_prerequisite_state, got {error}"
    );
    assert!(
        db.message().contains("idx_entities_tenant"),
        "the guard must name the broken index: {}",
        db.message()
    );
}

// ── Rollback ordering refuses on the real dependency ────────────────────────

/// The 0028 rollback drops `agents.tenant_id`, which the five bridge functions
/// read. Guarding only on the five foreign keys was not enough: rolling back
/// 0041 alone drops every one of them while leaving the bridges installed, so a
/// constraint-only guard was satisfied and 0028 would then drop a column five
/// live triggers still select — leaving those five tables unable to accept any
/// write at all, from a rollback that reported success.
#[sqlx::test(migrations = "./migrations")]
async fn the_step_one_rollback_refuses_while_the_bridges_are_installed(pool: PgPool) {
    // Drop every foreign key the old guard checked, and nothing else.
    sqlx::raw_sql(CONSTRAINTS_DOWN_SQL)
        .execute(&pool)
        .await
        .expect("0041 rollback");
    // And clear 0031/0030 too, or their own ordering guard fires first and this
    // test passes for the wrong reason — proving the grants guard works rather
    // than the bridge guard.
    sqlx::raw_sql(GRANTS_HARDENING_DOWN_SQL)
        .execute(&pool)
        .await
        .expect("0031 rollback");
    sqlx::raw_sql(GRANTS_DOWN_SQL)
        .execute(&pool)
        .await
        .expect("0030 rollback");
    let fks: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pg_constraint WHERE contype='f' \
         AND conname LIKE '%tenant_agent_fkey'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(fks, 0, "the constraint-only precondition is now satisfied");

    let mut conn = pool.acquire().await.unwrap();
    let error = sqlx::raw_sql(STEP_ONE_DOWN_SQL)
        .execute(&mut *conn)
        .await
        .unwrap_err();
    sqlx::raw_sql("ROLLBACK").execute(&mut *conn).await.unwrap();
    drop(conn);

    let db = error.as_database_error().expect("a database error");
    assert_eq!(
        db.code().as_deref(),
        Some("2BP01"),
        "must be dependent_objects_still_exist, got {error}"
    );
    assert!(
        db.message().contains("fn_entities_tenancy_bridge"),
        "the refusal must name what still depends on it: {}",
        db.message()
    );
    assert!(
        db.message()
            .contains("0032_tenancy_tranche1_prepare_down.sql"),
        "the refusal must name the script to run first: {}",
        db.message()
    );

    // And nothing was dropped: the refusal is inside the script's transaction.
    let still_there: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM information_schema.columns \
         WHERE table_name='agents' AND column_name='tenant_id')",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(still_there, "agents.tenant_id must survive the refusal");
}

// ── Rollback must retire completion evidence, not just retain it ────────────

const TRANCHE_1: &str = "TRANCHE_1_ROOTS_AND_DIRECT_AGENT_CHILDREN";

async fn insert_completed_checkpoint(pool: &PgPool, digest: &str) {
    sqlx::query(
        "INSERT INTO tenancy_backfill_checkpoints \
           (tranche, contract_digest, status, rows_total, rows_backfilled, \
            blocking_count, completed_at) \
         VALUES ($1, $2, 'COMPLETED', 7, 7, 0, NOW())",
    )
    .bind(TRANCHE_1)
    .bind(digest)
    .execute(pool)
    .await
    .expect("a clean completion must be recordable");
}

/// PREPARE, complete a backfill, roll the tranche back, reapply PREPARE.
///
/// The checkpoint table is deliberately retained across the rollback, which is
/// what made this sequence dangerous: the rollback drops every ownership value
/// the backfill wrote AND deletes migration version 32, so reapplying recreates
/// the columns with historical rows NULL again. A COMPLETED row surviving that
/// describes ownership which no longer exists, and a FINALIZE guard reading it
/// would validate constraints over a table nobody backfilled.
///
/// The rollback therefore transitions those rows to ABANDONED — the schema's
/// own word for "superseded, retained for history, never treated as
/// completion". This proves the authority is gone and the history is not.
#[sqlx::test(migrations = "./migrations")]
async fn a_rollback_retires_completion_evidence_it_invalidates(pool: PgPool) {
    let digest = crate::tenancy::report::inventory_digest();
    insert_completed_checkpoint(&pool, &digest).await;

    // A second completion for THIS tranche at a superseded digest. The partial
    // unique index permits it, and the rollback destroys the ownership both
    // rows describe, so both must be retired. Scoping the transition to the
    // current digest would leave this one authoritative — the original defect,
    // relocated rather than fixed.
    insert_completed_checkpoint(&pool, "sha256:superseded-digest").await;

    // Neighbours that must NOT be touched, so a future edit that widens or
    // drops the WHERE clause fails here rather than silently retiring evidence
    // about ownership this rollback never had any authority over.
    sqlx::query(
        "INSERT INTO tenancy_backfill_checkpoints \
           (tranche, contract_digest, status, rows_total, rows_backfilled, \
            blocking_count, completed_at) \
         VALUES ('TRANCHE_2_SESSIONS', $1, 'COMPLETED', 3, 3, 0, NOW())",
    )
    .bind(&digest)
    .execute(&pool)
    .await
    .expect("another tranche's completion");
    sqlx::query(
        "INSERT INTO tenancy_backfill_checkpoints \
           (tranche, contract_digest, status, rows_total, rows_backfilled, \
            blocking_count, resume_cursor) \
         VALUES ($1, 'sha256:in-flight', 'IN_PROGRESS', 9, 4, 0, 'cursor-42')",
    )
    .bind(TRANCHE_1)
    .execute(&pool)
    .await
    .expect("an in-flight attempt for this tranche");

    // Sanity: before the rollback this row is exactly what FINALIZE looks for.
    let authoritative: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM tenancy_backfill_checkpoints \
         WHERE tranche = $1 AND contract_digest = $2 AND status = 'COMPLETED' \
           AND blocking_count = 0",
    )
    .bind(TRANCHE_1)
    .bind(&digest)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(authoritative, 1, "the precondition must start satisfied");

    for (label, script) in [
        ("0041", CONSTRAINTS_DOWN_SQL),
        ("0033-0040", INDEXES_DOWN_SQL),
        ("0032", PREPARE_DOWN_SQL),
    ] {
        sqlx::raw_sql(script)
            .execute(&pool)
            .await
            .unwrap_or_else(|e| panic!("{label} rollback: {e}"));
    }

    // Reapply PREPARE, exactly as `sqlx migrate run` would after the rollback
    // deleted version 32.
    sqlx::raw_sql(PREPARE_UP_SQL)
        .execute(&pool)
        .await
        .expect("PREPARE must reapply cleanly");

    // The guard's own predicate must now match nothing.
    let still_authoritative: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM tenancy_backfill_checkpoints \
         WHERE tranche = $1 AND contract_digest = $2 AND status = 'COMPLETED' \
           AND blocking_count = 0",
    )
    .bind(TRANCHE_1)
    .bind(&digest)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        still_authoritative, 0,
        "a rolled-back tranche must not leave evidence that can authorize FINALIZE"
    );

    // History survives, with its counts and digest intact.
    let row = sqlx::query(
        "SELECT status, rows_total, rows_backfilled, completed_at \
           FROM tenancy_backfill_checkpoints WHERE tranche = $1 AND contract_digest = $2",
    )
    .bind(TRANCHE_1)
    .bind(&digest)
    .fetch_one(&pool)
    .await
    .expect("the checkpoint row must be retained for diagnosis");
    assert_eq!(row.get::<String, _>("status"), "ABANDONED");
    assert_eq!(row.get::<i64, _>("rows_total"), 7, "counts are diagnostic");
    assert_eq!(row.get::<i64, _>("rows_backfilled"), 7);
    assert!(
        row.get::<Option<chrono::DateTime<chrono::Utc>>, _>("completed_at")
            .is_none(),
        "an ABANDONED row cannot carry a completion time"
    );

    // Every tranche-1 completion is retired, not just the current-digest one.
    let superseded: String = sqlx::query_scalar(
        "SELECT status FROM tenancy_backfill_checkpoints \
         WHERE tranche = $1 AND contract_digest = 'sha256:superseded-digest'",
    )
    .bind(TRANCHE_1)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        superseded, "ABANDONED",
        "a completion at a superseded digest describes the same destroyed ownership and must \
         also be retired"
    );

    // The neighbours are untouched. Scoping is a real decision, so a future
    // edit that widens the WHERE clause has to fail here.
    let other_tranche: String = sqlx::query_scalar(
        "SELECT status FROM tenancy_backfill_checkpoints WHERE tranche = 'TRANCHE_2_SESSIONS'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        other_tranche, "COMPLETED",
        "this rollback destroys no tranche-2 ownership, so retiring its evidence would be a lie"
    );
    let in_flight: (String, Option<String>) = sqlx::query_as(
        "SELECT status, resume_cursor FROM tenancy_backfill_checkpoints \
         WHERE tranche = $1 AND contract_digest = 'sha256:in-flight'",
    )
    .bind(TRANCHE_1)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        in_flight,
        ("IN_PROGRESS".to_string(), Some("cursor-42".to_string())),
        "an in-flight attempt was never authoritative, so it is left exactly as it was"
    );

    // And a fresh backfill can record new completion evidence for the same
    // tranche and digest: the retained row no longer occupies the partial
    // unique index, which it would have if it had stayed COMPLETED.
    insert_completed_checkpoint(&pool, &digest).await;
    let fresh: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM tenancy_backfill_checkpoints \
         WHERE tranche = $1 AND contract_digest = $2 AND status = 'COMPLETED'",
    )
    .bind(TRANCHE_1)
    .bind(&digest)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        fresh, 1,
        "exactly one authoritative completion, the new one"
    );
}

// ── The declared partial predicate is verified, not merely permitted ────────

/// Replace the checkpoint completion index with `definition`, then report what
/// the audit says about `tenancy_backfill_checkpoints`.
async fn contract_status_with_index(pool: &PgPool, definition: &str) -> (String, Vec<String>) {
    sqlx::query("DROP INDEX IF EXISTS public.tenancy_backfill_checkpoints_completed_key")
        .execute(pool)
        .await
        .unwrap();
    sqlx::query(sqlx::AssertSqlSafe(definition.to_owned()))
        .execute(pool)
        .await
        .unwrap();

    let report = crate::tenancy::audit::run(pool, None)
        .await
        .expect("audit runs");
    let status = report
        .classified_tables
        .iter()
        .find(|t| t.table == "tenancy_backfill_checkpoints")
        .and_then(|t| t.contract_status)
        .map(|s| s.as_str().to_string())
        .unwrap_or_else(|| "MISSING".to_string());
    let drift: Vec<String> = report
        .findings
        .iter()
        .filter(|f| f.table_name == "tenancy_backfill_checkpoints")
        .map(|f| f.diagnostic.clone())
        .collect();
    (status, drift)
}

/// `require_non_partial: false` only ever *permitted* a partial index. It could
/// not require one, and said nothing about which rows the predicate selects.
/// Both gaps admitted a schema the audit called satisfied:
///
///   * a total unique index forbids the retry the partial scope exists to
///     allow, because an ABANDONED attempt would collide with a later one;
///   * a drifted predicate can admit two rows a FINALIZE guard would both read
///     as authoritative.
///
/// Each shape below is rejected, and the correct one still passes.
#[sqlx::test(migrations = "./migrations")]
async fn the_checkpoint_completion_index_predicate_is_verified_exactly(pool: PgPool) {
    let wrong = [
        (
            "total unique index",
            "CREATE UNIQUE INDEX tenancy_backfill_checkpoints_completed_key \
             ON tenancy_backfill_checkpoints (tranche, contract_digest)",
        ),
        (
            "wrong predicate value",
            "CREATE UNIQUE INDEX tenancy_backfill_checkpoints_completed_key \
             ON tenancy_backfill_checkpoints (tranche, contract_digest) \
             WHERE status = 'ABANDONED'",
        ),
        (
            "negated predicate",
            "CREATE UNIQUE INDEX tenancy_backfill_checkpoints_completed_key \
             ON tenancy_backfill_checkpoints (tranche, contract_digest) \
             WHERE status <> 'COMPLETED'",
        ),
        (
            "logically unrelated predicate",
            "CREATE UNIQUE INDEX tenancy_backfill_checkpoints_completed_key \
             ON tenancy_backfill_checkpoints (tranche, contract_digest) \
             WHERE rows_total > 0",
        ),
    ];

    for (label, definition) in wrong {
        let (status, drift) = contract_status_with_index(&pool, definition).await;
        assert_eq!(
            status, "DRIFTED",
            "{label}: the audit must reject this index, drift was {drift:?}"
        );
        assert!(
            drift.iter().any(|d| d.contains("partial on")),
            "{label}: the finding must name the required predicate: {drift:?}"
        );
    }

    // The shape the migration actually creates still satisfies the contract.
    let (status, drift) = contract_status_with_index(
        &pool,
        "CREATE UNIQUE INDEX tenancy_backfill_checkpoints_completed_key \
         ON tenancy_backfill_checkpoints (tranche, contract_digest) \
         WHERE status = 'COMPLETED'",
    )
    .await;
    assert_eq!(
        status, "SATISFIED",
        "the correctly declared index must pass: {drift:?}"
    );
}

// ── Recovering from a failed concurrent index build ─────────────────────────

const BUILD_MIGRATIONS: &[&str] = &[
    include_str!("../../migrations/0033_tenancy_tranche1_index_idx_archival_batches_tenant.sql"),
    include_str!("../../migrations/0034_tenancy_tranche1_index_idx_audit_logs_tenant.sql"),
    include_str!("../../migrations/0035_tenancy_tranche1_index_idx_entities_tenant.sql"),
    include_str!("../../migrations/0036_tenancy_tranche1_index_idx_memory_graph_tenant.sql"),
    include_str!("../../migrations/0037_tenancy_tranche1_index_idx_rmk_policies_tenant.sql"),
    include_str!(
        "../../migrations/0038_tenancy_tranche1_index_archival_batches_id_tenant_id_key.sql"
    ),
    include_str!("../../migrations/0039_tenancy_tranche1_index_entities_id_tenant_id_key.sql"),
    include_str!("../../migrations/0040_tenancy_tranche1_index_rmk_policies_id_tenant_id_key.sql"),
];
// Every migration and rollback script this module drives, declared once.
//
// These were previously re-declared inside individual test functions, which is
// how one file came to hold three separate constants for
// `rollback/0041_tenancy_tranche1_constraints_down.sql`. Rust resolves
// module-level items regardless of declaration order, so tests above this point
// use them freely.
const PREPARE_UP_SQL: &str = include_str!("../../migrations/0032_tenancy_tranche1_prepare.sql");
const CONSTRAINTS_UP: &str = include_str!("../../migrations/0041_tenancy_tranche1_constraints.sql");
const CONSTRAINTS_DOWN_SQL: &str =
    include_str!("../../rollback/0041_tenancy_tranche1_constraints_down.sql");
const INDEXES_DOWN_SQL: &str =
    include_str!("../../rollback/0033_tenancy_tranche1_indexes_down.sql");
const PREPARE_DOWN_SQL: &str =
    include_str!("../../rollback/0032_tenancy_tranche1_prepare_down.sql");
const STEP_ONE_DOWN_SQL: &str = include_str!("../../rollback/0028_agent_tenancy_identity_down.sql");
const GRANTS_DOWN_SQL: &str = include_str!("../../rollback/0030_credential_agent_grants_down.sql");
const GRANTS_HARDENING_DOWN_SQL: &str =
    include_str!("../../rollback/0031_credential_agent_grants_hardening_down.sql");

const TRANCHE1_INDEXES: &str = "'idx_archival_batches_tenant','idx_audit_logs_tenant',\
    'idx_entities_tenant','idx_memory_graph_tenant','idx_rmk_policies_tenant',\
    'archival_batches_id_tenant_id_key','entities_id_tenant_id_key',\
    'rmk_policies_id_tenant_id_key'";

async fn count_tranche1_indexes(pool: &PgPool) -> i64 {
    sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT count(*) FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace \
         WHERE n.nspname='public' AND c.relkind='i' AND c.relname IN ({TRANCHE1_INDEXES})"
    )))
    .fetch_one(pool)
    .await
    .unwrap()
}

/// The authoritative recovery path for a failed `CREATE INDEX CONCURRENTLY`.
///
/// A failed concurrent build leaves the index behind INVALID. The build files
/// carry `IF NOT EXISTS`, so the next `sqlx migrate run` skips the broken index
/// with a notice, the migration *succeeds*, and sqlx records the version — over
/// an index that enforces nothing, and which it will now never rebuild. That is
/// why "drop it and re-run its build migration" is not a recovery: the ledger
/// entry is already there.
///
/// The path that works is rollback/0033, which drops all eight indexes AND
/// deletes ledger versions 33-40, after which a normal migrate rebuilds them.
/// This proves each link: the guard rejects, the rollback clears both the
/// indexes and the ledger, the rebuild produces valid indexes, and 0041 then
/// applies.
#[sqlx::test(migrations = "./migrations")]
async fn a_failed_concurrent_build_recovers_through_the_indexes_rollback(pool: PgPool) {
    // Model the state a failed build leaves: index present but INVALID, with
    // 0041 not yet applied because its guard would have refused.
    sqlx::raw_sql(CONSTRAINTS_DOWN_SQL)
        .execute(&pool)
        .await
        .expect("0041 rollback");
    for stmt in [
        "CREATE UNIQUE INDEX IF NOT EXISTS archival_batches_id_tenant_id_key ON archival_batches (id, tenant_id)",
        "CREATE UNIQUE INDEX IF NOT EXISTS entities_id_tenant_id_key ON entities (id, tenant_id)",
        "CREATE UNIQUE INDEX IF NOT EXISTS rmk_policies_id_tenant_id_key ON rmk_policies (id, tenant_id)",
    ] {
        sqlx::query(sqlx::AssertSqlSafe(stmt.to_owned()))
            .execute(&pool)
            .await
            .unwrap();
    }
    sqlx::query(
        "UPDATE pg_index SET indisvalid = false \
         WHERE indexrelid = 'public.idx_entities_tenant'::regclass",
    )
    .execute(&pool)
    .await
    .unwrap();

    // 1. The guard rejects the invalid build, and says what actually recovers.
    let error = sqlx::raw_sql(CONSTRAINTS_UP)
        .execute(&pool)
        .await
        .unwrap_err();
    let db = error.as_database_error().expect("a database error");
    assert_eq!(db.code().as_deref(), Some("55000"), "{error}");
    assert!(
        db.message().contains("idx_entities_tenant"),
        "must name the broken index: {}",
        db.message()
    );
    assert!(
        db.message()
            .contains("rollback/0033_tenancy_tranche1_indexes_down.sql"),
        "must prescribe the rollback, not a re-run of the build migration: {}",
        db.message()
    );
    assert!(
        db.message().contains("will NOT fix"),
        "must say plainly that re-running the build migration does not work: {}",
        db.message()
    );

    // 2. The rollback clears the indexes AND the ledger entries that would
    //    otherwise stop sqlx ever rebuilding them.
    sqlx::raw_sql(INDEXES_DOWN_SQL)
        .execute(&pool)
        .await
        .expect("0033-0040 rollback");
    assert_eq!(
        count_tranche1_indexes(&pool).await,
        0,
        "indexes must be gone"
    );
    let ledger: i64 =
        sqlx::query_scalar("SELECT count(*) FROM _sqlx_migrations WHERE version BETWEEN 33 AND 40")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        ledger, 0,
        "ledger versions 33-40 must be cleared, or sqlx will never rebuild them"
    );

    // 3. A normal migrate rebuilds them, all valid.
    for build in BUILD_MIGRATIONS {
        sqlx::raw_sql(*build).execute(&pool).await.expect("rebuild");
    }
    let all_valid: bool = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT bool_and(i.indisvalid) FROM pg_index i JOIN pg_class c ON c.oid=i.indexrelid \
         WHERE c.relname IN ({TRANCHE1_INDEXES})"
    )))
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(all_valid, "every rebuilt index must be valid");

    // 4. And 0041 now applies.
    sqlx::raw_sql(CONSTRAINTS_UP)
        .execute(&pool)
        .await
        .expect("0041 must apply once the indexes are valid");
}

// ── Rollback catalog checks are scoped to the tables they mean ──────────────

/// `conname` is unique only per relation, so an unrelated table — or another
/// schema entirely — may legitimately hold a constraint called
/// `entities_id_tenant_id_key`. A guard matching on the bare name would refuse
/// a rollback that has nothing to do with it.
#[sqlx::test(migrations = "./migrations")]
async fn identically_named_constraints_elsewhere_do_not_block_rollback(pool: PgPool) {
    sqlx::raw_sql(CONSTRAINTS_DOWN_SQL)
        .execute(&pool)
        .await
        .expect("0041 rollback");

    for stmt in [
        "CREATE SCHEMA decoy",
        "CREATE TABLE decoy.entities (id uuid PRIMARY KEY, tenant_id uuid)",
        "ALTER TABLE decoy.entities ADD CONSTRAINT entities_id_tenant_id_key \
         UNIQUE (id, tenant_id)",
        "CREATE TABLE public.zz_unrelated (id uuid PRIMARY KEY, tenant_id uuid)",
        "ALTER TABLE public.zz_unrelated ADD CONSTRAINT entities_tenant_agent_fkey \
         CHECK (tenant_id IS NOT NULL)",
    ] {
        sqlx::query(sqlx::AssertSqlSafe(stmt.to_owned()))
            .execute(&pool)
            .await
            .unwrap();
    }

    // Both decoys carry names the guards look for. Neither is on a tranche-1
    // table in `public`, so neither may block.
    sqlx::raw_sql(INDEXES_DOWN_SQL)
        .execute(&pool)
        .await
        .expect("decoy constraints must not block the indexes rollback");
    sqlx::raw_sql(PREPARE_DOWN_SQL)
        .execute(&pool)
        .await
        .expect("decoy constraints must not block the prepare rollback");
}

// ── The dependency preflight is complete, and runs before any damage ────────

/// Rolling back 0032 or 0033 while 0041's foreign keys are still attached would
/// leave them referencing columns about to be dropped. Both scripts refuse, and
/// the refusal happens before any drop and before the ledger delete — so a
/// refused run leaves the database exactly as it was.
#[sqlx::test(migrations = "./migrations")]
async fn both_rollbacks_refuse_while_any_tranche1_foreign_key_remains(pool: PgPool) {
    for (label, script) in [("0033-0040", INDEXES_DOWN_SQL), ("0032", PREPARE_DOWN_SQL)] {
        // Each script opens its own transaction, so a RAISE leaves the
        // connection aborted and it must be cleared before reuse.
        let mut conn = pool.acquire().await.unwrap();
        let error = sqlx::raw_sql(script).execute(&mut *conn).await.unwrap_err();
        sqlx::raw_sql("ROLLBACK").execute(&mut *conn).await.unwrap();
        drop(conn);
        let db = error.as_database_error().expect("a database error");
        assert_eq!(
            db.code().as_deref(),
            Some("2BP01"),
            "{label}: must be dependent_objects_still_exist, got {error}"
        );
        assert!(
            db.message()
                .contains("entities_tenant_agent_fkey on entities"),
            "{label}: must name the constraint and its table: {}",
            db.message()
        );
    }

    // Nothing was destroyed by either refusal.
    let columns: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM information_schema.columns \
         WHERE table_schema='public' AND column_name IN ('agent_uuid','tenant_id') \
           AND table_name IN ('archival_batches','audit_logs','entities','memory_graph', \
                              'rmk_policies')",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        columns, 10,
        "no ownership column may be dropped by a refusal"
    );
    let ledger: i64 =
        sqlx::query_scalar("SELECT count(*) FROM _sqlx_migrations WHERE version BETWEEN 32 AND 41")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(ledger, 10, "no ledger row may be deleted by a refusal");
}

/// A partially unwound 0041 — unique constraints dropped, foreign keys still
/// attached — is the state an interrupted rollback leaves. The old guard checked
/// only the unique constraints and would have waved this through.
#[sqlx::test(migrations = "./migrations")]
async fn a_partially_unwound_0041_still_blocks_the_indexes_rollback(pool: PgPool) {
    for stmt in [
        "ALTER TABLE archival_batches DROP CONSTRAINT archival_batches_id_tenant_id_key",
        "ALTER TABLE entities DROP CONSTRAINT entities_id_tenant_id_key",
        "ALTER TABLE rmk_policies DROP CONSTRAINT rmk_policies_id_tenant_id_key",
    ] {
        sqlx::query(sqlx::AssertSqlSafe(stmt.to_owned()))
            .execute(&pool)
            .await
            .unwrap();
    }

    let mut conn = pool.acquire().await.unwrap();
    let error = sqlx::raw_sql(INDEXES_DOWN_SQL)
        .execute(&mut *conn)
        .await
        .unwrap_err();
    sqlx::raw_sql("ROLLBACK").execute(&mut *conn).await.unwrap();
    drop(conn);
    let db = error.as_database_error().expect("a database error");
    assert_eq!(
        db.code().as_deref(),
        Some("2BP01"),
        "must be dependent_objects_still_exist, got {error}"
    );
    assert!(
        db.message().contains("tenant_agent_fkey"),
        "the surviving foreign keys must be what blocks it: {}",
        db.message()
    );
}

/// `rollback/0028` received the same table-scoping fix as 0032 and 0033, and
/// needs its own decoy — a CHECK-typed one does not exercise it, because that
/// guard already filtered `contype = 'f'`. The decoy here is a genuine foreign
/// key carrying a tranche-1 constraint name on an unrelated table.
///
/// It deliberately references a local parent rather than `agents`: an FK
/// pointing at `agents(tenant_id, id)` would be a real dependency on the unique
/// key this rollback drops, and refusing it would be correct rather than a
/// false positive.
#[sqlx::test(migrations = "./migrations")]
async fn an_unrelated_foreign_key_of_the_same_name_does_not_block_step_one_rollback(pool: PgPool) {
    for script in [CONSTRAINTS_DOWN_SQL, INDEXES_DOWN_SQL, PREPARE_DOWN_SQL] {
        sqlx::raw_sql(script).execute(&pool).await.expect("unwind");
    }

    for stmt in [
        "CREATE TABLE public.zz_fk_parent (a uuid, b uuid, UNIQUE (a, b))",
        "CREATE TABLE public.zz_fk_decoy (a uuid, b uuid, \
         CONSTRAINT entities_tenant_agent_fkey FOREIGN KEY (a, b) \
         REFERENCES public.zz_fk_parent (a, b))",
    ] {
        sqlx::query(sqlx::AssertSqlSafe(stmt.to_owned()))
            .execute(&pool)
            .await
            .unwrap();
    }
    let decoy_is_a_real_fk: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM pg_constraint c JOIN pg_class t ON t.oid = c.conrelid \
         WHERE c.conname = 'entities_tenant_agent_fkey' AND c.contype = 'f' \
           AND t.relname = 'zz_fk_decoy')",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(
        decoy_is_a_real_fk,
        "the decoy must be FK-typed to be a real test"
    );

    sqlx::raw_sql(GRANTS_HARDENING_DOWN_SQL)
        .execute(&pool)
        .await
        .expect("0031 rollback");
    sqlx::raw_sql(GRANTS_DOWN_SQL)
        .execute(&pool)
        .await
        .expect("0030 rollback");
    sqlx::raw_sql(STEP_ONE_DOWN_SQL)
        .execute(&pool)
        .await
        .expect("an unrelated FK of the same name must not block the step-1 rollback");

    // And it actually did the work, rather than merely not erroring.
    let gone: bool = sqlx::query_scalar(
        "SELECT NOT EXISTS (SELECT 1 FROM information_schema.columns \
         WHERE table_schema='public' AND table_name='agents' \
           AND column_name='external_agent_id')",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(
        gone,
        "the rollback must have run, not just declined to fail"
    );
}

// ── Agent deletion survives the ownership foreign keys ──────────────────────

/// The five composite ownership keys 0041 attaches all default to
/// `ON DELETE NO ACTION`, which makes them a live constraint on the *existing*
/// agent-deletion path rather than inert PREPARE scaffolding. Four of the five
/// tables were already emptied before the parent row went:
/// `archival_batches` by the `ON DELETE CASCADE` on its `agent_id` key from
/// migration 0006, and `entities`, `memory_graph` and `audit_logs` by explicit
/// statements in `store::delete_agent`. `rmk_policies` was emptied by nothing,
/// so attaching its key turned `DELETE /api/v1/agents/:agent_id` into a 500 for
/// every agent that had ever been served by RMK.
///
/// Driven through `store::delete_agent` rather than hand-written SQL: the
/// contract under test is that function's, and a test that re-typed its
/// statement list would keep passing after the function stopped matching.
///
/// Every one of the five tables is populated first. An agent with no children
/// deletes cleanly whatever the constraints say, so a test that skipped the
/// setup would assert nothing at all.
#[sqlx::test(migrations = "./migrations")]
async fn deleting_an_agent_disposes_of_every_tranche1_child(pool: PgPool) {
    let state = crate::test_support::test_state(pool.clone());

    insert_agent(&pool, "doomed", Some(TENANT)).await;
    insert_agent(&pool, "bystander", Some(OTHER_TENANT)).await;

    for agent in ["doomed", "bystander"] {
        for (_, insert) in AGENT_BRIDGE_TABLES {
            sqlx::query(*insert)
                .bind(agent)
                .execute(&pool)
                .await
                .unwrap();
        }
        sqlx::query("INSERT INTO audit_logs (agent_id, event_type) VALUES ($1, 'ev')")
            .bind(agent)
            .execute(&pool)
            .await
            .unwrap();
        // An episode pointing at that agent's policy, so the retention half of
        // the contract is exercised rather than assumed.
        sqlx::query(
            "INSERT INTO rmk_episodes (agent_id, policy_id, task_success, token_savings, \
                                       retrieval_precision, eviction_cost, reward) \
             SELECT $1, id, 1, 1, 1, 1, 1 FROM rmk_policies WHERE agent_id = $1",
        )
        .bind(agent)
        .execute(&pool)
        .await
        .unwrap();
    }

    // The bridges must have populated ownership on all five, or the foreign keys
    // are skipped by MATCH SIMPLE and this test proves nothing.
    let owned: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM (
             SELECT 1 FROM archival_batches WHERE agent_id='doomed' AND tenant_id IS NOT NULL
             UNION ALL SELECT 1 FROM audit_logs   WHERE agent_id='doomed' AND tenant_id IS NOT NULL
             UNION ALL SELECT 1 FROM entities     WHERE agent_id='doomed' AND tenant_id IS NOT NULL
             UNION ALL SELECT 1 FROM memory_graph WHERE agent_id='doomed' AND tenant_id IS NOT NULL
             UNION ALL SELECT 1 FROM rmk_policies WHERE agent_id='doomed' AND tenant_id IS NOT NULL
         ) owned_rows",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        owned, 5,
        "all five ownership keys must be live for this agent, or NULL components \
         make the foreign keys unenforced and the test vacuous"
    );

    let deleted = crate::memory::store::delete_agent(&state, "doomed")
        .await
        .expect("deleting an agent that owns an RMK policy must not fail");
    assert!(deleted, "the agent existed, so it must report a deletion");

    for table in [
        "agents",
        "archival_batches",
        "audit_logs",
        "entities",
        "memory_graph",
        "rmk_policies",
    ] {
        let left: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT count(*) FROM {table} WHERE agent_id = 'doomed'"
        )))
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(left, 0, "{table}: nothing may survive the agent's deletion");
    }

    // The episode is a metrics record and is deliberately retained; only its
    // policy reference goes, via the ON DELETE SET NULL from migration 0018.
    let (episodes, orphaned): (i64, i64) = sqlx::query_as(
        "SELECT count(*), count(*) FILTER (WHERE policy_id IS NULL) \
         FROM rmk_episodes WHERE agent_id = 'doomed'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(episodes, 1, "the episode log must be retained");
    assert_eq!(
        orphaned, 1,
        "the retained episode must have had its policy reference set to NULL"
    );

    // The other tenant is untouched, including its policy and its episode.
    for table in [
        "agents",
        "archival_batches",
        "audit_logs",
        "entities",
        "memory_graph",
        "rmk_policies",
        "rmk_episodes",
    ] {
        let left: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT count(*) FROM {table} WHERE agent_id = 'bystander'"
        )))
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(left, 1, "{table}: the other tenant's row must be untouched");
    }
    let bystander_keeps_policy: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM rmk_episodes WHERE agent_id='bystander' AND policy_id IS NOT NULL",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        bystander_keeps_policy, 1,
        "the other tenant's episode must keep pointing at its own policy"
    );
}

/// Why the fix is an explicit DELETE and not `ON DELETE CASCADE` on the key.
///
/// The ownership columns are NULL-able throughout PREPARE and the keys are
/// `MATCH SIMPLE`, so PostgreSQL skips the constraint entirely for any row with
/// a NULL component — and a skipped constraint cascades nothing. Measured on
/// pg16: with `ON DELETE CASCADE` on `rmk_policies_tenant_agent_fkey`, deleting
/// the agent removed the bridge-populated policy and left the NULL-ownership one
/// behind. That is a split disposition decided by whether a row happened to be
/// written after 0032, which is exactly the window this tranche opens.
///
/// `WHERE agent_id = $1` has no such gap: `agents_agent_id_key` makes that
/// column globally unique, every `rmk_policies` row carries it NOT NULL since
/// migration 0017, and it is the same predicate the sibling deletes in
/// `delete_agent` already use.
#[sqlx::test(migrations = "./migrations")]
async fn deletion_also_removes_the_policies_a_cascade_would_have_missed(pool: PgPool) {
    let state = crate::test_support::test_state(pool.clone());
    insert_agent(&pool, "doomed", Some(TENANT)).await;

    // One row as a post-0032 writer produces it: ownership resolved by the bridge.
    sqlx::query(
        "INSERT INTO rmk_policies (agent_id, pressure_a, pressure_b, kp, ki, \
         graph_bonus_weight, retrieval_threshold) VALUES ('doomed', 1, 0, 0, 0, 0, 0)",
    )
    .execute(&pool)
    .await
    .unwrap();
    // One row as history holds it: ownership still NULL, awaiting BACKFILL.
    for stmt in [
        "ALTER TABLE rmk_policies DISABLE TRIGGER trg_rmk_policies_tenancy_bridge",
        "INSERT INTO rmk_policies (agent_id, pressure_a, pressure_b, kp, ki, \
         graph_bonus_weight, retrieval_threshold) VALUES ('doomed', 2, 0, 0, 0, 0, 0)",
        "ALTER TABLE rmk_policies ENABLE TRIGGER trg_rmk_policies_tenancy_bridge",
    ] {
        sqlx::query(sqlx::AssertSqlSafe(stmt.to_owned()))
            .execute(&pool)
            .await
            .unwrap();
    }

    let unowned: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM rmk_policies WHERE agent_id='doomed' AND tenant_id IS NULL",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        unowned, 1,
        "the pre-backfill row must really have NULL ownership, or the cascade \
         this test rules out would have reached it anyway"
    );

    crate::memory::store::delete_agent(&state, "doomed")
        .await
        .expect("deletion must succeed");

    let left: i64 = sqlx::query_scalar("SELECT count(*) FROM rmk_policies WHERE agent_id='doomed'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        left, 0,
        "both policies must go: a cascade would have left the NULL-ownership one"
    );
}

// ── A name is not an identity: hostile occupants ─────────────────────────────

/// Put the database back in the state 0033-0040 leave it: unique indexes built,
/// no 0041 constraint attached.
///
/// The rollback drops the indexes its unique constraints had adopted, so they are
/// rebuilt here -- plainly, not concurrently, which a test does not need.
async fn reset_to_pre_0041(pool: &PgPool) {
    sqlx::raw_sql(CONSTRAINTS_DOWN_SQL)
        .execute(pool)
        .await
        .expect("0041 rollback must succeed from a clean state");
    for stmt in [
        "CREATE UNIQUE INDEX IF NOT EXISTS archival_batches_id_tenant_id_key \
         ON archival_batches (id, tenant_id)",
        "CREATE UNIQUE INDEX IF NOT EXISTS entities_id_tenant_id_key \
         ON entities (id, tenant_id)",
        "CREATE UNIQUE INDEX IF NOT EXISTS rmk_policies_id_tenant_id_key \
         ON rmk_policies (id, tenant_id)",
    ] {
        sqlx::query(sqlx::AssertSqlSafe(stmt.to_owned()))
            .execute(pool)
            .await
            .unwrap();
    }
}

/// How many of the eight objects 0041 owns are currently attached, counted by
/// full structural identity rather than by name.
///
/// Counting `(name, type)` was not enough: several of the hostile occupants below
/// are themselves real foreign keys to `agents` carrying a tranche name, so a
/// name-based counter counts the occupant and reports the constraint as attached
/// when it never was. That is the same mistake the migration guard used to make,
/// reproduced in the test that is supposed to catch it.
async fn count_tranche1_constraints(pool: &PgPool) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) \
           FROM (VALUES ('f', 'archival_batches', 'archival_batches_tenant_agent_fkey'), \
                        ('f', 'audit_logs',       'audit_logs_tenant_agent_fkey'), \
                        ('f', 'entities',         'entities_tenant_agent_fkey'), \
                        ('f', 'memory_graph',     'memory_graph_tenant_agent_fkey'), \
                        ('f', 'rmk_policies',     'rmk_policies_tenant_agent_fkey'), \
                        ('u', 'archival_batches', 'archival_batches_id_tenant_id_key'), \
                        ('u', 'entities',         'entities_id_tenant_id_key'), \
                        ('u', 'rmk_policies',     'rmk_policies_id_tenant_id_key') \
                ) AS e(kind, tbl, name) \
           JOIN pg_constraint c \
             ON c.conname  = e.name \
            AND c.contype   = e.kind \
            AND c.conrelid  = to_regclass('public.' || quote_ident(e.tbl)) \
          WHERE NOT c.condeferrable AND NOT c.condeferred AND c.conparentid = 0 \
            AND (SELECT array_agg(a.attname::text ORDER BY k.ord) \
                   FROM unnest(c.conkey) WITH ORDINALITY AS k(attnum, ord) \
                   JOIN pg_attribute a \
                     ON a.attrelid = c.conrelid AND a.attnum = k.attnum) \
                = CASE e.kind WHEN 'f' THEN ARRAY['tenant_id', 'agent_uuid'] \
                                       ELSE ARRAY['id', 'tenant_id'] END \
            AND (e.kind = 'u' OR ( \
                    c.confrelid      = 'public.agents'::regclass \
                AND c.confupdtype    = 'a' \
                AND c.confdeltype    = 'a' \
                AND c.confmatchtype  = 's' \
                AND (SELECT array_agg(a.attname::text ORDER BY k.ord) \
                       FROM unnest(c.confkey) WITH ORDINALITY AS k(attnum, ord) \
                       JOIN pg_attribute a \
                         ON a.attrelid = c.confrelid AND a.attnum = k.attnum) \
                    = ARRAY['tenant_id', 'id']))",
    )
    .fetch_one(pool)
    .await
    .unwrap()
}

/// Run a script that must refuse, clear the aborted transaction its RAISE leaves
/// behind, and return `(sqlstate, message)`.
async fn expect_refusal(pool: &PgPool, script: &'static str, label: &str) -> (String, String) {
    let mut conn = pool.acquire().await.unwrap();
    let result = sqlx::raw_sql(script).execute(&mut *conn).await;
    // Each script opens its own transaction, so a RAISE leaves the connection
    // aborted and it must be cleared before the pool reuses it.
    // Unwrapped, matching the rest of this file. This helper only runs when the
    // script raised, so the connection is always inside an aborted transaction
    // and the ROLLBACK always has something to clear -- `.ok()` here would
    // silence a genuinely dead connection and hand it back to the pool with no
    // diagnostic.
    sqlx::raw_sql("ROLLBACK")
        .execute(&mut *conn)
        .await
        .expect("clearing the aborted transaction must succeed");
    drop(conn);
    let error = match result {
        Ok(_) => panic!("{label}: the script must refuse, but it succeeded"),
        Err(e) => e,
    };
    let db = error
        .as_database_error()
        .unwrap_or_else(|| panic!("{label}: expected a database error, got {error}"));
    (
        db.code().as_deref().unwrap_or_default().to_owned(),
        db.message().to_owned(),
    )
}

/// Every way a name-and-table guard can be fooled, and the refusal it must give.
///
/// The old guard asked only "is there a constraint of this name on this table".
/// Each shape below answers yes while being a different object, so the old guard
/// skipped creating the real constraint, sqlx recorded version 41 over a schema
/// that never received it, and `rollback/0041` would later drop the occupant by
/// name.
///
/// What is asserted is that **nothing was applied**: not one of the eight
/// constraints 0041 owns exists after the refusal, and the occupant is still
/// there. The `_sqlx_migrations` row is deliberately not asserted here, because
/// sqlx writes it, not this file -- driving the migration through `raw_sql` never
/// records a version, so "version 41 is absent" would be true whether the guard
/// worked or not. Failing the statement is what stops sqlx recording it, and that
/// is what these assertions pin.
///
/// The expected SQLSTATE is asserted, not merely "an error happened". A wrong
/// column name or a syntax slip inside the guard would also raise, and would
/// otherwise look like a pass.
#[sqlx::test(migrations = "./migrations")]
async fn a_same_named_object_of_the_wrong_shape_is_refused_not_adopted(pool: PgPool) {
    // (label, setup, cleanup, the phrase the diagnostic must contain)
    let cases: &[(&str, &[&str], &str, &str)] = &[
        (
            "CHECK constraint occupying a foreign-key name",
            &[
                "ALTER TABLE entities ADD CONSTRAINT entities_tenant_agent_fkey \
               CHECK (tenant_id IS NOT NULL)",
            ],
            "ALTER TABLE entities DROP CONSTRAINT entities_tenant_agent_fkey",
            "constraint type is 'c', expected 'f'",
        ),
        (
            "foreign key over the wrong local columns",
            &[
                "ALTER TABLE entities ADD CONSTRAINT entities_tenant_agent_fkey \
               FOREIGN KEY (agent_uuid, tenant_id) REFERENCES agents (id, tenant_id) NOT VALID",
            ],
            "ALTER TABLE entities DROP CONSTRAINT entities_tenant_agent_fkey",
            "local columns are '{agent_uuid,tenant_id}', expected '{tenant_id,agent_uuid}'",
        ),
        (
            "foreign key referencing the wrong table",
            &[
                "CREATE TABLE zz_decoy_parent (tenant_id uuid, agent_uuid uuid, \
                 UNIQUE (tenant_id, agent_uuid))",
                "ALTER TABLE entities ADD CONSTRAINT entities_tenant_agent_fkey \
                 FOREIGN KEY (tenant_id, agent_uuid) \
                 REFERENCES zz_decoy_parent (tenant_id, agent_uuid) NOT VALID",
            ],
            "ALTER TABLE entities DROP CONSTRAINT entities_tenant_agent_fkey",
            "references zz_decoy_parent, expected public.agents",
        ),
        (
            // The right table, the right column types, the target pair in the
            // wrong order -- so `agents(id, tenant_id)` is satisfied by the same
            // unique key and only `confkey` distinguishes it. Referencing
            // `external_agent_id` instead would be rejected by PostgreSQL for a
            // type mismatch and would never reach the guard.
            "foreign key referencing the wrong target columns",
            &[
                "ALTER TABLE entities ADD CONSTRAINT entities_tenant_agent_fkey \
               FOREIGN KEY (tenant_id, agent_uuid) REFERENCES agents (id, tenant_id) NOT VALID",
            ],
            "ALTER TABLE entities DROP CONSTRAINT entities_tenant_agent_fkey",
            "referenced columns are '{id,tenant_id}', expected '{tenant_id,id}'",
        ),
        (
            "foreign key with the wrong ON DELETE action",
            &[
                "ALTER TABLE rmk_policies ADD CONSTRAINT rmk_policies_tenant_agent_fkey \
               FOREIGN KEY (tenant_id, agent_uuid) REFERENCES agents (tenant_id, id) \
               ON DELETE CASCADE NOT VALID",
            ],
            "ALTER TABLE rmk_policies DROP CONSTRAINT rmk_policies_tenant_agent_fkey",
            "ON DELETE action is 'c', expected 'a' (NO ACTION)",
        ),
        (
            // Reaches the "constraint owns no index" branch, which no
            // format()-built case does. That branch appended a bare literal to a
            // TEXT[], which PostgreSQL resolves as anyarray || anyarray and then
            // fails to parse as an array literal -- so the guard crashed with
            // `malformed array literal` instead of refusing cleanly. Found by
            // review, not by the six shape cases, because every other append
            // goes through format() and is therefore already text-typed.
            "CHECK constraint occupying a UNIQUE name",
            &[
                "DROP INDEX entities_id_tenant_id_key",
                "ALTER TABLE entities ADD CONSTRAINT entities_id_tenant_id_key \
                 CHECK (tenant_id IS NOT NULL)",
            ],
            "ALTER TABLE entities DROP CONSTRAINT entities_id_tenant_id_key",
            "constraint type is 'c', expected 'u'",
        ),
        (
            "foreign key with the wrong ON UPDATE action",
            &[
                "ALTER TABLE entities ADD CONSTRAINT entities_tenant_agent_fkey \
               FOREIGN KEY (tenant_id, agent_uuid) REFERENCES agents (tenant_id, id) \
               ON UPDATE CASCADE NOT VALID",
            ],
            "ALTER TABLE entities DROP CONSTRAINT entities_tenant_agent_fkey",
            "ON UPDATE action is 'c', expected 'a' (NO ACTION)",
        ),
        (
            "foreign key with the wrong match type",
            &[
                "ALTER TABLE entities ADD CONSTRAINT entities_tenant_agent_fkey \
               FOREIGN KEY (tenant_id, agent_uuid) REFERENCES agents (tenant_id, id) \
               MATCH FULL NOT VALID",
            ],
            "ALTER TABLE entities DROP CONSTRAINT entities_tenant_agent_fkey",
            "match type is 'f', expected 's' (MATCH SIMPLE)",
        ),
        (
            "deferrable foreign key",
            &[
                "ALTER TABLE entities ADD CONSTRAINT entities_tenant_agent_fkey \
               FOREIGN KEY (tenant_id, agent_uuid) REFERENCES agents (tenant_id, id) \
               DEFERRABLE INITIALLY DEFERRED NOT VALID",
            ],
            "ALTER TABLE entities DROP CONSTRAINT entities_tenant_agent_fkey",
            "deferrability is (condeferrable=t, condeferred=t), expected (f, f)",
        ),
        (
            "UNIQUE constraint over the wrong columns",
            &[
                "DROP INDEX archival_batches_id_tenant_id_key",
                "ALTER TABLE archival_batches ADD CONSTRAINT archival_batches_id_tenant_id_key \
                 UNIQUE (id, agent_uuid)",
            ],
            "ALTER TABLE archival_batches DROP CONSTRAINT archival_batches_id_tenant_id_key",
            "local columns are '{id,agent_uuid}', expected '{id,tenant_id}'",
        ),
    ];

    for (label, setup, cleanup, expected_phrase) in cases {
        reset_to_pre_0041(&pool).await;
        assert_eq!(
            count_tranche1_constraints(&pool).await,
            0,
            "{label}: the pre-0041 state must have none of them attached"
        );

        for stmt in *setup {
            sqlx::query(sqlx::AssertSqlSafe((*stmt).to_owned()))
                .execute(&pool)
                .await
                .unwrap_or_else(|e| panic!("{label}: occupant setup failed: {e}"));
        }

        let (code, message) = expect_refusal(&pool, CONSTRAINTS_UP, label).await;
        assert_eq!(
            code, "42710",
            "{label}: must be duplicate_object, got {code}: {message}"
        );
        assert!(
            message.contains(expected_phrase),
            "{label}: the refusal must name the axis that differs \
             (expected to find {expected_phrase:?}): {message}"
        );
        assert!(
            message.contains("Rename or remove the occupant"),
            "{label}: the refusal must state a next action: {message}"
        );

        // Nothing was applied. This is the load-bearing half: the old guard's
        // failure was silent success, not a bad error message.
        assert_eq!(
            count_tranche1_constraints(&pool).await,
            0,
            "{label}: a refusal must not leave any of 0041's constraints attached"
        );

        // And the occupant is still whoever's it was.
        sqlx::query(sqlx::AssertSqlSafe((*cleanup).to_owned()))
            .execute(&pool)
            .await
            .unwrap_or_else(|e| panic!("{label}: the occupant must still be attached: {e}"));
    }
}

/// The idempotent rerun the guard must still allow.
///
/// A structural identity test is only useful if it accepts the tranche's own
/// object. Re-running 0041 over the constraints it created must succeed and leave
/// the catalog byte-identical -- otherwise the fix for the collision would have
/// broken every re-migration.
#[sqlx::test(migrations = "./migrations")]
async fn rerunning_0041_over_its_own_constraints_changes_nothing(pool: PgPool) {
    const SIGNATURE: &str = "SELECT md5(string_agg( \
            c.conname || ':' || c.contype::text || ':' || c.conkey::text || ':' \
            || COALESCE(c.confkey::text, '-') || ':' || COALESCE(c.confdeltype::text, '-') \
            || ':' || COALESCE(c.confupdtype::text, '-') \
            || ':' || COALESCE(c.confmatchtype::text, '-') || ':' || c.convalidated::text, \
            ',' ORDER BY c.conname)) \
         FROM pg_constraint c \
         WHERE c.conname LIKE '%tenant_agent_fkey' OR c.conname LIKE '%\\_id\\_tenant\\_id\\_key'";

    let before: String = sqlx::query_scalar(SIGNATURE)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        count_tranche1_constraints(&pool).await,
        8,
        "the migration under test must have attached all eight"
    );

    sqlx::raw_sql(CONSTRAINTS_UP)
        .execute(&pool)
        .await
        .expect("re-running 0041 over its own constraints must succeed");

    let after: String = sqlx::query_scalar(SIGNATURE)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        before, after,
        "an idempotent rerun must leave every constraint's structure identical"
    );
    assert_eq!(
        count_tranche1_constraints(&pool).await,
        8,
        "and must not have detached anything"
    );
}

/// `rollback/0041` must not drop a name it does not own.
///
/// `DROP CONSTRAINT IF EXISTS <name>` deletes whatever holds the name. With a
/// hostile occupant present that is somebody else's object, and the rollback
/// would have destroyed it while reporting success. Here the refusal is required
/// to leave the occupant attached, leave the tranche's own seven constraints
/// attached, and leave ledger version 41 in place -- the last is load-bearing
/// because this script deletes that row itself.
#[sqlx::test(migrations = "./migrations")]
async fn the_0041_rollback_refuses_to_drop_a_constraint_it_does_not_own(pool: PgPool) {
    for stmt in [
        "ALTER TABLE memory_graph DROP CONSTRAINT memory_graph_tenant_agent_fkey",
        "ALTER TABLE memory_graph ADD CONSTRAINT memory_graph_tenant_agent_fkey \
         CHECK (tenant_id IS NOT NULL)",
    ] {
        sqlx::query(sqlx::AssertSqlSafe(stmt.to_owned()))
            .execute(&pool)
            .await
            .unwrap();
    }

    let (code, message) = expect_refusal(&pool, CONSTRAINTS_DOWN_SQL, "0041 rollback").await;
    assert_eq!(
        code, "42710",
        "must be duplicate_object, got {code}: {message}"
    );
    assert!(
        message.contains("memory_graph_tenant_agent_fkey on public.memory_graph"),
        "the refusal must name the occupant and its table: {message}"
    );
    assert!(
        message.contains("Nothing has been dropped"),
        "the refusal must say the database is unchanged: {message}"
    );

    let occupant_survives: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM pg_constraint \
         WHERE conname = 'memory_graph_tenant_agent_fkey' AND contype = 'c')",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(
        occupant_survives,
        "the unrelated CHECK constraint must be left exactly where it was"
    );
    assert_eq!(
        count_tranche1_constraints(&pool).await,
        7,
        "the tranche's own constraints must not be dropped by a refused run"
    );
    let ledger: i64 =
        sqlx::query_scalar("SELECT count(*) FROM _sqlx_migrations WHERE version = 41")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(ledger, 1, "a refused rollback must not retire version 41");
}

/// The rollback's copy of the identity test, on every axis the migration's is.
///
/// The two guards are hand-duplicated rather than shared -- sqlx has no include
/// mechanism, and a rollback that depended on an object the forward migration
/// installs could not run after that object was removed. Duplication means they
/// can drift, and a down-script that silently accepted a foreign key pointing at
/// the wrong table would drop it. Testing the down copy on one axis only would
/// not catch that, so each axis is exercised against the rollback specifically.
#[sqlx::test(migrations = "./migrations")]
async fn the_0041_rollback_identity_test_covers_the_same_axes_as_the_migration(pool: PgPool) {
    let cases: &[(&str, &[&str], &str, &[&str])] = &[
        (
            "wrong local columns",
            &[
                "ALTER TABLE entities DROP CONSTRAINT entities_tenant_agent_fkey",
                "ALTER TABLE entities ADD CONSTRAINT entities_tenant_agent_fkey \
                 FOREIGN KEY (agent_uuid, tenant_id) REFERENCES agents (id, tenant_id) NOT VALID",
            ],
            "local columns are '{agent_uuid,tenant_id}', expected '{tenant_id,agent_uuid}'",
            &["ALTER TABLE entities DROP CONSTRAINT entities_tenant_agent_fkey"],
        ),
        (
            "wrong referenced columns",
            &[
                "ALTER TABLE entities DROP CONSTRAINT entities_tenant_agent_fkey",
                "ALTER TABLE entities ADD CONSTRAINT entities_tenant_agent_fkey \
                 FOREIGN KEY (tenant_id, agent_uuid) REFERENCES agents (id, tenant_id) NOT VALID",
            ],
            "referenced columns are '{id,tenant_id}', expected '{tenant_id,id}'",
            &["ALTER TABLE entities DROP CONSTRAINT entities_tenant_agent_fkey"],
        ),
        (
            "wrong ON DELETE action",
            &[
                "ALTER TABLE entities DROP CONSTRAINT entities_tenant_agent_fkey",
                "ALTER TABLE entities ADD CONSTRAINT entities_tenant_agent_fkey \
                 FOREIGN KEY (tenant_id, agent_uuid) REFERENCES agents (tenant_id, id) \
                 ON DELETE CASCADE NOT VALID",
            ],
            "ON DELETE action is 'c', expected 'a' (NO ACTION)",
            &["ALTER TABLE entities DROP CONSTRAINT entities_tenant_agent_fkey"],
        ),
        (
            "wrong match type",
            &[
                "ALTER TABLE entities DROP CONSTRAINT entities_tenant_agent_fkey",
                "ALTER TABLE entities ADD CONSTRAINT entities_tenant_agent_fkey \
                 FOREIGN KEY (tenant_id, agent_uuid) REFERENCES agents (tenant_id, id) \
                 MATCH FULL NOT VALID",
            ],
            "match type is 'f', expected 's' (MATCH SIMPLE)",
            &["ALTER TABLE entities DROP CONSTRAINT entities_tenant_agent_fkey"],
        ),
        (
            "deferrable",
            &[
                "ALTER TABLE entities DROP CONSTRAINT entities_tenant_agent_fkey",
                "ALTER TABLE entities ADD CONSTRAINT entities_tenant_agent_fkey \
                 FOREIGN KEY (tenant_id, agent_uuid) REFERENCES agents (tenant_id, id) \
                 DEFERRABLE INITIALLY DEFERRED NOT VALID",
            ],
            "deferrability is (condeferrable=t, condeferred=t), expected (f, f)",
            &["ALTER TABLE entities DROP CONSTRAINT entities_tenant_agent_fkey"],
        ),
        (
            "wrong local columns on a UNIQUE name",
            &[
                "ALTER TABLE archival_batches DROP CONSTRAINT archival_batches_id_tenant_id_key",
                "ALTER TABLE archival_batches ADD CONSTRAINT archival_batches_id_tenant_id_key \
                 UNIQUE (id, agent_uuid)",
            ],
            "local columns are '{id,agent_uuid}', expected '{id,tenant_id}'",
            &[
                // Dropping a UNIQUE constraint drops the index it adopted, so the
                // index has to be rebuilt before 0041 can re-adopt it.
                "ALTER TABLE archival_batches DROP CONSTRAINT archival_batches_id_tenant_id_key",
                "CREATE UNIQUE INDEX archival_batches_id_tenant_id_key \
                 ON archival_batches (id, tenant_id)",
            ],
        ),
    ];

    for (label, setup, expected_phrase, cleanup) in cases {
        for stmt in *setup {
            sqlx::query(sqlx::AssertSqlSafe((*stmt).to_owned()))
                .execute(&pool)
                .await
                .unwrap_or_else(|e| panic!("{label}: occupant setup failed: {e}"));
        }

        let (code, message) = expect_refusal(&pool, CONSTRAINTS_DOWN_SQL, label).await;
        assert_eq!(
            code, "42710",
            "{label}: must be duplicate_object, got {code}: {message}"
        );
        assert!(
            message.contains(expected_phrase),
            "{label}: the rollback must name the axis that differs \
             (expected {expected_phrase:?}): {message}"
        );
        assert!(
            message.contains("Nothing has been dropped"),
            "{label}: {message}"
        );

        // The refusal left every one of 0041's own objects attached, including
        // the ones it could have reached before hitting the occupant.
        let intact: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pg_constraint c \
             WHERE (c.contype = 'f' AND c.conname LIKE '%tenant\\_agent\\_fkey' \
                    AND c.confrelid = 'public.agents'::regclass) \
                OR (c.contype = 'u' AND c.conname IN ('archival_batches_id_tenant_id_key', \
                        'entities_id_tenant_id_key', 'rmk_policies_id_tenant_id_key'))",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            intact, 8,
            "{label}: a refused rollback must leave all eight names occupied"
        );

        // Remove the occupant, then let 0041 put its own object back for the
        // next case. The occupant has to go first: with it attached, 0041
        // correctly refuses, which is what the migration-side test asserts.
        for stmt in *cleanup {
            sqlx::query(sqlx::AssertSqlSafe((*stmt).to_owned()))
                .execute(&pool)
                .await
                .unwrap_or_else(|e| panic!("{label}: cleanup {stmt} failed: {e}"));
        }
        sqlx::raw_sql(CONSTRAINTS_UP)
            .execute(&pool)
            .await
            .unwrap_or_else(|e| panic!("{label}: could not restore the tranche state: {e}"));
    }
}

/// Adopting a unique constraint whose index is gone names what rebuilds it.
///
/// Dropping one of the three unique constraints takes its adopted index with it,
/// so a re-run of 0041 has nothing to adopt. Without the pre-check that is a bare
/// `relation "..." does not exist`; with it, the operator is told which
/// migrations build the index.
#[sqlx::test(migrations = "./migrations")]
async fn adopting_a_missing_unique_index_names_the_builds_that_create_it(pool: PgPool) {
    sqlx::query(sqlx::AssertSqlSafe(
        "ALTER TABLE entities DROP CONSTRAINT entities_id_tenant_id_key".to_owned(),
    ))
    .execute(&pool)
    .await
    .unwrap();
    let index_went_with_it: bool =
        sqlx::query_scalar("SELECT to_regclass('public.entities_id_tenant_id_key') IS NULL")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(
        index_went_with_it,
        "dropping the constraint must also drop the index it adopted, or this \
         test is not exercising the missing-index branch"
    );

    let (code, message) = expect_refusal(&pool, CONSTRAINTS_UP, "missing unique index").await;
    assert_eq!(
        code, "55000",
        "must be object_not_in_prerequisite_state, got {code}: {message}"
    );
    assert!(
        message.contains("unique index entities_id_tenant_id_key is missing"),
        "the refusal must name the missing index: {message}"
    );
    assert!(
        message.contains("migrations 0038-0040"),
        "and must name what rebuilds it: {message}"
    );
}

// ── Renamed child tables defeat name-based discovery ────────────────────────

/// Every tranche rollback refuses when a child table has been renamed.
///
/// `ALTER TABLE ... RENAME` moves a table and leaves its constraint names, its
/// trigger names and its index names untouched. The old guards joined
/// `pg_class.relname` against a literal table name, so a renamed child silently
/// dropped out of every one of them.
///
/// For `rollback/0033` that was destructive rather than merely inaccurate.
/// PostgreSQL does not rename indexes with their table, so `DROP INDEX
/// public.idx_audit_logs_tenant` still resolved; no constraint owns that index,
/// so nothing objected; and the script went on to delete ledger versions 33-40
/// while 0041's foreign keys were still attached. The tranche was left
/// half-unwound with a ledger claiming its indexes had never been built.
///
/// Each script is now checked through a handle a rename cannot move: 0041 and
/// 0028 through `pg_constraint.conindid` on the parent's unique key, 0033
/// additionally through `pg_index.indrelid`, and 0032 through
/// `pg_trigger.tgrelid`. After each refusal the protected columns, constraints
/// and ledger rows are asserted intact.
#[sqlx::test(migrations = "./migrations")]
async fn every_tranche_rollback_refuses_after_a_child_table_is_renamed(pool: PgPool) {
    // Literal-only by convention, not by type: `AssertSqlSafe` is the crate's
    // opt-out of SQL checking, so `from` and `to` must never come from anything
    // but a hardcoded name. Every call below passes one.
    async fn rename(pool: &PgPool, from: &str, to: &str) {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "ALTER TABLE {from} RENAME TO {to}"
        )))
        .execute(pool)
        .await
        .unwrap();
    }
    async fn ownership_columns(pool: &PgPool) -> i64 {
        sqlx::query_scalar(
            "SELECT count(*) FROM information_schema.columns \
             WHERE table_schema = 'public' AND column_name IN ('agent_uuid', 'tenant_id') \
               AND table_name IN ('archival_batches', 'audit_logs_moved', 'entities', \
                                  'memory_graph', 'rmk_policies')",
        )
        .fetch_one(pool)
        .await
        .unwrap()
    }
    async fn build_ledger(pool: &PgPool) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM _sqlx_migrations WHERE version BETWEEN 33 AND 40")
            .fetch_one(pool)
            .await
            .unwrap()
    }

    // The grants rollbacks first, so 0028's own tranche-1 guard is what refuses
    // rather than its earlier migration-0030 ordering guard.
    sqlx::raw_sql(GRANTS_HARDENING_DOWN_SQL)
        .execute(&pool)
        .await
        .expect("0031 rollback");
    sqlx::raw_sql(GRANTS_DOWN_SQL)
        .execute(&pool)
        .await
        .expect("0030 rollback");

    rename(&pool, "audit_logs", "audit_logs_moved").await;

    // Counted while the table is renamed, because `count_tranche1_constraints`
    // resolves tables by name and cannot see `audit_logs` under its new one. What
    // matters is that a refusal changes nothing, so the count before the refusal
    // is the right baseline -- an absolute 8 would be asserting the rename away.
    let attached_before = count_tranche1_constraints(&pool).await;
    assert_eq!(
        attached_before, 7,
        "the four unrenamed tables keep their seven constraints; audit_logs' is \
         still attached but is no longer reachable by that name, which is the \
         whole defect under test"
    );

    // ── 0041: the ownership key is found on a table the script does not name ──
    let (code, message) = expect_refusal(&pool, CONSTRAINTS_DOWN_SQL, "0041").await;
    assert_eq!(code, "55000", "0041: {message}");
    assert!(
        message.contains("audit_logs_tenant_agent_fkey is now on audit_logs_moved"),
        "0041 must name the key and the table it is actually on: {message}"
    );
    assert_eq!(
        count_tranche1_constraints(&pool).await,
        attached_before,
        "0041: a refusal must drop nothing"
    );
    let renamed_key_intact: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM pg_constraint \
         WHERE conname = 'audit_logs_tenant_agent_fkey' AND contype = 'f' \
           AND conrelid = 'public.audit_logs_moved'::regclass)",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(
        renamed_key_intact,
        "0041: the key on the renamed table must survive the refusal too"
    );

    // ── 0028: reverse dependency reports the renamed table by its real name ──
    let (code, message) = expect_refusal(&pool, STEP_ONE_DOWN_SQL, "0028").await;
    assert_eq!(code, "2BP01", "0028: {message}");
    assert!(
        message.contains("audit_logs_tenant_agent_fkey on audit_logs_moved"),
        "0028 must report the dependant where it actually is; the old \
         (name, table-name) join omitted it entirely: {message}"
    );
    let agents_key_intact: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM pg_constraint \
         WHERE conname = 'agents_tenant_id_id_key' AND conrelid = 'public.agents'::regclass)",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(
        agents_key_intact,
        "0028: the unique key it drops must survive its own refusal"
    );

    // ── 0033: the index survived the rename, so it must be refused by OID ────
    rename(&pool, "audit_logs_moved", "audit_logs").await;
    sqlx::raw_sql(CONSTRAINTS_DOWN_SQL)
        .execute(&pool)
        .await
        .expect("0041 rollback must succeed once the table is back");
    rename(&pool, "audit_logs", "audit_logs_moved").await;

    let (code, message) = expect_refusal(&pool, INDEXES_DOWN_SQL, "0033").await;
    assert_eq!(code, "55000", "0033: {message}");
    assert!(
        message.contains("idx_audit_logs_tenant")
            && message.contains("it is on audit_logs_moved, expected public.audit_logs"),
        "0033 must name the index and the table it now serves: {message}"
    );
    let index_survives: bool =
        sqlx::query_scalar("SELECT to_regclass('public.idx_audit_logs_tenant') IS NOT NULL")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(
        index_survives,
        "0033: the index a name-based drop would have removed must still be there"
    );
    assert_eq!(
        build_ledger(&pool).await,
        8,
        "0033: versions 33-40 must not be retired by a refused run -- this is the \
         state that left the ledger claiming the indexes were never built"
    );

    // ── 0032: the bridge trigger is the handle a rename cannot move ──────────
    rename(&pool, "audit_logs_moved", "audit_logs").await;
    sqlx::raw_sql(INDEXES_DOWN_SQL)
        .execute(&pool)
        .await
        .expect("0033 rollback must succeed once the table is back");
    rename(&pool, "audit_logs", "audit_logs_moved").await;

    let (code, message) = expect_refusal(&pool, PREPARE_DOWN_SQL, "0032").await;
    assert_eq!(code, "55000", "0032: {message}");
    assert!(
        message.contains("trg_audit_logs_tenancy_bridge is on audit_logs_moved"),
        "0032 must name the bridge trigger and the table it is on: {message}"
    );
    assert_eq!(
        ownership_columns(&pool).await,
        10,
        "0032: no ownership column may be dropped by a refused run"
    );
    let prepare_ledger: i64 =
        sqlx::query_scalar("SELECT count(*) FROM _sqlx_migrations WHERE version = 32")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        prepare_ledger, 1,
        "0032: version 32 must not be retired by a refused run"
    );
    let bridges: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace \
         WHERE n.nspname = 'public' AND p.proname LIKE 'fn\\_%\\_tenancy\\_bridge'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        bridges, 5,
        "0032: no bridge function may be dropped by a refused run"
    );
}

/// The rename hole that only opens in a half-unwound tranche.
///
/// Checking only the five ownership keys for a rename looks sufficient, because
/// every table carrying an adopted unique constraint carries an ownership key
/// too. That holds only while 0041 is fully applied. Drop the five keys -- the
/// state an interrupted rollback leaves -- rename `rmk_policies`, and a
/// key-only rename check finds nothing; the drop loop then resolves
/// `public.rmk_policies` to NULL, skips it as "already gone", drops the other two
/// unique constraints, deletes ledger version 41 and reports success. The
/// tranche is left with `rmk_policies_id_tenant_id_key` attached to the renamed
/// table and a ledger claiming 0041 was never applied.
///
/// Measured before the unique arm was added to the guard: exactly that, silently.
#[sqlx::test(migrations = "./migrations")]
async fn a_half_unwound_0041_still_refuses_after_a_rename(pool: PgPool) {
    for stmt in [
        "ALTER TABLE archival_batches DROP CONSTRAINT archival_batches_tenant_agent_fkey",
        "ALTER TABLE audit_logs DROP CONSTRAINT audit_logs_tenant_agent_fkey",
        "ALTER TABLE entities DROP CONSTRAINT entities_tenant_agent_fkey",
        "ALTER TABLE memory_graph DROP CONSTRAINT memory_graph_tenant_agent_fkey",
        "ALTER TABLE rmk_policies DROP CONSTRAINT rmk_policies_tenant_agent_fkey",
        "ALTER TABLE rmk_policies RENAME TO rmk_policies_old",
    ] {
        sqlx::query(sqlx::AssertSqlSafe(stmt.to_owned()))
            .execute(&pool)
            .await
            .unwrap();
    }

    let (code, message) = expect_refusal(&pool, CONSTRAINTS_DOWN_SQL, "half-unwound 0041").await;
    assert_eq!(code, "55000", "{message}");
    assert!(
        message.contains("rmk_policies_id_tenant_id_key is now on rmk_policies_old"),
        "the refusal must be raised by the UNIQUE arm of the rename guard, since no \
         ownership key is left to raise it: {message}"
    );

    // All three unique constraints survive, including the one on the renamed
    // table that a name-based drop would have orphaned.
    let uniques: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pg_constraint WHERE contype = 'u' AND conname IN ( \
             'archival_batches_id_tenant_id_key', 'entities_id_tenant_id_key', \
             'rmk_policies_id_tenant_id_key')",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        uniques, 3,
        "a refused run must not drop the two constraints it could still reach"
    );
    let ledger: i64 =
        sqlx::query_scalar("SELECT count(*) FROM _sqlx_migrations WHERE version = 41")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        ledger, 1,
        "and must not record the tranche as unapplied while a constraint is still attached"
    );
}

/// A table moved to another schema is refused, not skipped.
///
/// `ALTER TABLE ... SET SCHEMA` is the variant a rename guard misses. A rename
/// leaves the table's indexes where they were, so a lookup by index name still
/// resolves and the mismatch is visible; a schema move takes the indexes with it,
/// so `to_regclass('public.<index>')` yields NULL and the guard finds nothing to
/// compare.
///
/// Measured before both scripts learned to ask whether the table is in `public`
/// at all: rollback/0033 emitted one "does not exist, skipping" notice and
/// COMMITted, retiring ledger versions 33-40 while the moved table kept its
/// bridge trigger and its lookup index; rollback/0041 dropped what it could reach
/// and left the moved table's unique constraint attached.
///
/// Two phases, because each script reaches the check from a different state.
/// rollback/0033 refuses on the still-applied-0041 guard long before it looks at
/// tables, so its own missing-table check is only reachable once 0041 is properly
/// unwound.
#[sqlx::test(migrations = "./migrations")]
async fn a_table_moved_to_another_schema_refuses_both_rollbacks(pool: PgPool) {
    async fn exec(pool: &PgPool, stmt: &str) {
        sqlx::query(sqlx::AssertSqlSafe(stmt.to_owned()))
            .execute(pool)
            .await
            .unwrap_or_else(|e| panic!("{stmt}: {e}"));
    }
    async fn moved_unique_survives(pool: &PgPool) -> bool {
        sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM pg_constraint \
             WHERE conname = 'archival_batches_id_tenant_id_key' \
               AND conrelid = 'elsewhere.archival_batches'::regclass)",
        )
        .fetch_one(pool)
        .await
        .unwrap()
    }

    exec(&pool, "CREATE SCHEMA elsewhere").await;
    // The sibling key gone is what lets the move escape a key-only check.
    exec(
        &pool,
        "ALTER TABLE archival_batches DROP CONSTRAINT archival_batches_tenant_agent_fkey",
    )
    .await;
    exec(&pool, "ALTER TABLE archival_batches SET SCHEMA elsewhere").await;

    // ── Phase 1: rollback/0041 ────────────────────────────────────────────────
    let (code, message) = expect_refusal(&pool, CONSTRAINTS_DOWN_SQL, "0041").await;
    assert_eq!(code, "55000", "0041: {message}");
    assert!(
        message.contains("archival_batches"),
        "0041 must name the table that is no longer in public: {message}"
    );
    assert!(
        message.contains("Nothing has been dropped"),
        "0041 must say the database is unchanged: {message}"
    );
    assert!(
        moved_unique_survives(&pool).await,
        "0041: the moved table's unique constraint must survive the refusal"
    );
    let ledger: i64 =
        sqlx::query_scalar("SELECT count(*) FROM _sqlx_migrations WHERE version = 41")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(ledger, 1, "0041: version 41 must not be retired");

    // ── Phase 2: rollback/0033, reached only once 0041 is properly unwound ────
    exec(
        &pool,
        "ALTER TABLE elsewhere.archival_batches SET SCHEMA public",
    )
    .await;
    sqlx::raw_sql(CONSTRAINTS_DOWN_SQL)
        .execute(&pool)
        .await
        .expect("0041 rollback must succeed once the table is back in public");
    exec(&pool, "ALTER TABLE archival_batches SET SCHEMA elsewhere").await;

    let (code, message) = expect_refusal(&pool, INDEXES_DOWN_SQL, "0033").await;
    assert_eq!(code, "55000", "0033: {message}");
    assert!(
        message.contains("archival_batches"),
        "0033 must name the table that is no longer in public: {message}"
    );
    let index_survives: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE c.relname = 'idx_archival_batches_tenant' AND n.nspname = 'elsewhere')",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(
        index_survives,
        "0033: the moved lookup index must survive the refusal"
    );
    let build_ledger: i64 =
        sqlx::query_scalar("SELECT count(*) FROM _sqlx_migrations WHERE version BETWEEN 33 AND 40")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        build_ledger, 8,
        "0033: versions 33-40 must not be retired while a tranche index is still live elsewhere"
    );
}

// ── Identity by name, one object kind further down ──────────────────────────

/// An index holding a reserved name but built by someone else is not dropped.
///
/// Migrations 0033-0040 build with `CREATE INDEX ... IF NOT EXISTS`, so a
/// pre-existing index already holding a reserved name on the expected table is
/// skipped with a notice and never replaced. A rollback guard that checked only
/// `indrelid` would then treat it as the tranche's own, drop it, and retire ledger
/// versions 33-40 over an object the tranche never created -- the same defect the
/// 0041 constraint guards were rewritten to close, one object kind down.
#[sqlx::test(migrations = "./migrations")]
async fn the_indexes_rollback_refuses_an_index_it_does_not_own(pool: PgPool) {
    // 0041 first: its unique constraints own three of the eight indexes.
    sqlx::raw_sql(CONSTRAINTS_DOWN_SQL)
        .execute(&pool)
        .await
        .expect("0041 rollback");

    for stmt in [
        "DROP INDEX idx_audit_logs_tenant",
        // Right table, reserved name, right arity, wrong columns -- exactly what
        // the build's IF NOT EXISTS would have skipped over. Two columns on
        // purpose: a one-column decoy is caught by the arity check and never
        // reaches the column-name comparison.
        "CREATE INDEX idx_audit_logs_tenant ON audit_logs (event_type, created_at)",
    ] {
        sqlx::query(sqlx::AssertSqlSafe(stmt.to_owned()))
            .execute(&pool)
            .await
            .unwrap();
    }

    let (code, message) = expect_refusal(&pool, INDEXES_DOWN_SQL, "0033").await;
    assert_eq!(code, "55000", "{message}");
    assert!(
        message.contains("idx_audit_logs_tenant") && message.contains("columns are"),
        "the refusal must name the index and say its columns differ: {message}"
    );

    let occupant_survives: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM pg_indexes WHERE indexname = 'idx_audit_logs_tenant' \
           AND indexdef LIKE '%event_type%')",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(
        occupant_survives,
        "the unrelated index must be left exactly where it was"
    );
    let ledger: i64 =
        sqlx::query_scalar("SELECT count(*) FROM _sqlx_migrations WHERE version BETWEEN 33 AND 40")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        ledger, 8,
        "versions 33-40 must not be retired over an object the tranche never built"
    );
}

/// An unrelated table's same-named trigger must not block the 0032 rollback.
///
/// Trigger names are unique only per table. Discovering the five bridges by
/// `tgname` classified any other table's `trg_entities_tenancy_bridge` as a moved
/// tranche trigger and refused this rollback permanently, with all five real
/// bridges correctly in place -- a false refusal with no way out, since
/// rollback/0032 cannot remove a trigger it never created. The bridges are now
/// reached through `tgfoid`, the OID of the zero-argument `trigger`-returning
/// function migration 0032 installs.
#[sqlx::test(migrations = "./migrations")]
async fn an_unrelated_trigger_of_the_same_name_does_not_block_the_prepare_rollback(pool: PgPool) {
    for stmt in [
        "CREATE TABLE zz_trigger_decoy (id uuid PRIMARY KEY DEFAULT gen_random_uuid(), v int)",
        "CREATE FUNCTION zz_decoy_fn() RETURNS trigger LANGUAGE plpgsql AS $$ \
         BEGIN RETURN NEW; END $$",
        // Same name as a tranche bridge, on a table that has nothing to do with it.
        "CREATE TRIGGER trg_entities_tenancy_bridge BEFORE INSERT ON zz_trigger_decoy \
         FOR EACH ROW EXECUTE FUNCTION zz_decoy_fn()",
    ] {
        sqlx::query(sqlx::AssertSqlSafe(stmt.to_owned()))
            .execute(&pool)
            .await
            .unwrap();
    }
    // The decoy is looked up through this query twice: once now, to prove the
    // fixture built something, and once AFTER the rollback, to prove the
    // rollback left it alone. Re-running it is the whole point -- an assertion
    // on the value captured here could only ever restate the fixture, which is
    // exactly the dead assertion this test used to end on.
    const DECOY_PRESENT: &str =
        "SELECT EXISTS (SELECT 1 FROM pg_trigger t JOIN pg_class c ON c.oid = t.tgrelid \
         WHERE t.tgname = 'trg_entities_tenancy_bridge' AND c.relname = 'zz_trigger_decoy')";
    let decoy_exists: bool = sqlx::query_scalar(DECOY_PRESENT)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(
        decoy_exists,
        "the decoy trigger must exist to be a real test"
    );

    for script in [CONSTRAINTS_DOWN_SQL, INDEXES_DOWN_SQL] {
        sqlx::raw_sql(script).execute(&pool).await.expect("unwind");
    }
    sqlx::raw_sql(PREPARE_DOWN_SQL)
        .execute(&pool)
        .await
        .expect("an unrelated same-named trigger must not block the prepare rollback");

    // And it did the work rather than merely declining to fail.
    let columns: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM information_schema.columns \
         WHERE table_schema = 'public' AND column_name IN ('agent_uuid', 'tenant_id') \
           AND table_name IN ('archival_batches', 'audit_logs', 'entities', 'memory_graph', \
                              'rmk_policies')",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        columns, 0,
        "the rollback must have dropped the ownership columns"
    );
    // Re-queried against the catalog as it stands AFTER the rollback. This is
    // the assertion the test exists for: the rollback did its own work above,
    // and the unrelated trigger -- and the function it calls -- are still here.
    let decoy_survives: bool = sqlx::query_scalar(DECOY_PRESENT)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(
        decoy_survives,
        "the decoy trigger is none of the rollback's business and must survive it"
    );
    let decoy_fn_survives: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace \
         WHERE n.nspname = 'public' AND p.proname = 'zz_decoy_fn')",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(
        decoy_fn_survives,
        "and the function the decoy calls must survive it too"
    );
}

/// An overloaded bridge-function name must not make 0028 refuse forever.
///
/// PostgreSQL allows overloading, and the guard matched only `proname`. After the
/// real zero-argument bridges are gone, an unrelated
/// `fn_entities_tenancy_bridge(integer)` kept 0028 reporting tranche 1 as applied
/// and directing the operator to rollback/0032 -- which cannot remove an overload
/// it never created. The lookup is now pinned to the zero-argument
/// `trigger`-returning signature migration 0032 installs.
#[sqlx::test(migrations = "./migrations")]
async fn an_overloaded_bridge_function_name_does_not_block_step_one_rollback(pool: PgPool) {
    for script in [
        CONSTRAINTS_DOWN_SQL,
        INDEXES_DOWN_SQL,
        PREPARE_DOWN_SQL,
        GRANTS_HARDENING_DOWN_SQL,
        GRANTS_DOWN_SQL,
    ] {
        sqlx::raw_sql(script).execute(&pool).await.expect("unwind");
    }

    sqlx::query(sqlx::AssertSqlSafe(
        "CREATE FUNCTION public.fn_entities_tenancy_bridge(integer) RETURNS integer \
         LANGUAGE sql AS $$ SELECT $1 $$"
            .to_owned(),
    ))
    .execute(&pool)
    .await
    .unwrap();

    sqlx::raw_sql(STEP_ONE_DOWN_SQL)
        .execute(&pool)
        .await
        .expect("an unrelated overload must not be mistaken for an installed bridge");

    let gone: bool = sqlx::query_scalar(
        "SELECT NOT EXISTS (SELECT 1 FROM information_schema.columns \
         WHERE table_schema = 'public' AND table_name = 'agents' AND column_name = 'tenant_id')",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(
        gone,
        "the rollback must have run, not just declined to fail"
    );
}

/// A wrong-shaped index must not be adopted as the tranche's unique key.
///
/// `ADD CONSTRAINT ... UNIQUE USING INDEX` takes the constraint's columns from
/// whatever index it is handed. Migrations 0038-0040 build with `IF NOT EXISTS`,
/// so a pre-existing index already holding a reserved name is skipped and never
/// replaced — and 0041 would then adopt it, producing a constraint that is not
/// the promised key while sqlx records version 41 as applied. Measured before the
/// check: `archival_batches_id_tenant_id_key ON (id, agent_uuid)` was adopted as
/// `UNIQUE (id, agent_uuid)` without complaint.
///
/// The identity test elsewhere in 0041 cannot catch this: it only runs when a
/// CONSTRAINT already exists, and here only an index does.
#[sqlx::test(migrations = "./migrations")]
async fn a_wrong_shaped_index_is_not_adopted_as_the_unique_key(pool: PgPool) {
    for stmt in [
        "ALTER TABLE archival_batches DROP CONSTRAINT archival_batches_id_tenant_id_key",
        "CREATE UNIQUE INDEX archival_batches_id_tenant_id_key \
         ON archival_batches (id, agent_uuid)",
    ] {
        sqlx::query(sqlx::AssertSqlSafe(stmt.to_owned()))
            .execute(&pool)
            .await
            .unwrap();
    }

    let (code, message) = expect_refusal(&pool, CONSTRAINTS_UP, "wrong-shaped index").await;
    assert_eq!(code, "55000", "{message}");
    assert!(
        message.contains("its columns are '{id,agent_uuid}', expected '{id,tenant_id}'"),
        "the refusal must name the shape that differs: {message}"
    );

    let adopted: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pg_constraint \
         WHERE conname = 'archival_batches_id_tenant_id_key' AND contype = 'u'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        adopted, 0,
        "the wrong-shaped index must not have become a constraint"
    );
}

/// A renamed CONSTRAINT, as opposed to a renamed table, is also refused.
///
/// Renaming the constraint hides it from every guard that requires its original
/// `conname` — and for a unique constraint PostgreSQL renames the backing index
/// along with it, so there is no name left to find it by at all. Left undetected,
/// rollback/0041 drops the others and retires ledger version 41 while the renamed
/// object stays attached; 0033 then misses it too and clears 33-40; and 0032
/// finally fails on the surviving column dependency.
///
/// The foreign key is found from the parent's unique key by OID. The unique
/// constraint is found by shape: two columns over exactly (id, tenant_id) on a
/// tranche table, not deferrable, not partition-inherited.
#[sqlx::test(migrations = "./migrations")]
async fn a_renamed_constraint_is_refused_by_shape_not_missed_by_name(pool: PgPool) {
    for (label, rename, expected_phrase) in [
        (
            "renamed foreign key",
            "ALTER TABLE entities RENAME CONSTRAINT entities_tenant_agent_fkey TO zz_renamed_fk",
            "zz_renamed_fk on entities is a tranche 1 ownership key under an unexpected name",
        ),
        (
            "renamed unique constraint",
            "ALTER TABLE entities RENAME CONSTRAINT entities_id_tenant_id_key TO zz_renamed_uq",
            "zz_renamed_uq on entities is a tranche 1 unique key under an unexpected name",
        ),
    ] {
        sqlx::query(sqlx::AssertSqlSafe(rename.to_owned()))
            .execute(&pool)
            .await
            .unwrap_or_else(|e| panic!("{label}: {e}"));

        let (code, message) = expect_refusal(&pool, CONSTRAINTS_DOWN_SQL, label).await;
        assert_eq!(code, "55000", "{label}: {message}");
        assert!(
            message.contains(expected_phrase),
            "{label}: the refusal must identify it by shape and report its current name \
             (expected {expected_phrase:?}): {message}"
        );
        assert!(
            message.contains("Nothing has been dropped"),
            "{label}: {message}"
        );

        let ledger: i64 =
            sqlx::query_scalar("SELECT count(*) FROM _sqlx_migrations WHERE version = 41")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(ledger, 1, "{label}: version 41 must not be retired");
    }

    // Both renamed objects survived both refusals.
    let survivors: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pg_constraint WHERE conname IN ('zz_renamed_fk', 'zz_renamed_uq')",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(survivors, 2, "neither renamed constraint may be dropped");
}

// ── Concurrency: the parent must be locked before child cleanup ─────────────

/// The RMK worker can insert a policy into the gap between the child cleanup and
/// the parent delete.
///
/// Deleting the policies and then the agent is not enough on its own. While the
/// parent row still exists the bridge resolves ownership and the NO ACTION key
/// permits an insert, so a policy written after the `rmk_policies` delete and
/// before the `agents` delete makes the parent delete fail — the same 500, under
/// ordinary worker concurrency rather than a rare race.
///
/// Forced deterministically rather than by sleeping: a second connection opens a
/// transaction and inserts a policy, which takes `FOR KEY SHARE` on the agent row
/// through the foreign-key check and holds it. `delete_agent` then blocks — on
/// `SELECT ... FOR UPDATE` with the fix, on the parent `DELETE` without it — and
/// the test waits until a backend in *this test's own database* is genuinely
/// blocked before releasing the other transaction. The readiness gate is
/// described in full at the loop below; it deliberately does not use a bare
/// `pg_locks` scan, which is cluster-wide and would be satisfied by an unrelated
/// test. No timing assumption survives in the assertion path.
///
/// Driven through the real `store::delete_agent`, because the ordering under test
/// is that function's.
///
/// ── SERIALIZED LANE (issue #27) ─────────────────────────────────────────────
///
/// `#[ignore]` here removes this test from the ORDINARY PARALLEL SUITE only. It
/// is not skipped: `.github/workflows/benchmarks.yml` runs it, by this exact
/// name, with `--ignored --exact --test-threads=1`, as a required job that fails
/// if the filter matches no test.
///
/// The gate below is UNCHANGED and stays strict. It must keep failing when it
/// observes nothing blocked -- letting it pass on an unobserved interleaving is
/// what would make the concurrency proof vacuous, and that was explicitly
/// rejected. What is intermittent is not the contract but whether a given run
/// exercises the interleaving the proof depends on: at `--test-threads=16` on a
/// 10-core box, `delete_agent` acquires a pooled connection and then sits
/// `state=idle` with an empty query, never sending its `BEGIN`, so it is
/// genuinely unblocked and correctly absent from `pg_blocking_pids`.
///
/// The record in issue #27 previously said this had never appeared at CI's
/// parallelism. That is no longer true: the required Benchmark Proof job failed
/// on it at `2a68a78` and again at `67ba097`, so the parallelism argument no
/// longer holds and repeated reruns are not a resolution. Serializing this one
/// test is the owner-sanctioned lowest-risk remedy; the deeper tokio/sqlx
/// root-cause investigation remains open in #27 and is not closed by this.
///
/// Deliberately narrow: this attribute serializes exactly one test. It does not
/// pin the suite's parallelism, which was the other rejected option.
#[ignore = "issue #27: run serialized via the dedicated Benchmark Proof job \
            (--ignored --exact --test-threads=1); the strict gate is unchanged"]
#[sqlx::test(migrations = "./migrations")]
async fn a_policy_inserted_during_cleanup_cannot_break_agent_deletion(pool: PgPool) {
    let state = crate::test_support::test_state(pool.clone());
    insert_agent(&pool, "doomed", Some(TENANT)).await;
    sqlx::query(
        "INSERT INTO rmk_policies (agent_id, pressure_a, pressure_b, kp, ki, \
         graph_bonus_weight, retrieval_threshold) VALUES ('doomed', 0, 0, 0, 0, 0, 0)",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO rmk_episodes (agent_id, policy_id, task_success, token_savings, \
                                   retrieval_precision, eviction_cost, reward) \
         SELECT 'doomed', id, 1, 1, 1, 1, 1 FROM rmk_policies WHERE agent_id = 'doomed'",
    )
    .execute(&pool)
    .await
    .unwrap();

    // The worker: a policy insert held open inside a transaction.
    let mut worker = pool.acquire().await.unwrap();
    sqlx::raw_sql("BEGIN").execute(&mut *worker).await.unwrap();
    sqlx::query(
        "INSERT INTO rmk_policies (agent_id, pressure_a, pressure_b, kp, ki, \
         graph_bonus_weight, retrieval_threshold) VALUES ('doomed', 9, 0, 0, 0, 0, 0)",
    )
    .execute(&mut *worker)
    .await
    .expect("the worker's insert is legitimate: the agent still exists");

    let deleter = tokio::spawn({
        let state = state.clone();
        async move { crate::memory::store::delete_agent(&state, "doomed").await }
    });

    // Wait for the deleter to actually be blocked, rather than assuming it is.
    //
    // Scoped to this test's own database and to a backend something is genuinely
    // blocking. `pg_locks` is cluster-wide, so a bare `WHERE NOT granted` is
    // satisfied by any other test's contention -- and `#[sqlx::test]` gives each
    // test its own database on one shared cluster while `cargo test` runs them in
    // parallel, so that gate could release the worker before the deleter ever
    // blocked, leaving the test to exercise no interleaving at all.
    //
    // Filtering `pg_locks` by `database` or `relation` does NOT fix it: measured
    // on pg16, a FOR UPDATE waiting on FOR KEY SHARE waits on a `transactionid`
    // lock, which carries neither column. `pg_stat_activity` does carry
    // `datname`, and `pg_blocking_pids` confirms the backend is blocked rather
    // than merely idle.
    // Bounded by WALL CLOCK, not by an iteration count, so the budget means what
    // it says regardless of how slow each round trip to a shared cluster is.
    // The cost of the wide bound is paid only on the failure path -- a healthy
    // run leaves this loop on the first or second poll.
    //
    // KNOWN FLAKE, still open. Under `cargo test -- --test-threads=16` on a
    // 10-core machine this gate expires roughly 1 run in 6. Widening the budget
    // did NOT fix it and the original diagnosis -- "the 5s poll budget is too
    // small" -- is wrong. What actually happens, measured on pg16 with the
    // instrumentation below:
    //
    //   * the gate polls ~2250 times over the full 60s, so the loop is healthy;
    //   * `delete_agent` DOES acquire a pooled connection (`pool[max=5 size=2
    //     idle=0]`), and its backend sits `state=idle` with an EMPTY query --
    //     it never sends even the `BEGIN`;
    //   * it is therefore never blocked, so `pg_blocking_pids` correctly
    //     reports nothing, and the gate correctly refuses to accept the run;
    //   * the moment the test body stops and joins it, it returns `Ok(true)`
    //     and every remaining assertion in this test passes.
    //
    // So the deletion contract is not in question -- only whether a given run
    // exercised the interleaving. Replacing `tokio::spawn` with `tokio::join!`,
    // so both futures are driven by the same task, did not change the rate
    // either, which rules out plain task starvation; `#[sqlx::test]` builds a
    // `new_current_thread` runtime (sqlx-core `rt/mod.rs::test_block_on`) and
    // 16 of those oversubscribe 10 cores, but the stall is between acquiring a
    // connection and writing the first byte on it.
    //
    // Deliberately NOT worked around by letting the gate pass when it sees
    // nothing: that is the one change that would make this test stop proving
    // the thing it exists to prove. Left failing loudly, with the diagnostics
    // below, for whoever finishes it. CI runs `cargo test` at core count, where
    // it has not been observed.
    const READINESS_BUDGET: std::time::Duration = std::time::Duration::from_secs(60);
    let gate_started = std::time::Instant::now();
    let mut blocked = false;
    let mut polls = 0u32;
    let mut peer_detail = String::new();
    let mut finished_without_blocking = false;
    while gate_started.elapsed() < READINESS_BUDGET {
        polls += 1;
        // Reports every peer backend in this test's database, not just the
        // blocked count: a bare count cannot distinguish "the deleter never
        // connected" from "it connected and is idle" from "it is blocked", and
        // three rounds of diagnosis were spent because it did not.
        let (waiting, detail): (i64, Option<String>) = sqlx::query_as(
            "SELECT count(*) FILTER (WHERE cardinality(pg_blocking_pids(a.pid)) > 0), \
                    string_agg(format('[state=%s wait=%s/%s blockers=%s q=%s]', \
                                      a.state, a.wait_event_type, a.wait_event, \
                                      pg_blocking_pids(a.pid), left(a.query, 60)), ' ') \
               FROM pg_stat_activity a \
              WHERE a.datname = current_database() \
                AND a.pid <> pg_backend_pid()",
        )
        .fetch_one(&mut *worker)
        .await
        .unwrap();
        peer_detail = format!(
            "{} pool[max={} size={} idle={}]",
            detail.unwrap_or_default(),
            pool.options().get_max_connections(),
            pool.size(),
            pool.num_idle()
        );
        if waiting > 0 {
            blocked = true;
            break;
        }
        // Waiting past this point cannot change the answer: a finished task is
        // never going to block, so the remaining budget would buy only a slower
        // failure with less to say.
        if deleter.is_finished() {
            finished_without_blocking = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    let gate_waited = gate_started.elapsed();

    // Release the worker and join the task BEFORE asserting, so a failed gate
    // cannot leave the spawned task detached and still holding a pooled
    // connection while the test database is torn down around it.
    sqlx::raw_sql("COMMIT").execute(&mut *worker).await.unwrap();
    drop(worker);
    // Bounded, so a future regression that turns this wait into a real deadlock
    // fails with a message instead of hanging until CI's own timeout kills it.
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(30), deleter)
        .await
        .expect("the deletion must not hang once the worker has committed")
        .expect("the deleting task must not panic");

    // Rendered before `outcome` is consumed below, so a failing gate reports
    // what the deleter actually did rather than only that it was not seen.
    let deleter_said = match &outcome {
        Ok(deleted) => format!("Ok({deleted})"),
        Err(e) => format!("Err({e})"),
    };
    assert!(
        blocked,
        "the deleter must have been waiting on the worker's lock, or this test \
         proves nothing about the interleaving ({polls} polls over {gate_waited:?}; \
         finished_without_blocking={finished_without_blocking}; \
         peers {peer_detail}; the deleter returned {deleter_said}). See the \
         KNOWN FLAKE note above before concluding this is a regression."
    );
    let deleted = outcome.expect("deletion must survive a policy inserted during cleanup");
    assert!(deleted, "the agent existed, so it must report a deletion");

    // Both policies are gone, including the one written mid-cleanup.
    let left: i64 = sqlx::query_scalar("SELECT count(*) FROM rmk_policies WHERE agent_id='doomed'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(left, 0, "no policy may survive, whenever it was written");
    let agents_left: i64 =
        sqlx::query_scalar("SELECT count(*) FROM agents WHERE agent_id='doomed'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(agents_left, 0, "the agent must be gone");

    // The retained-episode contract is unchanged by the lock.
    let (episodes, orphaned): (i64, i64) = sqlx::query_as(
        "SELECT count(*), count(*) FILTER (WHERE policy_id IS NULL) \
         FROM rmk_episodes WHERE agent_id = 'doomed'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(episodes, 1, "the episode log is still retained");
    assert_eq!(orphaned, 1, "and its policy reference is still NULLed");
}

/// The lock must not change what deletion does when nothing is racing it.
#[sqlx::test(migrations = "./migrations")]
async fn locking_the_agent_preserves_the_uncontended_contract(pool: PgPool) {
    let state = crate::test_support::test_state(pool.clone());

    // A nonexistent agent still reports false, and still cleans up orphans.
    sqlx::query("INSERT INTO entities (agent_id, name, entity_type) VALUES ('ghost','e','person')")
        .execute(&pool)
        .await
        .unwrap();
    let deleted = crate::memory::store::delete_agent(&state, "ghost")
        .await
        .expect("deleting a nonexistent agent must not error");
    assert!(!deleted, "a nonexistent agent must report false");
    let orphans: i64 = sqlx::query_scalar("SELECT count(*) FROM entities WHERE agent_id='ghost'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        orphans, 0,
        "orphaned child rows must still be cleaned up, as they were before the lock"
    );

    // Pre-backfill policies and other tenants are unaffected.
    insert_agent(&pool, "doomed", Some(TENANT)).await;
    insert_agent(&pool, "bystander", Some(OTHER_TENANT)).await;
    for stmt in [
        "ALTER TABLE rmk_policies DISABLE TRIGGER trg_rmk_policies_tenancy_bridge",
        "INSERT INTO rmk_policies (agent_id, pressure_a, pressure_b, kp, ki, \
         graph_bonus_weight, retrieval_threshold) VALUES ('doomed', 2, 0, 0, 0, 0, 0)",
        "ALTER TABLE rmk_policies ENABLE TRIGGER trg_rmk_policies_tenancy_bridge",
        "INSERT INTO rmk_policies (agent_id, pressure_a, pressure_b, kp, ki, \
         graph_bonus_weight, retrieval_threshold) VALUES ('bystander', 1, 0, 0, 0, 0, 0)",
    ] {
        sqlx::query(sqlx::AssertSqlSafe(stmt.to_owned()))
            .execute(&pool)
            .await
            .unwrap();
    }
    assert!(crate::memory::store::delete_agent(&state, "doomed")
        .await
        .unwrap());
    let (doomed, bystander): (i64, i64) = sqlx::query_as(
        "SELECT count(*) FILTER (WHERE agent_id='doomed'), \
                count(*) FILTER (WHERE agent_id='bystander') FROM rmk_policies",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(doomed, 0, "the pre-backfill policy must still be removed");
    assert_eq!(bystander, 1, "the other tenant must be untouched");
}

// ── The 0033 ordering guard must not depend on constraint names ─────────────

/// Renaming 0041's constraints must not let the indexes rollback run early.
///
/// The ordering guard promised to refuse while 0041 is attached, but still found
/// its objects by `conname`. Renaming a foreign key hid it even though `conindid`
/// still identified it, and renaming a unique constraint renames its backing
/// index too, so the index-OID lookup found nothing either. Measured against the
/// previous head with all eight renamed: the script COMMITTED, dropping all five
/// lookup indexes and retiring versions 33-40 while all eight constraints stayed
/// attached.
///
/// Every one of the eight is renamed, so nothing left under an expected name can
/// do the blocking and the refusal cannot pass for the wrong reason.
#[sqlx::test(migrations = "./migrations")]
async fn the_indexes_rollback_refuses_while_renamed_0041_constraints_remain(pool: PgPool) {
    for stmt in [
        "ALTER TABLE archival_batches RENAME CONSTRAINT archival_batches_tenant_agent_fkey TO zz_f1",
        "ALTER TABLE audit_logs   RENAME CONSTRAINT audit_logs_tenant_agent_fkey   TO zz_f2",
        "ALTER TABLE entities     RENAME CONSTRAINT entities_tenant_agent_fkey     TO zz_f3",
        "ALTER TABLE memory_graph RENAME CONSTRAINT memory_graph_tenant_agent_fkey TO zz_f4",
        "ALTER TABLE rmk_policies RENAME CONSTRAINT rmk_policies_tenant_agent_fkey TO zz_f5",
        "ALTER TABLE archival_batches RENAME CONSTRAINT archival_batches_id_tenant_id_key TO zz_u1",
        "ALTER TABLE entities     RENAME CONSTRAINT entities_id_tenant_id_key      TO zz_u2",
        "ALTER TABLE rmk_policies RENAME CONSTRAINT rmk_policies_id_tenant_id_key  TO zz_u3",
    ] {
        sqlx::query(sqlx::AssertSqlSafe(stmt.to_owned()))
            .execute(&pool)
            .await
            .unwrap();
    }
    let under_expected_names: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pg_constraint WHERE conname LIKE '%tenant\\_agent\\_fkey' \
            OR conname IN ('archival_batches_id_tenant_id_key', 'entities_id_tenant_id_key', \
                           'rmk_policies_id_tenant_id_key')",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        under_expected_names, 0,
        "nothing may be left under an expected name, or an un-renamed object \
         could be what blocks the rollback and the test proves nothing"
    );

    let (code, message) = expect_refusal(&pool, INDEXES_DOWN_SQL, "0033").await;
    assert_eq!(code, "2BP01", "{message}");
    // All eight, not a sample: `remaining` is a string_agg over every matched
    // row, so a regression that stopped detecting exactly one table would still
    // refuse — and every other assertion here is agnostic to WHICH objects were
    // found. Asserting a subset would leave partial detection invisible.
    for renamed in [
        "zz_f1", "zz_f2", "zz_f3", "zz_f4", "zz_f5", "zz_u1", "zz_u2", "zz_u3",
    ] {
        assert!(
            message.contains(renamed),
            "the refusal must name {renamed} under its current name: {message}"
        );
    }

    let ledger: i64 =
        sqlx::query_scalar("SELECT count(*) FROM _sqlx_migrations WHERE version BETWEEN 33 AND 40")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(ledger, 8, "versions 33-40 must not be retired");
    assert_eq!(
        count_tranche1_indexes(&pool).await,
        5,
        "the three adopted unique indexes now carry renamed names, but the five \
         lookup indexes must all survive"
    );
    let still_attached: i64 =
        sqlx::query_scalar("SELECT count(*) FROM pg_constraint WHERE conname LIKE 'zz\\_%'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(still_attached, 8, "no renamed constraint may be dropped");
}

/// Migration 0030's and 0029's objects have the tranche's exact shape.
///
/// `credential_agent_grants_agent_fkey` is `(tenant_id, agent_uuid)` referencing
/// `agents (tenant_id, id)` through the same `conindid`, and
/// `credentials_id_tenant_id_key` is `UNIQUE (id, tenant_id)` — measured. A
/// shape-only rewrite of the guard, with the table scope dropped as a
/// simplification, would report both as tranche 1 objects and refuse this
/// rollback permanently. This pins the scope that stops it.
#[sqlx::test(migrations = "./migrations")]
async fn neighbouring_migrations_of_the_same_shape_do_not_block_the_indexes_rollback(pool: PgPool) {
    let lookalikes: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pg_constraint \
          WHERE conname IN ('credential_agent_grants_agent_fkey', 'credentials_id_tenant_id_key')",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        lookalikes, 2,
        "both look-alikes must be present for this test to mean anything"
    );

    sqlx::raw_sql(CONSTRAINTS_DOWN_SQL)
        .execute(&pool)
        .await
        .expect("0041 rollback");
    sqlx::raw_sql(INDEXES_DOWN_SQL)
        .execute(&pool)
        .await
        .expect("0030 and 0029 objects must not be mistaken for tranche 1 objects");

    let ledger: i64 =
        sqlx::query_scalar("SELECT count(*) FROM _sqlx_migrations WHERE version BETWEEN 33 AND 40")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(ledger, 0, "the legitimate unwind must actually have run");
}

// ── Complete index identity ─────────────────────────────────────────────────

/// `indkey` does not see ordering, access method, operator class or collation.
///
/// A pre-existing `idx_audit_logs_tenant ON (tenant_id DESC, agent_uuid)` has the
/// same `indkey`, arity, uniqueness, predicate and expression state as the index
/// migrations 0033-0037 build, so every property the guard compared matched and
/// it was treated as tranche-owned. `indoption` is the bitmask that distinguishes
/// them: bit 0 DESC, bit 1 NULLS FIRST, zero for the plain ordering these
/// migrations use.
///
/// Operator class and collation are compared too, and are honestly untestable on
/// this schema: `uuid_ops` is the only btree operator class PostgreSQL ships for
/// `uuid`, and `uuid` is not collatable, so no hostile occupant can differ on
/// those axes here. They are checked because the columns' types are not this
/// guard's to guarantee.
#[sqlx::test(migrations = "./migrations")]
async fn an_index_of_the_same_columns_but_different_semantics_is_not_ours(pool: PgPool) {
    let cases: &[(&str, &[&str], &str)] = &[
        (
            "DESC ordering",
            &[
                "DROP INDEX idx_audit_logs_tenant",
                "CREATE INDEX idx_audit_logs_tenant ON audit_logs (tenant_id DESC, agent_uuid)",
            ],
            "its column ordering is",
        ),
        (
            "NULLS FIRST ordering",
            &[
                "DROP INDEX idx_audit_logs_tenant",
                "CREATE INDEX idx_audit_logs_tenant ON audit_logs (tenant_id NULLS FIRST, agent_uuid)",
            ],
            "its column ordering is",
        ),
        (
            // BRIN, not hash. A hash index is necessarily single-column, so the
            // arity check fires too and the case cannot show the access-method
            // check is doing any work — deleting that check entirely would leave
            // the test green. BRIN takes both columns and is non-unique, so it
            // matches the canonical shape on every axis except the one under
            // test.
            "non-btree access method",
            &[
                "DROP INDEX idx_audit_logs_tenant",
                "CREATE INDEX idx_audit_logs_tenant ON audit_logs USING brin (tenant_id, agent_uuid)",
            ],
            "its access method is 'brin', expected 'btree'",
        ),
        (
            "single-column hash index",
            &[
                "DROP INDEX idx_audit_logs_tenant",
                "CREATE INDEX idx_audit_logs_tenant ON audit_logs USING hash (tenant_id)",
            ],
            "it has 1 column(s) (1 key), expected 2",
        ),
    ];

    for (label, setup, expected_phrase) in cases {
        // 0041 off first, so its ordering guard is satisfied and the index guard
        // is what refuses. `reset_to_pre_0041` also rebuilds the three unique
        // indexes the rollback takes with the constraints it drops.
        reset_to_pre_0041(&pool).await;
        for stmt in *setup {
            sqlx::query(sqlx::AssertSqlSafe((*stmt).to_owned()))
                .execute(&pool)
                .await
                .unwrap_or_else(|e| panic!("{label}: setup failed: {e}"));
        }

        let (code, message) = expect_refusal(&pool, INDEXES_DOWN_SQL, label).await;
        assert_eq!(code, "55000", "{label}: {message}");
        assert!(
            message.contains(expected_phrase),
            "{label}: the refusal must name the property that differs \
             (expected {expected_phrase:?}): {message}"
        );

        let survived: bool =
            sqlx::query_scalar("SELECT to_regclass('public.idx_audit_logs_tenant') IS NOT NULL")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(survived, "{label}: the occupant must be left attached");
        let ledger: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM _sqlx_migrations WHERE version BETWEEN 33 AND 40",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(ledger, 8, "{label}: versions 33-40 must not be retired");

        // Put the canonical index back for the next case.
        for stmt in [
            "DROP INDEX idx_audit_logs_tenant",
            "CREATE INDEX idx_audit_logs_tenant ON audit_logs (tenant_id, agent_uuid)",
        ] {
            sqlx::query(sqlx::AssertSqlSafe(stmt.to_owned()))
                .execute(&pool)
                .await
                .unwrap();
        }
    }

    // With every occupant gone, the rollback it kept refusing now runs.
    sqlx::raw_sql(INDEXES_DOWN_SQL)
        .execute(&pool)
        .await
        .expect("the canonical set must still be droppable once the occupants are gone");
    assert_eq!(count_tranche1_indexes(&pool).await, 0);
}

/// The canonical index is still recognised, and an INVALID one is still droppable.
///
/// The second half is the constraint that shapes the whole check: an interrupted
/// `CREATE INDEX CONCURRENTLY` leaves the index INVALID, and dropping it is
/// precisely what this rollback exists to do. Requiring `indisvalid` here would
/// make the one state that needs recovery the one state that cannot be recovered,
/// so validity is deliberately absent from the shape test — it is enforced in
/// migration 0041 instead, which must never adopt an invalid index.
#[sqlx::test(migrations = "./migrations")]
async fn the_canonical_index_is_recognised_valid_or_not(pool: PgPool) {
    sqlx::raw_sql(CONSTRAINTS_DOWN_SQL)
        .execute(&pool)
        .await
        .expect("0041 rollback");

    sqlx::query(
        "UPDATE pg_index SET indisvalid = false \
         WHERE indexrelid = 'public.idx_entities_tenant'::regclass",
    )
    .execute(&pool)
    .await
    .expect("the test role must be able to mark an index invalid");

    sqlx::raw_sql(INDEXES_DOWN_SQL)
        .execute(&pool)
        .await
        .expect("an INVALID tranche index must still be recognised and dropped");

    assert_eq!(
        count_tranche1_indexes(&pool).await,
        0,
        "every tranche index must be gone, including the invalid one"
    );
    let ledger: i64 =
        sqlx::query_scalar("SELECT count(*) FROM _sqlx_migrations WHERE version BETWEEN 33 AND 40")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(ledger, 0, "and the ledger must be cleared for the rebuild");
}

/// The forward migration's copy of the index-shape test, on its own axes.
///
/// `migrations/0041` and `rollback/0033` carry hand-duplicated shape checks, and
/// the round that added ordering, access method, collation and operator class to
/// both only tested the rollback's copy. A typo in the migration's `am` join or
/// its operator-class subquery would let it adopt a DESC-ordered index as the
/// tranche's unique constraint on a real deployment with nothing to catch it.
///
/// PostgreSQL would refuse the adoption itself — `ADD CONSTRAINT ... USING INDEX`
/// rejects an index without default sorting — so what this pins is that the
/// guard fires FIRST, with a diagnostic naming the axis, rather than leaving the
/// operator PostgreSQL's generic "does not have default sorting behavior".
#[sqlx::test(migrations = "./migrations")]
async fn the_migration_also_refuses_an_index_whose_ordering_is_not_ours(pool: PgPool) {
    reset_to_pre_0041(&pool).await;
    for stmt in [
        "DROP INDEX entities_id_tenant_id_key",
        "CREATE UNIQUE INDEX entities_id_tenant_id_key ON entities (id DESC, tenant_id)",
    ] {
        sqlx::query(sqlx::AssertSqlSafe(stmt.to_owned()))
            .execute(&pool)
            .await
            .unwrap();
    }

    let (code, message) = expect_refusal(&pool, CONSTRAINTS_UP, "DESC index at adoption").await;
    assert_eq!(code, "55000", "{message}");
    assert!(
        message.contains("its column ordering is"),
        "the migration's own guard must name the axis, not defer to PostgreSQL's \
         generic sorting error: {message}"
    );
    assert!(
        message.contains("entities_id_tenant_id_key"),
        "and must name the index: {message}"
    );

    let adopted: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pg_constraint \
         WHERE conname = 'entities_id_tenant_id_key' AND contype = 'u'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        adopted, 0,
        "the DESC index must not have become a constraint"
    );
}

// ── Provenance: the tranche creates, or it refuses.  It never adopts. ───────
//
// `ADD COLUMN IF NOT EXISTS`, `CREATE OR REPLACE FUNCTION` and `CREATE OR
// REPLACE TRIGGER` are what make migration 0032 re-runnable, and each is also a
// silent adoption of somebody else's object: rollback/0032 later destroys all
// twenty unconditionally. The column case is the one that loses data, and it is
// the one where the tranche's usual identity-by-shape rule cannot help -- a
// column holds rows, so an identically-shaped `uuid` column is not
// interchangeable with ours. Provenance, carried in `pg_description`, is.
//
// These tests drive the real migration and the real rollback script rather than
// a paraphrase of them, and each one asserts that the object it planted is
// still exactly as it was afterwards.

/// Unwind tranche 1 to the state migration 0032 expects to find: 0041 and the
/// concurrent index builds gone, and 0032's own objects gone with them.
async fn unwind_to_pre_prepare(pool: &PgPool) {
    for script in [CONSTRAINTS_DOWN_SQL, INDEXES_DOWN_SQL, PREPARE_DOWN_SQL] {
        sqlx::raw_sql(script)
            .execute(pool)
            .await
            .expect("the tranche must unwind cleanly before a provenance test plants anything");
    }
}

/// A pre-existing ownership column must stop the migration, not be adopted.
///
/// `ADD COLUMN IF NOT EXISTS` skips a column that is already there, so 0032
/// never created it -- and rollback/0032 then drops it anyway, taking every
/// row's value with it. Nothing in the tranche distinguished the two cases,
/// because shape cannot: a foreign `tenant_id uuid` is byte-for-byte the shape
/// 0032 would have added.
#[sqlx::test(migrations = "./migrations")]
async fn the_prepare_migration_refuses_a_pre_existing_ownership_column(pool: PgPool) {
    unwind_to_pre_prepare(&pool).await;

    // Somebody else's column, with somebody else's data in it.
    let squatter = Uuid::new_v4();
    for stmt in [
        "ALTER TABLE entities ADD COLUMN tenant_id UUID".to_owned(),
        "INSERT INTO entities (agent_id, name, entity_type) VALUES ('other', 'e', 'person')"
            .to_owned(),
        format!("UPDATE entities SET tenant_id = '{squatter}'"),
    ] {
        sqlx::query(sqlx::AssertSqlSafe(stmt))
            .execute(&pool)
            .await
            .unwrap();
    }

    let (code, message) = expect_refusal(
        &pool,
        PREPARE_UP_SQL,
        "0032 must not adopt a column it did not create",
    )
    .await;
    assert_eq!(code, "42701", "duplicate_column is the refusal's SQLSTATE");
    assert!(
        message.contains("entities.tenant_id"),
        "the refusal must name the column it found: {message}"
    );

    // The squatter's data is untouched -- this is the whole point.
    let survived: Option<Uuid> = sqlx::query_scalar("SELECT tenant_id FROM entities")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        survived,
        Some(squatter),
        "the pre-existing column's data must survive the refusal"
    );

    // And the refusal happened before anything was built.
    let bridges: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace \
         WHERE n.nspname = 'public' AND p.proname LIKE 'fn\\_%\\_tenancy\\_bridge'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        bridges, 0,
        "a refusing migration must leave the schema untouched, not half-built"
    );
}

/// A pre-existing bridge function must stop the migration, not be replaced.
///
/// `CREATE OR REPLACE FUNCTION` preserves the OID, so replacing one silently
/// changes the behaviour of every trigger already calling it -- and
/// rollback/0032 then drops it as though the tranche had created it. The
/// assertion that matters is not that the migration failed but that `prosrc` is
/// still the body somebody else wrote.
#[sqlx::test(migrations = "./migrations")]
async fn the_prepare_migration_refuses_a_pre_existing_bridge_function(pool: PgPool) {
    unwind_to_pre_prepare(&pool).await;

    // Same name, same signature, entirely different owner.
    sqlx::raw_sql(
        "CREATE FUNCTION public.fn_entities_tenancy_bridge() RETURNS trigger \
         LANGUAGE plpgsql AS $body$ BEGIN NEW.name := 'not_the_tranches_function'; \
         RETURN NEW; END $body$",
    )
    .execute(&pool)
    .await
    .unwrap();

    let (code, message) = expect_refusal(
        &pool,
        PREPARE_UP_SQL,
        "0032 must not replace a bridge function it did not create",
    )
    .await;
    assert_eq!(
        code, "42723",
        "duplicate_function is the refusal's SQLSTATE"
    );
    assert!(
        message.contains("public.fn_entities_tenancy_bridge()"),
        "the refusal must name the function it found: {message}"
    );

    let body: String = sqlx::query_scalar(
        "SELECT p.prosrc FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace \
         WHERE n.nspname = 'public' AND p.proname = 'fn_entities_tenancy_bridge' \
           AND p.pronargs = 0",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(
        body.contains("not_the_tranches_function"),
        "the pre-existing function's body must not have been replaced in place: {body}"
    );
}

/// A pre-existing bridge trigger must stop the migration, not be replaced.
///
/// The third object kind in the same family, and reachable the same way:
/// `CREATE OR REPLACE TRIGGER` replaces a same-named trigger on the same table,
/// and rollback/0032 drops it afterwards, leaving nothing to restore.
#[sqlx::test(migrations = "./migrations")]
async fn the_prepare_migration_refuses_a_pre_existing_bridge_trigger(pool: PgPool) {
    unwind_to_pre_prepare(&pool).await;

    // A trigger with a tranche name on a tranche table, calling something else.
    // Deliberately NOT named like a bridge function, so the function guard
    // cannot be what fires.
    for stmt in [
        "CREATE FUNCTION public.zz_keeper() RETURNS trigger LANGUAGE plpgsql \
         AS $body$ BEGIN RETURN NEW; END $body$",
        "CREATE TRIGGER trg_entities_tenancy_bridge BEFORE INSERT ON entities \
         FOR EACH ROW EXECUTE FUNCTION public.zz_keeper()",
    ] {
        sqlx::query(sqlx::AssertSqlSafe(stmt.to_owned()))
            .execute(&pool)
            .await
            .unwrap();
    }

    let (code, message) = expect_refusal(
        &pool,
        PREPARE_UP_SQL,
        "0032 must not replace a trigger it did not create",
    )
    .await;
    assert_eq!(code, "42710", "duplicate_object is the refusal's SQLSTATE");
    assert!(
        message.contains("trg_entities_tenancy_bridge on public.entities"),
        "the refusal must name the trigger and the table it is on: {message}"
    );

    let still_calls: String = sqlx::query_scalar(
        "SELECT p.proname FROM pg_trigger t JOIN pg_proc p ON p.oid = t.tgfoid \
         WHERE t.tgname = 'trg_entities_tenancy_bridge' \
           AND t.tgrelid = 'public.entities'::regclass AND NOT t.tgisinternal",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        still_calls, "zz_keeper",
        "the pre-existing trigger must still call its own function"
    );
}

/// The rollback must refuse an unstamped object rather than destroy it.
///
/// The other half of the contract. 0032 refuses to start over an unstamped
/// object, so an unstamped object at rollback time was either never this
/// tranche's or has had its provenance comment removed. Either way this script
/// must not drop it -- and must not have retired completion evidence on the way
/// to finding out, which is why the guard sits ahead of that block.
#[sqlx::test(migrations = "./migrations")]
async fn the_prepare_rollback_refuses_to_destroy_an_unstamped_column(pool: PgPool) {
    insert_completed_checkpoint(&pool, "digest-for-the-provenance-test").await;
    sqlx::raw_sql("COMMENT ON COLUMN entities.tenant_id IS NULL")
        .execute(&pool)
        .await
        .unwrap();

    for script in [CONSTRAINTS_DOWN_SQL, INDEXES_DOWN_SQL] {
        sqlx::raw_sql(script).execute(&pool).await.expect("unwind");
    }
    let (code, message) = expect_refusal(
        &pool,
        PREPARE_DOWN_SQL,
        "the rollback must refuse an ownership column it cannot prove is its own",
    )
    .await;
    assert_eq!(
        code, "55000",
        "object_not_in_prerequisite_state is the refusal's SQLSTATE"
    );
    assert!(
        message.contains("column entities.tenant_id"),
        "the refusal must name the unstamped object: {message}"
    );

    // Nothing was destroyed.
    let columns: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM information_schema.columns \
         WHERE table_schema = 'public' AND column_name IN ('agent_uuid', 'tenant_id') \
           AND table_name IN ('archival_batches', 'audit_logs', 'entities', 'memory_graph', \
                              'rmk_policies')",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(columns, 10, "all ten ownership columns must survive");

    // And nothing was changed on the way to refusing: the guard runs ahead of
    // the block that retires COMPLETED checkpoints.
    let still_completed: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM tenancy_backfill_checkpoints WHERE status = 'COMPLETED'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        still_completed, 1,
        "a refusing rollback must not have retired completion evidence first"
    );
}

/// Provenance must not cost the migration its idempotency.
///
/// The file's header promises re-running it is a no-op, and the guards are the
/// only thing standing between that promise and silent adoption. Re-applying
/// 0032 over its own work must therefore still succeed, and every one of the
/// twenty objects must still carry the stamp afterwards.
#[sqlx::test(migrations = "./migrations")]
async fn re_running_the_prepare_migration_over_its_own_work_is_a_no_op(pool: PgPool) {
    // 0041 depends on the columns, so it stays applied throughout; 0032 is
    // simply run again on top of the finished tranche, twice.
    for attempt in 1..=2 {
        sqlx::raw_sql(PREPARE_UP_SQL)
            .execute(&pool)
            .await
            .unwrap_or_else(|e| {
                panic!("re-run {attempt} of 0032 over its own work must succeed: {e}")
            });
    }

    let stamped: i64 = sqlx::query_scalar(
        "WITH marker AS (SELECT col_description(a.attrelid, a.attnum::int) AS c \
                           FROM pg_attribute a \
                          WHERE a.attrelid IN ('public.archival_batches'::regclass, \
                                               'public.audit_logs'::regclass, \
                                               'public.entities'::regclass, \
                                               'public.memory_graph'::regclass, \
                                               'public.rmk_policies'::regclass) \
                            AND a.attname IN ('agent_uuid', 'tenant_id') \
                            AND a.attnum > 0 AND NOT a.attisdropped \
                          UNION ALL \
                         SELECT obj_description(p.oid, 'pg_proc') \
                           FROM pg_proc p \
                          WHERE p.pronamespace = 'public'::regnamespace \
                            AND p.proname LIKE 'fn\\_%\\_tenancy\\_bridge' AND p.pronargs = 0 \
                          UNION ALL \
                         SELECT obj_description(t.oid, 'pg_trigger') \
                           FROM pg_trigger t \
                          WHERE t.tgname LIKE 'trg\\_%\\_tenancy\\_bridge' AND NOT t.tgisinternal) \
         SELECT count(*) FROM marker WHERE c LIKE 'AEON tenancy tranche 1 (migration 0032).%'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        stamped, 20,
        "ten columns, five functions and five triggers must all carry the provenance marker"
    );
}

/// The marker is a contract between two files, so drift must fail the build.
///
/// It is declared three times -- the guard and the stamping block in
/// migrations/0032, and the guard in rollback/0032 -- and a one-character
/// divergence would make the rollback refuse every object the migration
/// stamped, which is a permanent false refusal with no diagnostic pointing at
/// the cause.
#[sqlx::test(migrations = "./migrations")]
async fn the_provenance_marker_is_identical_everywhere_it_appears(pool: PgPool) {
    /// Concatenate the adjacent single-quoted literals of every
    /// `marker CONSTANT TEXT := ...;` declaration in a script.
    fn declared_markers(sql: &str) -> Vec<String> {
        const DECL: &str = "marker CONSTANT TEXT :=";
        sql.match_indices(DECL)
            .map(|(at, _)| {
                let tail = &sql[at + DECL.len()..];
                let mut rest = &tail[..tail.find(';').expect("a declaration ends in a semicolon")];
                let mut joined = String::new();
                while let Some(open) = rest.find('\'') {
                    let after = &rest[open + 1..];
                    let close = after.find('\'').expect("an unterminated SQL literal");
                    joined.push_str(&after[..close]);
                    rest = &after[close + 1..];
                }
                joined
            })
            .collect()
    }

    let declared: Vec<String> = declared_markers(PREPARE_UP_SQL)
        .into_iter()
        .chain(declared_markers(PREPARE_DOWN_SQL))
        .collect();
    assert_eq!(
        declared.len(),
        3,
        "two declarations in the migration and one in the rollback: {declared:#?}"
    );
    assert!(
        declared.windows(2).all(|w| w[0] == w[1]),
        "every declaration of the marker must be byte-identical: {declared:#?}"
    );

    // And the text the scripts declare is the text PostgreSQL actually holds,
    // so this test cannot pass over a marker that never reached the catalog.
    let in_catalog: String = sqlx::query_scalar(
        "SELECT col_description('public.entities'::regclass, \
                                (SELECT attnum FROM pg_attribute \
                                  WHERE attrelid = 'public.entities'::regclass \
                                    AND attname = 'tenant_id')::int)",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        in_catalog, declared[0],
        "the stamped comment must be exactly what the scripts compare against"
    );
}

/// Renamed tranche foreign keys must not hide from the rollback guard.
///
/// The guard's FK half was scoped by `conname`, the last name-based handle in a
/// file whose other handles are all OIDs, and the consequence is not a raw
/// error later -- it is silent destruction. `ALTER TABLE ... DROP COLUMN` drops
/// a dependent foreign key with no ERROR and no NOTICE, so a rollback that
/// cannot see the keys runs to completion and takes them with it, then deletes
/// migration version 41's work while the ledger still claims it applied.
///
/// All five are renamed, not one. Renaming one proves less than it looks:
/// the guard still refuses over the four siblings that kept their names, so the
/// key it cannot see is destroyed inside a run that appears to have been
/// stopped. The five-way rename is the state in which the pre-fix guard reports
/// "none" and destroys everything.
///
/// The index half of the guard cannot cover this, so the eight indexes are
/// removed first to leave the FK half testing alone.
#[sqlx::test(migrations = "./migrations")]
async fn the_prepare_rollback_still_refuses_over_renamed_tranche_foreign_keys(pool: PgPool) {
    for stmt in [
        "ALTER TABLE archival_batches DROP CONSTRAINT archival_batches_id_tenant_id_key",
        "ALTER TABLE entities DROP CONSTRAINT entities_id_tenant_id_key",
        "ALTER TABLE rmk_policies DROP CONSTRAINT rmk_policies_id_tenant_id_key",
        "DROP INDEX idx_archival_batches_tenant, idx_audit_logs_tenant, idx_entities_tenant, \
         idx_memory_graph_tenant, idx_rmk_policies_tenant",
        "ALTER TABLE archival_batches RENAME CONSTRAINT archival_batches_tenant_agent_fkey \
         TO zz_renamed_archival_batches",
        "ALTER TABLE audit_logs RENAME CONSTRAINT audit_logs_tenant_agent_fkey \
         TO zz_renamed_audit_logs",
        "ALTER TABLE entities RENAME CONSTRAINT entities_tenant_agent_fkey TO zz_renamed_entities",
        "ALTER TABLE memory_graph RENAME CONSTRAINT memory_graph_tenant_agent_fkey \
         TO zz_renamed_memory_graph",
        "ALTER TABLE rmk_policies RENAME CONSTRAINT rmk_policies_tenant_agent_fkey \
         TO zz_renamed_rmk_policies",
    ] {
        sqlx::query(sqlx::AssertSqlSafe(stmt.to_owned()))
            .execute(&pool)
            .await
            .unwrap();
    }

    let (code, message) = expect_refusal(
        &pool,
        PREPARE_DOWN_SQL,
        "renamed tranche foreign keys must still be seen",
    )
    .await;
    assert_eq!(
        code, "2BP01",
        "dependent_objects_still_exist is the refusal's SQLSTATE"
    );
    for (renamed, table) in [
        ("zz_renamed_archival_batches", "archival_batches"),
        ("zz_renamed_audit_logs", "audit_logs"),
        ("zz_renamed_entities", "entities"),
        ("zz_renamed_memory_graph", "memory_graph"),
        ("zz_renamed_rmk_policies", "rmk_policies"),
    ] {
        assert!(
            message.contains(&format!("{renamed} on {table}")),
            "the refusal must name each key by where it is, not by what it used to be \
             called; {renamed} is missing from: {message}"
        );
    }

    // All five are still attached, which is the outcome that matters: DROP
    // COLUMN would have removed them without a word.
    let still_there: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pg_constraint WHERE contype = 'f' AND conname LIKE 'zz\\_renamed\\_%'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(still_there, 5, "every renamed foreign key must survive");
    let columns: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM information_schema.columns \
         WHERE table_schema = 'public' AND column_name IN ('agent_uuid', 'tenant_id') \
           AND table_name IN ('archival_batches', 'audit_logs', 'entities', 'memory_graph', \
                              'rmk_policies')",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(columns, 10, "and no ownership column may have been dropped");
}

/// The rerun guard must name the axis it actually failed on.
///
/// Its condition tests ten properties of the backing index; its message printed
/// five values. A DESC-ordered or BRIN occupant fails on an axis the message
/// never printed, so every value in it read as correct and the operator was
/// told the index was wrong by evidence that appeared to say otherwise.
///
/// Reaching this state needs catalog surgery, because PostgreSQL refuses to
/// ADD CONSTRAINT over a non-default-sorted index -- which is exactly why the
/// diagnostic has to be exact. Nobody arrives here by accident, and there is
/// nothing else to go on.
#[sqlx::test(migrations = "./migrations")]
async fn the_rerun_guard_names_the_axis_a_desc_backed_constraint_fails_on(pool: PgPool) {
    sqlx::raw_sql(
        "UPDATE pg_index SET indoption = '1 0'::int2vector \
          WHERE indexrelid = 'public.entities_id_tenant_id_key'::regclass",
    )
    .execute(&pool)
    .await
    .unwrap();

    let (code, message) = expect_refusal(
        &pool,
        CONSTRAINTS_UP,
        "0041 must refuse a constraint backed by a DESC-ordered index",
    )
    .await;
    assert_eq!(code, "42710", "duplicate_object is the refusal's SQLSTATE");
    assert!(
        message.contains("column ordering") && message.contains("bit 0 is DESC"),
        "the refusal must name the axis that actually failed: {message}"
    );
    // And it must not have reported an axis that is fine.
    assert!(
        !message.contains("is not unique") && !message.contains("is partial"),
        "the refusal must not report axes the index satisfies: {message}"
    );
}

/// A pre-existing checkpoint table must stop the migration, not be adopted.
///
/// `CREATE TABLE IF NOT EXISTS` adopts as silently as `ADD COLUMN IF NOT
/// EXISTS`, and this is the worst object in the tranche to adopt: skipping the
/// CREATE also skips every CHECK inside it. A table of the right name and the
/// wrong shape then accepts a COMPLETED row that no backfill earned -- rows
/// backfilled exceeding rows total, a nonzero blocking count -- and that row is
/// exactly what FINALIZE_PRECONDITION trusts before validating constraints over
/// data nobody backfilled.
///
/// Checked by shape rather than by the provenance marker the columns, functions
/// and triggers use, because rollback/0032 never drops this table: adopting it
/// destroys nothing, and what adoption actually costs is the constraints.
#[sqlx::test(migrations = "./migrations")]
async fn the_prepare_migration_refuses_a_pre_existing_checkpoint_table(pool: PgPool) {
    unwind_to_pre_prepare(&pool).await;
    // The rollback deliberately retains the table, so it is still here; drop it
    // to model an occupant this tranche never built.
    for stmt in [
        "DROP TABLE tenancy_backfill_checkpoints",
        "CREATE TABLE tenancy_backfill_checkpoints ( \
             id UUID PRIMARY KEY DEFAULT gen_random_uuid(), tranche TEXT, \
             contract_digest TEXT, status TEXT, resume_cursor TEXT, \
             rows_total BIGINT, rows_backfilled BIGINT, blocking_count BIGINT, \
             started_at TIMESTAMPTZ DEFAULT NOW(), updated_at TIMESTAMPTZ DEFAULT NOW(), \
             completed_at TIMESTAMPTZ)",
        // A completion nobody earned. Every one of these values is refused by
        // the real table's CHECKs, which is the point.
        "INSERT INTO tenancy_backfill_checkpoints \
           (tranche, contract_digest, status, rows_total, rows_backfilled, \
            blocking_count, completed_at) \
         VALUES ('TRANCHE_1_ROOTS_AND_DIRECT_AGENT_CHILDREN', 'forged', 'COMPLETED', \
                 0, 999, 7, NOW())",
    ] {
        sqlx::query(sqlx::AssertSqlSafe(stmt.to_owned()))
            .execute(&pool)
            .await
            .unwrap();
    }

    let (code, message) = expect_refusal(
        &pool,
        PREPARE_UP_SQL,
        "0032 must not adopt a checkpoint table it did not create",
    )
    .await;
    assert_eq!(code, "42P07", "duplicate_table is the refusal's SQLSTATE");
    for missing in [
        "tenancy_backfill_checkpoints_status_ck",
        "tenancy_backfill_checkpoints_tranche_ck",
        "tenancy_backfill_checkpoints_counts_ck",
        "tenancy_backfill_checkpoints_completed_accounting_ck",
    ] {
        assert!(
            message.contains(missing),
            "the refusal must name each CHECK the occupant lacks; {missing} is missing \
             from: {message}"
        );
    }

    // The occupant is untouched -- this migration refuses, it does not repair.
    let forged: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM tenancy_backfill_checkpoints WHERE contract_digest = 'forged'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        forged, 1,
        "the occupant's rows are left exactly as they were"
    );
    let checks: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pg_constraint \
          WHERE conrelid = 'public.tenancy_backfill_checkpoints'::regclass AND contype = 'c'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        checks, 0,
        "and it still has none of the tranche's CHECKs, so nothing was half-applied"
    );
}

/// Correctly named but vacuous CHECKs must not pass for the real table.
///
/// Codex, P1 on `9a6c279`: the checkpoint guard matched `contype` and `conname`
/// and nothing else, so seven constraints carrying the tranche's exact names and
/// defined as `CHECK (true)` satisfied it. `CREATE TABLE IF NOT EXISTS` then
/// adopted the table and a forged COMPLETED row still reached FINALIZE --
/// identity-by-name at an eighth object kind, in the guard written to stop
/// identity-by-name. The constraint expressions are now compared against
/// PostgreSQL's own normalised rendering.
#[sqlx::test(migrations = "./migrations")]
async fn the_prepare_migration_refuses_a_checkpoint_table_with_vacuous_checks(pool: PgPool) {
    unwind_to_pre_prepare(&pool).await;
    let named_but_vacuous: String = [
        "tenancy_backfill_checkpoints_status_ck",
        "tenancy_backfill_checkpoints_tranche_ck",
        "tenancy_backfill_checkpoints_completed_shape_ck",
        "tenancy_backfill_checkpoints_completed_cursor_ck",
        "tenancy_backfill_checkpoints_counts_ck",
        "tenancy_backfill_checkpoints_completed_clean_ck",
        "tenancy_backfill_checkpoints_completed_accounting_ck",
    ]
    .iter()
    .map(|c| format!(", CONSTRAINT {c} CHECK (true)"))
    .collect();

    for stmt in [
        "DROP TABLE tenancy_backfill_checkpoints".to_owned(),
        format!(
            "CREATE TABLE tenancy_backfill_checkpoints ( \
                 id UUID PRIMARY KEY DEFAULT gen_random_uuid(), tranche TEXT, \
                 contract_digest TEXT, status TEXT, resume_cursor TEXT, \
                 rows_total BIGINT, rows_backfilled BIGINT, blocking_count BIGINT, \
                 started_at TIMESTAMPTZ DEFAULT NOW(), updated_at TIMESTAMPTZ DEFAULT NOW(), \
                 completed_at TIMESTAMPTZ{named_but_vacuous})"
        ),
        // Every value here is refused by the real CHECKs and accepted by these.
        "INSERT INTO tenancy_backfill_checkpoints \
           (tranche, contract_digest, status, rows_total, rows_backfilled, \
            blocking_count, completed_at) \
         VALUES ('NOT_EVEN_A_TRANCHE', 'forged', 'COMPLETED', 0, 999, 7, NULL)"
            .to_owned(),
    ] {
        sqlx::query(sqlx::AssertSqlSafe(stmt))
            .execute(&pool)
            .await
            .unwrap();
    }

    // The name-only guard this replaces would have seen all seven and adopted.
    let by_name: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pg_constraint \
          WHERE conrelid = 'public.tenancy_backfill_checkpoints'::regclass AND contype = 'c'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        by_name, 7,
        "the decoy must satisfy a name-only test to be a real test"
    );

    let (code, message) = expect_refusal(
        &pool,
        PREPARE_UP_SQL,
        "0032 must compare CHECK expressions, not just their names",
    )
    .await;
    assert_eq!(code, "42P07", "duplicate_table is the refusal's SQLSTATE");
    for named in [
        "tenancy_backfill_checkpoints_status_ck",
        "tenancy_backfill_checkpoints_counts_ck",
        "tenancy_backfill_checkpoints_completed_accounting_ck",
    ] {
        assert!(
            message.contains(&format!("differs:    constraint {named} definition")),
            "the refusal must name every constraint whose expression differs, and \
             {named} is absent from: {message}"
        );
    }

    // The forged row -- COMPLETED with no completed_at, an unknown tranche, more
    // rows backfilled than exist and a nonzero blocking count -- is exactly what
    // FINALIZE_PRECONDITION would have been handed.
    let forged: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM tenancy_backfill_checkpoints \
          WHERE status = 'COMPLETED' AND rows_backfilled > rows_total",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(forged, 1, "the occupant is left exactly as it was");
}

/// A total unique index must not pass for the tranche's partial one.
///
/// Found by self-audit after Codex's P1, by enumerating every `IF NOT EXISTS`
/// in the tranche and asking what identifies the object. This index was checked
/// for ownership and nothing else, and it is the object that makes "at most one
/// authoritative completion per tranche and digest" true. Its
/// `WHERE status = 'COMPLETED'` scoping is a named contract decision: it is what
/// keeps ABANDONED history retained rather than overwritten, which is exactly
/// what `rollback/0032` relies on when it retires a completion.
///
/// A TOTAL occupant is the sharp case rather than a non-unique one: it is
/// stricter, so nothing fails at insert time to reveal it, and it silently
/// forbids the second ABANDONED row the rollback is designed to leave behind.
#[sqlx::test(migrations = "./migrations")]
async fn the_prepare_migration_refuses_a_total_index_where_a_partial_one_belongs(pool: PgPool) {
    unwind_to_pre_prepare(&pool).await;

    // The table is left exactly as the tranche built it -- the rollback retains
    // it, so it is still here with all seven CHECKs intact. Only the index is
    // swapped, so the refusal below can be about nothing but index shape.
    for stmt in [
        "DROP INDEX tenancy_backfill_checkpoints_completed_key",
        "CREATE UNIQUE INDEX tenancy_backfill_checkpoints_completed_key \
           ON tenancy_backfill_checkpoints (tranche, contract_digest)",
    ] {
        sqlx::query(sqlx::AssertSqlSafe(stmt.to_owned()))
            .execute(&pool)
            .await
            .unwrap();
    }

    let (code, message) = expect_refusal(
        &pool,
        PREPARE_UP_SQL,
        "0032 must not adopt a total index where its partial one belongs",
    )
    .await;
    assert_eq!(code, "42P07", "duplicate_table is the refusal's SQLSTATE");
    assert!(
        message.contains("differs:    index tenancy_backfill_checkpoints_completed_key definition"),
        "the refusal must report the index definition that differs: {message}"
    );
}

/// The seven checkpoint CHECKs, in the source form migration 0032 declares them.
///
/// Written as `IN (...)` and bare comparisons exactly as 0032 does, so
/// PostgreSQL normalises them to the same `pg_get_expr` output the guard
/// compares against. A decoy built from these matches the guard on expression
/// and differs only in `convalidated`, which is the axis under test.
const CHECKPOINT_CHECK_SOURCE: &[(&str, &str)] = &[
    (
        "tenancy_backfill_checkpoints_status_ck",
        "status IN ('IN_PROGRESS', 'COMPLETED', 'ABANDONED')",
    ),
    (
        "tenancy_backfill_checkpoints_tranche_ck",
        "tranche IN ('TRANCHE_1_ROOTS_AND_DIRECT_AGENT_CHILDREN', 'TRANCHE_2_SESSIONS', \
         'TRANCHE_3_MEMORIES', 'TRANCHE_4_LINEAGE_AND_ARCHIVAL', 'TRANCHE_5_OPERATIONS', \
         'FINAL_CONSTRAINT_TIGHTENING')",
    ),
    (
        "tenancy_backfill_checkpoints_completed_shape_ck",
        "(status = 'COMPLETED') = (completed_at IS NOT NULL)",
    ),
    (
        "tenancy_backfill_checkpoints_completed_cursor_ck",
        "status <> 'COMPLETED' OR resume_cursor IS NULL",
    ),
    (
        "tenancy_backfill_checkpoints_counts_ck",
        "rows_total >= 0 AND rows_backfilled >= 0 AND blocking_count >= 0 \
         AND rows_backfilled <= rows_total",
    ),
    (
        "tenancy_backfill_checkpoints_completed_clean_ck",
        "status <> 'COMPLETED' OR blocking_count = 0",
    ),
    (
        "tenancy_backfill_checkpoints_completed_accounting_ck",
        "status <> 'COMPLETED' OR rows_backfilled = rows_total",
    ),
];

/// Replace the retained checkpoint table with a foreign occupant.
///
/// The rollback deliberately keeps the real table, so any test modelling an
/// occupant has to remove it first. The illegal row is inserted BEFORE the
/// constraints, which is exactly what makes `NOT VALID` the only way to attach
/// them afterwards.
async fn replace_checkpoint_table_with_decoy(pool: &PgPool, not_valid: bool) {
    for stmt in [
        "DROP TABLE public.tenancy_backfill_checkpoints".to_owned(),
        // Column-for-column what migration 0032 declares, including every NOT
        // NULL. That exactness is the point, and it was not here before: Codex
        // found the column arm of the guard missing by noticing that THIS
        // fixture -- the positive control asserting a correctly shaped occupant
        // is accepted -- left nearly every protocol column nullable and passed
        // anyway. A positive control that is not actually equivalent silently
        // licenses the gap it is supposed to rule out.
        "CREATE TABLE public.tenancy_backfill_checkpoints ( \
             id UUID PRIMARY KEY DEFAULT gen_random_uuid(), \
             tranche TEXT NOT NULL, contract_digest TEXT NOT NULL, status TEXT NOT NULL, \
             resume_cursor TEXT, \
             rows_total BIGINT NOT NULL DEFAULT 0, rows_backfilled BIGINT NOT NULL DEFAULT 0, \
             blocking_count BIGINT NOT NULL DEFAULT 0, \
             started_at TIMESTAMPTZ NOT NULL DEFAULT NOW(), \
             updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(), \
             completed_at TIMESTAMPTZ)"
            .to_owned(),
        // The seeded row differs by case, and that is the point. A row the real
        // CHECKs forbid is what makes NOT VALID the only way to attach them; a
        // legal row keeps the validated case a fair comparison rather than an
        // empty table.
        if not_valid {
            "INSERT INTO public.tenancy_backfill_checkpoints \
               (tranche, contract_digest, status, rows_total, rows_backfilled, \
                blocking_count, completed_at) \
             VALUES ('TRANCHE_1_ROOTS_AND_DIRECT_AGENT_CHILDREN', 'forged', 'COMPLETED', \
                     0, 999, 7, NOW())"
                .to_owned()
        } else {
            "INSERT INTO public.tenancy_backfill_checkpoints \
               (tranche, contract_digest, status, rows_total, rows_backfilled, \
                blocking_count, completed_at) \
             VALUES ('TRANCHE_1_ROOTS_AND_DIRECT_AGENT_CHILDREN', 'honest', 'COMPLETED', \
                     7, 7, 0, NOW())"
                .to_owned()
        },
    ] {
        sqlx::query(sqlx::AssertSqlSafe(stmt))
            .execute(pool)
            .await
            .unwrap();
    }

    let suffix = if not_valid { " NOT VALID" } else { "" };
    for (name, predicate) in CHECKPOINT_CHECK_SOURCE {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "ALTER TABLE public.tenancy_backfill_checkpoints \
               ADD CONSTRAINT {name} CHECK ({predicate}){suffix}"
        )))
        .execute(pool)
        .await
        .unwrap_or_else(|e| panic!("attaching {name} must succeed: {e}"));
    }

    // The fixture asserts its own equivalence rather than being trusted to have
    // it. Every negative test below mutates exactly one axis away from this
    // baseline, so if the baseline quietly stops matching the real table, those
    // tests stop proving the axis they name -- which is precisely how the
    // nullable-column and primary-key holes survived earlier rounds.
    assert_decoy_is_contract_equivalent(pool).await;
}

/// A NOT VALID CHECK matches by expression and asserts nothing about the rows.
///
/// Codex, on `1f5cdda`: `pg_get_expr` renders a `NOT VALID` constraint exactly as
/// it renders a validated one, so the expression comparison added in `fc15246`
/// was satisfied by constraints attached AFTER malformed rows were already in the
/// table. `CREATE TABLE IF NOT EXISTS` then adopted it and the forged COMPLETED
/// row survived -- a row violating completion accounting while still matching
/// what FINALIZE_PRECONDITION looks for. `convalidated` is what makes this guard
/// an assertion about the DATA rather than only about the schema.
#[sqlx::test(migrations = "./migrations")]
async fn the_prepare_migration_refuses_checkpoint_checks_that_were_never_validated(pool: PgPool) {
    unwind_to_pre_prepare(&pool).await;
    replace_checkpoint_table_with_decoy(&pool, true).await;

    // The decoy matches the expression comparison on all seven, so `convalidated`
    // is the only axis that can refuse it.
    let unvalidated: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pg_constraint \
          WHERE conrelid = 'public.tenancy_backfill_checkpoints'::regclass \
            AND contype = 'c' AND NOT convalidated",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        unvalidated, 7,
        "all seven must be attached and unvalidated for this to be a real test"
    );

    let (code, message) = expect_refusal(
        &pool,
        PREPARE_UP_SQL,
        "0032 must refuse CHECKs that were never validated against the rows",
    )
    .await;
    assert_eq!(code, "42P07", "duplicate_table is the refusal's SQLSTATE");
    assert!(
        message.contains(
            "differs:    constraint tenancy_backfill_checkpoints_completed_clean_ck \
             validated: reference=true live=false",
        ),
        "the refusal must report the unvalidated constraint: {message}"
    );

    // The row that would have reached FINALIZE is still there, still illegal.
    let forged: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM public.tenancy_backfill_checkpoints \
          WHERE status = 'COMPLETED' AND (rows_backfilled > rows_total OR blocking_count <> 0)",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(forged, 1, "the occupant is refused, not repaired");
}

/// The same decoy with VALIDATED constraints, and EMPTY, is still accepted.
///
/// Pins the axis. Without this, the tests above would keep passing even if the
/// comparator had started refusing every pre-existing table for an unrelated
/// reason -- which, against a whole-contract comparison, is a live risk rather
/// than a theoretical one.
///
/// Emptied first, deliberately. The fixture seeds an honest row so that the
/// NOT VALID variant has something for its constraints to be invalid against,
/// and a structurally perfect but UNSTAMPED table holding rows is refused by
/// the row gate -- see the test immediately below, which is the other half of
/// this pair.
#[sqlx::test(migrations = "./migrations")]
async fn an_empty_equivalent_checkpoint_table_is_still_accepted(pool: PgPool) {
    unwind_to_pre_prepare(&pool).await;
    replace_checkpoint_table_with_decoy(&pool, false).await;
    sqlx::raw_sql("DELETE FROM public.tenancy_backfill_checkpoints")
        .execute(&pool)
        .await
        .unwrap();

    sqlx::raw_sql(PREPARE_UP_SQL)
        .execute(&pool)
        .await
        .expect("an empty, contract-equivalent table must be adopted");
}

/// A contract-equivalent table that already holds rows, without this
/// migration's stamp, must be refused.
///
/// New in round 7, and a deliberate behaviour change: before this, a
/// structurally equivalent FOREIGN table containing rows was adopted, and its
/// rows became checkpoint evidence that FINALIZE reads. Structure cannot
/// establish where rows came from, so the row gate asks a different question --
/// did this migration create this table?
///
/// The stamp answers only that question. It is idempotency provenance, not row
/// authentication: a role that can create objects in `public` can write the
/// comment too. It is checked here so that a re-run over this migration's OWN
/// rows stays a no-op, which is the next test.
#[sqlx::test(migrations = "./migrations")]
async fn a_non_empty_unstamped_checkpoint_table_is_refused(pool: PgPool) {
    unwind_to_pre_prepare(&pool).await;
    replace_checkpoint_table_with_decoy(&pool, false).await;

    // The fixture asserts its own contract equivalence, and it seeded one
    // honest row, so structure cannot be what refuses this.
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM public.tenancy_backfill_checkpoints")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        rows, 1,
        "the occupant must hold a row for this test to mean anything"
    );
    let stamp: Option<String> = sqlx::query_scalar(
        "SELECT pg_catalog.col_description('public.tenancy_backfill_checkpoints'::regclass, \
                (SELECT attnum FROM pg_attribute \
                  WHERE attrelid = 'public.tenancy_backfill_checkpoints'::regclass \
                    AND attname = 'id')::int)",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(
        stamp.is_none(),
        "the foreign decoy must carry no stamp: {stamp:?}"
    );

    let (code, message) = expect_refusal(
        &pool,
        PREPARE_UP_SQL,
        "0032 must refuse a structurally equivalent table whose rows it did not write",
    )
    .await;
    assert_eq!(code, "42P07", "duplicate_table is the refusal's SQLSTATE");
    assert!(
        message.contains("already holds rows and does not carry"),
        "the refusal must say the rows are of unknown origin: {message}"
    );
}

/// Re-running over this migration's OWN rows stays a no-op.
///
/// The other side of the row gate. Migration 0032 creates the table and stamps
/// it, BACKFILL writes real checkpoint rows, and a later re-run must not refuse
/// its own work -- that is the idempotency contract stated at the head of the
/// file.
#[sqlx::test(migrations = "./migrations")]
async fn re_running_over_this_migrations_own_checkpoint_rows_is_a_no_op(pool: PgPool) {
    // The suite's migrations have already run 0032, so the table is stamped.
    insert_completed_checkpoint(&pool, "sha256:round-seven").await;

    sqlx::raw_sql(PREPARE_UP_SQL)
        .execute(&pool)
        .await
        .expect("a stamped table holding this migration's own rows must re-run cleanly");

    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM public.tenancy_backfill_checkpoints")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(rows, 1, "a no-op re-run must not disturb the rows");
}

/// An INVALID unique index enforces nothing and must not be adopted.
///
/// Codex, on `1f5cdda`. Deliberately the OPPOSITE of the rollback guards, which
/// ignore `indisvalid` so an interrupted concurrent build stays droppable
/// (decision 6). Adopting and dropping want different answers: a rollback should
/// still remove a broken index, and a migration must never accept one as the
/// object that guarantees uniqueness. 0041's adoption branch already refuses
/// INVALID for the same reason.
#[sqlx::test(migrations = "./migrations")]
async fn the_prepare_migration_refuses_an_invalid_completion_index(pool: PgPool) {
    sqlx::raw_sql(
        "UPDATE pg_index SET indisvalid = false \
          WHERE indexrelid = 'public.tenancy_backfill_checkpoints_completed_key'::regclass",
    )
    .execute(&pool)
    .await
    .unwrap();

    let (code, message) = expect_refusal(
        &pool,
        PREPARE_UP_SQL,
        "0032 must refuse an INVALID completion index",
    )
    .await;
    assert_eq!(code, "42P07", "duplicate_table is the refusal's SQLSTATE");
    assert!(
        message.contains(
            "differs:    index tenancy_backfill_checkpoints_completed_key valid: \
             reference=true live=false",
        ),
        "the refusal must report the validity it actually found: {message}"
    );
}

/// A writable schema ahead of `public` must not capture this migration's DDL.
///
/// Codex, on `1f5cdda`: the provenance guards inspect `public.<object>` while the
/// DDL was unqualified, so a `search_path` with a writable schema first created
/// the checkpoint table somewhere else entirely. sqlx would then record version
/// 32 as applied with no checkpoint table in `public` at all -- and the audit and
/// the FINALIZE protocol both look only in `public`. Every DDL target in 0032 is
/// now schema-qualified, so a guard and the statement it guards can no longer
/// disagree about which object they mean.
#[sqlx::test(migrations = "./migrations")]
async fn a_shadow_schema_cannot_capture_the_prepare_migrations_ddl(pool: PgPool) {
    unwind_to_pre_prepare(&pool).await;

    // One connection throughout: the search_path has to be set on the same
    // session that applies the migration.
    let mut conn = pool.acquire().await.unwrap();
    sqlx::raw_sql(
        "DROP TABLE public.tenancy_backfill_checkpoints; \
         CREATE SCHEMA zz_shadow; \
         SET search_path = zz_shadow, public",
    )
    .execute(&mut *conn)
    .await
    .unwrap();

    sqlx::raw_sql(PREPARE_UP_SQL)
        .execute(&mut *conn)
        .await
        .expect("a hostile search_path must not stop the migration");

    let (in_public, in_shadow): (Option<String>, Option<String>) = sqlx::query_as(
        "SELECT to_regclass('public.tenancy_backfill_checkpoints')::text, \
                to_regclass('zz_shadow.tenancy_backfill_checkpoints')::text",
    )
    .fetch_one(&mut *conn)
    .await
    .unwrap();
    assert!(
        in_public.is_some(),
        "the checkpoint table must land in public, where the audit and FINALIZE look"
    );
    assert_eq!(
        in_shadow, None,
        "and nothing of this tranche may land in the shadow schema"
    );

    let shadow_objects: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
          WHERE n.nspname = 'zz_shadow'",
    )
    .fetch_one(&mut *conn)
    .await
    .unwrap();
    assert_eq!(shadow_objects, 0, "the shadow schema must be left empty");

    // The same exposure applied to every unqualified ALTER, not just the table.
    let owned: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM information_schema.columns \
          WHERE table_schema = 'public' AND column_name IN ('agent_uuid', 'tenant_id') \
            AND table_name IN ('archival_batches', 'audit_logs', 'entities', 'memory_graph', \
                               'rmk_policies')",
    )
    .fetch_one(&mut *conn)
    .await
    .unwrap();
    assert_eq!(owned, 10, "all ten ownership columns must land in public");
}

/// Every function bound into any DEFAULT of `$1`, as `(column, function schema)`.
///
/// Read from `pg_attrdef.adbin` -- the stored, already-resolved expression --
/// rather than from `pg_get_expr`, whose rendering is itself search_path
/// dependent and so cannot be trusted to answer a search_path question. Every
/// `:funcid` in the tree is followed, so a capture nested inside a cast or a
/// wrapper is seen too.
///
/// `pg_depend` deliberately is not used: PostgreSQL records no dependency rows
/// for PINNED system objects, so a correctly-bound `pg_catalog.gen_random_uuid`
/// leaves no trace there at all, and absence would have to stand in for proof.
const DEFAULT_BINDINGS_SQL: &str = "SELECT a.attname::text, n.nspname::text \
       FROM pg_catalog.pg_attrdef ad \
       JOIN pg_catalog.pg_attribute a \
         ON a.attrelid = ad.adrelid AND a.attnum = ad.adnum \
       CROSS JOIN LATERAL pg_catalog.regexp_matches(ad.adbin::text, ':funcid (\\d+)', 'g') \
            AS m(parts) \
       JOIN pg_catalog.pg_proc p ON p.oid = m.parts[1]::oid \
       JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace \
      WHERE ad.adrelid = $1::regclass";

/// A hostile `search_path` must not choose which functions the checkpoint
/// table's DEFAULTs are bound to.
///
/// Codex, on `67ba097`, against the canonical comparator itself. `checkpoint_body`
/// writes `gen_random_uuid()` and `NOW()` unqualified, and a stored default is
/// resolved to a specific function AT CREATE TIME. `pg_catalog` is searched first
/// only while it is *implicit*; a caller who names it explicitly and late --
/// `hostile, public, pg_catalog` -- demotes it, and an attacker-owned
/// `gen_random_uuid()` is what gets bound.
///
/// The comparator cannot catch this, which is why it is worth its own test. The
/// same DDL builds both sides, so the identical hostile function is bound into
/// the reference AND the live table, the diff is empty, the migration reports
/// success -- and the table is wrong. Measured on `67ba097` before the fix: both
/// tables' `id` defaults depended on `zz_hostile.gen_random_uuid`, both
/// timestamp defaults on `zz_hostile.now`, the migration applied clean, and the
/// second defaulted insert died on the primary key.
///
/// This is the general blind spot of a two-sided diff -- anything poisoning both
/// sides identically -- and it is the same reason privileges are held to an
/// absolute policy rather than compared. The remedy is likewise not a better
/// comparison: it is pinning `search_path` to `pg_catalog` BEFORE either side is
/// built, so the hostile function is never in scope to be bound.
///
/// Evidence is taken from the catalog as well as from behaviour: the function
/// actually bound into each default is read back out of `pg_attrdef`, so this
/// asserts the identity of the bound function rather than merely that the values
/// it produces happen to look distinct.
#[sqlx::test(migrations = "./migrations")]
async fn a_hostile_search_path_cannot_capture_the_checkpoint_table_defaults(pool: PgPool) {
    unwind_to_pre_prepare(&pool).await;

    // One connection throughout: search_path is a session setting, and it has to
    // be the session that applies the migration.
    let mut conn = pool.acquire().await.unwrap();

    // `pg_catalog` named EXPLICITLY and LAST. Leaving it out would make it
    // implicitly first and no shadow could ever win -- the demotion IS the
    // attack, and the test would be vacuous without it.
    sqlx::raw_sql(
        "DROP TABLE public.tenancy_backfill_checkpoints; \
         CREATE SCHEMA zz_hostile; \
         CREATE FUNCTION zz_hostile.gen_random_uuid() RETURNS uuid \
             LANGUAGE sql IMMUTABLE AS $f$ \
                 SELECT '11111111-1111-4111-8111-111111111111'::uuid $f$; \
         CREATE FUNCTION zz_hostile.now() RETURNS timestamptz \
             LANGUAGE sql STABLE AS $f$ \
                 SELECT '2000-01-01T00:00:00Z'::timestamptz $f$; \
         SET search_path = zz_hostile, public, pg_catalog",
    )
    .execute(&mut *conn)
    .await
    .unwrap();

    // POTENCY CONTROL, before the migration runs. The shadows above are only a
    // real attack if unqualified DDL under this exact path actually binds them.
    // If PostgreSQL resolved past them for any reason, every assertion below
    // would pass while proving nothing. This plants a table with the same two
    // unqualified defaults and confirms the capture really happens.
    sqlx::raw_sql(
        "CREATE TABLE public.zz_potency_control ( \
             id UUID DEFAULT gen_random_uuid(), at TIMESTAMPTZ DEFAULT NOW())",
    )
    .execute(&mut *conn)
    .await
    .unwrap();
    let captured: Vec<(String, String)> = sqlx::query_as(DEFAULT_BINDINGS_SQL)
        .bind("public.zz_potency_control")
        .fetch_all(&mut *conn)
        .await
        .unwrap();
    assert!(
        captured.iter().any(|(_, schema)| schema == "zz_hostile"),
        "the planted shadows must actually capture unqualified DDL under this \
         search_path, or this test proves nothing: {captured:?}"
    );
    sqlx::raw_sql("DROP TABLE public.zz_potency_control")
        .execute(&mut *conn)
        .await
        .unwrap();

    // The migration runs inside an EXPLICIT transaction so that the canonical
    // reference -- `ON COMMIT DROP` in `pg_temp` -- is still alive to be
    // inspected. Both tables are built by the same DDL, so both have to be shown
    // clean; checking only the live one would leave the reference unproven.
    sqlx::raw_sql("BEGIN").execute(&mut *conn).await.unwrap();

    // The comparator must stay CLEAN. A refusal here is a failure of this test:
    // the point is that the migration succeeds and is correct, not that it
    // learns to refuse a hostile path.
    sqlx::raw_sql(PREPARE_UP_SQL)
        .execute(&mut *conn)
        .await
        .expect("a hostile search_path must not stop the migration, and must not capture it");

    let temp_reference: String = sqlx::query_scalar(
        "SELECT pg_catalog.quote_ident(pg_catalog.pg_my_temp_schema()::regnamespace::text) \
             || '.tenancy_backfill_checkpoints_reference'",
    )
    .fetch_one(&mut *conn)
    .await
    .unwrap();

    for target in [
        "public.tenancy_backfill_checkpoints",
        temp_reference.as_str(),
    ] {
        let bindings: Vec<(String, String)> = sqlx::query_as(DEFAULT_BINDINGS_SQL)
            .bind(target)
            .fetch_all(&mut *conn)
            .await
            .unwrap();

        // Every function reachable from every default, not just the first: a
        // default is an expression tree, and a capture anywhere in it counts.
        assert!(
            bindings.iter().all(|(_, schema)| schema == "pg_catalog"),
            "{target} must bind every default to pg_catalog, not to a shadow: {bindings:?}"
        );
        // ...and the three that matter must still be there, so that a default
        // silently vanishing cannot satisfy the assertion above by leaving
        // nothing to check.
        for column in ["id", "started_at", "updated_at"] {
            assert!(
                bindings.iter().any(|(col, _)| col == column),
                "{target}.{column} must still carry a function-backed default: {bindings:?}"
            );
        }
    }

    // Named exactly, on the live table: `id` bound to pg_catalog's
    // `gen_random_uuid`, both timestamps to pg_catalog's `now`.
    let named: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT a.attname::text, n.nspname::text, p.proname::text \
           FROM pg_catalog.pg_attrdef ad \
           JOIN pg_catalog.pg_attribute a \
             ON a.attrelid = ad.adrelid AND a.attnum = ad.adnum \
           CROSS JOIN LATERAL pg_catalog.regexp_matches(ad.adbin::text, ':funcid (\\d+)', 'g') \
                AS m(parts) \
           JOIN pg_catalog.pg_proc p ON p.oid = m.parts[1]::oid \
           JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace \
          WHERE ad.adrelid = 'public.tenancy_backfill_checkpoints'::regclass \
            AND a.attname IN ('id', 'started_at', 'updated_at')",
    )
    .fetch_all(&mut *conn)
    .await
    .unwrap();
    for expected in [
        ("id", "pg_catalog", "gen_random_uuid"),
        ("started_at", "pg_catalog", "now"),
        ("updated_at", "pg_catalog", "now"),
    ] {
        assert!(
            named
                .iter()
                .any(|(c, s, f)| (c.as_str(), s.as_str(), f.as_str()) == expected),
            "the stored default for {} must depend on {}.{}: {named:?}",
            expected.0,
            expected.1,
            expected.2
        );
    }

    sqlx::raw_sql("COMMIT").execute(&mut *conn).await.unwrap();

    // BEHAVIOURAL confirmation on top of the catalog evidence: the constant-UUID
    // shadow's signature failure is the second defaulted insert colliding on the
    // primary key. Two defaulted inserts, two distinct ids.
    sqlx::raw_sql(
        "INSERT INTO public.tenancy_backfill_checkpoints (tranche, contract_digest, status) \
         VALUES ('TRANCHE_1_ROOTS_AND_DIRECT_AGENT_CHILDREN', 'digest-1', 'IN_PROGRESS'); \
         INSERT INTO public.tenancy_backfill_checkpoints (tranche, contract_digest, status) \
         VALUES ('TRANCHE_2_SESSIONS', 'digest-2', 'IN_PROGRESS')",
    )
    .execute(&mut *conn)
    .await
    .expect("two defaulted inserts must not collide on a constant shadow uuid");

    let (rows, distinct_ids, constant_ids): (i64, i64, i64) = sqlx::query_as(
        "SELECT count(*), count(DISTINCT id), \
                count(*) FILTER (WHERE id = '11111111-1111-4111-8111-111111111111'::uuid) \
           FROM public.tenancy_backfill_checkpoints",
    )
    .fetch_one(&mut *conn)
    .await
    .unwrap();
    assert_eq!(rows, 2, "both defaulted inserts must have landed");
    assert_eq!(distinct_ids, 2, "each must have received its own id");
    assert_eq!(
        constant_ids, 0,
        "no row may carry the shadow's constant uuid"
    );

    // The timestamp shadow leaves its own fingerprint: a frozen year 2000.
    let frozen: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM public.tenancy_backfill_checkpoints \
          WHERE started_at < '2020-01-01T00:00:00Z'::timestamptz \
             OR updated_at < '2020-01-01T00:00:00Z'::timestamptz",
    )
    .fetch_one(&mut *conn)
    .await
    .unwrap();
    assert_eq!(
        frozen, 0,
        "no row may carry the shadow now()'s frozen clock"
    );

    // The migration restores the path it was handed rather than leaving its own
    // pinned behind.
    let path: String = sqlx::query_scalar("SHOW search_path")
        .fetch_one(&mut *conn)
        .await
        .unwrap();
    assert_eq!(
        path, "zz_hostile, public, pg_catalog",
        "the migration must restore the caller's search_path, not keep its own"
    );
}

/// Validated CHECKs do not make up for a missing NOT NULL.
///
/// Codex, on `c54889e`. Comparing the seven CHECK expressions left the COLUMNS
/// unchecked, and `CREATE TABLE IF NOT EXISTS` skips the whole declaration --
/// so a table carrying all seven validated CHECKs with `rows_total` nullable was
/// adopted and the NOT NULLs this protocol depends on silently never existed.
///
/// The CHECKs cannot substitute, which is the part worth stating: every one of
/// them tolerates NULL by construction, so `rows_backfilled <= rows_total` is
/// simply not false when either side is NULL, and a COMPLETED row with NULL
/// counts satisfies all seven while meaning nothing.
#[sqlx::test(migrations = "./migrations")]
async fn the_prepare_migration_refuses_a_checkpoint_table_with_nullable_counts(pool: PgPool) {
    unwind_to_pre_prepare(&pool).await;
    sqlx::raw_sql(
        "DROP TABLE public.tenancy_backfill_checkpoints; \
         CREATE TABLE public.tenancy_backfill_checkpoints ( \
             id UUID PRIMARY KEY DEFAULT gen_random_uuid(), \
             tranche TEXT NOT NULL, contract_digest TEXT NOT NULL, status TEXT NOT NULL, \
             resume_cursor TEXT, \
             rows_total BIGINT, rows_backfilled BIGINT NOT NULL DEFAULT 0, \
             blocking_count BIGINT NOT NULL DEFAULT 0, \
             started_at TIMESTAMPTZ NOT NULL DEFAULT NOW(), \
             updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(), \
             completed_at TIMESTAMPTZ)",
    )
    .execute(&pool)
    .await
    .unwrap();
    for (name, predicate) in CHECKPOINT_CHECK_SOURCE {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "ALTER TABLE public.tenancy_backfill_checkpoints \
               ADD CONSTRAINT {name} CHECK ({predicate})"
        )))
        .execute(&pool)
        .await
        .unwrap();
    }

    // All seven are present and validated, so only the column arm can refuse.
    let validated: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pg_constraint \
          WHERE conrelid = 'public.tenancy_backfill_checkpoints'::regclass \
            AND contype = 'c' AND convalidated",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        validated, 7,
        "the decoy must satisfy the CHECK arm entirely"
    );

    // A COMPLETED row with a NULL count satisfies every one of them, which is why
    // the NOT NULL is the thing actually carrying the guarantee.
    sqlx::raw_sql(
        "INSERT INTO public.tenancy_backfill_checkpoints \
           (tranche, contract_digest, status, rows_backfilled, blocking_count, completed_at) \
         VALUES ('TRANCHE_1_ROOTS_AND_DIRECT_AGENT_CHILDREN', 'nullish', 'COMPLETED', \
                 5, 0, NOW())",
    )
    .execute(&pool)
    .await
    .expect("a NULL rows_total slips past all seven CHECKs, which is the point");

    let (code, message) = expect_refusal(
        &pool,
        PREPARE_UP_SQL,
        "0032 must refuse a checkpoint table whose declared columns differ",
    )
    .await;
    assert_eq!(code, "42P07", "duplicate_table is the refusal's SQLSTATE");
    assert!(
        message.contains("differs:    column rows_total notnull: reference=true live=false"),
        "the refusal must name the column and the axis that differs: {message}"
    );
}

/// Every axis of the checkpoint contract the decoy is supposed to satisfy.
///
/// This exists because the fixture has now twice been the thing that hid a hole
/// in the guard: it was built with nearly every column nullable and passed
/// (Codex, on `c54889e`), and its equivalence has been asserted by reading it
/// rather than by checking it ever since. A positive control is only evidence if
/// it is provably the same shape as the real table, so the sameness is asserted
/// against the catalog instead of against the source text.
///
/// Called from the fixture itself, so a future simplification of the decoy fails
/// here rather than silently weakening every negative test that builds on it.
async fn assert_decoy_is_contract_equivalent(pool: &PgPool) {
    // Name, ordinal, type, nullability and default for all eleven, in order.
    // `pg_get_expr` renders the default exactly as the guard reads it.
    let shape: Vec<(i16, String, String, bool, Option<String>)> = sqlx::query_as(
        "SELECT a.attnum, a.attname, pg_catalog.format_type(a.atttypid, a.atttypmod), \
                a.attnotnull, pg_catalog.pg_get_expr(d.adbin, d.adrelid) \
           FROM pg_catalog.pg_attribute a \
           LEFT JOIN pg_catalog.pg_attrdef d \
                  ON d.adrelid = a.attrelid AND d.adnum = a.attnum \
          WHERE a.attrelid = 'public.tenancy_backfill_checkpoints'::regclass \
            AND a.attnum > 0 AND NOT a.attisdropped \
          ORDER BY a.attnum",
    )
    .fetch_all(pool)
    .await
    .unwrap();

    let expected: &[(i16, &str, &str, bool, Option<&str>)] = &[
        (1, "id", "uuid", true, Some("gen_random_uuid()")),
        (2, "tranche", "text", true, None),
        (3, "contract_digest", "text", true, None),
        (4, "status", "text", true, None),
        (5, "resume_cursor", "text", false, None),
        (6, "rows_total", "bigint", true, Some("0")),
        (7, "rows_backfilled", "bigint", true, Some("0")),
        (8, "blocking_count", "bigint", true, Some("0")),
        (
            9,
            "started_at",
            "timestamp with time zone",
            true,
            Some("now()"),
        ),
        (
            10,
            "updated_at",
            "timestamp with time zone",
            true,
            Some("now()"),
        ),
        (11, "completed_at", "timestamp with time zone", false, None),
    ];
    assert_eq!(
        shape.len(),
        expected.len(),
        "the decoy must carry exactly the declared columns: {shape:#?}"
    );
    for (got, want) in shape.iter().zip(expected) {
        assert_eq!(
            (
                got.0,
                got.1.as_str(),
                got.2.as_str(),
                got.3,
                got.4.as_deref()
            ),
            (want.0, want.1, want.2, want.3, want.4),
            "the decoy diverges from the declared contract at column {}",
            want.1
        );
    }

    // And the primary key, since that is now an axis the guard checks.
    let pk: Option<(String, bool, bool, String)> = sqlx::query_as(
        "SELECT k.conname, k.convalidated, k.condeferrable, \
                (SELECT string_agg(a.attname, ',' ORDER BY o.ord) \
                   FROM unnest(k.conkey) WITH ORDINALITY AS o(attnum, ord) \
                   JOIN pg_attribute a \
                     ON a.attrelid = k.conrelid AND a.attnum = o.attnum) \
           FROM pg_constraint k \
          WHERE k.conrelid = 'public.tenancy_backfill_checkpoints'::regclass \
            AND k.contype = 'p'",
    )
    .fetch_optional(pool)
    .await
    .unwrap();
    assert_eq!(
        pk.as_ref().map(|(_, validated, deferrable, cols)| (
            *validated,
            *deferrable,
            cols.as_str()
        )),
        Some((true, false, "id")),
        "the decoy must carry the validated, non-deferrable primary key on (id): {pk:#?}"
    );
}

/// A checkpoint table with no primary key at all must be refused.
///
/// CodeRabbit, blocking on `6f2311a`. The guard filtered `contype = 'c'`, so
/// nothing in it looked at `contype = 'p'`. `id UUID PRIMARY KEY` lives inside
/// the `CREATE TABLE IF NOT EXISTS` that adoption skips, so a table correct in
/// every other respect was adopted without the unique index behind its surrogate
/// row identity -- after which two checkpoint rows can share an `id` and every
/// by-id read of the completion evidence is ambiguous.
///
/// Mutation-checked by deleting only the primary-key arm: without it the
/// migration adopts this table and the test fails.
#[sqlx::test(migrations = "./migrations")]
async fn the_prepare_migration_refuses_a_checkpoint_table_without_a_primary_key(pool: PgPool) {
    unwind_to_pre_prepare(&pool).await;
    // Start from the fixture proved acceptable by
    // `a_validated_equivalent_checkpoint_table_is_still_accepted`, then change
    // exactly one axis. That is what makes the refusal below attributable to the
    // primary key rather than to some other way the fixture is not equivalent.
    replace_checkpoint_table_with_decoy(&pool, false).await;
    sqlx::raw_sql(
        "ALTER TABLE public.tenancy_backfill_checkpoints \
           DROP CONSTRAINT tenancy_backfill_checkpoints_pkey",
    )
    .execute(&pool)
    .await
    .unwrap();

    // The dropped constraint takes its index with it, so nothing else can be
    // supplying uniqueness on `id`.
    let indexes: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pg_index \
          WHERE indrelid = 'public.tenancy_backfill_checkpoints'::regclass \
            AND indisunique",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(indexes, 0, "the decoy must have no unique index left");

    // Two rows sharing an id is what adoption would actually permit.
    sqlx::raw_sql(
        "INSERT INTO public.tenancy_backfill_checkpoints \
           (id, tranche, contract_digest, status, rows_total, rows_backfilled, blocking_count) \
         VALUES ('11111111-1111-1111-1111-111111111111', \
                 'TRANCHE_1_ROOTS_AND_DIRECT_AGENT_CHILDREN', 'a', 'IN_PROGRESS', 0, 0, 0), \
                ('11111111-1111-1111-1111-111111111111', \
                 'TRANCHE_1_ROOTS_AND_DIRECT_AGENT_CHILDREN', 'b', 'IN_PROGRESS', 0, 0, 0)",
    )
    .execute(&pool)
    .await
    .expect("without the primary key a duplicate id is accepted, which is the point");

    let (code, message) = expect_refusal(
        &pool,
        PREPARE_UP_SQL,
        "0032 must refuse a checkpoint table with no primary key",
    )
    .await;
    assert_eq!(code, "42P07", "duplicate_table is the refusal's SQLSTATE");
    assert!(
        message.contains("missing:    constraint tenancy_backfill_checkpoints_pkey"),
        "the refusal must report the primary key as missing: {message}"
    );
}

/// A primary key on the wrong columns must be refused.
///
/// The sharper half of the finding: a table CAN have `contype = 'p'`, validated
/// and non-deferrable, and still not be this table -- `PRIMARY KEY (id, tranche)`
/// enforces uniqueness of the PAIR, so two rows may still share an `id`. A guard
/// that only asked "is there a primary key" would pass this.
///
/// Mutation-checked by relaxing the ordered column comparison to a mere existence
/// test: the decoy is then adopted and the test fails.
#[sqlx::test(migrations = "./migrations")]
async fn the_prepare_migration_refuses_a_wrong_shaped_primary_key(pool: PgPool) {
    unwind_to_pre_prepare(&pool).await;
    replace_checkpoint_table_with_decoy(&pool, false).await;
    sqlx::raw_sql(
        "ALTER TABLE public.tenancy_backfill_checkpoints \
           DROP CONSTRAINT tenancy_backfill_checkpoints_pkey; \
         ALTER TABLE public.tenancy_backfill_checkpoints \
           ADD CONSTRAINT tenancy_backfill_checkpoints_pkey PRIMARY KEY (id, tranche)",
    )
    .execute(&pool)
    .await
    .unwrap();

    // Assert up front that every OTHER axis of the primary key is satisfied, so
    // the ordered column set is provably the only thing that can refuse it.
    let (name, validated, deferrable): (String, bool, bool) = sqlx::query_as(
        "SELECT conname, convalidated, condeferrable FROM pg_constraint \
          WHERE conrelid = 'public.tenancy_backfill_checkpoints'::regclass \
            AND contype = 'p'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        (name.as_str(), validated, deferrable),
        ("tenancy_backfill_checkpoints_pkey", true, false),
        "the impostor must carry the right name, be validated and non-deferrable"
    );

    let (code, message) = expect_refusal(
        &pool,
        PREPARE_UP_SQL,
        "0032 must refuse a primary key on the wrong columns",
    )
    .await;
    assert_eq!(code, "42P07", "duplicate_table is the refusal's SQLSTATE");
    assert!(
        message.contains(
            "differs:    constraint tenancy_backfill_checkpoints_pkey columns: \
             reference=id live=id,tranche",
        ),
        "the refusal must report the key columns it actually found: {message}"
    );
}

/// A checkpoint table missing a declared default must be refused.
///
/// Codex, P1 on `6f2311a`. `CREATE TABLE IF NOT EXISTS` skips the declaration
/// wholesale, so the defaults go with it. This is not a cosmetic loss: the
/// protocol's own writers omit `id`, `started_at` and `updated_at` and rely on
/// them, and all three are NOT NULL -- so on an adopted table the first
/// checkpoint write raises 23502 and no completion can ever be recorded.
///
/// Mutation-checked by deleting only the defaults arm.
#[sqlx::test(migrations = "./migrations")]
async fn the_prepare_migration_refuses_a_checkpoint_table_missing_a_default(pool: PgPool) {
    unwind_to_pre_prepare(&pool).await;
    replace_checkpoint_table_with_decoy(&pool, false).await;
    sqlx::raw_sql(
        "ALTER TABLE public.tenancy_backfill_checkpoints \
           ALTER COLUMN started_at DROP DEFAULT",
    )
    .execute(&pool)
    .await
    .unwrap();

    // The column itself is untouched -- still there, still timestamptz, still
    // NOT NULL -- so the column arm cannot be what refuses this.
    let (typ, notnull): (String, bool) = sqlx::query_as(
        "SELECT pg_catalog.format_type(atttypid, atttypmod), attnotnull \
           FROM pg_attribute \
          WHERE attrelid = 'public.tenancy_backfill_checkpoints'::regclass \
            AND attname = 'started_at'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        (typ.as_str(), notnull),
        ("timestamp with time zone", true),
        "only the default may differ, so the column arm stays satisfied"
    );

    // And this is the concrete break: the protocol's own write now fails.
    let err = sqlx::raw_sql(
        "INSERT INTO public.tenancy_backfill_checkpoints \
           (tranche, contract_digest, status, rows_total, rows_backfilled, blocking_count) \
         VALUES ('TRANCHE_1_ROOTS_AND_DIRECT_AGENT_CHILDREN', 'd', 'IN_PROGRESS', 0, 0, 0)",
    )
    .execute(&pool)
    .await
    .expect_err("without its default, a NOT NULL started_at rejects the protocol's own insert");
    assert_eq!(
        err.as_database_error().and_then(|e| e.code()).as_deref(),
        Some("23502"),
        "the break is a not-null violation: {err}"
    );

    let (code, message) = expect_refusal(
        &pool,
        PREPARE_UP_SQL,
        "0032 must refuse a checkpoint table missing a declared default",
    )
    .await;
    assert_eq!(code, "42P07", "duplicate_table is the refusal's SQLSTATE");
    assert!(
        message.contains("differs:    column started_at default: reference=now() live=<none>"),
        "the refusal must name the column and both default states: {message}"
    );
}

/// A checkpoint table whose default is the WRONG expression must be refused.
///
/// The other half of the axis, and the reason the comparison is exact rather
/// than a presence test: `started_at DEFAULT '2000-01-01'` satisfies "has a
/// default" while silently backdating every checkpoint the protocol writes.
///
/// Mutation-checked by relaxing the comparison to `d.adbin IS NOT NULL`: the
/// decoy is then adopted and the test fails.
#[sqlx::test(migrations = "./migrations")]
async fn the_prepare_migration_refuses_a_checkpoint_table_with_a_forged_default(pool: PgPool) {
    unwind_to_pre_prepare(&pool).await;
    replace_checkpoint_table_with_decoy(&pool, false).await;
    sqlx::raw_sql(
        "ALTER TABLE public.tenancy_backfill_checkpoints \
           ALTER COLUMN started_at SET DEFAULT '2000-01-01'::timestamptz",
    )
    .execute(&pool)
    .await
    .unwrap();

    // A default IS present, so nothing but an exact comparison can refuse it.
    //
    // Read back rather than assumed, and asserted against below instead of a
    // hardcoded literal. CodeRabbit, on 2a68a78: `pg_get_expr` renders a
    // timestamptz Const through that type's output function, which uses the
    // session `TimeZone` -- so this same default renders `2000-01-01
    // 00:00:00+00` under UTC and `1999-12-31 16:00:00-08` under
    // America/Los_Angeles. Asserting the literal string would make the test
    // pass or fail on the runner's timezone rather than on the guard.
    // `quote_literal` because that is how the guard renders it into the
    // message, escaped inner quotes and all -- comparing against the bare
    // rendering would never match.
    let observed: String = sqlx::query_scalar(
        "SELECT pg_catalog.pg_get_expr(d.adbin, d.adrelid) \
           FROM pg_attrdef d \
           JOIN pg_attribute a ON a.attrelid = d.adrelid AND a.attnum = d.adnum \
          WHERE d.adrelid = 'public.tenancy_backfill_checkpoints'::regclass \
            AND a.attname = 'started_at'",
    )
    .fetch_one(&pool)
    .await
    .expect("the forged default must be present, not absent");

    let (code, message) = expect_refusal(
        &pool,
        PREPARE_UP_SQL,
        "0032 must refuse a checkpoint table whose default is not the declared one",
    )
    .await;
    assert_eq!(code, "42P07", "duplicate_table is the refusal's SQLSTATE");
    assert!(
        message.contains("differs:    column started_at default: reference=now() live=")
            && message.contains(&observed),
        "the refusal must report the observed default ({observed}) and the expected one: {message}"
    );
}

/// A column the contract does not declare must be refused.
///
/// Codex P1 and CodeRabbit, both independently on `2a68a78`. Every column arm
/// asked whether each declared column is present and correct; none asked
/// whether they are the ONLY columns. A twelfth column leaves ordinals 1-11
/// untouched, so even the ordinal arm passes.
///
/// The break is total rather than partial: the protocol's writers use named
/// column lists, so they cannot mention a column they do not know about, and a
/// `NOT NULL` one with no default rejects every checkpoint insert. Migration 32
/// is recorded as applied over a table the protocol cannot write to at all.
///
/// Mutation-checked by deleting only the undeclared-column arm.
#[sqlx::test(migrations = "./migrations")]
async fn the_prepare_migration_refuses_an_undeclared_checkpoint_column(pool: PgPool) {
    unwind_to_pre_prepare(&pool).await;
    replace_checkpoint_table_with_decoy(&pool, false).await;
    // Added WITH a default and then stripped of it, because the decoy already
    // holds a row and `ADD COLUMN ... NOT NULL` cannot backfill one without a
    // default. The end state is the occupant being modelled: a NOT NULL column
    // with no default, which existing rows satisfy and new named-list inserts
    // cannot.
    sqlx::raw_sql(
        "ALTER TABLE public.tenancy_backfill_checkpoints \
           ADD COLUMN foreign_required TEXT NOT NULL DEFAULT 'pre-existing'; \
         ALTER TABLE public.tenancy_backfill_checkpoints \
           ALTER COLUMN foreign_required DROP DEFAULT",
    )
    .execute(&pool)
    .await
    .unwrap();

    // The eleven declared columns are untouched and still at ordinals 1-11, so
    // no other column arm can be what refuses this.
    let declared_intact: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pg_attribute \
          WHERE attrelid = 'public.tenancy_backfill_checkpoints'::regclass \
            AND attnum BETWEEN 1 AND 11 AND NOT attisdropped",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        declared_intact, 11,
        "the declared eleven must be undisturbed"
    );

    // And this is the break, asserted rather than described.
    let err = sqlx::raw_sql(
        "INSERT INTO public.tenancy_backfill_checkpoints \
           (tranche, contract_digest, status, rows_total, rows_backfilled, blocking_count) \
         VALUES ('TRANCHE_1_ROOTS_AND_DIRECT_AGENT_CHILDREN', 'x', 'IN_PROGRESS', 0, 0, 0)",
    )
    .execute(&pool)
    .await
    .expect_err("a named-column insert cannot supply an undeclared NOT NULL column");
    assert_eq!(
        err.as_database_error().and_then(|e| e.code()).as_deref(),
        Some("23502"),
        "every checkpoint write would fail: {err}"
    );

    let (code, message) = expect_refusal(
        &pool,
        PREPARE_UP_SQL,
        "0032 must refuse a checkpoint table carrying an undeclared column",
    )
    .await;
    assert_eq!(code, "42P07", "duplicate_table is the refusal's SQLSTATE");
    assert!(
        message.contains("unexpected: column foreign_required"),
        "the refusal must name the undeclared column: {message}"
    );
}

/// An expression-keyed completion index must be refused.
///
/// Codex P1 on `2a68a78`, and the sharpest finding of the round. The column
/// comparison resolves `indkey` through an INNER join to `pg_attribute`, and an
/// expression key is stored as attnum 0 -- which matches no attribute, so the
/// join silently DROPS it and a three-key index aggregates to exactly the two
/// names expected. Every other axis passed.
///
/// The consequence is the guarantee itself: with `(id::text)` in the key, two
/// COMPLETED rows for the same tranche and digest coexist, which is precisely
/// the evidence FINALIZE reads.
///
/// Mutation-checked by deleting only `i.indexprs IS NOT NULL` and the two
/// attribute-count predicates.
#[sqlx::test(migrations = "./migrations")]
async fn the_prepare_migration_refuses_an_expression_keyed_completion_index(pool: PgPool) {
    unwind_to_pre_prepare(&pool).await;
    replace_checkpoint_table_with_decoy(&pool, false).await;
    sqlx::raw_sql(
        "CREATE UNIQUE INDEX tenancy_backfill_checkpoints_completed_key \
           ON public.tenancy_backfill_checkpoints (tranche, contract_digest, (id::text)) \
         WHERE status = 'COMPLETED'",
    )
    .execute(&pool)
    .await
    .unwrap();

    // The impostor satisfies every axis that existed before this fix -- unique,
    // valid, right table, right predicate, and its resolved column names
    // aggregate to exactly the expected pair, because the expression key
    // vanishes in the join. That is what makes the new axes provably the only
    // thing refusing it.
    let (unique, valid, resolved): (bool, bool, String) = sqlx::query_as(
        "SELECT i.indisunique, i.indisvalid, \
                (SELECT string_agg(a.attname, ',' ORDER BY o.ord) \
                   FROM unnest(i.indkey::int2[]) WITH ORDINALITY AS o(attnum, ord) \
                   JOIN pg_attribute a \
                     ON a.attrelid = i.indrelid AND a.attnum = o.attnum) \
           FROM pg_index i \
          WHERE i.indexrelid = \
                'public.tenancy_backfill_checkpoints_completed_key'::regclass",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        (unique, valid, resolved.as_str()),
        (true, true, "tranche,contract_digest"),
        "the impostor must look identical to the real index under the old checks"
    );

    // And it does not deliver the uniqueness the real index exists for.
    sqlx::raw_sql(
        "INSERT INTO public.tenancy_backfill_checkpoints \
           (tranche, contract_digest, status, rows_total, rows_backfilled, blocking_count, \
            completed_at) \
         VALUES ('TRANCHE_1_ROOTS_AND_DIRECT_AGENT_CHILDREN', 'dup', 'COMPLETED', 7, 7, 0, NOW()), \
                ('TRANCHE_1_ROOTS_AND_DIRECT_AGENT_CHILDREN', 'dup', 'COMPLETED', 7, 7, 0, NOW())",
    )
    .execute(&pool)
    .await
    .expect("distinct ids make the expression key unique, so both COMPLETED rows are accepted");

    let (code, message) = expect_refusal(
        &pool,
        PREPARE_UP_SQL,
        "0032 must refuse an expression-keyed completion index",
    )
    .await;
    assert_eq!(code, "42P07", "duplicate_table is the refusal's SQLSTATE");
    assert!(
        message.contains(
            "differs:    index tenancy_backfill_checkpoints_completed_key keyatts: \
             reference=2 live=3",
        ),
        "the refusal must report the extra (expression) key: {message}"
    );
}

/// A completion index carrying an INCLUDE payload must be refused.
///
/// The other half of "exactly two ordinary key columns": `indnkeyatts` counts
/// key columns and `indnatts` the total, so an INCLUDE payload passes a key
/// count check on its own. Included columns are not part of the declared index,
/// and adopting one means `CREATE UNIQUE INDEX IF NOT EXISTS` skips the name
/// while the object on disk is not what the contract describes.
///
/// Honest note on what carries this test, because the mutation check said
/// something different from what was expected. Deleting `i.indnatts <> 2`
/// alone does NOT make this pass: an INCLUDE payload puts its attribute into
/// `indkey` too, so the ordered-column comparison already aggregates to
/// `tranche,contract_digest,status` and refuses. The explicit count is
/// therefore redundant against this decoy rather than load-bearing.
///
/// It is kept deliberately: it states the "exactly two, nothing included"
/// intent where a reader looks for it, it fails with a diagnostic that
/// separates key columns from the payload, and it does not depend on the
/// column comparison continuing to be exact. What this test proves on its own
/// is that an INCLUDE payload is refused -- not which predicate refuses it.
#[sqlx::test(migrations = "./migrations")]
async fn the_prepare_migration_refuses_a_completion_index_with_included_columns(pool: PgPool) {
    unwind_to_pre_prepare(&pool).await;
    replace_checkpoint_table_with_decoy(&pool, false).await;
    sqlx::raw_sql(
        "CREATE UNIQUE INDEX tenancy_backfill_checkpoints_completed_key \
           ON public.tenancy_backfill_checkpoints (tranche, contract_digest) \
           INCLUDE (status) \
         WHERE status = 'COMPLETED'",
    )
    .execute(&pool)
    .await
    .unwrap();

    // Two KEY columns and no expressions, so only the total-attribute count can
    // refuse this one.
    let (keyatts, natts, exprs): (i16, i16, bool) = sqlx::query_as(
        "SELECT i.indnkeyatts, i.indnatts, (i.indexprs IS NOT NULL) \
           FROM pg_index i \
          WHERE i.indexrelid = \
                'public.tenancy_backfill_checkpoints_completed_key'::regclass",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        (keyatts, natts, exprs),
        (2, 3, false),
        "the payload must be the only difference: two key columns, three total"
    );

    let (code, message) = expect_refusal(
        &pool,
        PREPARE_UP_SQL,
        "0032 must refuse a completion index carrying an INCLUDE payload",
    )
    .await;
    assert_eq!(code, "42P07", "duplicate_table is the refusal's SQLSTATE");
    assert!(
        message.contains(
            "differs:    index tenancy_backfill_checkpoints_completed_key totalatts: \
             reference=2 live=3",
        ),
        "the refusal must separate key columns from the INCLUDE payload: {message}"
    );
}

/// A renamed primary key must be refused, even though it is otherwise perfect.
///
/// Codex P2 on `2a68a78`. This is the one place the tranche's "identity by
/// shape, never by name" rule inverts, and it is worth being explicit about
/// why: shape identity exists so a name cannot be *trusted*, not so a name the
/// contract itself specifies can be *ignored*. `inventory.rs` declares
/// `primary_key("tenancy_backfill_checkpoints_pkey", &["id"])` and
/// `audit.rs::verify_schema_contract` looks constraints up by `(table, name)`,
/// so a renamed key is adopted here, version 32 is recorded, and the schema
/// audit then reports contract drift nothing will repair.
///
/// Mutation-checked by deleting only the `conname` predicate.
#[sqlx::test(migrations = "./migrations")]
async fn the_prepare_migration_refuses_a_renamed_primary_key(pool: PgPool) {
    unwind_to_pre_prepare(&pool).await;
    replace_checkpoint_table_with_decoy(&pool, false).await;
    sqlx::raw_sql(
        "ALTER TABLE public.tenancy_backfill_checkpoints \
           RENAME CONSTRAINT tenancy_backfill_checkpoints_pkey TO zz_renamed_pkey",
    )
    .execute(&pool)
    .await
    .unwrap();

    // Every structural axis still holds -- it is the same constraint over the
    // same column, still validated and non-deferrable -- so the name is
    // provably the only axis that can refuse it.
    let (cols, validated, deferrable): (String, bool, bool) = sqlx::query_as(
        "SELECT (SELECT string_agg(a.attname, ',' ORDER BY o.ord) \
                   FROM unnest(k.conkey) WITH ORDINALITY AS o(attnum, ord) \
                   JOIN pg_attribute a \
                     ON a.attrelid = k.conrelid AND a.attnum = o.attnum), \
                k.convalidated, k.condeferrable \
           FROM pg_constraint k \
          WHERE k.conrelid = 'public.tenancy_backfill_checkpoints'::regclass \
            AND k.contype = 'p'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        (cols.as_str(), validated, deferrable),
        ("id", true, false),
        "only the name may differ"
    );

    let (code, message) = expect_refusal(
        &pool,
        PREPARE_UP_SQL,
        "0032 must refuse a primary key that does not carry the contract's name",
    )
    .await;
    assert_eq!(code, "42P07", "duplicate_table is the refusal's SQLSTATE");
    assert!(
        message.contains("missing:    constraint tenancy_backfill_checkpoints_pkey")
            && message.contains("unexpected: constraint zz_renamed_pkey"),
        "the refusal must name both the required and the found constraint: {message}"
    );
}

/// The completion index must be bound to the checkpoint table, not just named.
///
/// Codex, on `c54889e`. Index names are unique per SCHEMA, not per table, so a
/// same-named index on some other relation carrying `(tranche, contract_digest,
/// status)` satisfied uniqueness, validity, predicate and columns -- every axis
/// the shape arm checked -- and `CREATE UNIQUE INDEX IF NOT EXISTS` then skipped
/// the name. Version 32 would be recorded with no uniqueness guarantee on the
/// checkpoint table at all.
#[sqlx::test(migrations = "./migrations")]
async fn the_prepare_migration_refuses_a_completion_index_on_another_table(pool: PgPool) {
    unwind_to_pre_prepare(&pool).await;
    sqlx::raw_sql(
        "DROP TABLE public.tenancy_backfill_checkpoints; \
         CREATE TABLE public.zz_impostor (tranche TEXT, contract_digest TEXT, status TEXT); \
         CREATE UNIQUE INDEX tenancy_backfill_checkpoints_completed_key \
             ON public.zz_impostor (tranche, contract_digest) \
             WHERE status = 'COMPLETED'",
    )
    .execute(&pool)
    .await
    .unwrap();

    // The impostor matches every axis except the table it sits on.
    let (unique, valid, predicate): (bool, bool, String) = sqlx::query_as(
        "SELECT i.indisunique, i.indisvalid, pg_get_expr(i.indpred, i.indrelid) \
           FROM pg_index i \
          WHERE i.indexrelid = 'public.tenancy_backfill_checkpoints_completed_key'::regclass",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(unique && valid, "the impostor must be unique and valid");
    assert_eq!(
        predicate, "(status = 'COMPLETED'::text)",
        "and must carry the same predicate, so only indrelid can refuse it"
    );

    let (code, message) = expect_refusal(
        &pool,
        PREPARE_UP_SQL,
        "0032 must refuse a completion index that is not on the checkpoint table",
    )
    .await;
    assert_eq!(code, "42P07", "duplicate_table is the refusal's SQLSTATE");
    assert!(
        message.contains("already exists"),
        "with IF NOT EXISTS removed, a relation already holding the index name collides \
         instead of being silently skipped: {message}"
    );
}

/// A shadow schema must not capture the concurrent index builds either.
///
/// Codex, on `c54889e`: migrations 0033-0040 named their `ON` targets
/// unqualified, so a schema ahead of `public` holding a decoy `archival_batches`
/// captured the build. These are `-- no-transaction` migrations, which makes it
/// worse than the 0032 case: sqlx records each version as it succeeds, then 0041
/// searches only `public`, fails on the missing unique builds, and correcting
/// `search_path` afterwards cannot re-run versions the ledger already claims.
#[sqlx::test(migrations = "./migrations")]
async fn a_shadow_schema_cannot_capture_the_concurrent_index_builds(pool: PgPool) {
    // Unwind the tranche so the builds have work to do, then re-apply PREPARE --
    // the ownership columns must exist before the indexes can be built.
    for script in [CONSTRAINTS_DOWN_SQL, INDEXES_DOWN_SQL, PREPARE_DOWN_SQL] {
        sqlx::raw_sql(script).execute(&pool).await.expect("unwind");
    }

    let mut conn = pool.acquire().await.unwrap();
    sqlx::raw_sql(
        "CREATE SCHEMA zz_shadow; \
         CREATE TABLE zz_shadow.archival_batches (id UUID, tenant_id UUID, agent_uuid UUID); \
         CREATE TABLE zz_shadow.entities (id UUID, tenant_id UUID, agent_uuid UUID); \
         SET search_path = zz_shadow, public",
    )
    .execute(&mut *conn)
    .await
    .unwrap();

    sqlx::raw_sql(PREPARE_UP_SQL)
        .execute(&mut *conn)
        .await
        .expect("prepare must re-apply under a hostile search_path");
    for build in BUILD_MIGRATIONS {
        sqlx::raw_sql(*build)
            .execute(&mut *conn)
            .await
            .expect("each concurrent build must resolve its target in public");
    }

    // Every one of the eight must be on a public table, not a shadow decoy.
    let misplaced: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT count(*) FROM pg_index i \
           JOIN pg_class ic ON ic.oid = i.indexrelid \
           JOIN pg_class tc ON tc.oid = i.indrelid \
           JOIN pg_namespace tn ON tn.oid = tc.relnamespace \
          WHERE ic.relname IN ({TRANCHE1_INDEXES}) AND tn.nspname <> 'public'"
    )))
    .fetch_one(&mut *conn)
    .await
    .unwrap();
    assert_eq!(
        misplaced, 0,
        "no tranche index may have been built against a shadow table"
    );
    let shadow_indexes: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
          WHERE n.nspname = 'zz_shadow' AND c.relkind = 'i'",
    )
    .fetch_one(&mut *conn)
    .await
    .unwrap();
    assert_eq!(shadow_indexes, 0, "the shadow schema must hold no indexes");

    // And 0041 then applies, which is exactly what a captured build would break.
    sqlx::raw_sql(CONSTRAINTS_UP)
        .execute(&mut *conn)
        .await
        .expect("0041 must apply over builds that landed in public");
}

// ─────────────────────────────────────────────────────────────────────────────
// Round 7: the whole-contract comparator.
//
// Every negative below starts from `replace_checkpoint_table_with_decoy`, which
// asserts its own contract equivalence, and mutates exactly ONE property. The
// refusal is therefore attributable to that property and not to some other way
// the fixture failed to be equivalent -- the trap that hid the nullable-column
// and primary-key holes in earlier rounds.
// ─────────────────────────────────────────────────────────────────────────────

/// Plant a decoy, apply one mutation, and assert the refusal names `expected`.
async fn refuse_after(
    pool: &PgPool,
    mutation: &'static str,
    expected: &str,
    label: &str,
) -> String {
    unwind_to_pre_prepare(pool).await;
    replace_checkpoint_table_with_decoy(pool, false).await;
    sqlx::raw_sql("DELETE FROM public.tenancy_backfill_checkpoints")
        .execute(pool)
        .await
        .unwrap();
    // raw_sql, not query: several mutations below are multi-statement batches and
    // a prepared statement cannot carry more than one command.
    sqlx::raw_sql(mutation)
        .execute(pool)
        .await
        .unwrap_or_else(|e| panic!("the mutation must apply: {e}"));

    let (code, message) = expect_refusal(pool, PREPARE_UP_SQL, label).await;
    assert_eq!(code, "42P07", "duplicate_table is the refusal's SQLSTATE");
    assert!(
        message.contains(expected),
        "the refusal must report {expected:?}: {message}"
    );
    message
}

/// An undeclared CHECK constraint must be refused. (Codex P1 on `7d0733c`.)
///
/// The old guard looked its seven CHECKs up one way -- are the expected ones
/// present -- so an eighth was invisible. `CHECK (false) NOT VALID` leaves the
/// seven intact and rejects every subsequent insert, so 0032 records as applied
/// over a table BACKFILL cannot write to.
#[sqlx::test(migrations = "./migrations")]
async fn the_comparator_refuses_an_undeclared_check_constraint(pool: PgPool) {
    refuse_after(
        &pool,
        "ALTER TABLE public.tenancy_backfill_checkpoints \
           ADD CONSTRAINT zz_extra CHECK (false) NOT VALID",
        "unexpected: constraint zz_extra",
        "0032 must refuse a checkpoint table carrying an undeclared CHECK",
    )
    .await;
}

/// A generated protocol column must be refused. (Codex P1 on `7d0733c`.)
///
/// `rows_total BIGINT GENERATED ALWAYS AS (0) STORED` deparses its generation
/// expression as the expected default `0` and keeps name, type and nullability,
/// so every axis the old guard checked agreed. The protocol supplies
/// `rows_total` explicitly, and a generated column accepts only DEFAULT, so
/// every checkpoint write fails.
#[sqlx::test(migrations = "./migrations")]
async fn the_comparator_refuses_a_generated_protocol_column(pool: PgPool) {
    let message = refuse_after(
        &pool,
        "ALTER TABLE public.tenancy_backfill_checkpoints DROP COLUMN rows_total; \
         ALTER TABLE public.tenancy_backfill_checkpoints \
           ADD COLUMN rows_total BIGINT GENERATED ALWAYS AS (0) STORED NOT NULL",
        "differs:    column rows_total generated: reference= live=s",
        "0032 must refuse a generated protocol column",
    )
    .await;
    // Dropping and re-adding also moves the column and leaves a dropped slot;
    // both are reported, which is the comparator being exhaustive rather than
    // stopping at the first difference.
    assert!(
        message.contains("differs:    column rows_total attnum:"),
        "the moved ordinal must be reported too: {message}"
    );
}

/// An identity protocol column must be refused.
#[sqlx::test(migrations = "./migrations")]
async fn the_comparator_refuses_an_identity_protocol_column(pool: PgPool) {
    refuse_after(
        &pool,
        "ALTER TABLE public.tenancy_backfill_checkpoints ALTER COLUMN rows_total DROP DEFAULT; \
         ALTER TABLE public.tenancy_backfill_checkpoints \
           ALTER COLUMN rows_total ADD GENERATED BY DEFAULT AS IDENTITY",
        "differs:    column rows_total identity:",
        "0032 must refuse an identity protocol column",
    )
    .await;
}

/// An undeclared user trigger must be refused. (Codex P1 on `7d0733c`.)
///
/// Nothing in the old guard read `pg_trigger` for this table, so a BEFORE INSERT
/// trigger could rewrite an IN_PROGRESS write into a constraint-valid COMPLETED
/// row and FINALIZE would accept evidence no backfill earned.
#[sqlx::test(migrations = "./migrations")]
async fn the_comparator_refuses_an_undeclared_trigger(pool: PgPool) {
    refuse_after(
        &pool,
        "CREATE FUNCTION zz_forge() RETURNS trigger AS $$ BEGIN RETURN NEW; END $$ \
           LANGUAGE plpgsql; \
         CREATE TRIGGER zz_forge_trg BEFORE INSERT ON public.tenancy_backfill_checkpoints \
           FOR EACH ROW EXECUTE FUNCTION zz_forge()",
        "unexpected: trigger zz_forge_trg",
        "0032 must refuse a checkpoint table carrying an undeclared trigger",
    )
    .await;
}

/// A row-security policy must be refused.
#[sqlx::test(migrations = "./migrations")]
async fn the_comparator_refuses_a_row_security_policy(pool: PgPool) {
    let message = refuse_after(
        &pool,
        "ALTER TABLE public.tenancy_backfill_checkpoints ENABLE ROW LEVEL SECURITY; \
         CREATE POLICY zz_policy ON public.tenancy_backfill_checkpoints USING (true)",
        "unexpected: policy zz_policy",
        "0032 must refuse a checkpoint table carrying a policy",
    )
    .await;
    assert!(
        message.contains("differs:    relation self row_security: reference=false live=true"),
        "the relation-level flag must be reported as well: {message}"
    );
}

/// A rewrite rule must be refused, by definition and enabled state.
#[sqlx::test(migrations = "./migrations")]
async fn the_comparator_refuses_a_rewrite_rule(pool: PgPool) {
    refuse_after(
        &pool,
        "CREATE RULE zz_rule AS ON DELETE TO public.tenancy_backfill_checkpoints \
           DO INSTEAD NOTHING",
        "unexpected: rule zz_rule",
        "0032 must refuse a checkpoint table carrying a rewrite rule",
    )
    .await;
}

/// A table INHERITING FROM the checkpoint table must be refused.
///
/// Added because the mutation check found the `inheritance` category was not
/// load-bearing, and then that the obvious test for it did not fix that: an
/// occupant inheriting from a PARENT is already refused by the column category,
/// because inheriting changes `attislocal` and `attinhcount` on the inherited
/// columns.
///
/// A CHILD is the axis only `inheritance` can see. It changes no column of the
/// checkpoint table at all, and it is not cosmetic: a child makes every
/// unqualified `SELECT`/`UPDATE`/`DELETE` on this table also read and write the
/// child's rows, so checkpoint evidence could come from a relation FINALIZE
/// never looks at. PostgreSQL's own `relhassubclass` is deliberately NOT used
/// for this -- it is sticky and is not reliably cleared when the child goes
/// away, so it would produce false refusals.
#[sqlx::test(migrations = "./migrations")]
async fn the_comparator_refuses_a_table_inheriting_from_the_checkpoint_table(pool: PgPool) {
    refuse_after(
        &pool,
        "CREATE TABLE public.zz_child () INHERITS (public.tenancy_backfill_checkpoints)",
        "unexpected: inheritance child zz_child",
        "0032 must refuse a checkpoint table that has an inheritance child",
    )
    .await;
}

/// A permuted column order must be refused.
///
/// No test covered this before round 7. Ordinal position is part of the shape:
/// a positional `INSERT` and any `SELECT *` consumer bind to it.
#[sqlx::test(migrations = "./migrations")]
async fn the_comparator_refuses_a_permuted_column_order(pool: PgPool) {
    refuse_after(
        &pool,
        "ALTER TABLE public.tenancy_backfill_checkpoints DROP COLUMN completed_at; \
         ALTER TABLE public.tenancy_backfill_checkpoints ADD COLUMN completed_at TIMESTAMPTZ",
        "differs:    column completed_at attnum:",
        "0032 must refuse a checkpoint table whose columns are in another order",
    )
    .await;
}

/// A dropped column must be refused.
///
/// A dropped column still occupies its attnum and physical slot, and is evidence
/// the occupant is not the table this migration builds.
#[sqlx::test(migrations = "./migrations")]
async fn the_comparator_refuses_a_table_carrying_a_dropped_column(pool: PgPool) {
    refuse_after(
        &pool,
        "ALTER TABLE public.tenancy_backfill_checkpoints ADD COLUMN zz_temp TEXT; \
         ALTER TABLE public.tenancy_backfill_checkpoints DROP COLUMN zz_temp",
        "unexpected: column attnum 12",
        "0032 must refuse a checkpoint table that carries a dropped column",
    )
    .await;
}

/// A mis-shaped tranche lookup index must be refused.
///
/// Newly compared in round 7 by owner ruling. Earlier rounds deliberately left
/// this index ownership-checked only, on the reasoning that a mis-shaped
/// occupant is a performance question; a whole-contract comparison does not
/// carve out exceptions.
#[sqlx::test(migrations = "./migrations")]
async fn the_comparator_refuses_a_mis_shaped_tranche_index(pool: PgPool) {
    refuse_after(
        &pool,
        "CREATE INDEX idx_tenancy_backfill_checkpoints_tranche \
           ON public.tenancy_backfill_checkpoints (tranche)",
        "differs:    index idx_tenancy_backfill_checkpoints_tranche keyatts: reference=2 live=1",
        "0032 must refuse a mis-shaped tranche lookup index",
    )
    .await;
}

/// A non-default replica identity must be refused.
#[sqlx::test(migrations = "./migrations")]
async fn the_comparator_refuses_a_changed_replica_identity(pool: PgPool) {
    refuse_after(
        &pool,
        "ALTER TABLE public.tenancy_backfill_checkpoints REPLICA IDENTITY FULL",
        "differs:    relation self replica_identity: reference=d live=f",
        "0032 must refuse a checkpoint table whose replica identity was changed",
    )
    .await;
}

/// A non-index relation squatting an index name must be refused.
/// (Codex P1 on `7d0733c`.)
///
/// The old arm read `pg_index`, so a TABLE wearing the index name produced no
/// row and the arm was vacuous. `holder_kind` compares what actually holds the
/// name.
#[sqlx::test(migrations = "./migrations")]
async fn the_comparator_refuses_a_non_index_squatting_the_completion_index_name(pool: PgPool) {
    refuse_after(
        &pool,
        "CREATE TABLE public.tenancy_backfill_checkpoints_completed_key (x int)",
        "differs:    index tenancy_backfill_checkpoints_completed_key holder_kind: \
         reference=i live=r",
        "0032 must refuse a table squatting the completion index name",
    )
    .await;
}

/// The same, for the tranche lookup index name.
#[sqlx::test(migrations = "./migrations")]
async fn the_comparator_refuses_a_non_index_squatting_the_tranche_index_name(pool: PgPool) {
    refuse_after(
        &pool,
        "CREATE SEQUENCE public.idx_tenancy_backfill_checkpoints_tranche",
        "differs:    index idx_tenancy_backfill_checkpoints_tranche holder_kind: \
         reference=i live=S",
        "0032 must refuse a sequence squatting the tranche index name",
    )
    .await;
}

/// The checkpoint stamp must be spelled identically in both places it appears.
///
/// It is declared in the guard block and written again by the stamping block at
/// the foot of the file. SQL has no cross-statement constants and sqlx has no
/// include mechanism, so there is nothing to share and drift is a one-character
/// edit away -- and it fails in the worst direction: 0032 would stamp a string
/// its own guard no longer recognises, so a legitimate re-run over its own rows
/// would be refused as evidence of unknown origin.
///
/// Deliberately separate from `the_provenance_marker_is_identical_everywhere_it_
/// appears`: this is a different string with a different meaning, and folding it
/// into that test's `marker CONSTANT TEXT :=` parse is what would make either
/// test ambiguous.
#[test]
fn the_checkpoint_stamp_is_identical_in_both_places() {
    const STAMP_TAIL: &str =
        "Idempotency stamp for the checkpoint table: it records that this migration \
         created this table, so that a re-run over its own work is a no-op. It is NOT \
         evidence that the rows are authentic.";

    // The literal is wrapped across source lines in both places, so the
    // comparison is on the distinguishing fragments rather than the whole.
    for fragment in [
        "AEON tenancy tranche 1 (migration 0032). Idempotency stamp for the checkpoint ",
        "table: it records that this migration created this table, so that a re-run over ",
        "its own work is a no-op. It is NOT evidence that the rows are authentic.",
    ] {
        assert_eq!(
            PREPARE_UP_SQL.matches(fragment).count(),
            2,
            "the stamp fragment {fragment:?} must appear exactly twice -- once where the \
             guard reads it and once where the stamping block writes it"
        );
    }

    // And the assembled text is what this test claims it is, so a rename that
    // updated both places but changed the meaning still shows up here.
    assert!(
        STAMP_TAIL.contains("NOT evidence that the rows are authentic"),
        "the stamp must keep saying what it is not"
    );
}

/// A non-table wearing the checkpoint name is refused BEFORE any lock is taken.
#[sqlx::test(migrations = "./migrations")]
async fn the_prepare_migration_refuses_a_view_wearing_the_checkpoint_name(pool: PgPool) {
    unwind_to_pre_prepare(&pool).await;
    sqlx::raw_sql(
        "DROP TABLE public.tenancy_backfill_checkpoints; \
         CREATE VIEW public.tenancy_backfill_checkpoints AS SELECT 1 AS id",
    )
    .execute(&pool)
    .await
    .unwrap();

    let (code, message) = expect_refusal(
        &pool,
        PREPARE_UP_SQL,
        "0032 must refuse a view wearing the checkpoint table's name",
    )
    .await;
    assert_eq!(code, "42P07", "duplicate_table is the refusal's SQLSTATE");
    assert!(
        message.contains("is not an ordinary permanent table (relkind=v"),
        "the refusal must name the kind it found, before locking: {message}"
    );
}

/// A grant to PUBLIC on the checkpoint table is refused.
#[sqlx::test(migrations = "./migrations")]
async fn the_privilege_policy_refuses_a_table_grant_to_public(pool: PgPool) {
    unwind_to_pre_prepare(&pool).await;
    replace_checkpoint_table_with_decoy(&pool, false).await;
    sqlx::raw_sql(
        "DELETE FROM public.tenancy_backfill_checkpoints; \
         GRANT SELECT ON public.tenancy_backfill_checkpoints TO PUBLIC",
    )
    .execute(&pool)
    .await
    .unwrap();

    let (code, message) = expect_refusal(
        &pool,
        PREPARE_UP_SQL,
        "0032 must refuse a checkpoint table granted to PUBLIC",
    )
    .await;
    assert_eq!(code, "42501", "insufficient_privilege is the SQLSTATE");
    assert!(
        message.contains("privilege:  table grants SELECT to PUBLIC"),
        "the refusal must name the privilege and the grantee: {message}"
    );
}

/// A column-level grant is refused too.
#[sqlx::test(migrations = "./migrations")]
async fn the_privilege_policy_refuses_a_column_grant(pool: PgPool) {
    unwind_to_pre_prepare(&pool).await;
    replace_checkpoint_table_with_decoy(&pool, false).await;
    sqlx::raw_sql(
        "DELETE FROM public.tenancy_backfill_checkpoints; \
         GRANT SELECT (tranche) ON public.tenancy_backfill_checkpoints TO PUBLIC",
    )
    .execute(&pool)
    .await
    .unwrap();

    let (_, message) = expect_refusal(
        &pool,
        PREPARE_UP_SQL,
        "0032 must refuse a column-level grant on the checkpoint table",
    )
    .await;
    assert!(
        message.contains("privilege:  column tranche grants SELECT to PUBLIC"),
        "the refusal must name the column, the privilege and the grantee: {message}"
    );
}

/// GLOBAL default privileges are caught, on a table this migration creates itself.
///
/// This is why the privilege policy is absolute rather than a comparison against
/// the reference. Nobody runs GRANT here: `ALTER DEFAULT PRIVILEGES` silently
/// attaches the grant to every table the role subsequently creates -- including
/// both the table 0032 builds AND the canonical reference it is compared
/// against -- so a reference-versus-live comparison would find the two identical
/// and pass while both carried a grant the contract never intended.
#[sqlx::test(migrations = "./migrations")]
async fn the_privilege_policy_refuses_global_default_privileges(pool: PgPool) {
    unwind_to_pre_prepare(&pool).await;
    sqlx::raw_sql(
        "DROP TABLE public.tenancy_backfill_checkpoints; \
         ALTER DEFAULT PRIVILEGES GRANT SELECT ON TABLES TO PUBLIC",
    )
    .execute(&pool)
    .await
    .unwrap();

    let (code, message) = expect_refusal(
        &pool,
        PREPARE_UP_SQL,
        "0032 must refuse when global default privileges grant on the table it creates",
    )
    .await;
    assert_eq!(code, "42501", "insufficient_privilege is the SQLSTATE");
    assert!(
        message.contains("privilege:  table grants SELECT to PUBLIC"),
        "the refusal must name the grant default privileges attached: {message}"
    );
}

/// SCHEMA-SCOPED default privileges are caught by the same absolute policy.
#[sqlx::test(migrations = "./migrations")]
async fn the_privilege_policy_refuses_schema_default_privileges(pool: PgPool) {
    unwind_to_pre_prepare(&pool).await;
    sqlx::raw_sql(
        "DROP TABLE public.tenancy_backfill_checkpoints; \
         ALTER DEFAULT PRIVILEGES IN SCHEMA public GRANT SELECT ON TABLES TO PUBLIC",
    )
    .execute(&pool)
    .await
    .unwrap();

    let (code, message) = expect_refusal(
        &pool,
        PREPARE_UP_SQL,
        "0032 must refuse when schema default privileges grant on the table it creates",
    )
    .await;
    assert_eq!(code, "42501", "insufficient_privilege is the SQLSTATE");
    assert!(
        message.contains("privilege:  table grants SELECT to PUBLIC"),
        "the refusal must name the grant default privileges attached: {message}"
    );
}

/// The canonical reference leaves nothing behind -- checked in the SAME session.
///
/// This assertion is only meaningful on the connection that ran the migration.
/// A temp schema is per-session, so querying `pg_temp%` from another pooled
/// connection would inspect somebody else's namespace and pass trivially --
/// exactly the kind of vacuous check this PR has spent six rounds removing.
/// `pg_my_temp_schema()` binds the question to this backend.
#[sqlx::test(migrations = "./migrations")]
async fn the_canonical_reference_is_cleaned_up_in_the_running_session(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();

    sqlx::raw_sql(PREPARE_UP_SQL)
        .execute(&mut *conn)
        .await
        .expect("a re-run over this migration's own work must succeed");
    let left: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pg_class c \
          WHERE c.relnamespace = pg_my_temp_schema() AND c.relkind = 'r'",
    )
    .fetch_one(&mut *conn)
    .await
    .unwrap();
    assert_eq!(
        left, 0,
        "the reference must not survive a successful commit"
    );

    sqlx::raw_sql(
        "CREATE FUNCTION zz_cleanup_probe() RETURNS trigger AS $$ BEGIN RETURN NEW; END $$ \
           LANGUAGE plpgsql; \
         CREATE TRIGGER zz_cleanup_trg BEFORE INSERT ON public.tenancy_backfill_checkpoints \
           FOR EACH ROW EXECUTE FUNCTION zz_cleanup_probe()",
    )
    .execute(&mut *conn)
    .await
    .unwrap();
    sqlx::raw_sql(PREPARE_UP_SQL)
        .execute(&mut *conn)
        .await
        .expect_err("the planted trigger must be refused");
    sqlx::raw_sql("ROLLBACK").execute(&mut *conn).await.ok();

    let after_refusal: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pg_class c \
          WHERE c.relnamespace = pg_my_temp_schema() AND c.relkind = 'r'",
    )
    .fetch_one(&mut *conn)
    .await
    .unwrap();
    assert_eq!(
        after_refusal, 0,
        "the reference must not survive a refusal either"
    );

    let schemas: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pg_namespace \
          WHERE nspname NOT LIKE 'pg\\_%' AND nspname NOT IN ('public', 'information_schema')",
    )
    .fetch_one(&mut *conn)
    .await
    .unwrap();
    assert_eq!(schemas, 0, "no scratch schema may be left behind");
}

/// Two concurrent runs of 0032 must BOTH commit.
///
/// The second blocks on the advisory lock, then finds the table present,
/// stamped and equivalent, and completes as a clean no-op.
///
/// Failure of either run is a test failure, deliberately: "one succeeded and the
/// other errored" would be an alternative success condition that hides exactly
/// the race this test exists to disprove.
#[sqlx::test(migrations = "./migrations")]
async fn two_concurrent_runs_of_the_prepare_migration_both_commit(pool: PgPool) {
    // Both connections are reserved BEFORE either run starts. `#[sqlx::test]`'s
    // pool is a child of one capped master pool, so acquiring lazily can starve
    // the second task and turn a lock test into a pool test.
    let mut first = pool.acquire().await.unwrap();
    let mut second = pool.acquire().await.unwrap();

    let (a, b) = tokio::join!(
        async { sqlx::raw_sql(PREPARE_UP_SQL).execute(&mut *first).await },
        async { sqlx::raw_sql(PREPARE_UP_SQL).execute(&mut *second).await },
    );
    a.expect("the first concurrent run must commit");
    b.expect("the second must wait on the advisory lock, then complete as a no-op");

    let tables: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pg_class \
          WHERE relname = 'tenancy_backfill_checkpoints' AND relkind = 'r'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(tables, 1, "exactly one checkpoint table may exist");

    let indexes: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pg_index \
          WHERE indrelid = 'public.tenancy_backfill_checkpoints'::regclass",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        indexes, 3,
        "the primary key and both declared indexes, created exactly once"
    );
}
