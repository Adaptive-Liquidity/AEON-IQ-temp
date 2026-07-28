//! PostgreSQL-backed proofs for the Step 4A audit.
//!
//! Split from `audit.rs` so the no-database CI path can skip
//! `tenancy::audit_db_tests` the way it already skips `tenancy::db_tests`. The
//! integration job runs `cargo test` unfiltered, so these run there.
//!
//! Every test runs against a freshly migrated database, so what it observes is
//! the *effective* schema — including migration 0028's `ALTER`-added
//! `agents.tenant_id` and migration 0031's `agent_id` → `agent_uuid` rename,
//! neither of which appears in any `CREATE TABLE`.

use sqlx::{PgPool, Row};
use uuid::Uuid;

use super::audit::{self, ReasonCode, Severity, AUDIT_TRANSACTION};
use super::inventory::{self, TableClass, Tranche};
use super::report::{self, TenancyAuditReport};

const TENANT: &str = "11111111-1111-1111-1111-111111111111";
const OTHER_TENANT: &str = "22222222-2222-2222-2222-222222222222";

/// One coherent agent with a session, a memory, an entity and a working-memory
/// row — everything owned by one tenant and fully resolvable.
async fn seed_clean(pool: &PgPool, agent: &str, tenant: &str) {
    sqlx::query(
        "INSERT INTO agents (agent_id, tenant_id, external_agent_id) VALUES ($1, $2::uuid, $3)",
    )
    .bind(agent)
    .bind(tenant)
    .bind(format!("ext-{agent}"))
    .execute(pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO sessions (session_id, agent_id) VALUES ($1, $2)")
        .bind(format!("sess-{agent}"))
        .bind(agent)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO memories (agent_id, session_id, content, memory_type) \
         VALUES ($1, $2, 'ordinary content', 'fact')",
    )
    .bind(agent)
    .bind(format!("sess-{agent}"))
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO entities (agent_id, name, entity_type) VALUES ($1, 'thing', 'person')",
    )
    .bind(agent)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO working_memory (agent_id, session_id) VALUES ($1, $2)")
        .bind(agent)
        .bind(format!("sess-{agent}"))
        .execute(pool)
        .await
        .unwrap();
}

async fn audit(pool: &PgPool) -> TenancyAuditReport {
    audit::run(pool, None).await.expect("audit runs")
}

fn codes_for<'a>(report: &'a TenancyAuditReport, table: &str) -> Vec<&'a str> {
    let mut codes: Vec<&str> = report
        .findings
        .iter()
        .filter(|f| f.table_name == table)
        .map(|f| f.reason_code.as_str())
        .collect();
    codes.sort_unstable();
    codes.dedup();
    codes
}

fn has(report: &TenancyAuditReport, table: &str, code: ReasonCode) -> bool {
    report
        .findings
        .iter()
        .any(|f| f.table_name == table && f.reason_code == code)
}

// ── Inventory ───────────────────────────────────────────────────────────────

