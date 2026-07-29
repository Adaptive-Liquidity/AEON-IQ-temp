# AEON tenancy inventory — Step 4A

Generated from `src/tenancy/inventory.rs`. Do not edit by hand — the registry is the source of truth and a test regenerates this file and compares it.

- schema version: `step4b0.1`
- inventory digest: `sha256:56512cd95f687470a5c6a0504768e2a51798cd4c051e6bc3a1e55ad02630f681`
- tables classified: 22

## Classification summary

| Class | Tables |
|---|---|
| `TENANT_ROOT` | `agents` |
| `DIRECT_AGENT_CHILD` | `amp_controller_state`, `archival_batches`, `audit_logs`, `cognitive_hypervisor_timeline`, `credential_agent_grants`, `entities`, `extraction_jobs`, `memories`, `memory_conflicts`, `memory_graph`, `memory_retrieval_logs`, `retrieval_feedback`, `rmk_episodes`, `rmk_policies`, `sessions` |
| `SESSION_CHILD` | `working_memory` |
| `MEMORY_LINEAGE_CHILD` | `co_access_edges`, `memory_entity_links`, `memory_versions` |
| `TENANT_GLOBAL` | `credentials` |
| `SYSTEM_GLOBAL` | `agent_tenancy_migrations` |

## REQUIRED_CURRENT_SCHEMA_CONTRACT

**RUNTIME CONTRACT VERIFICATION: IMPLEMENTED**

Each audit execution verifies these declared requirements against its live PostgreSQL catalog.

Each table declares the schema objects its ownership joins and row identity rely on **today** — not the constraints Step 4B will add, which live in the migration plan. Constraints are matched on `contype`, ordered `conkey` attribute names, referenced schema and table, ordered `confkey` attribute names and `convalidated`; indexes on ordered key columns, `indisunique`, `indisvalid`, `indisready`, absence of `indpred` and `indexprs`, and key-column count. A name alone never satisfies a requirement.

This file is a **static** rendering of the registry. It records what every deployment is required to have, and it is not evidence that any particular deployment has it — only a run against that database can say so, and only for that database. Each run reports `SATISFIED`, `DRIFTED` or `NOT_EVALUATED` per table in its machine report.

## STEP 4B MIGRATION CONTRACT

Typed, not prose. Every planned column, index, unique target, foreign key and constraint below is a value in `tenancy::plan::PLANNED_OBJECTS`; this section is generated from it. Invariants check the plan as data — that every planned foreign key has a matching unique target in the same column order, that no key depends on a target created in a later tranche, and that every object has exactly one owning tranche.

**Nothing here has been created.** Step 4B-0 is the contract; the DDL belongs to Step 4B-1 onward.

### Three-stage tranche execution

1. **PREPARE**
1. **BACKFILL**
1. **FINALIZE**

FINALIZE migrations open with a guard that raises unless tenancy_backfill_checkpoints records a COMPLETED checkpoint for *this* tranche against the current contract digest with blocking_count = 0. `sqlx migrate run` therefore cannot advance PREPARE straight into FINALIZE: on a fresh database the guard fires because no backfill ran, and on a live database it fires until the operator's backfill command records completion. agent_tenancy_migrations cannot serve as that evidence — it has no tranche, no digest, no cursor and no status column — so Step 4B-1 must create the checkpoint table before the first backfill runs.

A migration containing CREATE INDEX CONCURRENTLY must carry `-- no-transaction` as its first line so sqlx runs it unwrapped. A concurrent build that fails leaves an INVALID index behind, which the Step 4A verifier already reports as drift, so a failed build cannot be mistaken for a healthy one.

### Lock profiles

| Operation | Locks taken |
|---|---|
| `ADD COLUMN NULL` | ACCESS EXCLUSIVE, catalog-only — no rewrite, no scan |
| `CREATE INDEX CONCURRENTLY` | SHARE UPDATE EXCLUSIVE; must run outside a transaction block |
| `ADD CONSTRAINT ... FOREIGN KEY ... NOT VALID` | SHARE ROW EXCLUSIVE on the child AND on the referenced table |
| `ADD CONSTRAINT ... CHECK ... NOT VALID` | ACCESS EXCLUSIVE on the table alone, brief — no row scan, and no parent is locked alongside it |
| `VALIDATE CONSTRAINT` | SHARE UPDATE EXCLUSIVE on the child, ROW SHARE on the referenced table |
| `SET NOT NULL` | ACCESS EXCLUSIVE; full scan unless a validated CHECK permits skip |
| `table rewrite` | ACCESS EXCLUSIVE for the whole rewrite |
| `DROP COLUMN` | ACCESS EXCLUSIVE, brief — catalog update only |

### Transitional write strategy

A database bridge trigger is chosen over exhaustive application dual-write. Dual-write requires enumerating every writer and keeping that enumeration exhaustive; it is defeated by a rollback to the previous release, by a second service, by a maintenance script, and by any psql session. Each of those silently reintroduces NULL ownership on new rows, and the failure surfaces later as a SET NOT NULL that cannot succeed. The trigger is attached to the table, so it holds for every writer including ones nobody enumerated, and it survives an application rollback because it is schema rather than code. It is installed in PREPARE, before any backfill, so no window exists in which new rows are unowned.

After historical backfill completes, a resumed legacy writer cannot create a row with NULL or contradictory ownership. The BEFORE trigger fires on INSERT and on UPDATE: when agent_uuid or tenant_id arrives NULL it resolves both from agents using the row's legacy agent_id; when they arrive non-NULL and disagree with what agents says for that agent_id, it raises. A legacy writer that supplies only agent_id therefore produces a fully owned row, and a writer that supplies a wrong tenant is rejected rather than silently believed. The one remaining case that yields NULL is an agent whose own tenant_id is NULL, which is exactly UNMAPPED_AGENT and is already a blocking finding.

### Planned objects by tranche

#### TRANCHE_1_ROOTS_AND_DIRECT_AGENT_CHILDREN

PREPARE builds 23 of the 24 objects declared here.

- agents UNIQUE (tenant_id, id) AS `agents_tenant_id_id_key` — already current *(nothing is built; retained only as a dependency prerequisite)*
- ALTER TABLE archival_batches ADD COLUMN agent_uuid UUID NULL — NULL-able now; NOT NULL in FUTURE STEP 7
- ALTER TABLE archival_batches ADD COLUMN tenant_id UUID NULL — NULL-able now; NOT NULL in FUTURE STEP 7
- CREATE INDEX CONCURRENTLY idx_archival_batches_tenant ON archival_batches (tenant_id, agent_uuid)
- archival_batches UNIQUE (id, tenant_id) AS `archival_batches_id_tenant_id_key` — created by Step 4B as an FK target
- ALTER TABLE archival_batches ADD CONSTRAINT archival_batches_tenant_agent_fkey FOREIGN KEY (tenant_id, agent_uuid) REFERENCES agents (tenant_id, id) NOT VALID
- ALTER TABLE audit_logs ADD COLUMN agent_uuid UUID NULL — NULL-able, and stays NULL-able
- ALTER TABLE audit_logs ADD COLUMN tenant_id UUID NULL — NULL-able, and stays NULL-able
- CREATE INDEX CONCURRENTLY idx_audit_logs_tenant ON audit_logs (tenant_id, agent_uuid)
- ALTER TABLE audit_logs ADD CONSTRAINT audit_logs_tenant_agent_fkey FOREIGN KEY (tenant_id, agent_uuid) REFERENCES agents (tenant_id, id) NOT VALID — MATCH SIMPLE: rows with a NULL key component are not checked
- ALTER TABLE entities ADD COLUMN agent_uuid UUID NULL — NULL-able now; NOT NULL in FUTURE STEP 7
- ALTER TABLE entities ADD COLUMN tenant_id UUID NULL — NULL-able now; NOT NULL in FUTURE STEP 7
- CREATE INDEX CONCURRENTLY idx_entities_tenant ON entities (tenant_id, agent_uuid)
- entities UNIQUE (id, tenant_id) AS `entities_id_tenant_id_key` — created by Step 4B as an FK target
- ALTER TABLE entities ADD CONSTRAINT entities_tenant_agent_fkey FOREIGN KEY (tenant_id, agent_uuid) REFERENCES agents (tenant_id, id) NOT VALID
- ALTER TABLE memory_graph ADD COLUMN agent_uuid UUID NULL — NULL-able now; NOT NULL in FUTURE STEP 7
- ALTER TABLE memory_graph ADD COLUMN tenant_id UUID NULL — NULL-able now; NOT NULL in FUTURE STEP 7
- CREATE INDEX CONCURRENTLY idx_memory_graph_tenant ON memory_graph (tenant_id, agent_uuid)
- ALTER TABLE memory_graph ADD CONSTRAINT memory_graph_tenant_agent_fkey FOREIGN KEY (tenant_id, agent_uuid) REFERENCES agents (tenant_id, id) NOT VALID
- ALTER TABLE rmk_policies ADD COLUMN agent_uuid UUID NULL — NULL-able now; NOT NULL in FUTURE STEP 7
- ALTER TABLE rmk_policies ADD COLUMN tenant_id UUID NULL — NULL-able now; NOT NULL in FUTURE STEP 7
- CREATE INDEX CONCURRENTLY idx_rmk_policies_tenant ON rmk_policies (tenant_id, agent_uuid)
- rmk_policies UNIQUE (id, tenant_id) AS `rmk_policies_id_tenant_id_key` — created by Step 4B as an FK target
- ALTER TABLE rmk_policies ADD CONSTRAINT rmk_policies_tenant_agent_fkey FOREIGN KEY (tenant_id, agent_uuid) REFERENCES agents (tenant_id, id) NOT VALID

#### TRANCHE_2_SESSIONS

PREPARE builds 10 of the 10 objects declared here.
- **PREPARE blocked**: `working_memory_session_fkey` — upsert_working_memory inserts into working_memory first and mirrors into sessions second, as two separate autocommit statements on the pool. Once working_memory_session_fkey exists the first statement references a sessions row the second has not written yet, so the first turn of every new session fails. NOT VALID does not defer this: it exempts historical rows, never new ones.

- ALTER TABLE sessions ADD COLUMN agent_uuid UUID NULL — NULL-able now; NOT NULL in FUTURE STEP 7
- ALTER TABLE sessions ADD COLUMN tenant_id UUID NULL — NULL-able now; NOT NULL in FUTURE STEP 7
- CREATE INDEX CONCURRENTLY idx_sessions_tenant ON sessions (tenant_id, agent_uuid)
- sessions UNIQUE (tenant_id, agent_uuid, session_id) AS `sessions_tenant_agent_session_key` — created by Step 4B as an FK target
- ALTER TABLE sessions ADD CONSTRAINT sessions_tenant_agent_fkey FOREIGN KEY (tenant_id, agent_uuid) REFERENCES agents (tenant_id, id) NOT VALID
- ALTER TABLE working_memory ADD COLUMN agent_uuid UUID NULL — NULL-able now; NOT NULL in FUTURE STEP 7
- ALTER TABLE working_memory ADD COLUMN tenant_id UUID NULL — NULL-able now; NOT NULL in FUTURE STEP 7
- CREATE INDEX CONCURRENTLY idx_working_memory_tenant ON working_memory (tenant_id, agent_uuid)
- ALTER TABLE working_memory ADD CONSTRAINT working_memory_tenant_agent_fkey FOREIGN KEY (tenant_id, agent_uuid) REFERENCES agents (tenant_id, id) NOT VALID
- ALTER TABLE working_memory ADD CONSTRAINT working_memory_session_fkey FOREIGN KEY (tenant_id, agent_uuid, session_id) REFERENCES sessions (tenant_id, agent_uuid, session_id) NOT VALID

#### TRANCHE_3_MEMORIES

PREPARE builds 18 of the 18 objects declared here.

- ALTER TABLE memories ADD COLUMN agent_uuid UUID NULL — NULL-able now; NOT NULL in FUTURE STEP 7
- ALTER TABLE memories ADD COLUMN tenant_id UUID NULL — NULL-able now; NOT NULL in FUTURE STEP 7
- CREATE INDEX CONCURRENTLY idx_memories_tenant ON memories (tenant_id, agent_uuid)
- memories UNIQUE (id, tenant_id) AS `memories_id_tenant_id_key` — created by Step 4B as an FK target
- ALTER TABLE memories ADD CONSTRAINT memories_tenant_agent_fkey FOREIGN KEY (tenant_id, agent_uuid) REFERENCES agents (tenant_id, id) NOT VALID
- ALTER TABLE memories ADD CONSTRAINT memories_archival_batch_tenant_fkey FOREIGN KEY (archival_batch_id, tenant_id) REFERENCES archival_batches (id, tenant_id) NOT VALID — MATCH SIMPLE: rows with a NULL key component are not checked
- ALTER TABLE extraction_jobs ADD COLUMN agent_uuid UUID NULL — NULL-able now; NOT NULL in FUTURE STEP 7
- ALTER TABLE extraction_jobs ADD COLUMN tenant_id UUID NULL — NULL-able now; NOT NULL in FUTURE STEP 7
- CREATE INDEX CONCURRENTLY idx_extraction_jobs_tenant ON extraction_jobs (tenant_id, agent_uuid)
- ALTER TABLE extraction_jobs ADD CONSTRAINT extraction_jobs_tenant_agent_fkey FOREIGN KEY (tenant_id, agent_uuid) REFERENCES agents (tenant_id, id) NOT VALID
- ALTER TABLE memory_retrieval_logs ADD COLUMN agent_uuid UUID NULL — NULL-able now; NOT NULL in FUTURE STEP 7
- ALTER TABLE memory_retrieval_logs ADD COLUMN tenant_id UUID NULL — NULL-able now; NOT NULL in FUTURE STEP 7
- CREATE INDEX CONCURRENTLY idx_memory_retrieval_logs_tenant ON memory_retrieval_logs (tenant_id, agent_uuid)
- ALTER TABLE memory_retrieval_logs ADD CONSTRAINT memory_retrieval_logs_tenant_agent_fkey FOREIGN KEY (tenant_id, agent_uuid) REFERENCES agents (tenant_id, id) NOT VALID
- ALTER TABLE cognitive_hypervisor_timeline ADD COLUMN agent_uuid UUID NULL — NULL-able now; NOT NULL in FUTURE STEP 7
- ALTER TABLE cognitive_hypervisor_timeline ADD COLUMN tenant_id UUID NULL — NULL-able now; NOT NULL in FUTURE STEP 7
- CREATE INDEX CONCURRENTLY idx_cht_tenant ON cognitive_hypervisor_timeline (tenant_id, agent_uuid)
- ALTER TABLE cognitive_hypervisor_timeline ADD CONSTRAINT cht_tenant_agent_fkey FOREIGN KEY (tenant_id, agent_uuid) REFERENCES agents (tenant_id, id) NOT VALID

