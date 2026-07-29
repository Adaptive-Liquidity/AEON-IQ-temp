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
    const GUARD: &str = include_str!("../../migrations/0041_tenancy_tranche1_constraints.sql");

    sqlx::query(
        "UPDATE pg_index SET indisvalid = false \
         WHERE indexrelid = 'public.idx_entities_tenant'::regclass",
    )
    .execute(&pool)
    .await
    .expect("the test role must be able to mark an index invalid");

    let error = sqlx::raw_sql(GUARD).execute(&pool).await.unwrap_err();
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
    const CONSTRAINTS_DOWN: &str =
        include_str!("../../rollback/0041_tenancy_tranche1_constraints_down.sql");
    const GRANTS_HARDENING_DOWN: &str =
        include_str!("../../rollback/0031_credential_agent_grants_hardening_down.sql");
    const GRANTS_DOWN: &str = include_str!("../../rollback/0030_credential_agent_grants_down.sql");
    const STEP_ONE_DOWN: &str = include_str!("../../rollback/0028_agent_tenancy_identity_down.sql");

    // Drop every foreign key the old guard checked, and nothing else.
    sqlx::raw_sql(CONSTRAINTS_DOWN)
        .execute(&pool)
        .await
        .expect("0041 rollback");
    // And clear 0031/0030 too, or their own ordering guard fires first and this
    // test passes for the wrong reason — proving the grants guard works rather
    // than the bridge guard.
    sqlx::raw_sql(GRANTS_HARDENING_DOWN)
        .execute(&pool)
        .await
        .expect("0031 rollback");
    sqlx::raw_sql(GRANTS_DOWN)
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
    let error = sqlx::raw_sql(STEP_ONE_DOWN)
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
    const CONSTRAINTS_DOWN: &str =
        include_str!("../../rollback/0041_tenancy_tranche1_constraints_down.sql");
    const INDEXES_DOWN: &str =
        include_str!("../../rollback/0033_tenancy_tranche1_indexes_down.sql");
    const PREPARE_DOWN: &str =
        include_str!("../../rollback/0032_tenancy_tranche1_prepare_down.sql");
    const PREPARE_UP: &str = include_str!("../../migrations/0032_tenancy_tranche1_prepare.sql");

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
        ("0041", CONSTRAINTS_DOWN),
        ("0033-0040", INDEXES_DOWN),
        ("0032", PREPARE_DOWN),
    ] {
        sqlx::raw_sql(script)
            .execute(&pool)
            .await
            .unwrap_or_else(|e| panic!("{label} rollback: {e}"));
    }

    // Reapply PREPARE, exactly as `sqlx migrate run` would after the rollback
    // deleted version 32.
    sqlx::raw_sql(PREPARE_UP)
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
const CONSTRAINTS_UP: &str = include_str!("../../migrations/0041_tenancy_tranche1_constraints.sql");
const CONSTRAINTS_DOWN_SQL: &str =
    include_str!("../../rollback/0041_tenancy_tranche1_constraints_down.sql");
const INDEXES_DOWN_SQL: &str =
    include_str!("../../rollback/0033_tenancy_tranche1_indexes_down.sql");
const PREPARE_DOWN_SQL: &str =
    include_str!("../../rollback/0032_tenancy_tranche1_prepare_down.sql");

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
    assert_eq!(db.code().as_deref(), Some("2BP01"), "{error}");
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
    const GRANTS_HARDENING_DOWN: &str =
        include_str!("../../rollback/0031_credential_agent_grants_hardening_down.sql");
    const GRANTS_DOWN: &str = include_str!("../../rollback/0030_credential_agent_grants_down.sql");
    const STEP_ONE_DOWN: &str = include_str!("../../rollback/0028_agent_tenancy_identity_down.sql");

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

    sqlx::raw_sql(GRANTS_HARDENING_DOWN)
        .execute(&pool)
        .await
        .expect("0031 rollback");
    sqlx::raw_sql(GRANTS_DOWN)
        .execute(&pool)
        .await
        .expect("0030 rollback");
    sqlx::raw_sql(STEP_ONE_DOWN)
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