#[sqlx::test(migrations = "./migrations")]
async fn discovery_reads_the_effective_schema_not_the_migration_text(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let schema = inventory::discover(&mut conn, inventory::APPLICATION_SCHEMA)
        .await
        .unwrap();

    // `agents.tenant_id` arrives via ALTER in 0028 and appears in no CREATE
    // TABLE; `credential_agent_grants.agent_uuid` is a 0031 rename. A
    // migration-text scan reports neither, which is why the inventory comes
    // from the catalogs.
    let agents = schema.table("agents").expect("agents discovered");
    assert!(agents.column("tenant_id").is_some(), "ALTER-added column");

    let grants = schema
        .table("credential_agent_grants")
        .expect("grants discovered");
    assert!(
        grants.column("agent_uuid").is_some(),
        "0031 rename observed"
    );
    assert!(
        grants.column("agent_id").is_none(),
        "the pre-0031 name must be gone"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn sqlx_and_extension_objects_are_separated_from_application_tables(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let schema = inventory::discover(&mut conn, inventory::APPLICATION_SCHEMA)
        .await
        .unwrap();

    assert!(
        !schema.table_names().contains(&"_sqlx_migrations"),
        "the migration ledger is not application data"
    );
    assert!(
        schema
            .excluded
            .iter()
            .any(|o| o.name == "_sqlx_migrations" && o.reason == inventory::EXCLUSION_SQLX_LEDGER),
        "…but it is reported, not silently dropped: {:?}",
        schema.excluded
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn every_live_application_table_is_registered(pool: PgPool) {
    // The count is discovered, never hard-coded: later migrations add tables,
    // and the registry has to keep up with the database rather than with a
    // number written down once.
    let report = audit(&pool).await;
    let unclassified: Vec<&str> = report
        .findings
        .iter()
        .filter(|f| f.reason_code == ReasonCode::UnclassifiedTable)
        .map(|f| f.table_name.as_str())
        .collect();
    assert!(unclassified.is_empty(), "unclassified: {unclassified:?}");
    assert_eq!(
        report.discovered_application_tables.len(),
        inventory::REGISTRY.len(),
        "registry and live schema must have the same membership"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn a_new_unclassified_table_is_reported(pool: PgPool) {
    // The mechanism that forces a future migration to make a tenancy decision.
    sqlx::query("CREATE TABLE public.brand_new_feature (id UUID PRIMARY KEY, agent_id TEXT)")
        .execute(&pool)
        .await
        .unwrap();

    let report = audit(&pool).await;
    assert!(
        has(&report, "brand_new_feature", ReasonCode::UnclassifiedTable),
        "{:?}",
        codes_for(&report, "brand_new_feature")
    );
    assert!(report.is_blocked(), "an undecided table must block");
}

#[sqlx::test(migrations = "./migrations")]
async fn a_registered_table_that_no_longer_exists_is_reported(pool: PgPool) {
    // `memory_graph` has no dependents, so it drops without cascading.
    sqlx::query("DROP TABLE public.memory_graph")
        .execute(&pool)
        .await
        .unwrap();

    let report = audit(&pool).await;
    assert!(
        has(&report, "memory_graph", ReasonCode::InventoryTableMissing),
        "{:?}",
        codes_for(&report, "memory_graph")
    );
}

// ── Catalog drift refuses to build the scanner ──────────────────────────────

#[sqlx::test(migrations = "./migrations")]
async fn a_dropped_ownership_column_is_reported_as_drift(pool: PgPool) {
    sqlx::query("ALTER TABLE public.memory_graph DROP COLUMN agent_id")
        .execute(&pool)
        .await
        .unwrap();

    let report = audit(&pool).await;
    assert!(
        has(&report, "memory_graph", ReasonCode::SchemaRelationshipDrift),
        "{:?}",
        codes_for(&report, "memory_graph")
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn a_drifted_row_identity_blocks_query_construction(pool: PgPool) {
    // The registry declares `memory_graph`'s identity as `id UUID`. Change the
    // live primary key and the declaration no longer describes the table, so no
    // scanner query may be built for it: auditing rows through a key the
    // database does not have would produce findings about nothing.
    seed_clean(&pool, "agent-one", TENANT).await;
    sqlx::query(
        "INSERT INTO memory_graph (agent_id, subject, predicate, object) \
         VALUES ('ghost', 's', 'p', 'o')",
    )
    .execute(&pool)
    .await
    .unwrap();

    // The orphan is reported while the identity still holds.
    let before = audit(&pool).await;
    assert!(
        has(&before, "memory_graph", ReasonCode::OrphanedAgentReference),
        "precondition: the scanner runs and finds the orphan"
    );

    sqlx::query("ALTER TABLE public.memory_graph DROP CONSTRAINT memory_graph_pkey")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("ALTER TABLE public.memory_graph ADD PRIMARY KEY (agent_id, subject, predicate)")
        .execute(&pool)
        .await
        .unwrap();

    let after = audit(&pool).await;
    assert!(
        has(&after, "memory_graph", ReasonCode::SchemaRelationshipDrift),
        "the identity mismatch must be reported: {:?}",
        codes_for(&after, "memory_graph")
    );
    assert!(
        !has(&after, "memory_graph", ReasonCode::OrphanedAgentReference),
        "no row scanner may run for a drifted table: {:?}",
        codes_for(&after, "memory_graph")
    );
    // The absence of row findings cannot be read as cleanliness, because the
    // drift itself blocks the tranche.
    assert!(after.is_blocked());
    let tranche = after
        .tranche_readiness
        .iter()
        .find(|t| t.tranche == Tranche::RootsAndDirectAgentChildren)
        .unwrap();
    assert!(!tranche.ready, "{:?}", tranche.blocking_reasons);
}

#[sqlx::test(migrations = "./migrations")]
async fn a_catalog_identifier_is_never_interpolated(pool: PgPool) {
    // A column whose name could not survive the identifier validator, made part
    // of the primary key. The registry does not declare it, so the audit
    // reports drift and builds no query. Before this correction the primary key
    // came from the catalog and this name would have reached query
    // construction.
    sqlx::query(
        "ALTER TABLE public.memory_graph ADD COLUMN \"Evil Name\" TEXT NOT NULL DEFAULT ''",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("ALTER TABLE public.memory_graph DROP CONSTRAINT memory_graph_pkey")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("ALTER TABLE public.memory_graph ADD PRIMARY KEY (\"Evil Name\")")
        .execute(&pool)
        .await
        .unwrap();

    // The audit completes rather than erroring: an unsafe *catalog* name is
    // drift, not a registry bug, so it is reported and scanning is skipped.
    let report = audit(&pool).await;
    assert!(has(
        &report,
        "memory_graph",
        ReasonCode::SchemaRelationshipDrift
    ));
    assert!(
        !has(&report, "memory_graph", ReasonCode::OrphanedAgentReference),
        "no scanner query may be built from a catalog-supplied key"
    );
}

// ── Mismatch scanning ───────────────────────────────────────────────────────

#[sqlx::test(migrations = "./migrations")]
async fn clean_fixtures_produce_no_blocking_findings(pool: PgPool) {
    // The control. Without it every adversarial test below could be passing
    // because the scanner reports everything it is shown.
    seed_clean(&pool, "agent-one", TENANT).await;
    seed_clean(&pool, "agent-two", OTHER_TENANT).await;

    let report = audit(&pool).await;
    let blocking: Vec<String> = report
        .findings
        .iter()
        .filter(|f| f.severity == Severity::Blocking)
        .map(|f| format!("{} {}", f.table_name, f.reason_code))
        .collect();
    assert!(blocking.is_empty(), "clean fixtures blocked: {blocking:?}");
    assert_eq!(report.verdict(), "READY");
    assert!(report.tranche_readiness.iter().all(|t| t.ready));
}

#[sqlx::test(migrations = "./migrations")]
async fn an_orphaned_agent_reference_is_reported(pool: PgPool) {
    // `memories.agent_id` has no foreign key, so a memory can name an agent
    // that does not exist — exactly the legacy shape this audit is for.
    sqlx::query(
        "INSERT INTO memories (agent_id, content, memory_type) \
         VALUES ('ghost-agent', 'orphan', 'fact')",
    )
    .execute(&pool)
    .await
    .unwrap();

    let report = audit(&pool).await;
    assert!(
        has(&report, "memories", ReasonCode::OrphanedAgentReference),
        "{:?}",
        codes_for(&report, "memories")
    );
    assert!(has(&report, "memories", ReasonCode::LegacyUnmapped));
}

#[sqlx::test(migrations = "./migrations")]
async fn an_agent_with_a_null_tenant_is_reported_as_unmapped(pool: PgPool) {
    // Step 1 leaves unmapped agents with `tenant_id IS NULL` deliberately, so
    // no `WHERE tenant_id = $1` can ever match them. Their children inherit the
    // problem, and both facts are reported.
    sqlx::query("INSERT INTO agents (agent_id, external_agent_id) VALUES ('drifted', 'ext-d')")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO memories (agent_id, content, memory_type) \
         VALUES ('drifted', 'child', 'fact')",
    )
    .execute(&pool)
    .await
    .unwrap();

    let report = audit(&pool).await;
    assert!(
        has(&report, "agents", ReasonCode::LegacyUnmapped),
        "the agent itself: {:?}",
        codes_for(&report, "agents")
    );
    assert!(
        has(&report, "memories", ReasonCode::UnmappedAgent),
        "the child row: {:?}",
        codes_for(&report, "memories")
    );
    assert!(has(&report, "memories", ReasonCode::LegacyUnmapped));
}

#[sqlx::test(migrations = "./migrations")]
async fn an_orphaned_session_reference_is_reported(pool: PgPool) {
    seed_clean(&pool, "agent-one", TENANT).await;
    // A memory naming a session that does not exist. The agent still resolves,
    // so this is specifically the *session* half failing.
    sqlx::query(
        "INSERT INTO memories (agent_id, session_id, content, memory_type) \
         VALUES ('agent-one', 'no-such-session', 'x', 'fact')",
    )
    .execute(&pool)
    .await
    .unwrap();

    let report = audit(&pool).await;
    assert!(
        has(&report, "memories", ReasonCode::OrphanedSessionReference),
        "{:?}",
        codes_for(&report, "memories")
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn an_orphaned_memory_reference_is_reported(pool: PgPool) {
    seed_clean(&pool, "agent-one", TENANT).await;
    // `memory_conflicts.memory_a` is nullable and its FK is droppable, which is
    // how a legacy dump would arrive.
    sqlx::query(
        "ALTER TABLE public.memory_conflicts DROP CONSTRAINT memory_conflicts_memory_a_fkey",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO memory_conflicts (agent_id, memory_a, reason) \
         VALUES ('agent-one', gen_random_uuid(), 'stale')",
    )
    .execute(&pool)
    .await
    .unwrap();

    let report = audit(&pool).await;
    assert!(
        has(
            &report,
            "memory_conflicts",
            ReasonCode::OrphanedMemoryReference
        ),
        "{:?}",
        codes_for(&report, "memory_conflicts")
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn a_null_ownership_link_is_reported(pool: PgPool) {
    // `audit_logs.agent_id` is the one canonical root the schema legitimately
    // allows to be NULL — some events genuinely have no agent — so this is
    // reachable without drifting the schema. That matters: a relaxed NOT NULL
    // would be catalog drift, and a drifted table is never scanned, so the
    // finding could not be produced that way.
    sqlx::query("INSERT INTO audit_logs (event_type) VALUES ('agentless-event')")
        .execute(&pool)
        .await
        .unwrap();

    let report = audit(&pool).await;
    assert!(
        has(&report, "audit_logs", ReasonCode::NullOwnershipLink),
        "{:?}",
        codes_for(&report, "audit_logs")
    );
    // …and the row is unassignable, which is the row-level state the code
    // exists to describe.
    assert!(has(&report, "audit_logs", ReasonCode::LegacyUnmapped));
    // No drift: the registry declares this column NULL-able and it is.
    assert!(
        !has(&report, "audit_logs", ReasonCode::SchemaRelationshipDrift),
        "a legitimately NULL-able column is not drift"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn a_relaxed_not_null_is_drift_and_stops_the_scan(pool: PgPool) {
    // The other half: relaxing a column the registry declares NOT NULL is a
    // disagreement about the schema itself, so it is reported as drift and no
    // scanner query is built for that table.
    sqlx::query("ALTER TABLE public.memories ALTER COLUMN agent_id DROP NOT NULL")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO memories (content, memory_type) VALUES ('no owner', 'fact')")
        .execute(&pool)
        .await
        .unwrap();

    let report = audit(&pool).await;
    assert!(
        has(&report, "memories", ReasonCode::SchemaRelationshipDrift),
        "{:?}",
        codes_for(&report, "memories")
    );
    assert!(
        !has(&report, "memories", ReasonCode::NullOwnershipLink),
        "a drifted table must not be scanned: {:?}",
        codes_for(&report, "memories")
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn a_cross_tenant_parent_and_child_are_reported(pool: PgPool) {
    seed_clean(&pool, "agent-one", TENANT).await;
    seed_clean(&pool, "agent-two", OTHER_TENANT).await;

    // A version whose denormalised agent belongs to a different tenant than its
    // parent memory. The canonical path says tenant 1, the secondary says
    // tenant 2, and the scanner must refuse rather than prefer either.
    sqlx::query(
        "INSERT INTO memory_versions (memory_id, agent_id, version_number, content, \
                                      memory_type, confidence, change_type, changed_by) \
         SELECT id, 'agent-two', 1, 'v', 'fact', 0.5, 'create', 't' FROM memories \
          WHERE agent_id = 'agent-one' LIMIT 1",
    )
    .execute(&pool)
    .await
    .unwrap();

    let report = audit(&pool).await;
    assert!(
        has(
            &report,
            "memory_versions",
            ReasonCode::CrossTenantParentChild
        ),
        "{:?}",
        codes_for(&report, "memory_versions")
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn an_ownership_path_disagreement_within_one_tenant_is_reported(pool: PgPool) {
    // Same tenant, different agents: not a tenant leak, but the two paths still
    // disagree about who owns the row, and the audit says so with its own code.
    seed_clean(&pool, "agent-one", TENANT).await;
    seed_clean(&pool, "agent-sib", TENANT).await;

    sqlx::query(
        "INSERT INTO memory_versions (memory_id, agent_id, version_number, content, \
                                      memory_type, confidence, change_type, changed_by) \
         SELECT id, 'agent-sib', 1, 'v', 'fact', 0.5, 'create', 't' FROM memories \
          WHERE agent_id = 'agent-one' LIMIT 1",
    )
    .execute(&pool)
    .await
    .unwrap();

    let report = audit(&pool).await;
    assert!(
        has(
            &report,
            "memory_versions",
            ReasonCode::OwnershipPathDisagreement
        ),
        "{:?}",
        codes_for(&report, "memory_versions")
    );
    assert!(
        !has(
            &report,
            "memory_versions",
            ReasonCode::CrossTenantParentChild
        ),
        "no tenant boundary was crossed, so that code must not fire"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn a_future_composite_fk_mismatch_is_reported(pool: PgPool) {
    // The parent row exists but its tenant is NULL, so a composite FK to
    // (id, tenant_id) could never match it — a distinct failure from the parent
    // being absent.
    sqlx::query("INSERT INTO agents (agent_id, external_agent_id) VALUES ('unmapped', 'ext-u')")
        .execute(&pool)
        .await
        .unwrap();
    seed_clean(&pool, "agent-one", TENANT).await;
    sqlx::query(
        "INSERT INTO memory_versions (memory_id, agent_id, version_number, content, \
                                      memory_type, confidence, change_type, changed_by) \
         SELECT id, 'unmapped', 1, 'v', 'fact', 0.5, 'create', 't' FROM memories \
          WHERE agent_id = 'agent-one' LIMIT 1",
    )
    .execute(&pool)
    .await
    .unwrap();

    let report = audit(&pool).await;
    assert!(
        has(
            &report,
            "memory_versions",
            ReasonCode::FutureCompositeFkMismatch
        ),
        "{:?}",
        codes_for(&report, "memory_versions")
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn a_future_tenant_uniqueness_collision_is_reported(pool: PgPool) {
    // Two agents in the *same* tenant using the same session identifier. Legal
    // today, because `sessions` is unique on (agent_id, session_id); fatal to a
    // future UNIQUE (tenant_id, session_id), and the index build would be where
    // it was discovered.
    seed_clean(&pool, "agent-one", TENANT).await;
    sqlx::query(
        "INSERT INTO agents (agent_id, tenant_id, external_agent_id) \
         VALUES ('agent-dup', $1::uuid, 'ext-dup')",
    )
    .bind(TENANT)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO sessions (session_id, agent_id) VALUES ('sess-agent-one', 'agent-dup')",
    )
    .execute(&pool)
    .await
    .unwrap();

    let report = audit(&pool).await;
    assert!(
        has(
            &report,
            "sessions",
            ReasonCode::FutureTenantUniquenessCollision
        ),
        "{:?}",
        codes_for(&report, "sessions")
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn a_blank_legacy_identifier_is_advisory_not_blocking(pool: PgPool) {
    seed_clean(&pool, "agent-one", TENANT).await;
    sqlx::query(
        "INSERT INTO memory_graph (agent_id, subject, predicate, object) VALUES ('', 's', 'p', 'o')",
    )
    .execute(&pool)
    .await
    .unwrap();

    let report = audit(&pool).await;
    let malformed = report
        .findings
        .iter()
        .find(|f| {
            f.table_name == "memory_graph" && f.reason_code == ReasonCode::MalformedLegacyIdentifier
        })
        .expect("blank identifier must be reported");
    assert_eq!(malformed.severity, Severity::Advisory);

    // The same row also fails to resolve, so the tranche blocks — but on the
    // blocking code, never on the advisory one.
    let tranche = report
        .tranche_readiness
        .iter()
        .find(|t| t.tranche == Tranche::RootsAndDirectAgentChildren)
        .unwrap();
    assert!(
        !tranche
            .blocking_reasons
            .iter()
            .any(|r| r.contains("MALFORMED_LEGACY_IDENTIFIER")),
        "an advisory code must never appear as a blocking reason: {:?}",
        tranche.blocking_reasons
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn a_system_global_table_is_never_given_an_inferred_tenant(pool: PgPool) {
    seed_clean(&pool, "agent-one", TENANT).await;
    let report = audit(&pool).await;

    let entry = inventory::entry("agent_tenancy_migrations").unwrap();
    assert_eq!(entry.class, TableClass::SystemGlobal);
    assert!(
        entry.canonical_path.is_none(),
        "a SYSTEM_GLOBAL table must claim no ownership path at all"
    );
    for code in [
        ReasonCode::LegacyUnmapped,
        ReasonCode::UnmappedAgent,
        ReasonCode::UnresolvableOwner,
    ] {
        assert!(
            !has(&report, "agent_tenancy_migrations", code),
            "{code} was attributed to a SYSTEM_GLOBAL table"
        );
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn an_unjustified_global_classification_would_block(pool: PgPool) {
    // The registry cannot be mutated at run time, so the rule is proven where
    // it is enforced: evidence is mandatory and the code that fires without it
    // blocks.
    assert_eq!(
        ReasonCode::GlobalScopeUnverified.severity(),
        Severity::Blocking
    );
    for entry in inventory::REGISTRY {
        if entry.class == TableClass::SystemGlobal {
            assert!(
                entry.global_scope_evidence.is_some(),
                "{}: SYSTEM_GLOBAL without evidence",
                entry.table
            );
        }
    }

    // …and because this table does carry ownership-shaped columns, the audit
    // surfaces that as advisory context rather than accepting it silently.
    let report = audit(&pool).await;
    let advisory = report.findings.iter().any(|f| {
        f.table_name == "agent_tenancy_migrations"
            && f.reason_code == ReasonCode::GlobalScopeUnverified
            && f.severity == Severity::Advisory
    });
    assert!(
        advisory,
        "{:?}",
        codes_for(&report, "agent_tenancy_migrations")
    );
    assert!(!report.is_blocked(), "advisory context must not block");
}

// ── Read-only and determinism ───────────────────────────────────────────────

#[sqlx::test(migrations = "./migrations")]
async fn a_write_inside_the_audit_transaction_is_rejected(pool: PgPool) {
    // The same statement the audit opens with — shared as a constant so this
    // cannot drift onto a different transaction.
    let mut conn = pool.acquire().await.unwrap();
    sqlx::raw_sql(AUDIT_TRANSACTION)
        .execute(&mut *conn)
        .await
        .unwrap();

    let error = sqlx::query(
        "INSERT INTO agents (agent_id, external_agent_id) VALUES ('sneaky', 'ext-sneaky')",
    )
    .execute(&mut *conn)
    .await
    .expect_err("PostgreSQL must refuse a write in a READ ONLY transaction");
    let rendered = format!("{error:?}");
    assert!(
        rendered.contains("read-only") || rendered.contains("25006"),
        "{rendered}"
    );
    sqlx::raw_sql("ROLLBACK").execute(&mut *conn).await.unwrap();
}

#[sqlx::test(migrations = "./migrations")]
async fn the_audit_changes_no_rows(pool: PgPool) {
    // Read-only is enforced by the database; this demonstrates it rather than
    // inferring it from the source, by fingerprinting before and after.
    seed_clean(&pool, "agent-one", TENANT).await;

    let fingerprint = |pool: PgPool| async move {
        let row = sqlx::query(
            "SELECT (SELECT count(*) FROM agents) AS a, (SELECT count(*) FROM memories) AS m, \
                    (SELECT count(*) FROM sessions) AS s, \
                    (SELECT md5(string_agg(agent_id, ',' ORDER BY agent_id)) FROM agents) AS d",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        (
            row.get::<i64, _>("a"),
            row.get::<i64, _>("m"),
            row.get::<i64, _>("s"),
            row.get::<Option<String>, _>("d"),
        )
    };

    let before = fingerprint(pool.clone()).await;
    let _ = audit(&pool).await;
    let after = fingerprint(pool.clone()).await;
    assert_eq!(before, after, "the audit modified the database");
}

#[sqlx::test(migrations = "./migrations")]
async fn repeated_runs_are_byte_identical(pool: PgPool) {
    seed_clean(&pool, "agent-one", TENANT).await;
    sqlx::query(
        "INSERT INTO memories (agent_id, content, memory_type) VALUES ('ghost', 'x', 'fact')",
    )
    .execute(&pool)
    .await
    .unwrap();

    let first = audit(&pool).await;
    let second = audit(&pool).await;
    assert_eq!(
        first.to_json().unwrap(),
        second.to_json().unwrap(),
        "two runs over one snapshot must serialize identically"
    );
    assert_eq!(first.to_string(), second.to_string());

    // Findings are ordered, not merely equal by accident.
    let keys: Vec<(String, String)> = first
        .findings
        .iter()
        .map(|f| (f.table_name.clone(), f.reason_code.as_str().to_string()))
        .collect();
    let mut sorted = keys.clone();
    sorted.sort();
    assert_eq!(keys, sorted, "findings must be deterministically ordered");
}

#[sqlx::test(migrations = "./migrations")]
async fn generated_at_is_the_only_field_that_varies(pool: PgPool) {
    let anonymous = audit::run(&pool, None).await.unwrap();
    let stamped = audit::run(&pool, Some("2026-07-27T00:00:00Z".into()))
        .await
        .unwrap();

    assert_eq!(anonymous.generated_at, None);
    assert_eq!(
        stamped.generated_at.as_deref(),
        Some("2026-07-27T00:00:00Z")
    );

    let mut normalised = stamped.clone();
    normalised.generated_at = None;
    assert_eq!(
        anonymous.to_json().unwrap(),
        normalised.to_json().unwrap(),
        "nothing but the injected timestamp may differ"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn rust_and_postgresql_agree_on_the_pseudonym(pool: PgPool) {
    // The framing is implemented twice — once in Rust, once as generated SQL —
    // so the two are compared rather than assumed to match. Boundary cases are
    // included: values whose lengths differ but whose bare concatenation would
    // not.
    for (table, raw) in [
        ("amp_controller_state", "assistant"),
        ("amp_controller_state", ""),
        ("ab", "c"),
        ("a", "bc"),
        ("weird", "quote'and|pipe"),
        ("unicode", "caf\u{e9}"),
    ] {
        let expected = audit::row_pseudonym(table, raw);
        let actual: String = sqlx::query_scalar(
            "SELECT encode(sha256(convert_to( \
               octet_length($1::text)::text || ':' || $1::text || \
               octet_length($2::text)::text || ':' || $2::text || \
               octet_length($3::text)::text || ':' || $3::text, 'UTF8')), 'hex')",
        )
        .bind(audit::ROW_ID_DOMAIN)
        .bind(table)
        .bind(raw)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(expected, actual, "table={table} raw={raw}");
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn the_checked_in_artifacts_match_the_registry(pool: PgPool) {
    // The registry is the source of truth; the artifacts exist for review. If
    // they drift, a reviewer is reading a decision the code no longer makes.
    let json_on_disk = include_str!("../../docs/tenancy/step4a-inventory.json");
    let markdown_on_disk = include_str!("../../docs/tenancy/step4a-inventory.md");

    assert_eq!(
        report::render_inventory_json().unwrap().trim(),
        json_on_disk.trim(),
        "docs/tenancy/step4a-inventory.json is stale; regenerate it"
    );
    assert_eq!(
        report::render_inventory_markdown().trim(),
        markdown_on_disk.trim(),
        "docs/tenancy/step4a-inventory.md is stale; regenerate it"
    );

    // The digest the report carries is the digest the artifact was rendered
    // from, so the two can be told apart.
    let report = audit(&pool).await;
    assert!(json_on_disk.contains(&report.inventory_digest));

    // …and it covers a payload that structurally cannot contain it.
    let payload = serde_json::to_string(&report::canonical_inventory_payload()).unwrap();
    assert!(
        !payload.contains("inventory_digest"),
        "the digest must not be part of what it covers"
    );
}

// ── Confidentiality ─────────────────────────────────────────────────────────

#[sqlx::test(migrations = "./migrations")]
async fn no_row_content_reaches_either_report_form(pool: PgPool) {
    // Seed every category of sensitive value the audited tables can hold, then
    // require that none of it appears in either rendering. The report model has
    // no field that could carry row text, so this checks a structural property
    // — but a filter is what a future change would reach for, and this is what
    // would catch it.
    const MEMORY_CONTENT: &str = "SECRET-MEMORY-CONTENT-a1b2c3";
    const QUERY_TEXT: &str = "SECRET-RETRIEVAL-QUERY-d4e5f6";
    const PROVIDER_TEXT: &str = "SECRET-PROVIDER-RESPONSE-778899";
    const TOKEN: &str = "sk-SECRET-CREDENTIAL-TOKEN-00112233";
    const KEY_MATERIAL: &str = "SECRET-HMAC-PEPPER-deadbeefcafe";
    const AGENT_ID: &str = "SECRET-CALLER-SUPPLIED-AGENT-ID";
    const SESSION_ID: &str = "SECRET-CALLER-SUPPLIED-SESSION-ID";

    sqlx::query(
        "INSERT INTO agents (agent_id, tenant_id, external_agent_id) VALUES ($1, $2::uuid, $3)",
    )
    .bind(AGENT_ID)
    .bind(TENANT)
    .bind(KEY_MATERIAL)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO sessions (session_id, agent_id) VALUES ($1, $2)")
        .bind(SESSION_ID)
        .bind(AGENT_ID)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO memories (agent_id, session_id, content, memory_type, provenance) \
         VALUES ($1, $2, $3, 'fact', $4)",
    )
    .bind(AGENT_ID)
    .bind(SESSION_ID)
    .bind(MEMORY_CONTENT)
    .bind(PROVIDER_TEXT)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO memory_retrieval_logs (agent_id, session_id, query_text, query_hash) \
         VALUES ($1, $2, $3, 'h')",
    )
    .bind(AGENT_ID)
    .bind(SESSION_ID)
    .bind(QUERY_TEXT)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO entities (agent_id, name, entity_type) VALUES ($1, $2, 'person')")
        .bind(AGENT_ID)
        .bind(TOKEN)
        .execute(&pool)
        .await
        .unwrap();
    // An unmapped agent, so the report is non-empty and actually has findings
    // that could have leaked something.
    sqlx::query("INSERT INTO agents (agent_id, external_agent_id) VALUES ('drifted', 'ext-d')")
        .execute(&pool)
        .await
        .unwrap();

    let report = audit(&pool).await;
    assert!(!report.findings.is_empty(), "the report must have content");

    let json = report.to_json().unwrap();
    let text = report.to_string();
    for secret in [
        MEMORY_CONTENT,
        QUERY_TEXT,
        PROVIDER_TEXT,
        TOKEN,
        KEY_MATERIAL,
        AGENT_ID,
        SESSION_ID,
    ] {
        assert!(!json.contains(secret), "JSON leaked `{secret}`");
        assert!(!text.contains(secret), "operator output leaked `{secret}`");
    }

    // Nor may the credential MAC column ever be read: the generated SQL names
    // only registry columns, and `secret_mac` is not one of them.
    assert!(!json.contains("secret_mac"));
}

#[sqlx::test(migrations = "./migrations")]
async fn a_caller_controlled_text_key_is_pseudonymised(pool: PgPool) {
    // `amp_controller_state`'s primary key *is* the caller-supplied
    // `agent_id`, so any identifier the report emits for it must be the
    // domain-separated digest — and must equal what Rust computes.
    const AGENT_ID: &str = "SECRET-CONTROLLER-AGENT-ID";
    sqlx::query(
        "INSERT INTO amp_controller_state (agent_id, aggressiveness, integral_error) \
         VALUES ($1, 0.5, 0.0)",
    )
    .bind(AGENT_ID)
    .execute(&pool)
    .await
    .unwrap();

    let report = audit(&pool).await;
    let identifier = report
        .findings
        .iter()
        .find(|f| f.table_name == "amp_controller_state")
        .and_then(|f| f.row_identifier.clone())
        .expect("an orphaned controller row must be reported");

    assert!(!identifier.contains(AGENT_ID), "{identifier}");
    assert_eq!(identifier.len(), 64, "a full SHA-256 digest: {identifier}");
    assert_eq!(
        identifier,
        audit::row_pseudonym("amp_controller_state", AGENT_ID),
        "the emitted pseudonym must be the framed digest, not an ad-hoc hash"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn a_surrogate_key_is_emitted_readably(pool: PgPool) {
    // The counterpart: identifiers that are already opaque stay readable, so an
    // operator can actually look the row up.
    sqlx::query(
        "INSERT INTO memories (agent_id, content, memory_type) VALUES ('ghost', 'x', 'fact')",
    )
    .execute(&pool)
    .await
    .unwrap();

    let report = audit(&pool).await;
    let identifier = report
        .findings
        .iter()
        .find(|f| f.table_name == "memories")
        .and_then(|f| f.row_identifier.clone())
        .expect("the orphan must be reported");
    assert!(Uuid::parse_str(&identifier).is_ok(), "{identifier}");
}

// ── Tranche readiness ───────────────────────────────────────────────────────

#[sqlx::test(migrations = "./migrations")]
async fn a_tranche_blocks_on_its_own_tables_and_others_are_unaffected(pool: PgPool) {
    seed_clean(&pool, "agent-one", TENANT).await;
    assert!(
        audit(&pool).await.tranche_readiness.iter().all(|t| t.ready),
        "clean fixtures leave every tranche ready"
    );

    // `entities` is a tranche-1 table; break only that one.
    sqlx::query("INSERT INTO entities (agent_id, name, entity_type) VALUES ('ghost', 'n', 'x')")
        .execute(&pool)
        .await
        .unwrap();

    let report = audit(&pool).await;
    let tranche_one = report
        .tranche_readiness
        .iter()
        .find(|t| t.tranche == Tranche::RootsAndDirectAgentChildren)
        .unwrap();
    assert!(!tranche_one.ready, "tranche 1 must block");
    assert!(
        tranche_one
            .blocking_reasons
            .iter()
            .any(|r| r.starts_with("entities:")),
        "{:?}",
        tranche_one.blocking_reasons
    );

    // Readiness is per-tranche, not one global flag.
    let tranche_two = report
        .tranche_readiness
        .iter()
        .find(|t| t.tranche == Tranche::Sessions)
        .unwrap();
    assert!(tranche_two.ready, "{:?}", tranche_two.blocking_reasons);
}

#[sqlx::test(migrations = "./migrations")]
async fn the_report_carries_the_schema_version_and_digest(pool: PgPool) {
    let report = audit(&pool).await;
    assert_eq!(report.schema_version, audit::REPORT_SCHEMA_VERSION);
    assert!(report.inventory_digest.starts_with("sha256:"));
    assert_eq!(report.inventory_digest.len(), "sha256:".len() + 64);
    assert_eq!(report.classified_tables.len(), inventory::REGISTRY.len());
    // Both renderings describe the same object.
    assert!(report.to_string().contains(&report.inventory_digest));
    assert!(report.to_json().unwrap().contains(&report.inventory_digest));
}