#### TRANCHE_4_LINEAGE_AND_ARCHIVAL

PREPARE builds 22 of the 22 objects declared here.

- ALTER TABLE memory_versions ADD COLUMN tenant_id UUID NULL — NULL-able now; NOT NULL in FUTURE STEP 7
- CREATE INDEX CONCURRENTLY idx_memory_versions_tenant ON memory_versions (tenant_id)
- ALTER TABLE memory_versions ADD CONSTRAINT memory_versions_memory_tenant_fkey FOREIGN KEY (memory_id, tenant_id) REFERENCES memories (id, tenant_id) NOT VALID
- ALTER TABLE memory_entity_links ADD COLUMN tenant_id UUID NULL — NULL-able now; NOT NULL in FUTURE STEP 7
- CREATE INDEX CONCURRENTLY idx_memory_entity_links_tenant ON memory_entity_links (tenant_id)
- ALTER TABLE memory_entity_links ADD CONSTRAINT memory_entity_links_memory_tenant_fkey FOREIGN KEY (memory_id, tenant_id) REFERENCES memories (id, tenant_id) NOT VALID
- ALTER TABLE memory_entity_links ADD CONSTRAINT memory_entity_links_entity_tenant_fkey FOREIGN KEY (entity_id, tenant_id) REFERENCES entities (id, tenant_id) NOT VALID
- ALTER TABLE co_access_edges ADD COLUMN tenant_id UUID NULL — NULL-able now; NOT NULL in FUTURE STEP 7
- CREATE INDEX CONCURRENTLY idx_co_access_edges_tenant ON co_access_edges (tenant_id)
- ALTER TABLE co_access_edges ADD CONSTRAINT co_access_edges_memory_a_tenant_fkey FOREIGN KEY (memory_a, tenant_id) REFERENCES memories (id, tenant_id) NOT VALID
- ALTER TABLE co_access_edges ADD CONSTRAINT co_access_edges_memory_b_tenant_fkey FOREIGN KEY (memory_b, tenant_id) REFERENCES memories (id, tenant_id) NOT VALID
- ALTER TABLE memory_conflicts ADD COLUMN agent_uuid UUID NULL — NULL-able now; NOT NULL in FUTURE STEP 7
- ALTER TABLE memory_conflicts ADD COLUMN tenant_id UUID NULL — NULL-able now; NOT NULL in FUTURE STEP 7
- CREATE INDEX CONCURRENTLY idx_memory_conflicts_tenant ON memory_conflicts (tenant_id, agent_uuid)
- ALTER TABLE memory_conflicts ADD CONSTRAINT memory_conflicts_tenant_agent_fkey FOREIGN KEY (tenant_id, agent_uuid) REFERENCES agents (tenant_id, id) NOT VALID
- ALTER TABLE memory_conflicts ADD CONSTRAINT memory_conflicts_memory_a_tenant_fkey FOREIGN KEY (memory_a, tenant_id) REFERENCES memories (id, tenant_id) NOT VALID — MATCH SIMPLE: rows with a NULL key component are not checked
- ALTER TABLE memory_conflicts ADD CONSTRAINT memory_conflicts_memory_b_tenant_fkey FOREIGN KEY (memory_b, tenant_id) REFERENCES memories (id, tenant_id) NOT VALID — MATCH SIMPLE: rows with a NULL key component are not checked
- ALTER TABLE retrieval_feedback ADD COLUMN agent_uuid UUID NULL — NULL-able now; NOT NULL in FUTURE STEP 7
- ALTER TABLE retrieval_feedback ADD COLUMN tenant_id UUID NULL — NULL-able now; NOT NULL in FUTURE STEP 7
- CREATE INDEX CONCURRENTLY idx_retrieval_feedback_tenant ON retrieval_feedback (tenant_id, agent_uuid)
- ALTER TABLE retrieval_feedback ADD CONSTRAINT retrieval_feedback_tenant_agent_fkey FOREIGN KEY (tenant_id, agent_uuid) REFERENCES agents (tenant_id, id) NOT VALID
- ALTER TABLE retrieval_feedback ADD CONSTRAINT retrieval_feedback_memory_tenant_fkey FOREIGN KEY (memory_id, tenant_id) REFERENCES memories (id, tenant_id) NOT VALID — MATCH SIMPLE: rows with a NULL key component are not checked

#### TRANCHE_5_OPERATIONS

PREPARE builds 9 of the 9 objects declared here.

- ALTER TABLE amp_controller_state ADD COLUMN agent_uuid UUID NULL — NULL-able now; NOT NULL in FUTURE STEP 7
- ALTER TABLE amp_controller_state ADD COLUMN tenant_id UUID NULL — NULL-able now; NOT NULL in FUTURE STEP 7
- CREATE INDEX CONCURRENTLY idx_amp_controller_state_tenant ON amp_controller_state (tenant_id, agent_uuid)
- ALTER TABLE amp_controller_state ADD CONSTRAINT amp_controller_state_tenant_agent_fkey FOREIGN KEY (tenant_id, agent_uuid) REFERENCES agents (tenant_id, id) NOT VALID
- ALTER TABLE rmk_episodes ADD COLUMN agent_uuid UUID NULL — NULL-able now; NOT NULL in FUTURE STEP 7
- ALTER TABLE rmk_episodes ADD COLUMN tenant_id UUID NULL — NULL-able now; NOT NULL in FUTURE STEP 7
- CREATE INDEX CONCURRENTLY idx_rmk_episodes_tenant ON rmk_episodes (tenant_id, agent_uuid)
- ALTER TABLE rmk_episodes ADD CONSTRAINT rmk_episodes_tenant_agent_fkey FOREIGN KEY (tenant_id, agent_uuid) REFERENCES agents (tenant_id, id) NOT VALID
- ALTER TABLE rmk_episodes ADD CONSTRAINT rmk_episodes_policy_tenant_fkey FOREIGN KEY (policy_id, tenant_id) REFERENCES rmk_policies (id, tenant_id) NOT VALID — MATCH SIMPLE: rows with a NULL key component are not checked

#### FINAL_CONSTRAINT_TIGHTENING

PREPARE builds 1 of the 1 objects declared here.

- ALTER TABLE agents ADD CONSTRAINT agents_tenant_id_not_null_chk CHECK (tenant_id IS NOT NULL) NOT VALID

### Uniqueness transitions

- `ALREADY_IMPLIED` — `agents`
- `RELAXATION` — `sessions`
- `NARROWING` — none planned *(requires a collision probe before creation)*

- `sessions`: (agent_id, session_id) → (tenant_id, agent_uuid, session_id) — **RELAXATION**. agent_uuid is a one-for-one re-identification of the legacy agent_id, and tenant_id is added rather than substituted for anything. Two rows distinguished by (agent_id, session_id) remain distinguished by (tenant_id, agent_uuid, session_id), so nothing legal today can collide under the new tuple.
- `agents`: (tenant_id, external_agent_id) → (tenant_id, external_agent_id) — **ALREADY_IMPLIED**. UNIQUE (tenant_id, external_agent_id) already exists. Step 4B adds no uniqueness to agents, so the tuple is unchanged.

### Structurally unreachable required-zero codes

Recorded rather than silently dropped. The enum values stay — they are a stable contract — but these table/code pairs cannot currently fire, so leaving them in a required-zero set would make a gate look stricter than it is.

- `memories` / `ORPHANED_SESSION_REFERENCE` — memories' canonical path is AGENT_TEXT and its session is CONTEXT_ONLY, so no session path is registered and the code cannot fire for this table. Removed from its required-zero set: a gate that can never fail is not a gate.
- `co_access_edges` / `ORPHANED_MEMORY_REFERENCE` — every memory reference is backed by a declared foreign key, so an orphan cannot exist while the contract holds; producing one means dropping that key, which is SCHEMA_RELATIONSHIP_DRIFT and blocks by a different code.
- `memory_entity_links` / `ORPHANED_MEMORY_REFERENCE` — same structure as co_access_edges: FK-backed, so the orphan count is withheld under drift and the drift is what blocks.
- `memory_versions` / `ORPHANED_MEMORY_REFERENCE` — same structure as co_access_edges: FK-backed, so the orphan count is withheld under drift and the drift is what blocks.
- `co_access_edges` / `ORPHANED_AGENT_REFERENCE` — co_access_edges is the only table in the schema with no agent column. The audit derives an orphan code from the path kind, and both of this table's paths — memory_a and memory_b — are Memory paths, so a missing parent raises ORPHANED_MEMORY_REFERENCE and never ORPHANED_AGENT_REFERENCE. Requiring it to be zero was requiring zero of something that cannot be produced. LEGACY_UNMAPPED and UNMAPPED_AGENT are *not* removed with it: those come off the canonical chain memory_a -> memories.agent_id -> agents.tenant_id and remain reachable transitively, so they stay in the required-zero set.
- `agents` / `FUTURE_TENANT_UNIQUENESS_COLLISION` — a consequence of keeping session identity agent-scoped. No table in the plan narrows a uniqueness rule: `sessions` moves from UNIQUE (agent_id, session_id) to UNIQUE (tenant_id, agent_uuid, session_id), which is a superset and cannot collide, and `agents` adds nothing because UNIQUE (tenant_id, external_agent_id) already exists. With no narrowing planned anywhere, the pre-check has nothing to pre-check, so `future_unique_columns` is None on every table and the query no longer runs. The reason code stays in the catalogue because a future narrowing tuple would make it fire again.

## Session semantics

Concluded from each table's actual insert paths, update paths, read predicates and 
constraints — not from whether the column happens to be NULL-able.

| Table | Session role | Evidence |
|---|---|---|
| `cognitive_hypervisor_timeline` | `CONTEXT_ONLY` | Read by `WHERE agent_id = $1` and `WHERE id = $1`; the hash chain is per-agent. No query selects by session. |
| `extraction_jobs` | `CONTEXT_ONLY` | The worker claims jobs with an unfiltered queue scan over `extraction_jobs`, never by session; session_id is payload context carried through to the extracted memories. |
| `memories` | `CONTEXT_ONLY` | Every read predicate is `WHERE agent_id = ...` or `WHERE id = ...`; no query selects memories by session. Deletion is by agent or by id. The session records where a memory came from, not who owns it. |
| `memory_retrieval_logs` | `CONTEXT_ONLY` | Two insert paths exist and one omits session_id entirely (`INSERT INTO memory_retrieval_logs (agent_id, query_hash, injected_memory_ids)`), so a log line is well-formed without a session. No read predicate uses it. |
| `rmk_episodes` | `CONTEXT_ONLY` | Read by `WHERE agent_id = $1 ORDER BY session_id`: the session is a grouping key *within* an agent's episodes, not an addressing key. The owner is the agent. |
| `working_memory` | `CANONICAL` | INSERT carries session_id; rows are deleted by `WHERE agent_id = $1 AND session_id = $2`, i.e. addressed *by* their session; UNIQUE (agent_id, session_id) is exactly the session's own unique key. The session is what identifies the row, so a missing one is a broken row rather than absent context. |

## Migration tranches

### TRANCHE_1_ROOTS_AND_DIRECT_AGENT_CHILDREN

- `agents`
- `archival_batches`
- `audit_logs`
- `credential_agent_grants`
- `credentials`
- `entities`
- `memory_graph`
- `rmk_policies`

### TRANCHE_2_SESSIONS

- `sessions`
- `working_memory`

### TRANCHE_3_MEMORIES

- `cognitive_hypervisor_timeline`
- `extraction_jobs`
- `memories`
- `memory_retrieval_logs`

### TRANCHE_4_LINEAGE_AND_ARCHIVAL

- `co_access_edges`
- `memory_conflicts`
- `memory_entity_links`
- `memory_versions`
- `retrieval_feedback`

### TRANCHE_5_OPERATIONS

- `amp_controller_state`
- `rmk_episodes`

