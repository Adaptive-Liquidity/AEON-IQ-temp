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
    let decoy_exists: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM pg_trigger t JOIN pg_class c ON c.oid = t.tgrelid \
         WHERE t.tgname = 'trg_entities_tenancy_bridge' AND c.relname = 'zz_trigger_decoy')",
    )
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
    assert!(
        decoy_exists,
        "and the decoy trigger is none of its business"
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
