//! Tranche 1 PREPARE behaviour, measured against a live database.
//!
//! Split from `audit_db_tests.rs` for the same reason that file was split from
//! `audit.rs`: the no-database CI path skips `tenancy::*_db_tests` by name, and
//! the integration job runs `cargo test` unfiltered.
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