### FINAL_CONSTRAINT_TIGHTENING

- `agent_tenancy_migrations`

## Table detail

### `agent_tenancy_migrations`

- **class**: `SYSTEM_GLOBAL`
- **tranche**: `FINAL_CONSTRAINT_TIGHTENING`
- **rationale**: System bookkeeping. Carries a tenant-shaped column, which is exactly why it needs stated evidence rather than an assumption.
- **row identity**: `id` (surrogate, emitted as-is)
- **REQUIRED_CURRENT_SCHEMA_CONTRACT** — *verified against the live catalog on every audit run; per-run status is in the machine report, not here.*
  - `agent_tenancy_migrations_pkey` PRIMARY KEY (id), validated
- **canonical path**: *(none — SYSTEM_GLOBAL)*
- **global-scope evidence**: Ledger of tenancy *migration runs*, written only by the step-1 backfill and read only by `assert_multi_tenant_preconditions`. It is on no request path. Its `legacy_tenant_id` is an input parameter of the run that produced the row, not ownership of the row: migration 0028 constrains it with `(mode = 'single_tenant') = (legacy_tenant_id IS NOT NULL)`, so in `mapped` mode it is NULL by construction. Giving these rows a tenant would misrepresent an operator action as tenant data.
- **planned objects**: *(none — this table owes no DDL)*
- **initial nullability**: n/a — no ownership columns are added
- **backfill shape**: `n/a — SYSTEM_GLOBAL tables are never backfilled with an inferred tenant`
- **must be zero**: `GLOBAL_SCOPE_UNVERIFIED`
- **lock profile**: none — not modified
- **rollback dependencies**: *(none)*
- **must stay inactive**: *(none)*

### `agents`

- **class**: `TENANT_ROOT`
- **tranche**: `TRANCHE_1_ROOTS_AND_DIRECT_AGENT_CHILDREN`
- **rationale**: The tenant root: it holds `tenant_id` itself and is what every other ownership path resolves to.
- **row identity**: `id` (surrogate, emitted as-is)
- **REQUIRED_CURRENT_SCHEMA_CONTRACT** — *verified against the live catalog on every audit run; per-run status is in the machine report, not here.*
  - `agents_pkey` PRIMARY KEY (id), validated
  - `agents_agent_id_key` UNIQUE (agent_id), validated
  - `agents_tenant_id_external_agent_id_key` UNIQUE (tenant_id, external_agent_id), validated
  - `agents_tenant_id_id_key` UNIQUE (tenant_id, id), validated
- **canonical path**: `row.tenant_id (authoritative)`
- **consistency**: `agents` is the authority: every other path in this registry terminates here. `tenant_id` is still NULL-able because step 1 deliberately leaves unmapped agents unreachable rather than inventing a tenant for them; every such row is reported as UNMAPPED_AGENT.
- **uniqueness transition**: `ALREADY_IMPLIED` — (tenant_id, external_agent_id) → (tenant_id, external_agent_id). UNIQUE (tenant_id, external_agent_id) already exists. Step 4B adds no uniqueness to agents, so the tuple is unchanged.
- **planned objects**:
  - [TRANCHE_1_ROOTS_AND_DIRECT_AGENT_CHILDREN] agents UNIQUE (tenant_id, id) AS `agents_tenant_id_id_key` — already current
  - [FINAL_CONSTRAINT_TIGHTENING] ALTER TABLE agents ADD CONSTRAINT agents_tenant_id_not_null_chk CHECK (tenant_id IS NOT NULL) NOT VALID
- **initial nullability**: already present from migration 0028
- **backfill shape**: `already backfilled by step 1 under an explicit LEGACY_MIGRATION_MODE`
- **must be zero**: `LEGACY_UNMAPPED`, `NULL_OWNERSHIP_LINK`, `UNMAPPED_AGENT`
- **validate step**: SET NOT NULL on agents.tenant_id once UNMAPPED_AGENT reaches zero
- **future uniqueness**: Nothing is added. `agents` already carries UNIQUE (tenant_id, external_agent_id) from migration 0028, which is the tenant-scoped identity, and UNIQUE (tenant_id, id), which is the composite FK target every child references. `agents_agent_id_key` is *not* replaced by UNIQUE (tenant_id, agent_id): that would introduce a second tenant-scoped name for the same agent, leaving two identity keys to keep in agreement. The legacy global key is removed together with the legacy `agent_id` column in FUTURE STEP 7, once Step 5 endpoint enforcement and Step 6 legacy-key retirement have merged.
- **lock profile**: SET NOT NULL takes ACCESS EXCLUSIVE and scans the table; on PG 16 a previously validated CHECK lets it skip the scan
- **rollback dependencies**: `rollback/0028_agent_tenancy_identity_down.sql`
- **must stay inactive**: `POST /agents`, `GET /agents`

### `amp_controller_state`

- **class**: `DIRECT_AGENT_CHILD`
- **tranche**: `TRANCHE_5_OPERATIONS`
- **rationale**: Reads as global controller state, but its primary key *is* `agent_id` and `rmk_worker` writes one row per agent. It is per-agent state — the name is the only thing about it that suggests otherwise, which is why the classification is taken from the key and the writers instead.
- **row identity**: `agent_id` (caller text, pseudonymised)
- **REQUIRED_CURRENT_SCHEMA_CONTRACT** — *verified against the live catalog on every audit run; per-run status is in the machine report, not here.*
  - `amp_controller_state_pkey` PRIMARY KEY (agent_id), validated
- **canonical path**: `row.agent_id -> agents.agent_id -> agents.id -> agents.tenant_id`
- **planned objects**:
  - [TRANCHE_5_OPERATIONS] ALTER TABLE amp_controller_state ADD COLUMN agent_uuid UUID NULL — NULL-able now; NOT NULL in FUTURE STEP 7
  - [TRANCHE_5_OPERATIONS] ALTER TABLE amp_controller_state ADD COLUMN tenant_id UUID NULL — NULL-able now; NOT NULL in FUTURE STEP 7
  - [TRANCHE_5_OPERATIONS] CREATE INDEX CONCURRENTLY idx_amp_controller_state_tenant ON amp_controller_state (tenant_id, agent_uuid)
  - [TRANCHE_5_OPERATIONS] ALTER TABLE amp_controller_state ADD CONSTRAINT amp_controller_state_tenant_agent_fkey FOREIGN KEY (tenant_id, agent_uuid) REFERENCES agents (tenant_id, id) NOT VALID
    - **match semantics**: `ALL_ROWS_CHECKED`
- **transitional writes**: `AGENT_BRIDGE_TRIGGER` — `trg_amp_controller_state_tenancy_bridge` (`fn_amp_controller_state_tenancy_bridge`) installed in PREPARE, before any backfill
- **initial nullability**: added NULL-able; NOT NULL only in FINAL_CONSTRAINT_TIGHTENING, after backfill verification reports zero blocking findings
- **backfill shape**: `UPDATE amp_controller_state s SET agent_uuid = a.id, tenant_id = a.tenant_id FROM agents a WHERE a.agent_id = s.agent_id`
- **must be zero**: `LEGACY_UNMAPPED`, `ORPHANED_AGENT_REFERENCE`, `UNMAPPED_AGENT`, `UNRESOLVABLE_OWNER`, `NULL_OWNERSHIP_LINK`
- **validate step**: VALIDATE CONSTRAINT after backfill
- **lock profile**: primary key is the legacy TEXT `agent_id`; re-keying to `agent_uuid` is a table rewrite and is deferred to FINAL_CONSTRAINT_TIGHTENING
- **rollback dependencies**: `tranche 1`
- **must stay inactive**: `rmk_worker`, `extraction_worker`, `hnsw_maintenance`

### `archival_batches`

- **class**: `DIRECT_AGENT_CHILD`
- **tranche**: `TRANCHE_1_ROOTS_AND_DIRECT_AGENT_CHILDREN`
- **rationale**: Has a real FK to `agents(agent_id)`, so its owner is already enforced. Must precede `memories`, which references it.
- **row identity**: `id` (surrogate, emitted as-is)
- **REQUIRED_CURRENT_SCHEMA_CONTRACT** — *verified against the live catalog on every audit run; per-run status is in the machine report, not here.*
  - `archival_batches_pkey` PRIMARY KEY (id), validated
  - `archival_batches_agent_id_fkey` FOREIGN KEY (agent_id) REFERENCES agents(agent_id), validated
- **canonical path**: `row.agent_id -> agents.agent_id -> agents.id -> agents.tenant_id`
- **planned objects**:
  - [TRANCHE_1_ROOTS_AND_DIRECT_AGENT_CHILDREN] ALTER TABLE archival_batches ADD COLUMN agent_uuid UUID NULL — NULL-able now; NOT NULL in FUTURE STEP 7
  - [TRANCHE_1_ROOTS_AND_DIRECT_AGENT_CHILDREN] ALTER TABLE archival_batches ADD COLUMN tenant_id UUID NULL — NULL-able now; NOT NULL in FUTURE STEP 7
  - [TRANCHE_1_ROOTS_AND_DIRECT_AGENT_CHILDREN] CREATE INDEX CONCURRENTLY idx_archival_batches_tenant ON archival_batches (tenant_id, agent_uuid)
  - [TRANCHE_1_ROOTS_AND_DIRECT_AGENT_CHILDREN] archival_batches UNIQUE (id, tenant_id) AS `archival_batches_id_tenant_id_key` — created by Step 4B as an FK target
  - [TRANCHE_1_ROOTS_AND_DIRECT_AGENT_CHILDREN] ALTER TABLE archival_batches ADD CONSTRAINT archival_batches_tenant_agent_fkey FOREIGN KEY (tenant_id, agent_uuid) REFERENCES agents (tenant_id, id) NOT VALID
    - **match semantics**: `ALL_ROWS_CHECKED`
- **transitional writes**: `AGENT_BRIDGE_TRIGGER` — `trg_archival_batches_tenancy_bridge` (`fn_archival_batches_tenancy_bridge`) installed in PREPARE, before any backfill
- **initial nullability**: added NULL-able; NOT NULL only in FINAL_CONSTRAINT_TIGHTENING, after backfill verification reports zero blocking findings
- **backfill shape**: `UPDATE archival_batches b SET agent_uuid = a.id, tenant_id = a.tenant_id FROM agents a WHERE a.agent_id = b.agent_id`
- **must be zero**: `LEGACY_UNMAPPED`, `ORPHANED_AGENT_REFERENCE`, `UNMAPPED_AGENT`, `UNRESOLVABLE_OWNER`, `NULL_OWNERSHIP_LINK`
- **validate step**: VALIDATE CONSTRAINT after backfill
- **lock profile**: ADD COLUMN NULL is metadata-only; ADD CONSTRAINT NOT VALID takes SHARE ROW EXCLUSIVE on this table and on agents, and VALIDATE takes the weaker SHARE UPDATE EXCLUSIVE here with ROW SHARE on agents
- **rollback dependencies**: `memories (memories.archival_batch_id references this table)`
- **must stay inactive**: `archival worker`, `POST /memories`, `GET /memories`

### `audit_logs`

- **class**: `DIRECT_AGENT_CHILD`
- **tranche**: `TRANCHE_1_ROOTS_AND_DIRECT_AGENT_CHILDREN`
- **rationale**: Agent-scoped when an agent is known. LEGACY_UNMAPPED exists as a row-level code precisely for tables like this one, which hold both assignable and unassignable rows.
- **row identity**: `id` (surrogate, emitted as-is)
- **REQUIRED_CURRENT_SCHEMA_CONTRACT** — *verified against the live catalog on every audit run; per-run status is in the machine report, not here.*
  - `audit_logs_pkey` PRIMARY KEY (id), validated
- **canonical path**: `row.agent_id -> agents.agent_id -> agents.id -> agents.tenant_id`
- **consistency**: `agent_id` is NULL-able here and nowhere else among the direct children. A NULL is not schema drift — some events genuinely have no agent — but it is unassignable, so those rows are reported LEGACY_UNMAPPED and the table cannot take a NOT NULL tenant until their disposition is decided.
- **planned objects**:
  - [TRANCHE_1_ROOTS_AND_DIRECT_AGENT_CHILDREN] ALTER TABLE audit_logs ADD COLUMN agent_uuid UUID NULL — NULL-able, and stays NULL-able
  - [TRANCHE_1_ROOTS_AND_DIRECT_AGENT_CHILDREN] ALTER TABLE audit_logs ADD COLUMN tenant_id UUID NULL — NULL-able, and stays NULL-able
  - [TRANCHE_1_ROOTS_AND_DIRECT_AGENT_CHILDREN] CREATE INDEX CONCURRENTLY idx_audit_logs_tenant ON audit_logs (tenant_id, agent_uuid)
  - [TRANCHE_1_ROOTS_AND_DIRECT_AGENT_CHILDREN] ALTER TABLE audit_logs ADD CONSTRAINT audit_logs_tenant_agent_fkey FOREIGN KEY (tenant_id, agent_uuid) REFERENCES agents (tenant_id, id) NOT VALID — MATCH SIMPLE: rows with a NULL key component are not checked
    - **match semantics**: `MATCH_SIMPLE_NULL_KEY_ROWS_UNCHECKED` — NULL-able local columns: `tenant_id`, `agent_uuid`
