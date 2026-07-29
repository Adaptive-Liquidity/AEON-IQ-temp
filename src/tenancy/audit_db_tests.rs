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
use super::plan;
use super::report::{self, ContractStatus, TenancyAuditReport};

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
    // The root is itself an unmapped agent. Before the tenant-root-aware
    // predicate this never fired, because the tenant-column path resolves no
    // agent to compare against.
    assert!(
        has(&report, "agents", ReasonCode::UnmappedAgent),
        "the root must report itself unmapped: {:?}",
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
async fn a_root_only_unmapped_agent_keeps_tranche_one_blocked(pool: PgPool) {
    // No child rows at all — just one agent whose tenant is NULL. This is the
    // shape that slipped through: with nothing downstream to report, the audit
    // saw only NULL_OWNERSHIP_LINK, and tranche 1 read READY even though its
    // planned `SET NOT NULL on agents.tenant_id` could not possibly succeed.
    sqlx::query("INSERT INTO agents (agent_id, external_agent_id) VALUES ('lonely', 'ext-lonely')")
        .execute(&pool)
        .await
        .unwrap();

    let report = audit(&pool).await;
    assert!(
        has(&report, "agents", ReasonCode::UnmappedAgent),
        "{:?}",
        codes_for(&report, "agents")
    );
    assert!(has(&report, "agents", ReasonCode::LegacyUnmapped));

    let tranche_one = report
        .tranche_readiness
        .iter()
        .find(|t| t.tranche == Tranche::RootsAndDirectAgentChildren)
        .unwrap();
    assert!(
        !tranche_one.ready,
        "tranche 1 must block on its own root: {:?}",
        tranche_one.blocking_reasons
    );
    assert!(
        tranche_one
            .blocking_reasons
            .iter()
            .any(|r| r == "agents: UNMAPPED_AGENT"),
        "{:?}",
        tranche_one.blocking_reasons
    );
    assert!(report.is_blocked());
}

#[sqlx::test(migrations = "./migrations")]
async fn an_orphaned_session_reference_is_reported(pool: PgPool) {
    // `working_memory` is the only table where a missing session is a genuine
    // inconsistency, because it is the only one whose row and whose session row
    // are created by the *same* upsert. Everywhere else a session reference can
    // legitimately outrun its session — see the CONTEXT_ONLY test below.
    seed_clean(&pool, "agent-one", TENANT).await;
    sqlx::query(
        "INSERT INTO working_memory (agent_id, session_id) \
         VALUES ('agent-one', 'no-such-session')",
    )
    .execute(&pool)
    .await
    .unwrap();

    let report = audit(&pool).await;
    let finding = report
        .findings
        .iter()
        .find(|f| {
            f.table_name == "working_memory"
                && f.reason_code == ReasonCode::OrphanedSessionReference
        })
        .unwrap_or_else(|| {
            panic!(
                "expected an orphaned session: {:?}",
                codes_for(&report, "working_memory")
            )
        });
    // Stays BLOCKING. The two production writes are not atomic, so a live audit
    // can catch the gap between them — the registry records that a readiness
    // scan must run with extraction writers quiesced. A *persistent* orphan is
    // still a real inconsistency, and weakening this finding would hide it.
    assert_eq!(finding.severity, Severity::Blocking);
    assert!(report.is_blocked());
}

#[sqlx::test(migrations = "./migrations")]
async fn a_context_only_session_never_reports_an_orphan(pool: PgPool) {
    // The five tables whose `session_id` is CONTEXT_ONLY. `sessions` rows are
    // created by exactly one code path — the working-memory upsert — and it
    // runs at the *end* of successful extraction, so a pending job, a
    // permanently failed job, or a memory written before that upsert
    // legitimately names a session that does not exist. Reporting those as
    // orphans would block Step 4B on entirely normal states.
    seed_clean(&pool, "agent-one", TENANT).await;

    for statement in [
        "INSERT INTO memories (agent_id, session_id, content, memory_type) \
         VALUES ('agent-one', 'no-such-session', 'x', 'fact')",
        "INSERT INTO extraction_jobs (agent_id, session_id, payload, status, next_attempt_at) \
         VALUES ('agent-one', 'no-such-session', '{}'::jsonb, 'pending', NOW())",
        "INSERT INTO memory_retrieval_logs (agent_id, session_id, query_hash) \
         VALUES ('agent-one', 'no-such-session', 'h')",
        "INSERT INTO cognitive_hypervisor_timeline (agent_id, session_id, event_type, occurred_at) \
         VALUES ('agent-one', 'no-such-session', 'x', NOW())",
        "INSERT INTO rmk_episodes (agent_id, session_id, task_success, token_savings, \
                                   retrieval_precision, eviction_cost, reward) \
         VALUES ('agent-one', 'no-such-session', 0, 0, 0, 0, 0)",
    ] {
        sqlx::query(statement).execute(&pool).await.unwrap();
    }

    let report = audit(&pool).await;
    for table in [
        "memories",
        "extraction_jobs",
        "memory_retrieval_logs",
        "cognitive_hypervisor_timeline",
        "rmk_episodes",
    ] {
        assert!(
            !has(&report, table, ReasonCode::OrphanedSessionReference),
            "{table} is CONTEXT_ONLY and must not report a session orphan: {:?}",
            codes_for(&report, table)
        );
        // …and the registry says so explicitly, rather than the field merely
        // having been forgotten.
        let entry = inventory::entry(table).unwrap();
        assert_eq!(
            entry.session_semantics.map(|s| s.role),
            Some(inventory::SessionRole::ContextOnly),
            "{table}"
        );
        assert!(
            entry
                .secondary_paths
                .iter()
                .chain(entry.canonical_path.iter())
                .all(|p| !matches!(p.kind, inventory::PathKind::Session { .. })),
            "{table} must carry no session ownership path"
        );
    }

    // The whole report is clean: these are normal states, not findings.
    assert!(!report.is_blocked(), "{:?}", report.findings);
}

#[sqlx::test(migrations = "./migrations")]
async fn an_orphaned_memory_reference_drifts_before_it_can_be_counted(pool: PgPool) {
    // Every memory reference in this schema is backed by a declared foreign
    // key, so an orphan cannot exist while the contract holds — producing one
    // requires dropping the very constraint the registry requires.
    //
    // That makes this the boundary case for the three-way gate: the drop is
    // contract drift, so the table is DRIFTED and its ordinary ownership
    // conclusions are withheld. The orphan rows are *not* individually counted.
    // They are not lost either — the drift blocks the tranche, which is the
    // decision those counts would have fed.
    seed_clean(&pool, "agent-one", TENANT).await;
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
    assert_eq!(
        status_of(&report, "memory_conflicts"),
        Some(ContractStatus::Drifted),
        "{:?}",
        diagnostics_for(&report, "memory_conflicts")
    );
    assert!(drift_mentions(
        &report,
        "memory_conflicts",
        "memory_conflicts_memory_a_fkey"
    ));
    assert!(
        !has(
            &report,
            "memory_conflicts",
            ReasonCode::OrphanedMemoryReference
        ),
        "an orphan count computed without the FK that guarantees the parent is an ordinary \
         authoritative conclusion, and must be withheld: {:?}",
        codes_for(&report, "memory_conflicts")
    );
    assert!(
        !report
            .tranche_readiness
            .iter()
            .find(|t| t.tranche == Tranche::LineageAndArchival)
            .unwrap()
            .ready,
        "the withheld counts must not read as cleanliness"
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
async fn two_agents_in_one_tenant_may_share_a_session_identifier(pool: PgPool) {
    // The scenario that decided the session-identity question. Two agents in the
    // *same* tenant using the same caller-supplied session string is ordinary
    // caller behaviour — `session_id` is TEXT the caller chooses, and `default`
    // or `main` is exactly what several agents will pick.
    //
    // Under a tenant-scoped `UNIQUE (tenant_id, session_id)` this would be a
    // collision, and the index build would be where it was discovered. Step 4B
    // therefore keeps session identity AGENT-scoped —
    // `UNIQUE (tenant_id, agent_uuid, session_id)` — which is a superset of the
    // current `(agent_id, session_id)` key and so cannot collide.
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
        !has(
            &report,
            "sessions",
            ReasonCode::FutureTenantUniquenessCollision
        ),
        "agent-scoped session identity must not report this as a collision: {:?}",
        codes_for(&report, "sessions")
    );
    // …and the rows are otherwise clean, so the absence is a real pass rather
    // than a scan that never ran.
    assert_eq!(
        status_of(&report, "sessions"),
        Some(ContractStatus::Satisfied)
    );
    assert!(!report.is_blocked(), "{:?}", codes_for(&report, "sessions"));
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
    // Opened exactly the way the audit opens it — `begin_with` on the shared
    // constant — so this cannot drift onto a different transaction or a
    // different mechanism than the one production uses.
    let mut tx = pool.begin_with(AUDIT_TRANSACTION).await.unwrap();

    let error = sqlx::query(
        "INSERT INTO agents (agent_id, external_agent_id) VALUES ('sneaky', 'ext-sneaky')",
    )
    .execute(&mut *tx)
    .await
    .expect_err("PostgreSQL must refuse a write in a READ ONLY transaction");
    let rendered = format!("{error:?}");
    assert!(
        rendered.contains("read-only") || rendered.contains("25006"),
        "{rendered}"
    );
    tx.rollback().await.unwrap();
}

#[sqlx::test(migrations = "./migrations")]
async fn the_audit_returns_its_connection_usable(pool: PgPool) {
    // The managed transaction's point: whatever the audit does, the connection
    // goes back to the pool with no transaction open and no READ ONLY still in
    // force. A manual BEGIN/COMMIT leaks that state on any early return, and
    // the next borrower would silently fail its writes.
    seed_clean(&pool, "agent-one", TENANT).await;
    audit(&pool).await;

    // A write on a pooled connection afterwards proves both halves at once.
    sqlx::query("INSERT INTO agents (agent_id, external_agent_id) VALUES ('after', 'ext-after')")
        .execute(&pool)
        .await
        .expect("the pool must hand back a writable, transaction-free connection");

    let in_transaction: bool = sqlx::query("SELECT pg_current_xact_id_if_assigned() IS NOT NULL")
        .fetch_one(&pool)
        .await
        .unwrap()
        .get(0);
    assert!(
        !in_transaction,
        "no audit transaction may still be open on a pooled connection"
    );

    // And the audit still works on a pool that has been used this way.
    let report = audit(&pool).await;
    assert_eq!(report.schema_version, audit::REPORT_SCHEMA_VERSION);
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

    // Every finding for the table, not just the first. Checking one leaves the
    // others unexamined, and the ones a future change adds would be exactly the
    // unexamined ones — a second finding emitting the raw key would have passed
    // a first-only assertion unchanged.
    let findings: Vec<_> = report
        .findings
        .iter()
        .filter(|f| f.table_name == "amp_controller_state")
        .collect();
    assert!(
        !findings.is_empty(),
        "an orphaned controller row must be reported"
    );

    let expected = audit::row_pseudonym("amp_controller_state", AGENT_ID);
    let mut seen_identifier = false;
    for finding in &findings {
        let Some(identifier) = &finding.row_identifier else {
            continue;
        };
        seen_identifier = true;
        assert!(
            !identifier.contains(AGENT_ID),
            "{} leaked the raw key: {identifier}",
            finding.reason_code
        );
        assert_eq!(
            identifier.len(),
            64,
            "{} must emit a full SHA-256 digest: {identifier}",
            finding.reason_code
        );
        assert_eq!(
            *identifier, expected,
            "{} must emit the framed digest, not an ad-hoc hash",
            finding.reason_code
        );
    }
    assert!(
        seen_identifier,
        "at least one finding must carry an example identifier, or this proves nothing"
    );

    // …and neither rendering may carry the raw key anywhere else either — a
    // diagnostic string or a path label would leak it just as effectively as a
    // row identifier.
    let json = report.to_json().unwrap();
    let text = report.to_string();
    assert!(!json.contains(AGENT_ID), "machine JSON leaked the raw key");
    assert!(
        !text.contains(AGENT_ID),
        "operator rendering leaked the raw key"
    );
    assert!(
        json.contains(&expected),
        "the pseudonym must actually reach the machine report"
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

// ── Live current-schema contract verification ───────────────────────────────
//
// Every test here breaks exactly one declared guarantee and requires the audit
// to notice. The control — an untouched migrated schema reporting SATISFIED for
// every table — is what stops the verifier from passing by rejecting
// everything.

fn status_of(report: &TenancyAuditReport, table: &str) -> Option<ContractStatus> {
    report
        .classified_tables
        .iter()
        .find(|t| t.table == table)
        .expect("table is in the registry")
        .contract_status
}

/// Did the audit report drift naming this object?
fn drift_mentions(report: &TenancyAuditReport, table: &str, needle: &str) -> bool {
    report.findings.iter().any(|f| {
        f.table_name == table
            && f.reason_code == ReasonCode::SchemaRelationshipDrift
            && f.diagnostic.contains(needle)
    })
}

/// Every diagnostic for one table, for assertion messages.
fn diagnostics_for(report: &TenancyAuditReport, table: &str) -> Vec<String> {
    report
        .findings
        .iter()
        .filter(|f| f.table_name == table)
        .map(|f| f.diagnostic.clone())
        .collect()
}

#[sqlx::test(migrations = "./migrations")]
async fn the_unchanged_migrated_schema_satisfies_every_declared_contract(pool: PgPool) {
    // The control case. Without it, a verifier that rejected every object would
    // pass all the destructive tests below and prove nothing at all.
    let report = audit(&pool).await;

    for table in &report.classified_tables {
        assert_eq!(
            table.contract_status,
            Some(ContractStatus::Satisfied),
            "`{}` must satisfy its declared contract on an untouched schema; findings: {:?}",
            table.table,
            diagnostics_for(&report, table.table)
        );
    }

    // …and none of that status came from an empty declaration.
    let declared: usize = report
        .classified_tables
        .iter()
        .map(|t| t.required_current_schema_contract.len())
        .sum();
    assert!(
        declared >= inventory::REGISTRY.len(),
        "every table must declare at least one object, or SATISFIED is vacuous: {declared}"
    );

    assert!(
        !report
            .findings
            .iter()
            .any(|f| f.reason_code == ReasonCode::SchemaRelationshipDrift),
        "a clean schema must produce no drift"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn a_dropped_primary_key_is_fatal_not_merely_drifted(pool: PgPool) {
    // The primary key is also the declared row identity, so losing it is fatal
    // structural drift rather than contract drift: no query may be built at
    // all, and the status must say NOT_EVALUATED rather than DRIFTED.
    sqlx::query("ALTER TABLE public.memory_graph DROP CONSTRAINT memory_graph_pkey")
        .execute(&pool)
        .await
        .unwrap();

    let report = audit(&pool).await;
    assert_eq!(
        status_of(&report, "memory_graph"),
        Some(ContractStatus::NotEvaluated),
        "{:?}",
        codes_for(&report, "memory_graph")
    );
    assert!(has(
        &report,
        "memory_graph",
        ReasonCode::SchemaRelationshipDrift
    ));
    // Everything else stayed evaluable.
    assert_eq!(
        status_of(&report, "memories"),
        Some(ContractStatus::Satisfied)
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn a_dropped_unique_constraint_is_reported(pool: PgPool) {
    sqlx::query("ALTER TABLE public.agents DROP CONSTRAINT agents_tenant_id_external_agent_id_key")
        .execute(&pool)
        .await
        .unwrap();

    let report = audit(&pool).await;
    assert_eq!(status_of(&report, "agents"), Some(ContractStatus::Drifted));
    assert!(
        drift_mentions(&report, "agents", "does not exist"),
        "{:?}",
        diagnostics_for(&report, "agents")
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn a_dropped_standalone_unique_index_is_reported(pool: PgPool) {
    // `idx_sessions_agent_session` has no `pg_constraint` row at all — it is a
    // bare `CREATE UNIQUE INDEX`. A verifier that consulted only
    // `pg_constraint` would report it missing on a healthy database *and* miss
    // it actually being dropped here.
    sqlx::query("DROP INDEX public.idx_sessions_agent_session")
        .execute(&pool)
        .await
        .unwrap();

    let report = audit(&pool).await;
    assert_eq!(
        status_of(&report, "sessions"),
        Some(ContractStatus::Drifted)
    );
    assert!(
        drift_mentions(&report, "sessions", "idx_sessions_agent_session"),
        "{:?}",
        diagnostics_for(&report, "sessions")
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn a_dropped_foreign_key_is_reported(pool: PgPool) {
    sqlx::query(
        "ALTER TABLE public.archival_batches DROP CONSTRAINT archival_batches_agent_id_fkey",
    )
    .execute(&pool)
    .await
    .unwrap();

    let report = audit(&pool).await;
    assert_eq!(
        status_of(&report, "archival_batches"),
        Some(ContractStatus::Drifted)
    );
    assert!(drift_mentions(
        &report,
        "archival_batches",
        "archival_batches_agent_id_fkey"
    ));
}

#[sqlx::test(migrations = "./migrations")]
async fn reordered_constraint_columns_are_reported(pool: PgPool) {
    // Same name, same columns, different order. Every name-based check passes;
    // the object is not the one that was declared.
    sqlx::query("ALTER TABLE public.agents DROP CONSTRAINT agents_tenant_id_external_agent_id_key")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "ALTER TABLE public.agents ADD CONSTRAINT agents_tenant_id_external_agent_id_key \
         UNIQUE (external_agent_id, tenant_id)",
    )
    .execute(&pool)
    .await
    .unwrap();

    let report = audit(&pool).await;
    assert_eq!(status_of(&report, "agents"), Some(ContractStatus::Drifted));
    assert!(
        drift_mentions(&report, "agents", "in that order"),
        "{:?}",
        diagnostics_for(&report, "agents")
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn a_foreign_key_pointing_at_the_wrong_columns_is_reported(pool: PgPool) {
    // The FK still exists, still has its declared name, still covers the
    // declared local column — and references a different column of the same
    // parent, so it guarantees something else entirely.
    sqlx::query(
        "ALTER TABLE public.agents ADD CONSTRAINT agents_external_only_key \
         UNIQUE (external_agent_id)",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "ALTER TABLE public.archival_batches DROP CONSTRAINT archival_batches_agent_id_fkey",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "ALTER TABLE public.archival_batches ADD CONSTRAINT archival_batches_agent_id_fkey \
         FOREIGN KEY (agent_id) REFERENCES public.agents (external_agent_id)",
    )
    .execute(&pool)
    .await
    .unwrap();

    let report = audit(&pool).await;
    assert_eq!(
        status_of(&report, "archival_batches"),
        Some(ContractStatus::Drifted)
    );
    assert!(
        drift_mentions(&report, "archival_batches", "is required to reference ("),
        "{:?}",
        diagnostics_for(&report, "archival_batches")
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn a_not_valid_foreign_key_is_reported(pool: PgPool) {
    // NOT VALID constrains new rows only. The scanner treats this join as
    // authoritative over rows that already exist, which is exactly what
    // NOT VALID does not promise.
    sqlx::query(
        "ALTER TABLE public.archival_batches DROP CONSTRAINT archival_batches_agent_id_fkey",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "ALTER TABLE public.archival_batches ADD CONSTRAINT archival_batches_agent_id_fkey \
         FOREIGN KEY (agent_id) REFERENCES public.agents (agent_id) NOT VALID",
    )
    .execute(&pool)
    .await
    .unwrap();

    let report = audit(&pool).await;
    assert_eq!(
        status_of(&report, "archival_batches"),
        Some(ContractStatus::Drifted)
    );
    assert!(drift_mentions(&report, "archival_batches", "NOT VALID"));
}

#[sqlx::test(migrations = "./migrations")]
async fn a_partial_unique_index_does_not_satisfy_the_contract(pool: PgPool) {
    // Uniqueness only inside the predicate. Rows outside it are unconstrained,
    // so a join assuming at most one match is not backed by this.
    sqlx::query("DROP INDEX public.idx_sessions_agent_session")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "CREATE UNIQUE INDEX idx_sessions_agent_session ON public.sessions (agent_id, session_id) \
         WHERE agent_id <> 'excluded'",
    )
    .execute(&pool)
    .await
    .unwrap();

    let report = audit(&pool).await;
    assert_eq!(
        status_of(&report, "sessions"),
        Some(ContractStatus::Drifted)
    );
    assert!(drift_mentions(&report, "sessions", "non-partial"));
}

#[sqlx::test(migrations = "./migrations")]
async fn an_expression_index_does_not_satisfy_the_contract(pool: PgPool) {
    // `lower(agent_id)` being unique says nothing about `agent_id` being
    // unique. The catalog reports the key column as an expression, which
    // matches no declared column name.
    sqlx::query("DROP INDEX public.idx_sessions_agent_session")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "CREATE UNIQUE INDEX idx_sessions_agent_session \
         ON public.sessions (lower(agent_id), session_id)",
    )
    .execute(&pool)
    .await
    .unwrap();

    let report = audit(&pool).await;
    assert_eq!(
        status_of(&report, "sessions"),
        Some(ContractStatus::Drifted)
    );
    assert!(drift_mentions(&report, "sessions", "expression index"));
}

#[sqlx::test(migrations = "./migrations")]
async fn a_non_unique_replacement_index_does_not_satisfy_the_contract(pool: PgPool) {
    // Same name, same columns, same order — and no uniqueness guarantee.
    sqlx::query("DROP INDEX public.idx_sessions_agent_session")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "CREATE INDEX idx_sessions_agent_session ON public.sessions (agent_id, session_id)",
    )
    .execute(&pool)
    .await
    .unwrap();

    let report = audit(&pool).await;
    assert_eq!(
        status_of(&report, "sessions"),
        Some(ContractStatus::Drifted)
    );
    assert!(drift_mentions(&report, "sessions", "required to be UNIQUE"));
}

#[sqlx::test(migrations = "./migrations")]
async fn an_invalid_index_does_not_satisfy_the_contract(pool: PgPool) {
    // Built the way an invalid index actually arises: a CONCURRENTLY build that
    // fails on existing duplicates leaves the index in place, INVALID. It looks
    // complete, enforces nothing, and the planner will not use it.
    sqlx::query("DROP INDEX public.idx_sessions_agent_session")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO agents (agent_id, tenant_id, external_agent_id) \
         VALUES ('dupe', $1::uuid, 'ext-dupe')",
    )
    .bind(TENANT)
    .execute(&pool)
    .await
    .unwrap();
    for _ in 0..2 {
        sqlx::query("INSERT INTO sessions (session_id, agent_id) VALUES ('same', 'dupe')")
            .execute(&pool)
            .await
            .unwrap();
    }
    sqlx::query(
        "CREATE UNIQUE INDEX CONCURRENTLY idx_sessions_agent_session \
         ON public.sessions (agent_id, session_id)",
    )
    .execute(&pool)
    .await
    .expect_err("the duplicate rows must make the concurrent build fail");

    let invalid: bool = sqlx::query(
        "SELECT NOT i.indisvalid FROM pg_index i JOIN pg_class c ON c.oid = i.indexrelid \
          WHERE c.relname = 'idx_sessions_agent_session'",
    )
    .fetch_one(&pool)
    .await
    .unwrap()
    .get(0);
    assert!(
        invalid,
        "precondition: the failed build left an INVALID index"
    );

    let report = audit(&pool).await;
    assert_eq!(
        status_of(&report, "sessions"),
        Some(ContractStatus::Drifted)
    );
    assert!(
        drift_mentions(&report, "sessions", "INVALID"),
        "{:?}",
        diagnostics_for(&report, "sessions")
    );
}

// ── The three-way gate ──────────────────────────────────────────────────────

#[sqlx::test(migrations = "./migrations")]
async fn contract_drift_suppresses_ordinary_ownership_conclusions(pool: PgPool) {
    // An unmapped agent would normally make `sessions` report UNMAPPED_AGENT.
    // With the uniqueness contract broken, that conclusion rests on a guarantee
    // that no longer holds, so it must be withheld — and the drift must block
    // the tranche, so withholding it cannot be read as the table being clean.
    sqlx::query("INSERT INTO agents (agent_id, external_agent_id) VALUES ('unmapped', 'ext-u')")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO sessions (session_id, agent_id) VALUES ('s-1', 'unmapped')")
        .execute(&pool)
        .await
        .unwrap();

    let before = audit(&pool).await;
    assert!(
        has(&before, "sessions", ReasonCode::UnmappedAgent),
        "precondition: the conclusion is reachable while the contract holds"
    );
    assert_eq!(
        status_of(&before, "sessions"),
        Some(ContractStatus::Satisfied)
    );

    sqlx::query("DROP INDEX public.idx_sessions_agent_session")
        .execute(&pool)
        .await
        .unwrap();

    let after = audit(&pool).await;
    assert_eq!(status_of(&after, "sessions"), Some(ContractStatus::Drifted));
    assert!(
        !has(&after, "sessions", ReasonCode::UnmappedAgent),
        "authoritative conclusions must be suppressed under contract drift: {:?}",
        codes_for(&after, "sessions")
    );
    assert!(has(&after, "sessions", ReasonCode::SchemaRelationshipDrift));
    assert!(
        !after
            .tranche_readiness
            .iter()
            .find(|t| t.tranche == Tranche::Sessions)
            .unwrap()
            .ready,
        "drift must block the tranche, so the missing findings cannot read as clean"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn fatal_identity_drift_runs_no_ambiguity_query(pool: PgPool) {
    // Under fatal drift the row identity is not usable, so the ambiguity query
    // — which groups by a path rooted on the same table — must not be
    // constructed either. NOT_EVALUATED is the whole point: "not looked at"
    // must never render as "fine".
    sqlx::query("ALTER TABLE public.memory_graph DROP CONSTRAINT memory_graph_pkey")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("ALTER TABLE public.memory_graph ADD PRIMARY KEY (agent_id, subject, predicate)")
        .execute(&pool)
        .await
        .unwrap();

    let report = audit(&pool).await;
    assert_eq!(
        status_of(&report, "memory_graph"),
        Some(ContractStatus::NotEvaluated)
    );
    assert!(
        !has(
            &report,
            "memory_graph",
            ReasonCode::AmbiguousLegacyIdentifier
        ),
        "no ambiguity query may run against an invalid identity: {:?}",
        codes_for(&report, "memory_graph")
    );
    assert!(report.is_blocked());
}

// ── Ambiguity detection ─────────────────────────────────────────────────────

/// Remove the global uniqueness of `agents.agent_id` so one legacy identifier
/// can resolve to more than one agent. `CASCADE` also drops the foreign keys
/// that depend on it, which is precisely the state this check exists for.
async fn unmake_agent_id_unique(pool: &PgPool) {
    sqlx::query("ALTER TABLE public.agents DROP CONSTRAINT agents_agent_id_key CASCADE")
        .execute(pool)
        .await
        .unwrap();
}

/// Two agents sharing one legacy identifier.
async fn seed_shared_identifier(pool: &PgPool, tenants: [&str; 2]) {
    unmake_agent_id_unique(pool).await;
    for (i, tenant) in tenants.iter().enumerate() {
        sqlx::query(
            "INSERT INTO agents (agent_id, tenant_id, external_agent_id) \
             VALUES ('shared', $1::uuid, $2)",
        )
        .bind(tenant)
        .bind(format!("ext-{i}"))
        .execute(pool)
        .await
        .unwrap();
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn one_reference_resolving_to_two_agents_in_one_tenant_is_ambiguous(pool: PgPool) {
    seed_shared_identifier(&pool, [TENANT, TENANT]).await;
    sqlx::query(
        "INSERT INTO memories (agent_id, content, memory_type) VALUES ('shared', 'x', 'fact')",
    )
    .execute(&pool)
    .await
    .unwrap();

    let report = audit(&pool).await;
    let finding = report
        .findings
        .iter()
        .find(|f| {
            f.table_name == "memories"
                && f.reason_code == ReasonCode::AmbiguousLegacyIdentifier
                && f.diagnostic.contains("more than one agent")
        })
        .unwrap_or_else(|| panic!("{:?}", diagnostics_for(&report, "memories")));

    // The grouped key is the caller's own string, so it is pseudonymised
    // exactly as a row identifier would be — same domain, same framing.
    let identifier = finding
        .row_identifier
        .as_ref()
        .expect("an example group must be emitted");
    assert_eq!(identifier.len(), 64, "{identifier}");
    assert_eq!(*identifier, audit::row_pseudonym("memories", "shared"));
    assert!(report.is_blocked());
}

#[sqlx::test(migrations = "./migrations")]
async fn one_reference_resolving_across_tenants_is_ambiguous(pool: PgPool) {
    seed_shared_identifier(&pool, [TENANT, OTHER_TENANT]).await;
    sqlx::query(
        "INSERT INTO memories (agent_id, content, memory_type) VALUES ('shared', 'x', 'fact')",
    )
    .execute(&pool)
    .await
    .unwrap();

    let report = audit(&pool).await;
    assert!(
        report.findings.iter().any(|f| {
            f.table_name == "memories"
                && f.reason_code == ReasonCode::AmbiguousLegacyIdentifier
                && f.diagnostic.contains("more than one tenant")
        }),
        "{:?}",
        diagnostics_for(&report, "memories")
    );
    assert!(report.is_blocked());
}

#[sqlx::test(migrations = "./migrations")]
async fn ambiguity_is_still_measured_under_uniqueness_drift(pool: PgPool) {
    // The one scan that survives contract drift, because its whole subject is
    // what the missing uniqueness actually costs.
    seed_shared_identifier(&pool, [TENANT, TENANT]).await;
    sqlx::query("INSERT INTO sessions (session_id, agent_id) VALUES ('s-1', 'shared')")
        .execute(&pool)
        .await
        .unwrap();

    let report = audit(&pool).await;
    // `sessions` lost the foreign key that depended on the dropped unique
    // constraint, so its contract has drifted…
    assert_eq!(
        status_of(&report, "sessions"),
        Some(ContractStatus::Drifted)
    );
    // …and the ambiguity diagnostic still ran.
    assert!(
        has(&report, "sessions", ReasonCode::AmbiguousLegacyIdentifier),
        "{:?}",
        codes_for(&report, "sessions")
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn a_unique_legacy_reference_is_never_called_ambiguous(pool: PgPool) {
    // The control: many rows sharing one reference that resolves to exactly one
    // agent is normal, not ambiguous.
    seed_clean(&pool, "agent-one", TENANT).await;
    for i in 0..3 {
        sqlx::query(
            "INSERT INTO memories (agent_id, content, memory_type) \
             VALUES ('agent-one', $1, 'fact')",
        )
        .bind(format!("m{i}"))
        .execute(&pool)
        .await
        .unwrap();
    }

    let report = audit(&pool).await;
    assert!(
        !has(&report, "memories", ReasonCode::AmbiguousLegacyIdentifier),
        "{:?}",
        codes_for(&report, "memories")
    );
    assert_eq!(
        status_of(&report, "memories"),
        Some(ContractStatus::Satisfied)
    );
}

// ── Global readiness blockers ───────────────────────────────────────────────

#[sqlx::test(migrations = "./migrations")]
async fn an_unclassified_table_blocks_every_tranche(pool: PgPool) {
    // It belongs to no tranche, so scoping the block per-table hid it entirely:
    // a brand-new table with no tenancy decision left every tranche READY.
    sqlx::query("CREATE TABLE public.brand_new_thing (id uuid PRIMARY KEY, agent_id text)")
        .execute(&pool)
        .await
        .unwrap();

    let report = audit(&pool).await;
    assert!(has(
        &report,
        "brand_new_thing",
        ReasonCode::UnclassifiedTable
    ));
    for tranche in &report.tranche_readiness {
        assert!(
            !tranche.ready,
            "{} must be blocked by an unclassified table",
            tranche.tranche.as_str()
        );
        assert!(tranche
            .blocking_reasons
            .iter()
            .any(|r| r.contains("brand_new_thing")));
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn a_missing_registered_table_blocks_every_tranche(pool: PgPool) {
    sqlx::query("DROP TABLE public.memory_conflicts CASCADE")
        .execute(&pool)
        .await
        .unwrap();

    let report = audit(&pool).await;
    assert!(has(
        &report,
        "memory_conflicts",
        ReasonCode::InventoryTableMissing
    ));
    assert_eq!(
        status_of(&report, "memory_conflicts"),
        Some(ContractStatus::NotEvaluated),
        "a table that is gone was never evaluated"
    );
    for tranche in &report.tranche_readiness {
        assert!(!tranche.ready, "{}", tranche.tranche.as_str());
    }
}

// ── Future uniqueness ───────────────────────────────────────────────────────

#[sqlx::test(migrations = "./migrations")]
async fn unresolved_tenants_do_not_manufacture_a_uniqueness_collision(pool: PgPool) {
    // `GROUP BY` folds all NULLs into one group, so including unresolved rows
    // collided every unowned row with every other and reported a violation no
    // future index would ever see. Their real blocker is UNMAPPED_AGENT, which
    // is reported against the same rows.
    sqlx::query("INSERT INTO agents (agent_id, external_agent_id) VALUES ('no-tenant', 'ext-nt')")
        .execute(&pool)
        .await
        .unwrap();
    for i in 0..3 {
        sqlx::query("INSERT INTO sessions (session_id, agent_id) VALUES ($1, 'no-tenant')")
            .bind(format!("s-{i}"))
            .execute(&pool)
            .await
            .unwrap();
    }

    let report = audit(&pool).await;
    assert!(
        !has(
            &report,
            "sessions",
            ReasonCode::FutureTenantUniquenessCollision
        ),
        "unresolved tenants must be excluded from the grouping: {:?}",
        diagnostics_for(&report, "sessions")
    );
    // The rows are still blocked — by the code that actually describes them.
    assert!(has(&report, "sessions", ReasonCode::UnmappedAgent));
}

// ── Nullability and gate reachability ───────────────────────────────────────

/// Nullability is a property of `(table, column)` and never of a column name.
///
/// Proven in both directions against the live schema, because a name-keyed list
/// got this wrong in both at once: it marked keys unenforced that are fully
/// enforced, and left keys enforced that `MATCH SIMPLE` silently skips. Only the
/// database can settle it, so only the database is asked.
#[sqlx::test(migrations = "./migrations")]
async fn nullability_is_a_property_of_table_and_column_not_of_a_name(pool: PgPool) {
    async fn is_nullable(pool: &PgPool, table: &str, column: &str) -> Option<bool> {
        let raw: Option<String> = sqlx::query_scalar(
            "SELECT is_nullable FROM information_schema.columns \
             WHERE table_schema = 'public' AND table_name = $1 AND column_name = $2",
        )
        .bind(table)
        .bind(column)
        .fetch_optional(pool)
        .await
        .unwrap();
        raw.map(|v| v == "YES")
    }

    // Direction 1 — everything the plan records as NULL-able really is. A pair
    // that is actually NOT NULL would mark a key unenforced that the database
    // enforces in full, understating the guarantee.
    for (table, column) in plan::PRE_EXISTING_NULLABLE {
        let observed = is_nullable(&pool, table, column)
            .await
            .unwrap_or_else(|| panic!("`{table}.{column}` is recorded NULL-able but absent"));
        assert!(
            observed,
            "`{table}.{column}` is recorded as NULL-able but the schema says NOT NULL; the key \
             that names it is enforced for every row, and marking it unenforced understates the \
             constraint"
        );
    }

    // Direction 2 — nothing else in a planned key is NULL-able. A missing pair
    // is the dangerous direction: the key would be treated as evidence of
    // ownership for rows MATCH SIMPLE never checks.
    //
    // This half skips ownership columns Step 4B has not created yet and pairs
    // already covered by direction 1, so it is counted: a filter that quietly
    // excluded everything would leave the loop asserting nothing at all, which
    // is the same vacuous-gate failure this PR removes elsewhere.
    let mut checked = 0usize;
    for object in plan::PLANNED_OBJECTS {
        if object.unenforced_when_null().is_none() {
            continue;
        }
        let table = object.table();
        for column in object.local_columns() {
            // Ownership columns are Step 4B's to create; they do not exist yet.
            if matches!(*column, "agent_uuid" | "tenant_id") {
                continue;
            }
            if plan::PRE_EXISTING_NULLABLE.contains(&(table, column)) {
                continue;
            }
            let Some(observed) = is_nullable(&pool, table, column).await else {
                continue; // not created yet
            };
            assert!(
                !observed,
                "`{table}.{column}` is NULL-able in the live schema but the plan does not record \
                 it; MATCH SIMPLE leaves rows with a NULL there unchecked, so `{}` would be \
                 treated as evidence of ownership for rows nothing verified",
                object.name()
            );
            checked += 1;
        }
    }
    assert!(
        checked >= 6,
        "direction 2 checked only {checked} column(s); the planned keys name at least six \
         pre-existing non-ownership columns (working_memory.session_id, \
         memory_versions.memory_id, memory_entity_links.memory_id, \
         memory_entity_links.entity_id, co_access_edges.memory_a, co_access_edges.memory_b), so \
         a lower count means the filters excluded real work and the loop is asserting nothing"
    );

    // The trap itself, stated as a fact about the database rather than a
    // comment: one name, two answers, on each of the two names that collide.
    for (name, not_null_on, nullable_on) in [
        ("memory_id", "memory_versions", "retrieval_feedback"),
        ("memory_a", "co_access_edges", "memory_conflicts"),
    ] {
        assert_eq!(
            is_nullable(&pool, not_null_on, name).await,
            Some(false),
            "`{not_null_on}.{name}` must be NOT NULL"
        );
        assert_eq!(
            is_nullable(&pool, nullable_on, name).await,
            Some(true),
            "`{nullable_on}.{name}` must be NULL-able — same name, opposite answer, which is why \
             the list is keyed on the pair"
        );
    }
}

/// `co_access_edges` gates the codes its paths can produce, and only those.
///
/// It is the only table in the schema with no agent column, so
/// `ORPHANED_AGENT_REFERENCE` cannot be produced for it: the audit derives the
/// orphan code from the path kind, and both of its paths are `Memory`. But
/// `UNMAPPED_AGENT` and `LEGACY_UNMAPPED` come off the canonical chain
/// `memory_a -> memories.agent_id -> agents.tenant_id` and remain reachable, so
/// removing them with it would have opened a real gate.
#[sqlx::test(migrations = "./migrations")]
async fn co_access_edges_gates_only_the_codes_its_paths_can_produce(pool: PgPool) {
    sqlx::query("INSERT INTO agents (agent_id, external_agent_id) VALUES ('nomap', 'ext-nomap')")
        .execute(&pool)
        .await
        .unwrap();
    let ids: Vec<Uuid> = sqlx::query_scalar(
        "INSERT INTO memories (agent_id, content, memory_type) \
         VALUES ('nomap', 'left', 'fact'), ('nomap', 'right', 'fact') RETURNING id",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    // `co_access_edges` carries CHECK (memory_a < memory_b) — edges are stored
    // order-agnostically — so the pair is normalised by the database's own
    // comparison rather than by an assumption about UUID ordering in Rust.
    sqlx::query(
        "INSERT INTO co_access_edges (memory_a, memory_b) \
         VALUES (LEAST($1, $2), GREATEST($1, $2))",
    )
    .bind(ids[0])
    .bind(ids[1])
    .execute(&pool)
    .await
    .unwrap();

    let report = audit(&pool).await;
    let observed = codes_for(&report, "co_access_edges");

    // Reachable transitively — the gate can fail, so it is a gate.
    for reachable in [ReasonCode::UnmappedAgent, ReasonCode::LegacyUnmapped] {
        assert!(
            has(&report, "co_access_edges", reachable),
            "{reachable} must reach co_access_edges through its memory chain, so it stays in the \
             required-zero set: {observed:?}"
        );
    }

    // Structurally impossible — no agent column means no agent path.
    assert!(
        !has(
            &report,
            "co_access_edges",
            ReasonCode::OrphanedAgentReference
        ),
        "co_access_edges has no agent column, so the audit cannot produce \
         ORPHANED_AGENT_REFERENCE for it: {observed:?}"
    );

    // The registry must agree with what the audit can actually emit.
    let entry = inventory::entry("co_access_edges").expect("registered");
    assert!(
        !entry
            .plan
            .required_zero_codes
            .contains(&ReasonCode::OrphanedAgentReference),
        "requiring zero of a code that cannot be produced is not a gate"
    );
    for reachable in [ReasonCode::UnmappedAgent, ReasonCode::LegacyUnmapped] {
        assert!(
            entry.plan.required_zero_codes.contains(&reachable),
            "{reachable} is reachable here and must remain gated"
        );
    }
}

/// `agent_tenancy_migrations` cannot carry checkpoint evidence.
///
/// The contract asserts this in prose; only a database can settle it. Measured
/// rather than remembered, because a hardcoded column list would keep agreeing
/// with itself forever after a migration added the very column it denies.
#[sqlx::test(migrations = "./migrations")]
async fn agent_tenancy_migrations_cannot_carry_checkpoint_evidence(pool: PgPool) {
    let columns: Vec<String> = sqlx::query_scalar(
        "SELECT column_name FROM information_schema.columns \
         WHERE table_schema = 'public' AND table_name = 'agent_tenancy_migrations'",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert!(
        !columns.is_empty(),
        "agent_tenancy_migrations must exist for this comparison to mean anything"
    );

    // The four things a FINALIZE guard would have to read, none of which this
    // table has. If a migration ever adds one, this fails and the contract's
    // stated reason has to be revisited rather than silently going stale.
    for missing in ["tranche", "digest", "cursor", "status"] {
        assert!(
            !columns.iter().any(|c| c.contains(missing)),
            "agent_tenancy_migrations now has a `{missing}`-like column ({columns:?}); revisit \
             whether it can carry checkpoint evidence, because plan.rs says it cannot"
        );
    }

    // The replacement names all four roles the existing table lacks.
    let planned: Vec<&str> = plan::TENANCY_BACKFILL_CHECKPOINTS
        .columns
        .iter()
        .map(|c| c.name)
        .collect();
    for required in ["tranche", "contract_digest", "status", "resume_cursor"] {
        assert!(
            planned.contains(&required),
            "the planned checkpoint table must declare `{required}`: {planned:?}"
        );
    }
    // And it does not exist yet — this PR plans it, it does not create it.
    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM information_schema.tables \
         WHERE table_schema = 'public' AND table_name = 'tenancy_backfill_checkpoints')",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(
        !exists,
        "Step 4B-0 is a contract: it declares tenancy_backfill_checkpoints, and Step 4B-1 creates \
         it. A table appearing here means DDL leaked into the contract step."
    );
}