- **transitional writes**: `CONDITIONAL_AGENT_BRIDGE_TRIGGER` — `trg_audit_logs_tenancy_bridge` installed in PREPARE, before any backfill; resolves conditionally (AGENTLESS_ALLOWED, RESOLVED_AND_VERIFIED, PRESERVED_UNRESOLVED, CONTRADICTION_REJECTED) — audit_logs records agentless events — startup, configuration change, administrative action — which are valid rows with no owning agent, so its ownership columns stay NULL-able permanently and LEGACY_UNMAPPED is deliberately absent from its required-zero set. That is not a reason to install no bridge. Declining to resolve ownership at all also declines it for the rows that *do* name a resolvable agent, which then stay NULL for no reason and are indistinguishable from genuinely agentless ones. The bridge is therefore conditional rather than absent: it owns what it can resolve, leaves what it cannot so the audit reports it, and refuses a row that contradicts agents outright.
- **initial nullability**: added NULL-able and **stays** NULL-able until the disposition of agent-less audit rows is decided
- **backfill shape**: `UPDATE audit_logs l SET agent_uuid = a.id, tenant_id = a.tenant_id FROM agents a WHERE a.agent_id = l.agent_id`
- **must be zero**: `ORPHANED_AGENT_REFERENCE`, `UNMAPPED_AGENT`, `UNRESOLVABLE_OWNER`
- **validate step**: VALIDATE CONSTRAINT after backfill
- **lock profile**: append-mostly and potentially large; ADD COLUMN NULL is metadata-only
- **rollback dependencies**: `tranche 1`
- **must stay inactive**: `all write paths that emit audit events`

### `co_access_edges`

- **class**: `MEMORY_LINEAGE_CHILD`
- **tranche**: `TRANCHE_4_LINEAGE_AND_ARCHIVAL`
- **rationale**: No agent column exists. Both `memory_a` and `memory_b` are NOT NULL FKs to `memories`, so ownership is entirely lineage-derived.
- **row identity**: `memory_a` (surrogate, emitted as-is), `memory_b` (surrogate, emitted as-is)
- **REQUIRED_CURRENT_SCHEMA_CONTRACT** — *verified against the live catalog on every audit run; per-run status is in the machine report, not here.*
  - `co_access_edges_pkey` PRIMARY KEY (memory_a, memory_b), validated
  - `co_access_edges_memory_a_fkey` FOREIGN KEY (memory_a) REFERENCES memories(id), validated
  - `co_access_edges_memory_b_fkey` FOREIGN KEY (memory_b) REFERENCES memories(id), validated
- **canonical path**: `row.memory_id -> memories.id -> memories.agent_id -> agents.agent_id -> agents.tenant_id`
- **secondary paths**:
  - `row.memory_id -> memories.id -> memories.agent_id -> agents.agent_id -> agents.tenant_id`
- **consistency**: Both endpoints must resolve to the same tenant. This is the only table with no agent column at all, so the two memory references are the entire ownership story — and an edge spanning two tenants is precisely the cross-tenant link the audit exists to find.
- **planned objects**:
  - [TRANCHE_4_LINEAGE_AND_ARCHIVAL] ALTER TABLE co_access_edges ADD COLUMN tenant_id UUID NULL — NULL-able now; NOT NULL in FUTURE STEP 7
  - [TRANCHE_4_LINEAGE_AND_ARCHIVAL] CREATE INDEX CONCURRENTLY idx_co_access_edges_tenant ON co_access_edges (tenant_id)
  - [TRANCHE_4_LINEAGE_AND_ARCHIVAL] ALTER TABLE co_access_edges ADD CONSTRAINT co_access_edges_memory_a_tenant_fkey FOREIGN KEY (memory_a, tenant_id) REFERENCES memories (id, tenant_id) NOT VALID
    - **match semantics**: `ALL_ROWS_CHECKED`
  - [TRANCHE_4_LINEAGE_AND_ARCHIVAL] ALTER TABLE co_access_edges ADD CONSTRAINT co_access_edges_memory_b_tenant_fkey FOREIGN KEY (memory_b, tenant_id) REFERENCES memories (id, tenant_id) NOT VALID
    - **match semantics**: `ALL_ROWS_CHECKED`
- **transitional writes**: `MEMORY_BRIDGE_TRIGGER` — `trg_co_access_edges_tenancy_bridge` (`fn_co_access_edges_tenancy_bridge`) installed in PREPARE, before any backfill, resolving through the parent its backfill authority names
- **backfill source**: `memories ma ON ma.id = e.memory_a, memories mb ON mb.id = e.memory_b`
- **all paths must agree**: `ma.tenant_id IS NOT NULL AND ma.tenant_id = mb.tenant_id`
- **on disagreement**: left NULL and reported; the audit's blocking finding is the output, and no side is picked
- **initial nullability**: added NULL-able; NOT NULL only in FINAL_CONSTRAINT_TIGHTENING, after backfill verification reports zero blocking findings
- **backfill shape**: `UPDATE co_access_edges e SET tenant_id = m.tenant_id FROM memories m WHERE m.id = e.memory_a, only where memory_a and memory_b agree`
- **must be zero**: `LEGACY_UNMAPPED`, `UNMAPPED_AGENT`, `UNRESOLVABLE_OWNER`, `NULL_OWNERSHIP_LINK`, `CROSS_TENANT_PARENT_CHILD`, `OWNERSHIP_PATH_DISAGREEMENT`
- **validate step**: VALIDATE both constraints after backfill
- **lock profile**: two FK validations, each SHARE UPDATE EXCLUSIVE on co_access_edges with ROW SHARE on memories — concurrent DML continues throughout
- **rollback dependencies**: `memories (tranche 3)`
- **must stay inactive**: `co-access edge maintenance`, `retrieval scoring`

### `cognitive_hypervisor_timeline`

- **class**: `DIRECT_AGENT_CHILD`
- **tranche**: `TRANCHE_3_MEMORIES`
- **rationale**: The NOT NULL `agent_id` is the canonical path. `session_id` is CONTEXT_ONLY: it is not an ownership path and it is not a consistency path, because a row may legitimately outlive or precede its `sessions` row.
- **row identity**: `id` (surrogate, emitted as-is)
- **REQUIRED_CURRENT_SCHEMA_CONTRACT** — *verified against the live catalog on every audit run; per-run status is in the machine report, not here.*
  - `cognitive_hypervisor_timeline_pkey` PRIMARY KEY (id), validated
- **session role**: `CONTEXT_ONLY` — Read by `WHERE agent_id = $1` and `WHERE id = $1`; the hash chain is per-agent. No query selects by session.
- **canonical path**: `row.agent_id -> agents.agent_id -> agents.id -> agents.tenant_id`
- **consistency**: Measured, then deliberately excluded from ownership. `sessions` rows are created by exactly one code path in the codebase, the working-memory upsert, and it runs at the *end* of successful extraction. A pending job, a permanently failed job, or a memory written before that upsert therefore legitimately references a session that does not exist. Registering the session as an ownership or consistency path would emit blocking ORPHANED_SESSION_REFERENCE findings on entirely normal states, so it is recorded as context only rather than dropped silently: CONTEXT_ONLY says *investigated and non-authoritative*, where an absent conclusion would only say *not investigated*.
- **planned objects**:
  - [TRANCHE_3_MEMORIES] ALTER TABLE cognitive_hypervisor_timeline ADD COLUMN agent_uuid UUID NULL — NULL-able now; NOT NULL in FUTURE STEP 7
  - [TRANCHE_3_MEMORIES] ALTER TABLE cognitive_hypervisor_timeline ADD COLUMN tenant_id UUID NULL — NULL-able now; NOT NULL in FUTURE STEP 7
  - [TRANCHE_3_MEMORIES] CREATE INDEX CONCURRENTLY idx_cht_tenant ON cognitive_hypervisor_timeline (tenant_id, agent_uuid)
  - [TRANCHE_3_MEMORIES] ALTER TABLE cognitive_hypervisor_timeline ADD CONSTRAINT cht_tenant_agent_fkey FOREIGN KEY (tenant_id, agent_uuid) REFERENCES agents (tenant_id, id) NOT VALID
    - **match semantics**: `ALL_ROWS_CHECKED`
- **transitional writes**: `AGENT_BRIDGE_TRIGGER` — `trg_cht_tenancy_bridge` (`fn_cht_tenancy_bridge`) installed in PREPARE, before any backfill
- **initial nullability**: added NULL-able; NOT NULL only in FINAL_CONSTRAINT_TIGHTENING, after backfill verification reports zero blocking findings
- **backfill shape**: `UPDATE cognitive_hypervisor_timeline t SET agent_uuid = a.id, tenant_id = a.tenant_id FROM agents a WHERE a.agent_id = t.agent_id`
- **must be zero**: `LEGACY_UNMAPPED`, `ORPHANED_AGENT_REFERENCE`, `UNMAPPED_AGENT`, `UNRESOLVABLE_OWNER`, `NULL_OWNERSHIP_LINK`
- **validate step**: VALIDATE CONSTRAINT after backfill
- **lock profile**: append-only hash-chained timeline; ADD COLUMN NULL is metadata-only
- **rollback dependencies**: `sessions (tranche 2)`
- **must stay inactive**: `POST /hypervisor/events`, `GET /hypervisor/timeline`

### `credential_agent_grants`

- **class**: `DIRECT_AGENT_CHILD`
- **tranche**: `TRANCHE_1_ROOTS_AND_DIRECT_AGENT_CHILDREN`
- **rationale**: The only table already keyed on the internal UUID. Its tenant is materialised but derived — the composite FK to `agents(tenant_id, id)` is what makes it authoritative — so it is a direct agent child, not a root.
- **row identity**: `credential_id` (surrogate, emitted as-is), `agent_uuid` (surrogate, emitted as-is)
- **REQUIRED_CURRENT_SCHEMA_CONTRACT** — *verified against the live catalog on every audit run; per-run status is in the machine report, not here.*
  - `credential_agent_grants_pkey` PRIMARY KEY (credential_id, agent_uuid), validated
  - `credential_agent_grants_agent_fkey` FOREIGN KEY (tenant_id, agent_uuid) REFERENCES agents(tenant_id, id), validated
  - `credential_agent_grants_credential_fkey` FOREIGN KEY (credential_id, tenant_id) REFERENCES credentials(id, tenant_id), validated
- **canonical path**: `row.agent_uuid -> agents.id -> agents.tenant_id`
- **secondary paths**:
  - `row.tenant_id (materialised, FK-enforced)`
- **consistency**: The materialised `tenant_id` must equal the tenant reached through `agent_uuid`. Migration 0030 already enforces this with composite foreign keys to both parents, so a disagreement here would mean the constraint itself had drifted — which is why the check is kept rather than assumed.
- **planned objects**: *(none — this table owes no DDL)*
- **initial nullability**: already NOT NULL from migration 0030
- **backfill shape**: `n/a — already tenant-scoped`
- **must be zero**: `OWNERSHIP_PATH_DISAGREEMENT`, `SCHEMA_RELATIONSHIP_DRIFT`
- **lock profile**: none — not modified
- **rollback dependencies**: `rollback/0031_credential_agent_grants_hardening_down.sql`, `rollback/0030_credential_agent_grants_down.sql`
- **must stay inactive**: *(none)*

### `credentials`

- **class**: `TENANT_GLOBAL`
- **tranche**: `TRANCHE_1_ROOTS_AND_DIRECT_AGENT_CHILDREN`
- **rationale**: TENANT_GLOBAL rather than TENANT_ROOT: it carries an authoritative tenant but is not the terminus other tables resolve to, and it is deliberately not bound to any one agent. TENANT_ROOT is reserved for `agents` and any future pure `tenants` table.
- **row identity**: `id` (surrogate, emitted as-is)
- **REQUIRED_CURRENT_SCHEMA_CONTRACT** — *verified against the live catalog on every audit run; per-run status is in the machine report, not here.*
  - `credentials_pkey` PRIMARY KEY (id), validated
  - `credentials_id_tenant_id_key` UNIQUE (id, tenant_id), validated
- **canonical path**: `row.tenant_id (authoritative, NOT NULL)`
- **consistency**: `tenant_id` is NOT NULL and is the credential's own authority. No agent parent exists to cross-check it against: a `tenant_wide` credential reaches every agent in the tenant, and an agent-restricted one reaches only those named in `credential_agent_grants` — so the credential is owned by one tenant without being owned by one agent.
- **planned objects**: *(none — this table owes no DDL)*
- **initial nullability**: already NOT NULL from migration 0029
- **backfill shape**: `n/a — created tenant-scoped`
- **must be zero**: `SCHEMA_RELATIONSHIP_DRIFT`
- **lock profile**: none — not modified
- **rollback dependencies**: *(none)*
- **must stay inactive**: *(none)*

### `entities`

- **class**: `DIRECT_AGENT_CHILD`
- **tranche**: `TRANCHE_1_ROOTS_AND_DIRECT_AGENT_CHILDREN`
- **rationale**: Per-agent extraction output, keyed by the legacy identifier.
- **row identity**: `id` (surrogate, emitted as-is)
- **REQUIRED_CURRENT_SCHEMA_CONTRACT** — *verified against the live catalog on every audit run; per-run status is in the machine report, not here.*
  - `entities_pkey` PRIMARY KEY (id), validated
- **canonical path**: `row.agent_id -> agents.agent_id -> agents.id -> agents.tenant_id`
- **planned objects**:
  - [TRANCHE_1_ROOTS_AND_DIRECT_AGENT_CHILDREN] ALTER TABLE entities ADD COLUMN agent_uuid UUID NULL — NULL-able now; NOT NULL in FUTURE STEP 7
  - [TRANCHE_1_ROOTS_AND_DIRECT_AGENT_CHILDREN] ALTER TABLE entities ADD COLUMN tenant_id UUID NULL — NULL-able now; NOT NULL in FUTURE STEP 7
  - [TRANCHE_1_ROOTS_AND_DIRECT_AGENT_CHILDREN] CREATE INDEX CONCURRENTLY idx_entities_tenant ON entities (tenant_id, agent_uuid)
  - [TRANCHE_1_ROOTS_AND_DIRECT_AGENT_CHILDREN] entities UNIQUE (id, tenant_id) AS `entities_id_tenant_id_key` — created by Step 4B as an FK target
  - [TRANCHE_1_ROOTS_AND_DIRECT_AGENT_CHILDREN] ALTER TABLE entities ADD CONSTRAINT entities_tenant_agent_fkey FOREIGN KEY (tenant_id, agent_uuid) REFERENCES agents (tenant_id, id) NOT VALID
    - **match semantics**: `ALL_ROWS_CHECKED`
- **transitional writes**: `AGENT_BRIDGE_TRIGGER` — `trg_entities_tenancy_bridge` (`fn_entities_tenancy_bridge`) installed in PREPARE, before any backfill
- **initial nullability**: added NULL-able; NOT NULL only in FINAL_CONSTRAINT_TIGHTENING, after backfill verification reports zero blocking findings
- **backfill shape**: `UPDATE entities e SET agent_uuid = a.id, tenant_id = a.tenant_id FROM agents a WHERE a.agent_id = e.agent_id`
- **must be zero**: `LEGACY_UNMAPPED`, `ORPHANED_AGENT_REFERENCE`, `UNMAPPED_AGENT`, `UNRESOLVABLE_OWNER`, `NULL_OWNERSHIP_LINK`
- **validate step**: VALIDATE CONSTRAINT after backfill
- **future uniqueness**: `entities_agent_id_name_key` is UNIQUE (agent_id, name) and stays agent-scoped: an entity belongs to one agent, and widening it to the tenant would merge two agents' entity namespaces. Recorded so the decision is explicit rather than an omission.
- **lock profile**: ADD COLUMN NULL is metadata-only; ADD CONSTRAINT NOT VALID takes SHARE ROW EXCLUSIVE on this table and on agents, and VALIDATE takes the weaker SHARE UPDATE EXCLUSIVE here with ROW SHARE on agents
- **rollback dependencies**: `memory_entity_links (tranche 4)`
- **must stay inactive**: `extraction_worker`, `POST /entities`

### `extraction_jobs`

- **class**: `DIRECT_AGENT_CHILD`
- **tranche**: `TRANCHE_3_MEMORIES`
- **rationale**: A work queue whose rows are owned by the agent that enqueued them; the session is optional context.
- **row identity**: `id` (surrogate, emitted as-is)
- **REQUIRED_CURRENT_SCHEMA_CONTRACT** — *verified against the live catalog on every audit run; per-run status is in the machine report, not here.*
  - `extraction_jobs_pkey` PRIMARY KEY (id), validated
- **session role**: `CONTEXT_ONLY` — The worker claims jobs with an unfiltered queue scan over `extraction_jobs`, never by session; session_id is payload context carried through to the extracted memories.
- **canonical path**: `row.agent_id -> agents.agent_id -> agents.id -> agents.tenant_id`
- **consistency**: Measured, then deliberately excluded from ownership. `sessions` rows are created by exactly one code path in the codebase, the working-memory upsert, and it runs at the *end* of successful extraction. A pending job, a permanently failed job, or a memory written before that upsert therefore legitimately references a session that does not exist. Registering the session as an ownership or consistency path would emit blocking ORPHANED_SESSION_REFERENCE findings on entirely normal states, so it is recorded as context only rather than dropped silently: CONTEXT_ONLY says *investigated and non-authoritative*, where an absent conclusion would only say *not investigated*.
- **planned objects**:
  - [TRANCHE_3_MEMORIES] ALTER TABLE extraction_jobs ADD COLUMN agent_uuid UUID NULL — NULL-able now; NOT NULL in FUTURE STEP 7
  - [TRANCHE_3_MEMORIES] ALTER TABLE extraction_jobs ADD COLUMN tenant_id UUID NULL — NULL-able now; NOT NULL in FUTURE STEP 7
  - [TRANCHE_3_MEMORIES] CREATE INDEX CONCURRENTLY idx_extraction_jobs_tenant ON extraction_jobs (tenant_id, agent_uuid)
  - [TRANCHE_3_MEMORIES] ALTER TABLE extraction_jobs ADD CONSTRAINT extraction_jobs_tenant_agent_fkey FOREIGN KEY (tenant_id, agent_uuid) REFERENCES agents (tenant_id, id) NOT VALID
    - **match semantics**: `ALL_ROWS_CHECKED`
- **transitional writes**: `AGENT_BRIDGE_TRIGGER` — `trg_extraction_jobs_tenancy_bridge` (`fn_extraction_jobs_tenancy_bridge`) installed in PREPARE, before any backfill
- **initial nullability**: added NULL-able; NOT NULL only in FINAL_CONSTRAINT_TIGHTENING, after backfill verification reports zero blocking findings
- **backfill shape**: `UPDATE extraction_jobs j SET agent_uuid = a.id, tenant_id = a.tenant_id FROM agents a WHERE a.agent_id = j.agent_id`
- **must be zero**: `LEGACY_UNMAPPED`, `ORPHANED_AGENT_REFERENCE`, `UNMAPPED_AGENT`, `UNRESOLVABLE_OWNER`, `NULL_OWNERSHIP_LINK`
- **validate step**: VALIDATE CONSTRAINT after backfill
- **lock profile**: queue table with frequent updates; ADD COLUMN NULL is metadata-only
- **rollback dependencies**: `sessions (tranche 2)`
- **must stay inactive**: `extraction_worker`

### `memories`

- **class**: `DIRECT_AGENT_CHILD`
- **tranche**: `TRANCHE_3_MEMORIES`
- **rationale**: Root of the memory lineage, but its own owner is the agent: `session_id` is NULL-able and `archival_batch_id` is NULL-able, so neither can be canonical.
- **row identity**: `id` (surrogate, emitted as-is)
- **REQUIRED_CURRENT_SCHEMA_CONTRACT** — *verified against the live catalog on every audit run; per-run status is in the machine report, not here.*
  - `memories_pkey` PRIMARY KEY (id), validated
  - `memories_archival_batch_id_fkey` FOREIGN KEY (archival_batch_id) REFERENCES archival_batches(id), validated
- **session role**: `CONTEXT_ONLY` — Every read predicate is `WHERE agent_id = ...` or `WHERE id = ...`; no query selects memories by session. Deletion is by agent or by id. The session records where a memory came from, not who owns it.
- **canonical path**: `row.agent_id -> agents.agent_id -> agents.id -> agents.tenant_id`
- **secondary paths**:
  - `row.archival_batch_id -> archival_batches.id -> agent_id -> tenant`
- **consistency**: The archival batch, where present, must resolve to the same agent: an archived memory whose batch belongs to a different agent is a blocking disagreement, not a reason to prefer one path. The session is context only — `sessions` rows are created solely by the working-memory upsert at the end of successful extraction, so a memory written before it legitimately references a session that does not yet exist.
- **planned objects**:
  - [TRANCHE_3_MEMORIES] ALTER TABLE memories ADD COLUMN agent_uuid UUID NULL — NULL-able now; NOT NULL in FUTURE STEP 7
  - [TRANCHE_3_MEMORIES] ALTER TABLE memories ADD COLUMN tenant_id UUID NULL — NULL-able now; NOT NULL in FUTURE STEP 7
  - [TRANCHE_3_MEMORIES] CREATE INDEX CONCURRENTLY idx_memories_tenant ON memories (tenant_id, agent_uuid)
  - [TRANCHE_3_MEMORIES] memories UNIQUE (id, tenant_id) AS `memories_id_tenant_id_key` — created by Step 4B as an FK target
  - [TRANCHE_3_MEMORIES] ALTER TABLE memories ADD CONSTRAINT memories_tenant_agent_fkey FOREIGN KEY (tenant_id, agent_uuid) REFERENCES agents (tenant_id, id) NOT VALID
    - **match semantics**: `ALL_ROWS_CHECKED`
  - [TRANCHE_3_MEMORIES] ALTER TABLE memories ADD CONSTRAINT memories_archival_batch_tenant_fkey FOREIGN KEY (archival_batch_id, tenant_id) REFERENCES archival_batches (id, tenant_id) NOT VALID — MATCH SIMPLE: rows with a NULL key component are not checked
    - **match semantics**: `MATCH_SIMPLE_NULL_KEY_ROWS_UNCHECKED` — NULL-able local columns: `archival_batch_id`
- **transitional writes**: `MULTI_PATH_BRIDGE_TRIGGER` — `trg_memories_tenancy_bridge` installed in PREPARE, before any backfill; writes only when agents and archival_batches agree, and never picks a side
- **backfill source**: `agents a ON a.agent_id = m.agent_id`
- **all paths must agree**: `a.tenant_id IS NOT NULL AND (m.archival_batch_id IS NULL OR EXISTS (SELECT 1 FROM archival_batches b WHERE b.id = m.archival_batch_id AND b.tenant_id = a.tenant_id))`
- **on disagreement**: left NULL and reported; the audit's blocking finding is the output, and no side is picked
- **initial nullability**: added NULL-able; NOT NULL only in FINAL_CONSTRAINT_TIGHTENING, after backfill verification reports zero blocking findings
- **backfill shape**: `UPDATE memories m SET agent_uuid = a.id, tenant_id = a.tenant_id FROM agents a WHERE a.agent_id = m.agent_id`
- **must be zero**: `LEGACY_UNMAPPED`, `ORPHANED_AGENT_REFERENCE`, `UNMAPPED_AGENT`, `UNRESOLVABLE_OWNER`, `NULL_OWNERSHIP_LINK`, `OWNERSHIP_PATH_DISAGREEMENT`
- **validate step**: VALIDATE CONSTRAINT after backfill
- **future uniqueness**: Adds UNIQUE (id, tenant_id) — not a new restriction, since `id` is already the primary key, but the composite target every lineage child's FK needs in order to force one tenant across parent and child.
- **lock profile**: the largest table, and the one carrying `embedding vector(1536)`; ADD COLUMN NULL is metadata-only, but the backfill should be batched and the FK added NOT VALID
- **rollback dependencies**: `memory_versions`, `memory_entity_links`, `co_access_edges`, `memory_conflicts`, `retrieval_feedback`
- **must stay inactive**: `POST /memories`, `GET /memories`, `archival worker`, `rmk_worker`

### `memory_conflicts`

- **class**: `DIRECT_AGENT_CHILD`
- **tranche**: `TRANCHE_4_LINEAGE_AND_ARCHIVAL`
- **rationale**: Relates two memories, but its NOT NULL `agent_id` is the only reliable link; the memory references are nullable and therefore secondary.
- **row identity**: `id` (surrogate, emitted as-is)
- **REQUIRED_CURRENT_SCHEMA_CONTRACT** — *verified against the live catalog on every audit run; per-run status is in the machine report, not here.*
  - `memory_conflicts_pkey` PRIMARY KEY (id), validated
  - `memory_conflicts_memory_a_fkey` FOREIGN KEY (memory_a) REFERENCES memories(id), validated
  - `memory_conflicts_memory_b_fkey` FOREIGN KEY (memory_b) REFERENCES memories(id), validated
- **canonical path**: `row.agent_id -> agents.agent_id -> agents.id -> agents.tenant_id`
- **secondary paths**:
  - `row.memory_id -> memories.id -> memories.agent_id -> agents.agent_id -> agents.tenant_id`
  - `row.memory_id -> memories.id -> memories.agent_id -> agents.agent_id -> agents.tenant_id`
- **consistency**: Both memory references are NULL-able, so neither can be canonical. Where present, each must resolve to the same tenant as the row's agent.
- **planned objects**:
  - [TRANCHE_4_LINEAGE_AND_ARCHIVAL] ALTER TABLE memory_conflicts ADD COLUMN agent_uuid UUID NULL — NULL-able now; NOT NULL in FUTURE STEP 7
  - [TRANCHE_4_LINEAGE_AND_ARCHIVAL] ALTER TABLE memory_conflicts ADD COLUMN tenant_id UUID NULL — NULL-able now; NOT NULL in FUTURE STEP 7
  - [TRANCHE_4_LINEAGE_AND_ARCHIVAL] CREATE INDEX CONCURRENTLY idx_memory_conflicts_tenant ON memory_conflicts (tenant_id, agent_uuid)
  - [TRANCHE_4_LINEAGE_AND_ARCHIVAL] ALTER TABLE memory_conflicts ADD CONSTRAINT memory_conflicts_tenant_agent_fkey FOREIGN KEY (tenant_id, agent_uuid) REFERENCES agents (tenant_id, id) NOT VALID
    - **match semantics**: `ALL_ROWS_CHECKED`
  - [TRANCHE_4_LINEAGE_AND_ARCHIVAL] ALTER TABLE memory_conflicts ADD CONSTRAINT memory_conflicts_memory_a_tenant_fkey FOREIGN KEY (memory_a, tenant_id) REFERENCES memories (id, tenant_id) NOT VALID — MATCH SIMPLE: rows with a NULL key component are not checked
    - **match semantics**: `MATCH_SIMPLE_NULL_KEY_ROWS_UNCHECKED` — NULL-able local columns: `memory_a`
  - [TRANCHE_4_LINEAGE_AND_ARCHIVAL] ALTER TABLE memory_conflicts ADD CONSTRAINT memory_conflicts_memory_b_tenant_fkey FOREIGN KEY (memory_b, tenant_id) REFERENCES memories (id, tenant_id) NOT VALID — MATCH SIMPLE: rows with a NULL key component are not checked
    - **match semantics**: `MATCH_SIMPLE_NULL_KEY_ROWS_UNCHECKED` — NULL-able local columns: `memory_b`
- **transitional writes**: `MULTI_PATH_BRIDGE_TRIGGER` — `trg_memory_conflicts_tenancy_bridge` installed in PREPARE, before any backfill; writes only when agents and memories agree, and never picks a side
- **backfill source**: `agents a ON a.agent_id = c.agent_id`
- **all paths must agree**: `a.tenant_id IS NOT NULL AND (c.memory_a IS NULL OR EXISTS (SELECT 1 FROM memories m WHERE m.id = c.memory_a AND m.tenant_id = a.tenant_id)) AND (c.memory_b IS NULL OR EXISTS (SELECT 1 FROM memories m WHERE m.id = c.memory_b AND m.tenant_id = a.tenant_id))`
- **on disagreement**: left NULL and reported; the audit's blocking finding is the output, and no side is picked
- **initial nullability**: added NULL-able; NOT NULL only in FINAL_CONSTRAINT_TIGHTENING, after backfill verification reports zero blocking findings
- **backfill shape**: `UPDATE memory_conflicts c SET agent_uuid = a.id, tenant_id = a.tenant_id FROM agents a WHERE a.agent_id = c.agent_id`
- **must be zero**: `LEGACY_UNMAPPED`, `ORPHANED_AGENT_REFERENCE`, `UNMAPPED_AGENT`, `UNRESOLVABLE_OWNER`, `NULL_OWNERSHIP_LINK`
- **validate step**: VALIDATE all three after backfill
- **lock profile**: small; three FK validations
- **rollback dependencies**: `memories (tranche 3)`
- **must stay inactive**: `conflict detection`

### `memory_entity_links`

- **class**: `MEMORY_LINEAGE_CHILD`
- **tranche**: `TRANCHE_4_LINEAGE_AND_ARCHIVAL`
- **rationale**: Primary key is `(memory_id, entity_id)`; both are NOT NULL FKs. The memory is canonical because it is also what the tenant column will be keyed to.
- **row identity**: `memory_id` (surrogate, emitted as-is), `entity_id` (surrogate, emitted as-is)
- **REQUIRED_CURRENT_SCHEMA_CONTRACT** — *verified against the live catalog on every audit run; per-run status is in the machine report, not here.*
  - `memory_entity_links_pkey` PRIMARY KEY (memory_id, entity_id), validated
  - `memory_entity_links_memory_id_fkey` FOREIGN KEY (memory_id) REFERENCES memories(id), validated
  - `memory_entity_links_entity_id_fkey` FOREIGN KEY (entity_id) REFERENCES entities(id), validated
- **canonical path**: `row.memory_id -> memories.id -> memories.agent_id -> agents.agent_id -> agents.tenant_id`
- **secondary paths**:
  - `row.agent_id -> agents.agent_id -> agents.id -> agents.tenant_id`
  - `row.entity_id -> entities.id -> entities.agent_id -> tenant`
- **consistency**: Three independent routes to a tenant — the memory, the denormalised agent, and the entity — and all three must agree. This is the table where a silent fallback would be most tempting and most wrong: a link whose memory and entity belong to different tenants is a cross-tenant join, and picking either answer would launder it.
- **planned objects**:
  - [TRANCHE_4_LINEAGE_AND_ARCHIVAL] ALTER TABLE memory_entity_links ADD COLUMN tenant_id UUID NULL — NULL-able now; NOT NULL in FUTURE STEP 7
  - [TRANCHE_4_LINEAGE_AND_ARCHIVAL] CREATE INDEX CONCURRENTLY idx_memory_entity_links_tenant ON memory_entity_links (tenant_id)
  - [TRANCHE_4_LINEAGE_AND_ARCHIVAL] ALTER TABLE memory_entity_links ADD CONSTRAINT memory_entity_links_memory_tenant_fkey FOREIGN KEY (memory_id, tenant_id) REFERENCES memories (id, tenant_id) NOT VALID
    - **match semantics**: `ALL_ROWS_CHECKED`
  - [TRANCHE_4_LINEAGE_AND_ARCHIVAL] ALTER TABLE memory_entity_links ADD CONSTRAINT memory_entity_links_entity_tenant_fkey FOREIGN KEY (entity_id, tenant_id) REFERENCES entities (id, tenant_id) NOT VALID
    - **match semantics**: `ALL_ROWS_CHECKED`
- **transitional writes**: `MULTI_PATH_BRIDGE_TRIGGER` — `trg_memory_entity_links_tenancy_bridge` installed in PREPARE, before any backfill; writes only when memories and entities and agents agree, and never picks a side
- **backfill source**: `memories m ON m.id = l.memory_id, entities e ON e.id = l.entity_id, agents a ON a.agent_id = l.agent_id`
- **all paths must agree**: `m.tenant_id IS NOT NULL AND m.tenant_id = e.tenant_id AND m.tenant_id = a.tenant_id`
- **on disagreement**: left NULL and reported; the audit's blocking finding is the output, and no side is picked
- **initial nullability**: added NULL-able; NOT NULL only in FINAL_CONSTRAINT_TIGHTENING, after backfill verification reports zero blocking findings
- **backfill shape**: `UPDATE memory_entity_links l SET tenant_id = m.tenant_id FROM memories m WHERE m.id = l.memory_id`
- **must be zero**: `LEGACY_UNMAPPED`, `ORPHANED_AGENT_REFERENCE`, `UNMAPPED_AGENT`, `UNRESOLVABLE_OWNER`, `NULL_OWNERSHIP_LINK`, `CROSS_TENANT_PARENT_CHILD`, `OWNERSHIP_PATH_DISAGREEMENT`
- **validate step**: VALIDATE both after backfill
- **lock profile**: join table; two FK validations
- **rollback dependencies**: `memories (tranche 3)`, `entities (tranche 1)`
- **must stay inactive**: `extraction_worker`

### `memory_graph`

- **class**: `DIRECT_AGENT_CHILD`
- **tranche**: `TRANCHE_1_ROOTS_AND_DIRECT_AGENT_CHILDREN`
- **rationale**: Subject/predicate/object triples scoped to one agent. Despite the name it holds no FK to `memories`, so it is not lineage.
- **row identity**: `id` (surrogate, emitted as-is)
- **REQUIRED_CURRENT_SCHEMA_CONTRACT** — *verified against the live catalog on every audit run; per-run status is in the machine report, not here.*
  - `memory_graph_pkey` PRIMARY KEY (id), validated
- **canonical path**: `row.agent_id -> agents.agent_id -> agents.id -> agents.tenant_id`
- **planned objects**:
  - [TRANCHE_1_ROOTS_AND_DIRECT_AGENT_CHILDREN] ALTER TABLE memory_graph ADD COLUMN agent_uuid UUID NULL — NULL-able now; NOT NULL in FUTURE STEP 7
  - [TRANCHE_1_ROOTS_AND_DIRECT_AGENT_CHILDREN] ALTER TABLE memory_graph ADD COLUMN tenant_id UUID NULL — NULL-able now; NOT NULL in FUTURE STEP 7
  - [TRANCHE_1_ROOTS_AND_DIRECT_AGENT_CHILDREN] CREATE INDEX CONCURRENTLY idx_memory_graph_tenant ON memory_graph (tenant_id, agent_uuid)
  - [TRANCHE_1_ROOTS_AND_DIRECT_AGENT_CHILDREN] ALTER TABLE memory_graph ADD CONSTRAINT memory_graph_tenant_agent_fkey FOREIGN KEY (tenant_id, agent_uuid) REFERENCES agents (tenant_id, id) NOT VALID
    - **match semantics**: `ALL_ROWS_CHECKED`
- **transitional writes**: `AGENT_BRIDGE_TRIGGER` — `trg_memory_graph_tenancy_bridge` (`fn_memory_graph_tenancy_bridge`) installed in PREPARE, before any backfill
- **initial nullability**: added NULL-able; NOT NULL only in FINAL_CONSTRAINT_TIGHTENING, after backfill verification reports zero blocking findings
- **backfill shape**: `UPDATE memory_graph g SET agent_uuid = a.id, tenant_id = a.tenant_id FROM agents a WHERE a.agent_id = g.agent_id`
- **must be zero**: `LEGACY_UNMAPPED`, `ORPHANED_AGENT_REFERENCE`, `UNMAPPED_AGENT`, `UNRESOLVABLE_OWNER`, `NULL_OWNERSHIP_LINK`
- **validate step**: VALIDATE CONSTRAINT after backfill
- **lock profile**: ADD COLUMN NULL is metadata-only
- **rollback dependencies**: `tranche 1`
- **must stay inactive**: `extraction_worker`, `GET /graph`

### `memory_retrieval_logs`

- **class**: `DIRECT_AGENT_CHILD`
- **tranche**: `TRANCHE_3_MEMORIES`
- **rationale**: Per-agent retrieval telemetry. Its `candidate_memory_ids` arrays are not foreign keys and are deliberately not treated as an ownership path.
- **row identity**: `id` (surrogate, emitted as-is)
- **REQUIRED_CURRENT_SCHEMA_CONTRACT** — *verified against the live catalog on every audit run; per-run status is in the machine report, not here.*
  - `memory_retrieval_logs_pkey` PRIMARY KEY (id), validated
- **session role**: `CONTEXT_ONLY` — Two insert paths exist and one omits session_id entirely (`INSERT INTO memory_retrieval_logs (agent_id, query_hash, injected_memory_ids)`), so a log line is well-formed without a session. No read predicate uses it.
- **canonical path**: `row.agent_id -> agents.agent_id -> agents.id -> agents.tenant_id`
- **consistency**: Measured, then deliberately excluded from ownership. `sessions` rows are created by exactly one code path in the codebase, the working-memory upsert, and it runs at the *end* of successful extraction. A pending job, a permanently failed job, or a memory written before that upsert therefore legitimately references a session that does not exist. Registering the session as an ownership or consistency path would emit blocking ORPHANED_SESSION_REFERENCE findings on entirely normal states, so it is recorded as context only rather than dropped silently: CONTEXT_ONLY says *investigated and non-authoritative*, where an absent conclusion would only say *not investigated*.
- **planned objects**:
  - [TRANCHE_3_MEMORIES] ALTER TABLE memory_retrieval_logs ADD COLUMN agent_uuid UUID NULL — NULL-able now; NOT NULL in FUTURE STEP 7
  - [TRANCHE_3_MEMORIES] ALTER TABLE memory_retrieval_logs ADD COLUMN tenant_id UUID NULL — NULL-able now; NOT NULL in FUTURE STEP 7
  - [TRANCHE_3_MEMORIES] CREATE INDEX CONCURRENTLY idx_memory_retrieval_logs_tenant ON memory_retrieval_logs (tenant_id, agent_uuid)
  - [TRANCHE_3_MEMORIES] ALTER TABLE memory_retrieval_logs ADD CONSTRAINT memory_retrieval_logs_tenant_agent_fkey FOREIGN KEY (tenant_id, agent_uuid) REFERENCES agents (tenant_id, id) NOT VALID
    - **match semantics**: `ALL_ROWS_CHECKED`
- **transitional writes**: `AGENT_BRIDGE_TRIGGER` — `trg_memory_retrieval_logs_tenancy_bridge` (`fn_memory_retrieval_logs_tenancy_bridge`) installed in PREPARE, before any backfill
- **initial nullability**: added NULL-able; NOT NULL only in FINAL_CONSTRAINT_TIGHTENING, after backfill verification reports zero blocking findings
- **backfill shape**: `UPDATE memory_retrieval_logs l SET agent_uuid = a.id, tenant_id = a.tenant_id FROM agents a WHERE a.agent_id = l.agent_id`
- **must be zero**: `LEGACY_UNMAPPED`, `ORPHANED_AGENT_REFERENCE`, `UNMAPPED_AGENT`, `UNRESOLVABLE_OWNER`, `NULL_OWNERSHIP_LINK`
- **validate step**: VALIDATE CONSTRAINT after backfill
- **lock profile**: append-heavy; also carries `query_text`, which the report must never echo
- **rollback dependencies**: `sessions (tranche 2)`
- **must stay inactive**: `retrieval path`, `GET /memories/search`

### `memory_versions`

- **class**: `MEMORY_LINEAGE_CHILD`
- **tranche**: `TRANCHE_4_LINEAGE_AND_ARCHIVAL`
- **rationale**: `memory_id` is a NOT NULL FK to `memories`; the version's tenant is the memory's tenant by definition.
- **row identity**: `id` (surrogate, emitted as-is)
- **REQUIRED_CURRENT_SCHEMA_CONTRACT** — *verified against the live catalog on every audit run; per-run status is in the machine report, not here.*
  - `memory_versions_pkey` PRIMARY KEY (id), validated
  - `memory_versions_memory_id_fkey` FOREIGN KEY (memory_id) REFERENCES memories(id), validated
- **canonical path**: `row.memory_id -> memories.id -> memories.agent_id -> agents.agent_id -> agents.tenant_id`
- **secondary paths**:
  - `row.agent_id -> agents.agent_id -> agents.id -> agents.tenant_id`
- **consistency**: The denormalised `agent_id` must name the same agent the parent memory does. A version that claims a different agent than its memory is a blocking disagreement.
- **planned objects**:
  - [TRANCHE_4_LINEAGE_AND_ARCHIVAL] ALTER TABLE memory_versions ADD COLUMN tenant_id UUID NULL — NULL-able now; NOT NULL in FUTURE STEP 7
  - [TRANCHE_4_LINEAGE_AND_ARCHIVAL] CREATE INDEX CONCURRENTLY idx_memory_versions_tenant ON memory_versions (tenant_id)
  - [TRANCHE_4_LINEAGE_AND_ARCHIVAL] ALTER TABLE memory_versions ADD CONSTRAINT memory_versions_memory_tenant_fkey FOREIGN KEY (memory_id, tenant_id) REFERENCES memories (id, tenant_id) NOT VALID
    - **match semantics**: `ALL_ROWS_CHECKED`
- **transitional writes**: `MEMORY_BRIDGE_TRIGGER` — `trg_memory_versions_tenancy_bridge` (`fn_memory_versions_tenancy_bridge`) installed in PREPARE, before any backfill, resolving through the parent its backfill authority names
- **backfill source**: `memories m ON m.id = v.memory_id`
- **all paths must agree**: `m.tenant_id IS NOT NULL`
- **on disagreement**: left NULL and reported; the audit's blocking finding is the output, and no side is picked
- **initial nullability**: added NULL-able; NOT NULL only in FINAL_CONSTRAINT_TIGHTENING, after backfill verification reports zero blocking findings
- **backfill shape**: `UPDATE memory_versions v SET tenant_id = m.tenant_id FROM memories m WHERE m.id = v.memory_id`
- **must be zero**: `LEGACY_UNMAPPED`, `ORPHANED_AGENT_REFERENCE`, `UNMAPPED_AGENT`, `UNRESOLVABLE_OWNER`, `NULL_OWNERSHIP_LINK`, `CROSS_TENANT_PARENT_CHILD`, `OWNERSHIP_PATH_DISAGREEMENT`
- **validate step**: VALIDATE CONSTRAINT after backfill
- **future uniqueness**: `memory_versions_memory_id_version_number_key` stays scoped to the memory. Widening it to the tenant would be wrong: version numbers are per-memory.
- **lock profile**: potentially the second-largest table; batch the backfill
- **rollback dependencies**: `memories (tranche 3)`
- **must stay inactive**: `POST /memories`, `time-travel read paths`

### `retrieval_feedback`

- **class**: `DIRECT_AGENT_CHILD`
- **tranche**: `TRANCHE_4_LINEAGE_AND_ARCHIVAL`
- **rationale**: Owned by the agent that gave the feedback; the memory reference survives the memory's deletion as NULL, which is exactly why it is secondary.
- **row identity**: `id` (surrogate, emitted as-is)
- **REQUIRED_CURRENT_SCHEMA_CONTRACT** — *verified against the live catalog on every audit run; per-run status is in the machine report, not here.*
  - `retrieval_feedback_pkey` PRIMARY KEY (id), validated
  - `retrieval_feedback_memory_id_fkey` FOREIGN KEY (memory_id) REFERENCES memories(id), validated
- **canonical path**: `row.agent_id -> agents.agent_id -> agents.id -> agents.tenant_id`
- **secondary paths**:
  - `row.memory_id -> memories.id -> memories.agent_id -> agents.agent_id -> agents.tenant_id`
- **consistency**: `memory_id` is `ON DELETE SET NULL`, so it is absent for feedback whose memory has been deleted and cannot be canonical. Where present it must agree with the agent.
- **planned objects**:
  - [TRANCHE_4_LINEAGE_AND_ARCHIVAL] ALTER TABLE retrieval_feedback ADD COLUMN agent_uuid UUID NULL — NULL-able now; NOT NULL in FUTURE STEP 7
  - [TRANCHE_4_LINEAGE_AND_ARCHIVAL] ALTER TABLE retrieval_feedback ADD COLUMN tenant_id UUID NULL — NULL-able now; NOT NULL in FUTURE STEP 7
  - [TRANCHE_4_LINEAGE_AND_ARCHIVAL] CREATE INDEX CONCURRENTLY idx_retrieval_feedback_tenant ON retrieval_feedback (tenant_id, agent_uuid)
  - [TRANCHE_4_LINEAGE_AND_ARCHIVAL] ALTER TABLE retrieval_feedback ADD CONSTRAINT retrieval_feedback_tenant_agent_fkey FOREIGN KEY (tenant_id, agent_uuid) REFERENCES agents (tenant_id, id) NOT VALID
    - **match semantics**: `ALL_ROWS_CHECKED`
  - [TRANCHE_4_LINEAGE_AND_ARCHIVAL] ALTER TABLE retrieval_feedback ADD CONSTRAINT retrieval_feedback_memory_tenant_fkey FOREIGN KEY (memory_id, tenant_id) REFERENCES memories (id, tenant_id) NOT VALID — MATCH SIMPLE: rows with a NULL key component are not checked
    - **match semantics**: `MATCH_SIMPLE_NULL_KEY_ROWS_UNCHECKED` — NULL-able local columns: `memory_id`
- **transitional writes**: `MULTI_PATH_BRIDGE_TRIGGER` — `trg_retrieval_feedback_tenancy_bridge` installed in PREPARE, before any backfill; writes only when agents and memories agree, and never picks a side
- **backfill source**: `agents a ON a.agent_id = f.agent_id`
- **all paths must agree**: `a.tenant_id IS NOT NULL AND (f.memory_id IS NULL OR EXISTS (SELECT 1 FROM memories m WHERE m.id = f.memory_id AND m.tenant_id = a.tenant_id))`
- **on disagreement**: left NULL and reported; the audit's blocking finding is the output, and no side is picked
- **initial nullability**: added NULL-able; NOT NULL only in FINAL_CONSTRAINT_TIGHTENING, after backfill verification reports zero blocking findings
- **backfill shape**: `UPDATE retrieval_feedback f SET agent_uuid = a.id, tenant_id = a.tenant_id FROM agents a WHERE a.agent_id = f.agent_id`
- **must be zero**: `LEGACY_UNMAPPED`, `ORPHANED_AGENT_REFERENCE`, `UNMAPPED_AGENT`, `UNRESOLVABLE_OWNER`, `NULL_OWNERSHIP_LINK`
- **validate step**: VALIDATE both after backfill
- **lock profile**: small; the FK to memories must tolerate a NULL memory_id (MATCH SIMPLE)
- **rollback dependencies**: `memories (tranche 3)`
- **must stay inactive**: `retrieval feedback endpoint`, `rmk_worker`

### `rmk_episodes`

- **class**: `DIRECT_AGENT_CHILD`
- **tranche**: `TRANCHE_5_OPERATIONS`
- **rationale**: Reinforcement episodes recorded per agent; the policy is a secondary link that may be NULL after a policy is deleted.
- **row identity**: `id` (surrogate, emitted as-is)
- **REQUIRED_CURRENT_SCHEMA_CONTRACT** — *verified against the live catalog on every audit run; per-run status is in the machine report, not here.*
  - `rmk_episodes_pkey` PRIMARY KEY (id), validated
  - `rmk_episodes_policy_id_fkey` FOREIGN KEY (policy_id) REFERENCES rmk_policies(id), validated
- **session role**: `CONTEXT_ONLY` — Read by `WHERE agent_id = $1 ORDER BY session_id`: the session is a grouping key *within* an agent's episodes, not an addressing key. The owner is the agent.
- **canonical path**: `row.agent_id -> agents.agent_id -> agents.id -> agents.tenant_id`
- **secondary paths**:
  - `row.policy_id -> rmk_policies.id -> rmk_policies.agent_id -> tenant`
- **consistency**: An episode's policy, where set, must belong to the same agent. `policy_id` is `ON DELETE SET NULL`, so it cannot be canonical.
- **planned objects**:
  - [TRANCHE_5_OPERATIONS] ALTER TABLE rmk_episodes ADD COLUMN agent_uuid UUID NULL — NULL-able now; NOT NULL in FUTURE STEP 7
  - [TRANCHE_5_OPERATIONS] ALTER TABLE rmk_episodes ADD COLUMN tenant_id UUID NULL — NULL-able now; NOT NULL in FUTURE STEP 7
  - [TRANCHE_5_OPERATIONS] CREATE INDEX CONCURRENTLY idx_rmk_episodes_tenant ON rmk_episodes (tenant_id, agent_uuid)
  - [TRANCHE_5_OPERATIONS] ALTER TABLE rmk_episodes ADD CONSTRAINT rmk_episodes_tenant_agent_fkey FOREIGN KEY (tenant_id, agent_uuid) REFERENCES agents (tenant_id, id) NOT VALID
    - **match semantics**: `ALL_ROWS_CHECKED`
  - [TRANCHE_5_OPERATIONS] ALTER TABLE rmk_episodes ADD CONSTRAINT rmk_episodes_policy_tenant_fkey FOREIGN KEY (policy_id, tenant_id) REFERENCES rmk_policies (id, tenant_id) NOT VALID — MATCH SIMPLE: rows with a NULL key component are not checked
    - **match semantics**: `MATCH_SIMPLE_NULL_KEY_ROWS_UNCHECKED` — NULL-able local columns: `policy_id`
- **transitional writes**: `MULTI_PATH_BRIDGE_TRIGGER` — `trg_rmk_episodes_tenancy_bridge` installed in PREPARE, before any backfill; writes only when agents and rmk_policies agree, and never picks a side
- **backfill source**: `agents a ON a.agent_id = e.agent_id`
- **all paths must agree**: `a.tenant_id IS NOT NULL AND (e.policy_id IS NULL OR EXISTS (SELECT 1 FROM rmk_policies p WHERE p.id = e.policy_id AND p.tenant_id = a.tenant_id))`
- **on disagreement**: left NULL and reported; the audit's blocking finding is the output, and no side is picked
- **initial nullability**: added NULL-able; NOT NULL only in FINAL_CONSTRAINT_TIGHTENING, after backfill verification reports zero blocking findings
- **backfill shape**: `UPDATE rmk_episodes e SET agent_uuid = a.id, tenant_id = a.tenant_id FROM agents a WHERE a.agent_id = e.agent_id`
- **must be zero**: `LEGACY_UNMAPPED`, `ORPHANED_AGENT_REFERENCE`, `UNMAPPED_AGENT`, `UNRESOLVABLE_OWNER`, `NULL_OWNERSHIP_LINK`
- **validate step**: VALIDATE both after backfill
- **lock profile**: append-heavy training log
- **rollback dependencies**: `rmk_policies (tranche 1)`, `sessions (tranche 2)`
- **must stay inactive**: `rmk_worker`, `extraction_worker`, `hnsw_maintenance`

### `rmk_policies`

- **class**: `DIRECT_AGENT_CHILD`
- **tranche**: `TRANCHE_1_ROOTS_AND_DIRECT_AGENT_CHILDREN`
- **rationale**: One policy set per agent; must precede `rmk_episodes`, which references it.
- **row identity**: `id` (surrogate, emitted as-is)
- **REQUIRED_CURRENT_SCHEMA_CONTRACT** — *verified against the live catalog on every audit run; per-run status is in the machine report, not here.*
  - `rmk_policies_pkey` PRIMARY KEY (id), validated
- **canonical path**: `row.agent_id -> agents.agent_id -> agents.id -> agents.tenant_id`
- **planned objects**:
  - [TRANCHE_1_ROOTS_AND_DIRECT_AGENT_CHILDREN] ALTER TABLE rmk_policies ADD COLUMN agent_uuid UUID NULL — NULL-able now; NOT NULL in FUTURE STEP 7
  - [TRANCHE_1_ROOTS_AND_DIRECT_AGENT_CHILDREN] ALTER TABLE rmk_policies ADD COLUMN tenant_id UUID NULL — NULL-able now; NOT NULL in FUTURE STEP 7
  - [TRANCHE_1_ROOTS_AND_DIRECT_AGENT_CHILDREN] CREATE INDEX CONCURRENTLY idx_rmk_policies_tenant ON rmk_policies (tenant_id, agent_uuid)
  - [TRANCHE_1_ROOTS_AND_DIRECT_AGENT_CHILDREN] rmk_policies UNIQUE (id, tenant_id) AS `rmk_policies_id_tenant_id_key` — created by Step 4B as an FK target
  - [TRANCHE_1_ROOTS_AND_DIRECT_AGENT_CHILDREN] ALTER TABLE rmk_policies ADD CONSTRAINT rmk_policies_tenant_agent_fkey FOREIGN KEY (tenant_id, agent_uuid) REFERENCES agents (tenant_id, id) NOT VALID
    - **match semantics**: `ALL_ROWS_CHECKED`
- **transitional writes**: `AGENT_BRIDGE_TRIGGER` — `trg_rmk_policies_tenancy_bridge` (`fn_rmk_policies_tenancy_bridge`) installed in PREPARE, before any backfill
- **initial nullability**: added NULL-able; NOT NULL only in FINAL_CONSTRAINT_TIGHTENING, after backfill verification reports zero blocking findings
- **backfill shape**: `UPDATE rmk_policies p SET agent_uuid = a.id, tenant_id = a.tenant_id FROM agents a WHERE a.agent_id = p.agent_id`
- **must be zero**: `LEGACY_UNMAPPED`, `ORPHANED_AGENT_REFERENCE`, `UNMAPPED_AGENT`, `UNRESOLVABLE_OWNER`, `NULL_OWNERSHIP_LINK`
- **validate step**: VALIDATE CONSTRAINT after backfill
- **future uniqueness**: Adds UNIQUE (id, tenant_id) as the FK target for episodes.
- **lock profile**: small
- **rollback dependencies**: `rmk_episodes (tranche 5)`
- **must stay inactive**: `rmk_worker`, `extraction_worker`, `hnsw_maintenance`

### `sessions`

- **class**: `DIRECT_AGENT_CHILD`
- **tranche**: `TRANCHE_2_SESSIONS`
- **rationale**: Owned by its agent through a real FK to `agents(agent_id)`. Everything session-scoped resolves through `(agent_id, session_id)`, which is the only unique key on this table besides its surrogate primary key.
- **row identity**: `id` (surrogate, emitted as-is)
- **REQUIRED_CURRENT_SCHEMA_CONTRACT** — *verified against the live catalog on every audit run; per-run status is in the machine report, not here.*
  - `sessions_pkey` PRIMARY KEY (id), validated
  - `idx_sessions_agent_session` INDEX (agent_id, session_id) - unique, valid, ready, non-partial, no expressions
  - `sessions_agent_id_fkey` FOREIGN KEY (agent_id) REFERENCES agents(agent_id), validated
- **canonical path**: `row.agent_id -> agents.agent_id -> agents.id -> agents.tenant_id`
- **consistency**: A session's owner is its agent. This table is a direct agent child, not a session child — it is the parent every session child resolves through.
- **uniqueness transition**: `RELAXATION` — (agent_id, session_id) → (tenant_id, agent_uuid, session_id). agent_uuid is a one-for-one re-identification of the legacy agent_id, and tenant_id is added rather than substituted for anything. Two rows distinguished by (agent_id, session_id) remain distinguished by (tenant_id, agent_uuid, session_id), so nothing legal today can collide under the new tuple.
- **planned objects**:
  - [TRANCHE_2_SESSIONS] ALTER TABLE sessions ADD COLUMN agent_uuid UUID NULL — NULL-able now; NOT NULL in FUTURE STEP 7
  - [TRANCHE_2_SESSIONS] ALTER TABLE sessions ADD COLUMN tenant_id UUID NULL — NULL-able now; NOT NULL in FUTURE STEP 7
  - [TRANCHE_2_SESSIONS] CREATE INDEX CONCURRENTLY idx_sessions_tenant ON sessions (tenant_id, agent_uuid)
  - [TRANCHE_2_SESSIONS] sessions UNIQUE (tenant_id, agent_uuid, session_id) AS `sessions_tenant_agent_session_key` — created by Step 4B as an FK target
  - [TRANCHE_2_SESSIONS] ALTER TABLE sessions ADD CONSTRAINT sessions_tenant_agent_fkey FOREIGN KEY (tenant_id, agent_uuid) REFERENCES agents (tenant_id, id) NOT VALID
    - **match semantics**: `ALL_ROWS_CHECKED`
- **transitional writes**: `AGENT_BRIDGE_TRIGGER` — `trg_sessions_tenancy_bridge` (`fn_sessions_tenancy_bridge`) installed in PREPARE, before any backfill
- **initial nullability**: added NULL-able; NOT NULL only in FINAL_CONSTRAINT_TIGHTENING, after backfill verification reports zero blocking findings
- **backfill shape**: `UPDATE sessions s SET agent_uuid = a.id, tenant_id = a.tenant_id FROM agents a WHERE a.agent_id = s.agent_id`
- **must be zero**: `LEGACY_UNMAPPED`, `ORPHANED_AGENT_REFERENCE`, `UNMAPPED_AGENT`, `UNRESOLVABLE_OWNER`, `NULL_OWNERSHIP_LINK`
- **validate step**: VALIDATE CONSTRAINT after backfill
- **future uniqueness**: Session identity stays AGENT-scoped. `idx_sessions_agent_session` is UNIQUE (agent_id, session_id) today and becomes UNIQUE (tenant_id, agent_uuid, session_id) as `sessions_tenant_agent_session_key`, which is the FK target `working_memory` references. A tenant-scoped UNIQUE (tenant_id, session_id) is explicitly **rejected**: `session_id` is caller-supplied TEXT, so two agents in one tenant sharing the string `default` is ordinary caller behaviour, and widening identity to the tenant would make that a collision. Because the new tuple is a superset of the current one, the change is a relaxation and cannot collide.
- **lock profile**: ADD COLUMN NULL is metadata-only; a new UNIQUE index should be built CONCURRENTLY outside the migration transaction
- **rollback dependencies**: `working_memory`, `memories`, `memory_retrieval_logs`, `extraction_jobs`, `cognitive_hypervisor_timeline`, `rmk_episodes`
- **must stay inactive**: `POST /sessions`, `every session-scoped write`

### `working_memory`

- **class**: `SESSION_CHILD`
- **tranche**: `TRANCHE_2_SESSIONS`
- **rationale**: The one genuine session child: `session_id` is NOT NULL and the row is meaningless without its session.
- **row identity**: `id` (surrogate, emitted as-is)
- **REQUIRED_CURRENT_SCHEMA_CONTRACT** — *verified against the live catalog on every audit run; per-run status is in the machine report, not here.*
  - `working_memory_pkey` PRIMARY KEY (id), validated
  - `working_memory_agent_id_session_id_key` UNIQUE (agent_id, session_id), validated
- **session role**: `CANONICAL` — INSERT carries session_id; rows are deleted by `WHERE agent_id = $1 AND session_id = $2`, i.e. addressed *by* their session; UNIQUE (agent_id, session_id) is exactly the session's own unique key. The session is what identifies the row, so a missing one is a broken row rather than absent context.
- **canonical path**: `(row.agent_id, row.session_id) -> sessions(agent_id, session_id) -> agents.agent_id -> agents.id -> agents.tenant_id`
- **secondary paths**:
  - `row.agent_id -> agents.agent_id -> agents.id -> agents.tenant_id`
- **consistency**: The only table whose session reference is NOT NULL, and whose own unique key `(agent_id, session_id)` is exactly the session's unique key. The denormalised agent must match the session's agent. OPERATIONAL CAVEAT: `working_memory` and its `sessions` row are written by two consecutive statements, not one atomic unit - the upsert writes working memory first and the session second. A live audit can therefore observe the brief state between them and report a session orphan that is about to exist. The finding is deliberately left BLOCKING, because a persistent orphan is a real inconsistency and weakening it would hide that, so a migration-readiness scan must be run with extraction writers quiesced or drained, or re-run once activity settles.
- **planned objects**:
  - [TRANCHE_2_SESSIONS] ALTER TABLE working_memory ADD COLUMN agent_uuid UUID NULL — NULL-able now; NOT NULL in FUTURE STEP 7
  - [TRANCHE_2_SESSIONS] ALTER TABLE working_memory ADD COLUMN tenant_id UUID NULL — NULL-able now; NOT NULL in FUTURE STEP 7
  - [TRANCHE_2_SESSIONS] CREATE INDEX CONCURRENTLY idx_working_memory_tenant ON working_memory (tenant_id, agent_uuid)
  - [TRANCHE_2_SESSIONS] ALTER TABLE working_memory ADD CONSTRAINT working_memory_tenant_agent_fkey FOREIGN KEY (tenant_id, agent_uuid) REFERENCES agents (tenant_id, id) NOT VALID
    - **match semantics**: `ALL_ROWS_CHECKED`
  - [TRANCHE_2_SESSIONS] ALTER TABLE working_memory ADD CONSTRAINT working_memory_session_fkey FOREIGN KEY (tenant_id, agent_uuid, session_id) REFERENCES sessions (tenant_id, agent_uuid, session_id) NOT VALID
    - **match semantics**: `ALL_ROWS_CHECKED`
- **transitional writes**: `SESSION_BRIDGE_TRIGGER` — `trg_working_memory_tenancy_bridge` (`fn_working_memory_tenancy_bridge`) installed in PREPARE, before any backfill, resolving through the parent its backfill authority names
- **backfill source**: `sessions s ON s.agent_id = w.agent_id AND s.session_id = w.session_id`
- **all paths must agree**: `s.tenant_id IS NOT NULL AND s.agent_uuid IS NOT NULL`
- **on disagreement**: left NULL and reported; sessions must be fully backfilled first, so a NULL here means either the session row does not exist yet or its own backfill has not run
- **initial nullability**: added NULL-able; NOT NULL only in FINAL_CONSTRAINT_TIGHTENING, after backfill verification reports zero blocking findings
- **backfill shape**: `UPDATE working_memory w SET agent_uuid = s.agent_uuid, tenant_id = s.tenant_id FROM sessions s WHERE s.agent_id = w.agent_id AND s.session_id = w.session_id`
- **must be zero**: `LEGACY_UNMAPPED`, `ORPHANED_AGENT_REFERENCE`, `ORPHANED_SESSION_REFERENCE`, `UNMAPPED_AGENT`, `UNRESOLVABLE_OWNER`, `NULL_OWNERSHIP_LINK`, `OWNERSHIP_PATH_DISAGREEMENT`
- **validate step**: VALIDATE CONSTRAINT after backfill
- **future uniqueness**: `working_memory_agent_id_session_id_key` moves with `sessions` and stays agent-scoped: UNIQUE (tenant_id, agent_uuid, session_id). It also gains a real composite foreign key — (tenant_id, agent_uuid, session_id) REFERENCES sessions(tenant_id, agent_uuid, session_id) — so the session reference is enforced by the database rather than by convention. The two uniqueness rules must move together or they disagree about what identifies a session.
- **lock profile**: one row per active session; small
- **rollback dependencies**: `sessions (tranche 2)`
- **must stay inactive**: `every session-scoped write`

## Reason-code catalogue

| Code | Severity | Meaning |
|---|---|---|
| `UNCLASSIFIED_TABLE` | BLOCKING | a discovered application table has no registry entry, so no tenancy decision has been made for it |
| `INVENTORY_TABLE_MISSING` | BLOCKING | the registry names a table absent from the live schema |
| `MULTIPLE_CLASSIFICATIONS` | BLOCKING | a table has more than one semantic inventory entry |
| `MISSING_CANONICAL_OWNERSHIP_PATH` | BLOCKING | a non-SYSTEM_GLOBAL table lacks exactly one canonical tenant path |
| `SCHEMA_RELATIONSHIP_DRIFT` | BLOCKING | a required column, foreign key, uniqueness rule or relationship no longer matches the ownership definition |
| `LEGACY_UNMAPPED` | BLOCKING | the row cannot be assigned to a tenant safely from authoritative data |
| `ORPHANED_AGENT_REFERENCE` | BLOCKING | an agent reference resolves to no agent |
| `UNMAPPED_AGENT` | BLOCKING | the owning agent exists but agents.tenant_id is NULL |
| `ORPHANED_SESSION_REFERENCE` | BLOCKING | a required session reference resolves to no session |
| `ORPHANED_MEMORY_REFERENCE` | BLOCKING | a required memory reference resolves to no memory |
| `ORPHANED_VERSION_REFERENCE` | BLOCKING | a required memory-version reference resolves to no version (reserved: no table in the current schema references memory_versions.id, so it cannot be emitted today) |
| `UNRESOLVABLE_OWNER` | BLOCKING | the canonical ownership chain terminates without an authoritative owner |
| `NULL_OWNERSHIP_LINK` | BLOCKING | a required link in the canonical ownership chain is NULL |
| `OWNERSHIP_PATH_DISAGREEMENT` | BLOCKING | canonical and secondary ownership paths resolve to different owners within one tenant |
| `CROSS_TENANT_PARENT_CHILD` | BLOCKING | two ownership paths for the same row resolve to different tenants |
| `MALFORMED_LEGACY_IDENTIFIER` | ADVISORY | a legacy identifier cannot be interpreted according to its schema contract |
| `AMBIGUOUS_LEGACY_IDENTIFIER` | BLOCKING | one legacy identifier maps to more than one possible owner |
| `FUTURE_TENANT_UNIQUENESS_COLLISION` | BLOCKING | existing rows would violate a planned UNIQUE (tenant_id, ...) rule |
| `FUTURE_COMPOSITE_FK_MISMATCH` | BLOCKING | existing rows would violate a planned composite tenant/owner foreign key |
| `GLOBAL_SCOPE_UNVERIFIED` | BLOCKING | a table is classified SYSTEM_GLOBAL without evidence that global scope is safe |
