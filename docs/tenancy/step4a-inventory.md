# AEON tenancy inventory — Step 4A

Generated from `src/tenancy/inventory.rs`. Do not edit by hand — the registry is the source of truth and a test regenerates this file and compares it.

- schema version: `step4a.1`
- inventory digest: `sha256:283fd55811aefba6e717cbc91f23bd2fa9eb5beeb956a1bd53266b7d0226542b`
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

**Status: DECLARED — runtime verification is not implemented in this checkpoint.**

Each table declares the schema objects its ownership joins and row identity rely on **today** — not the constraints Step 4B will add, which live in the migration plan. Nothing listed here has been checked against a live database: these are registry declarations, not verified live-database facts. Until the verifier lands, a dropped or altered constraint would go unreported.

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

### TRANCHE_5_TENANT_GLOBAL_OPERATIONS

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
- **REQUIRED_CURRENT_SCHEMA_CONTRACT** — *Status: DECLARED; runtime verification is not implemented in this checkpoint.*
  - `agent_tenancy_migrations_pkey` PRIMARY KEY (id), validated
- **canonical path**: *(none — SYSTEM_GLOBAL)*
- **global-scope evidence**: Ledger of tenancy *migration runs*, written only by the step-1 backfill and read only by `assert_multi_tenant_preconditions`. It is on no request path. Its `legacy_tenant_id` is an input parameter of the run that produced the row, not ownership of the row: migration 0028 constrains it with `(mode = 'single_tenant') = (legacy_tenant_id IS NOT NULL)`, so in `mapped` mode it is NULL by construction. Giving these rows a tenant would misrepresent an operator action as tenant data.
- **added columns**: *(none)*
- **initial nullability**: n/a — no ownership columns are added
- **backfill**: `n/a — SYSTEM_GLOBAL tables are never backfilled with an inferred tenant`
- **must be zero**: `GLOBAL_SCOPE_UNVERIFIED`
- **pre-validation indexes**: *(none)*
- **planned composite FKs**: *(none)*
- **NOT VALID appropriate**: false
- **lock profile**: none — not modified
- **rollback dependencies**: *(none)*
- **must stay inactive**: *(none)*

### `agents`

- **class**: `TENANT_ROOT`
- **tranche**: `TRANCHE_1_ROOTS_AND_DIRECT_AGENT_CHILDREN`
- **rationale**: The tenant root: it holds `tenant_id` itself and is what every other ownership path resolves to.
- **row identity**: `id` (surrogate, emitted as-is)
- **REQUIRED_CURRENT_SCHEMA_CONTRACT** — *Status: DECLARED; runtime verification is not implemented in this checkpoint.*
  - `agents_pkey` PRIMARY KEY (id), validated
  - `agents_agent_id_key` UNIQUE (agent_id), validated
  - `agents_tenant_id_external_agent_id_key` UNIQUE (tenant_id, external_agent_id), validated
  - `agents_tenant_id_id_key` UNIQUE (tenant_id, id), validated
- **canonical path**: `row.tenant_id (authoritative)`
- **consistency**: `agents` is the authority: every other path in this registry terminates here. `tenant_id` is still NULL-able because step 1 deliberately leaves unmapped agents unreachable rather than inventing a tenant for them; every such row is reported as UNMAPPED_AGENT.
- **added columns**: *(none)*
- **initial nullability**: already present from migration 0028
- **backfill**: `already backfilled by step 1 under an explicit LEGACY_MIGRATION_MODE`
- **must be zero**: `LEGACY_UNMAPPED`, `NULL_OWNERSHIP_LINK`, `UNMAPPED_AGENT`, `FUTURE_TENANT_UNIQUENESS_COLLISION`
- **pre-validation indexes**: `agents_tenant_id_id_key (exists)`
- **planned composite FKs**: *(none)*
- **NOT VALID appropriate**: false
- **validate step**: SET NOT NULL on agents.tenant_id once UNMAPPED_AGENT reaches zero
- **future uniqueness**: `agents_agent_id_key` is globally UNIQUE on the legacy TEXT identifier. Once tenants are real that is a cross-tenant namespace: two tenants cannot both use the identifier `assistant`. It must become UNIQUE (tenant_id, agent_id), which is a relaxation and cannot collide — the collision risk runs the other way and is checked on `sessions`.
- **lock profile**: SET NOT NULL takes ACCESS EXCLUSIVE and scans the table; on PG 16 a previously validated CHECK lets it skip the scan
- **rollback dependencies**: `rollback/0028_agent_tenancy_identity_down.sql`
- **must stay inactive**: `POST /agents`, `GET /agents`

### `amp_controller_state`

- **class**: `DIRECT_AGENT_CHILD`
- **tranche**: `TRANCHE_5_TENANT_GLOBAL_OPERATIONS`
- **rationale**: Reads as global controller state, but its primary key *is* `agent_id` and `rmk_worker` writes one row per agent. It is per-agent state — the name is the only thing about it that suggests otherwise, which is why the classification is taken from the key and the writers instead.
- **row identity**: `agent_id` (caller text, pseudonymised)
- **REQUIRED_CURRENT_SCHEMA_CONTRACT** — *Status: DECLARED; runtime verification is not implemented in this checkpoint.*
  - `amp_controller_state_pkey` PRIMARY KEY (agent_id), validated
- **canonical path**: `row.agent_id -> agents.agent_id -> agents.id -> agents.tenant_id`
- **added columns**: `agent_uuid UUID`, `tenant_id UUID`
- **initial nullability**: added NULL-able; NOT NULL only in FINAL_CONSTRAINT_TIGHTENING, after backfill verification reports zero blocking findings
- **backfill**: `UPDATE amp_controller_state s SET agent_uuid = a.id, tenant_id = a.tenant_id FROM agents a WHERE a.agent_id = s.agent_id`
- **must be zero**: `LEGACY_UNMAPPED`, `ORPHANED_AGENT_REFERENCE`, `UNMAPPED_AGENT`, `UNRESOLVABLE_OWNER`, `NULL_OWNERSHIP_LINK`
- **pre-validation indexes**: `idx_amp_controller_state_tenant (tenant_id, agent_uuid)`
- **planned composite FKs**: `FOREIGN KEY (tenant_id, agent_uuid) REFERENCES agents(tenant_id, id)`
- **NOT VALID appropriate**: true
- **validate step**: VALIDATE CONSTRAINT after backfill
- **lock profile**: primary key is the legacy TEXT `agent_id`; re-keying to `agent_uuid` is a table rewrite and is deferred to FINAL_CONSTRAINT_TIGHTENING
- **rollback dependencies**: `tranche 1`
- **must stay inactive**: `rmk_worker`, `extraction_worker`, `hnsw_maintenance`

### `archival_batches`

- **class**: `DIRECT_AGENT_CHILD`
- **tranche**: `TRANCHE_1_ROOTS_AND_DIRECT_AGENT_CHILDREN`
- **rationale**: Has a real FK to `agents(agent_id)`, so its owner is already enforced. Must precede `memories`, which references it.
- **row identity**: `id` (surrogate, emitted as-is)
- **REQUIRED_CURRENT_SCHEMA_CONTRACT** — *Status: DECLARED; runtime verification is not implemented in this checkpoint.*
  - `archival_batches_pkey` PRIMARY KEY (id), validated
  - `archival_batches_agent_id_fkey` FOREIGN KEY (agent_id) REFERENCES agents(agent_id), validated
- **canonical path**: `row.agent_id -> agents.agent_id -> agents.id -> agents.tenant_id`
- **added columns**: `agent_uuid UUID`, `tenant_id UUID`
- **initial nullability**: added NULL-able; NOT NULL only in FINAL_CONSTRAINT_TIGHTENING, after backfill verification reports zero blocking findings
- **backfill**: `UPDATE archival_batches b SET agent_uuid = a.id, tenant_id = a.tenant_id FROM agents a WHERE a.agent_id = b.agent_id`
- **must be zero**: `LEGACY_UNMAPPED`, `ORPHANED_AGENT_REFERENCE`, `UNMAPPED_AGENT`, `UNRESOLVABLE_OWNER`, `NULL_OWNERSHIP_LINK`
- **pre-validation indexes**: `idx_archival_batches_tenant (tenant_id, agent_uuid)`
- **planned composite FKs**: `FOREIGN KEY (tenant_id, agent_uuid) REFERENCES agents(tenant_id, id)`
- **NOT VALID appropriate**: true
- **validate step**: VALIDATE CONSTRAINT after backfill
- **lock profile**: ADD COLUMN NULL is metadata-only; the FK validation takes SHARE ROW EXCLUSIVE
- **rollback dependencies**: `memories (memories.archival_batch_id references this table)`
- **must stay inactive**: `archival worker`, `POST /memories`, `GET /memories`

### `audit_logs`

- **class**: `DIRECT_AGENT_CHILD`
- **tranche**: `TRANCHE_1_ROOTS_AND_DIRECT_AGENT_CHILDREN`
- **rationale**: Agent-scoped when an agent is known. LEGACY_UNMAPPED exists as a row-level code precisely for tables like this one, which hold both assignable and unassignable rows.
- **row identity**: `id` (surrogate, emitted as-is)
- **REQUIRED_CURRENT_SCHEMA_CONTRACT** — *Status: DECLARED; runtime verification is not implemented in this checkpoint.*
  - `audit_logs_pkey` PRIMARY KEY (id), validated
- **canonical path**: `row.agent_id -> agents.agent_id -> agents.id -> agents.tenant_id`
- **consistency**: `agent_id` is NULL-able here and nowhere else among the direct children. A NULL is not schema drift — some events genuinely have no agent — but it is unassignable, so those rows are reported LEGACY_UNMAPPED and the table cannot take a NOT NULL tenant until their disposition is decided.
- **added columns**: `agent_uuid UUID`, `tenant_id UUID`
- **initial nullability**: added NULL-able and **stays** NULL-able until the disposition of agent-less audit rows is decided
- **backfill**: `UPDATE audit_logs l SET agent_uuid = a.id, tenant_id = a.tenant_id FROM agents a WHERE a.agent_id = l.agent_id`
- **must be zero**: `ORPHANED_AGENT_REFERENCE`, `UNMAPPED_AGENT`, `UNRESOLVABLE_OWNER`
- **pre-validation indexes**: `idx_audit_logs_tenant (tenant_id, agent_uuid)`
- **planned composite FKs**: `FOREIGN KEY (tenant_id, agent_uuid) REFERENCES agents(tenant_id, id)`
- **NOT VALID appropriate**: true
- **validate step**: VALIDATE CONSTRAINT after backfill
- **lock profile**: append-mostly and potentially large; ADD COLUMN NULL is metadata-only
- **rollback dependencies**: `tranche 1`
- **must stay inactive**: `all write paths that emit audit events`

### `co_access_edges`

- **class**: `MEMORY_LINEAGE_CHILD`
- **tranche**: `TRANCHE_4_LINEAGE_AND_ARCHIVAL`
- **rationale**: No agent column exists. Both `memory_a` and `memory_b` are NOT NULL FKs to `memories`, so ownership is entirely lineage-derived.
- **row identity**: `memory_a` (surrogate, emitted as-is), `memory_b` (surrogate, emitted as-is)
- **REQUIRED_CURRENT_SCHEMA_CONTRACT** — *Status: DECLARED; runtime verification is not implemented in this checkpoint.*
  - `co_access_edges_pkey` PRIMARY KEY (memory_a, memory_b), validated
  - `co_access_edges_memory_a_fkey` FOREIGN KEY (memory_a) REFERENCES memories(id), validated
  - `co_access_edges_memory_b_fkey` FOREIGN KEY (memory_b) REFERENCES memories(id), validated
- **canonical path**: `row.memory_id -> memories.id -> memories.agent_id -> agents.agent_id -> agents.tenant_id`
- **secondary paths**:
  - `row.memory_id -> memories.id -> memories.agent_id -> agents.agent_id -> agents.tenant_id`
- **consistency**: Both endpoints must resolve to the same tenant. This is the only table with no agent column at all, so the two memory references are the entire ownership story — and an edge spanning two tenants is precisely the cross-tenant link the audit exists to find.
- **added columns**: `tenant_id UUID`
- **initial nullability**: added NULL-able; NOT NULL only in FINAL_CONSTRAINT_TIGHTENING, after backfill verification reports zero blocking findings
- **backfill**: `UPDATE co_access_edges e SET tenant_id = m.tenant_id FROM memories m WHERE m.id = e.memory_a, only where memory_a and memory_b agree`
- **must be zero**: `LEGACY_UNMAPPED`, `ORPHANED_MEMORY_REFERENCE`, `ORPHANED_AGENT_REFERENCE`, `UNMAPPED_AGENT`, `UNRESOLVABLE_OWNER`, `NULL_OWNERSHIP_LINK`, `CROSS_TENANT_PARENT_CHILD`, `OWNERSHIP_PATH_DISAGREEMENT`
- **pre-validation indexes**: `idx_co_access_edges_tenant (tenant_id)`
- **planned composite FKs**: `FOREIGN KEY (memory_a, tenant_id) REFERENCES memories(id, tenant_id)`, `FOREIGN KEY (memory_b, tenant_id) REFERENCES memories(id, tenant_id)`
- **NOT VALID appropriate**: true
- **validate step**: VALIDATE both constraints after backfill
- **lock profile**: two FK validations, each SHARE ROW EXCLUSIVE on memories
- **rollback dependencies**: `memories (tranche 3)`
- **must stay inactive**: `co-access edge maintenance`, `retrieval scoring`

### `cognitive_hypervisor_timeline`

- **class**: `DIRECT_AGENT_CHILD`
- **tranche**: `TRANCHE_3_MEMORIES`
- **rationale**: `session_id` is NULL-able, so the session cannot be the canonical path; the NOT NULL `agent_id` is. The session remains a consistency check.
- **row identity**: `id` (surrogate, emitted as-is)
- **REQUIRED_CURRENT_SCHEMA_CONTRACT** — *Status: DECLARED; runtime verification is not implemented in this checkpoint.*
  - `cognitive_hypervisor_timeline_pkey` PRIMARY KEY (id), validated
- **session role**: `CONTEXT_ONLY` — Read by `WHERE agent_id = $1` and `WHERE id = $1`; the hash chain is per-agent. No query selects by session.
- **canonical path**: `row.agent_id -> agents.agent_id -> agents.id -> agents.tenant_id`
- **consistency**: Measured, then deliberately excluded from ownership. `sessions` rows are created by exactly one code path in the codebase, the working-memory upsert, and it runs at the *end* of successful extraction. A pending job, a permanently failed job, or a memory written before that upsert therefore legitimately references a session that does not exist. Registering the session as an ownership or consistency path would emit blocking ORPHANED_SESSION_REFERENCE findings on entirely normal states, so it is recorded as context only rather than dropped silently: CONTEXT_ONLY says *investigated and non-authoritative*, where an absent conclusion would only say *not investigated*.
- **added columns**: `agent_uuid UUID`, `tenant_id UUID`
- **initial nullability**: added NULL-able; NOT NULL only in FINAL_CONSTRAINT_TIGHTENING, after backfill verification reports zero blocking findings
- **backfill**: `UPDATE cognitive_hypervisor_timeline t SET agent_uuid = a.id, tenant_id = a.tenant_id FROM agents a WHERE a.agent_id = t.agent_id`
- **must be zero**: `LEGACY_UNMAPPED`, `ORPHANED_AGENT_REFERENCE`, `UNMAPPED_AGENT`, `UNRESOLVABLE_OWNER`, `NULL_OWNERSHIP_LINK`
- **pre-validation indexes**: `idx_cht_tenant (tenant_id, agent_uuid)`
- **planned composite FKs**: `FOREIGN KEY (tenant_id, agent_uuid) REFERENCES agents(tenant_id, id)`
- **NOT VALID appropriate**: true
- **validate step**: VALIDATE CONSTRAINT after backfill
- **lock profile**: append-only hash-chained timeline; ADD COLUMN NULL is metadata-only
- **rollback dependencies**: `sessions (tranche 2)`
- **must stay inactive**: `POST /hypervisor/events`, `GET /hypervisor/timeline`

### `credential_agent_grants`

- **class**: `DIRECT_AGENT_CHILD`
- **tranche**: `TRANCHE_1_ROOTS_AND_DIRECT_AGENT_CHILDREN`
- **rationale**: The only table already keyed on the internal UUID. Its tenant is materialised but derived — the composite FK to `agents(tenant_id, id)` is what makes it authoritative — so it is a direct agent child, not a root.
- **row identity**: `credential_id` (surrogate, emitted as-is), `agent_uuid` (surrogate, emitted as-is)
- **REQUIRED_CURRENT_SCHEMA_CONTRACT** — *Status: DECLARED; runtime verification is not implemented in this checkpoint.*
  - `credential_agent_grants_pkey` PRIMARY KEY (credential_id, agent_uuid), validated
  - `credential_agent_grants_agent_fkey` FOREIGN KEY (tenant_id, agent_uuid) REFERENCES agents(tenant_id, id), validated
  - `credential_agent_grants_credential_fkey` FOREIGN KEY (credential_id, tenant_id) REFERENCES credentials(id, tenant_id), validated
- **canonical path**: `row.agent_uuid -> agents.id -> agents.tenant_id`
- **secondary paths**:
  - `row.tenant_id (materialised, FK-enforced)`
- **consistency**: The materialised `tenant_id` must equal the tenant reached through `agent_uuid`. Migration 0030 already enforces this with composite foreign keys to both parents, so a disagreement here would mean the constraint itself had drifted — which is why the check is kept rather than assumed.
- **added columns**: *(none)*
- **initial nullability**: already NOT NULL from migration 0030
- **backfill**: `n/a — already tenant-scoped`
- **must be zero**: `OWNERSHIP_PATH_DISAGREEMENT`, `SCHEMA_RELATIONSHIP_DRIFT`
- **pre-validation indexes**: *(none)*
- **planned composite FKs**: *(none)*
- **NOT VALID appropriate**: false
- **lock profile**: none — not modified
- **rollback dependencies**: `rollback/0031_credential_agent_grants_hardening_down.sql`, `rollback/0030_credential_agent_grants_down.sql`
- **must stay inactive**: *(none)*

### `credentials`

- **class**: `TENANT_GLOBAL`
- **tranche**: `TRANCHE_1_ROOTS_AND_DIRECT_AGENT_CHILDREN`
- **rationale**: TENANT_GLOBAL rather than TENANT_ROOT: it carries an authoritative tenant but is not the terminus other tables resolve to, and it is deliberately not bound to any one agent. TENANT_ROOT is reserved for `agents` and any future pure `tenants` table.
- **row identity**: `id` (surrogate, emitted as-is)
- **REQUIRED_CURRENT_SCHEMA_CONTRACT** — *Status: DECLARED; runtime verification is not implemented in this checkpoint.*
  - `credentials_pkey` PRIMARY KEY (id), validated
  - `credentials_id_tenant_id_key` UNIQUE (id, tenant_id), validated
- **canonical path**: `row.tenant_id (authoritative, NOT NULL)`
- **consistency**: `tenant_id` is NOT NULL and is the credential's own authority. No agent parent exists to cross-check it against: a `tenant_wide` credential reaches every agent in the tenant, and an agent-restricted one reaches only those named in `credential_agent_grants` — so the credential is owned by one tenant without being owned by one agent.
- **added columns**: *(none)*
- **initial nullability**: already NOT NULL from migration 0029
- **backfill**: `n/a — created tenant-scoped`
- **must be zero**: `SCHEMA_RELATIONSHIP_DRIFT`
- **pre-validation indexes**: *(none)*
- **planned composite FKs**: *(none)*
- **NOT VALID appropriate**: false
- **lock profile**: none — not modified
- **rollback dependencies**: *(none)*
- **must stay inactive**: *(none)*

### `entities`

- **class**: `DIRECT_AGENT_CHILD`
- **tranche**: `TRANCHE_1_ROOTS_AND_DIRECT_AGENT_CHILDREN`
- **rationale**: Per-agent extraction output, keyed by the legacy identifier.
- **row identity**: `id` (surrogate, emitted as-is)
- **REQUIRED_CURRENT_SCHEMA_CONTRACT** — *Status: DECLARED; runtime verification is not implemented in this checkpoint.*
  - `entities_pkey` PRIMARY KEY (id), validated
- **canonical path**: `row.agent_id -> agents.agent_id -> agents.id -> agents.tenant_id`
- **added columns**: `agent_uuid UUID`, `tenant_id UUID`
- **initial nullability**: added NULL-able; NOT NULL only in FINAL_CONSTRAINT_TIGHTENING, after backfill verification reports zero blocking findings
- **backfill**: `UPDATE entities e SET agent_uuid = a.id, tenant_id = a.tenant_id FROM agents a WHERE a.agent_id = e.agent_id`
- **must be zero**: `LEGACY_UNMAPPED`, `ORPHANED_AGENT_REFERENCE`, `UNMAPPED_AGENT`, `UNRESOLVABLE_OWNER`, `NULL_OWNERSHIP_LINK`
- **pre-validation indexes**: `idx_entities_tenant (tenant_id, agent_uuid)`
- **planned composite FKs**: `FOREIGN KEY (tenant_id, agent_uuid) REFERENCES agents(tenant_id, id)`
- **NOT VALID appropriate**: true
- **validate step**: VALIDATE CONSTRAINT after backfill
- **future uniqueness**: `entities_agent_id_name_key` is UNIQUE (agent_id, name) and stays agent-scoped: an entity belongs to one agent, and widening it to the tenant would merge two agents' entity namespaces. Recorded so the decision is explicit rather than an omission.
- **lock profile**: ADD COLUMN NULL is metadata-only; validation is SHARE ROW EXCLUSIVE
- **rollback dependencies**: `memory_entity_links (tranche 4)`
- **must stay inactive**: `extraction_worker`, `POST /entities`

### `extraction_jobs`

- **class**: `DIRECT_AGENT_CHILD`
- **tranche**: `TRANCHE_3_MEMORIES`
- **rationale**: A work queue whose rows are owned by the agent that enqueued them; the session is optional context.
- **row identity**: `id` (surrogate, emitted as-is)
- **REQUIRED_CURRENT_SCHEMA_CONTRACT** — *Status: DECLARED; runtime verification is not implemented in this checkpoint.*
  - `extraction_jobs_pkey` PRIMARY KEY (id), validated
- **session role**: `CONTEXT_ONLY` — The worker claims jobs with an unfiltered queue scan over `extraction_jobs`, never by session; session_id is payload context carried through to the extracted memories.
- **canonical path**: `row.agent_id -> agents.agent_id -> agents.id -> agents.tenant_id`
- **consistency**: Measured, then deliberately excluded from ownership. `sessions` rows are created by exactly one code path in the codebase, the working-memory upsert, and it runs at the *end* of successful extraction. A pending job, a permanently failed job, or a memory written before that upsert therefore legitimately references a session that does not exist. Registering the session as an ownership or consistency path would emit blocking ORPHANED_SESSION_REFERENCE findings on entirely normal states, so it is recorded as context only rather than dropped silently: CONTEXT_ONLY says *investigated and non-authoritative*, where an absent conclusion would only say *not investigated*.
- **added columns**: `agent_uuid UUID`, `tenant_id UUID`
- **initial nullability**: added NULL-able; NOT NULL only in FINAL_CONSTRAINT_TIGHTENING, after backfill verification reports zero blocking findings
- **backfill**: `UPDATE extraction_jobs j SET agent_uuid = a.id, tenant_id = a.tenant_id FROM agents a WHERE a.agent_id = j.agent_id`
- **must be zero**: `LEGACY_UNMAPPED`, `ORPHANED_AGENT_REFERENCE`, `UNMAPPED_AGENT`, `UNRESOLVABLE_OWNER`, `NULL_OWNERSHIP_LINK`
- **pre-validation indexes**: `idx_extraction_jobs_tenant (tenant_id, agent_uuid)`
- **planned composite FKs**: `FOREIGN KEY (tenant_id, agent_uuid) REFERENCES agents(tenant_id, id)`
- **NOT VALID appropriate**: true
- **validate step**: VALIDATE CONSTRAINT after backfill
- **lock profile**: queue table with frequent updates; ADD COLUMN NULL is metadata-only
- **rollback dependencies**: `sessions (tranche 2)`
- **must stay inactive**: `extraction_worker`

### `memories`

- **class**: `DIRECT_AGENT_CHILD`
- **tranche**: `TRANCHE_3_MEMORIES`
- **rationale**: Root of the memory lineage, but its own owner is the agent: `session_id` is NULL-able and `archival_batch_id` is NULL-able, so neither can be canonical.
- **row identity**: `id` (surrogate, emitted as-is)
- **REQUIRED_CURRENT_SCHEMA_CONTRACT** — *Status: DECLARED; runtime verification is not implemented in this checkpoint.*
  - `memories_pkey` PRIMARY KEY (id), validated
  - `memories_archival_batch_id_fkey` FOREIGN KEY (archival_batch_id) REFERENCES archival_batches(id), validated
- **session role**: `CONTEXT_ONLY` — Every read predicate is `WHERE agent_id = ...` or `WHERE id = ...`; no query selects memories by session. Deletion is by agent or by id. The session records where a memory came from, not who owns it.
- **canonical path**: `row.agent_id -> agents.agent_id -> agents.id -> agents.tenant_id`
- **secondary paths**:
  - `row.archival_batch_id -> archival_batches.id -> agent_id -> tenant`
- **consistency**: The archival batch, where present, must resolve to the same agent: an archived memory whose batch belongs to a different agent is a blocking disagreement, not a reason to prefer one path. The session is context only — `sessions` rows are created solely by the working-memory upsert at the end of successful extraction, so a memory written before it legitimately references a session that does not yet exist.
- **added columns**: `agent_uuid UUID`, `tenant_id UUID`
- **initial nullability**: added NULL-able; NOT NULL only in FINAL_CONSTRAINT_TIGHTENING, after backfill verification reports zero blocking findings
- **backfill**: `UPDATE memories m SET agent_uuid = a.id, tenant_id = a.tenant_id FROM agents a WHERE a.agent_id = m.agent_id`
- **must be zero**: `LEGACY_UNMAPPED`, `ORPHANED_AGENT_REFERENCE`, `ORPHANED_SESSION_REFERENCE`, `UNMAPPED_AGENT`, `UNRESOLVABLE_OWNER`, `NULL_OWNERSHIP_LINK`, `OWNERSHIP_PATH_DISAGREEMENT`
- **pre-validation indexes**: `idx_memories_tenant (tenant_id, agent_uuid)`, `memories_id_tenant_id_key UNIQUE (id, tenant_id) — required as the FK target for every lineage child`
- **planned composite FKs**: `FOREIGN KEY (tenant_id, agent_uuid) REFERENCES agents(tenant_id, id)`, `FOREIGN KEY (archival_batch_id, tenant_id) REFERENCES archival_batches(id, tenant_id)`
- **NOT VALID appropriate**: true
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
- **REQUIRED_CURRENT_SCHEMA_CONTRACT** — *Status: DECLARED; runtime verification is not implemented in this checkpoint.*
  - `memory_conflicts_pkey` PRIMARY KEY (id), validated
  - `memory_conflicts_memory_a_fkey` FOREIGN KEY (memory_a) REFERENCES memories(id), validated
  - `memory_conflicts_memory_b_fkey` FOREIGN KEY (memory_b) REFERENCES memories(id), validated
- **canonical path**: `row.agent_id -> agents.agent_id -> agents.id -> agents.tenant_id`
- **secondary paths**:
  - `row.memory_id -> memories.id -> memories.agent_id -> agents.agent_id -> agents.tenant_id`
  - `row.memory_id -> memories.id -> memories.agent_id -> agents.agent_id -> agents.tenant_id`
- **consistency**: Both memory references are NULL-able, so neither can be canonical. Where present, each must resolve to the same tenant as the row's agent.
- **added columns**: `agent_uuid UUID`, `tenant_id UUID`
- **initial nullability**: added NULL-able; NOT NULL only in FINAL_CONSTRAINT_TIGHTENING, after backfill verification reports zero blocking findings
- **backfill**: `UPDATE memory_conflicts c SET agent_uuid = a.id, tenant_id = a.tenant_id FROM agents a WHERE a.agent_id = c.agent_id`
- **must be zero**: `LEGACY_UNMAPPED`, `ORPHANED_AGENT_REFERENCE`, `UNMAPPED_AGENT`, `UNRESOLVABLE_OWNER`, `NULL_OWNERSHIP_LINK`
- **pre-validation indexes**: `idx_memory_conflicts_tenant (tenant_id, agent_uuid)`
- **planned composite FKs**: `FOREIGN KEY (tenant_id, agent_uuid) REFERENCES agents(tenant_id, id)`, `FOREIGN KEY (memory_a, tenant_id) REFERENCES memories(id, tenant_id)`, `FOREIGN KEY (memory_b, tenant_id) REFERENCES memories(id, tenant_id)`
- **NOT VALID appropriate**: true
- **validate step**: VALIDATE all three after backfill
- **lock profile**: small; three FK validations
- **rollback dependencies**: `memories (tranche 3)`
- **must stay inactive**: `conflict detection`

### `memory_entity_links`

- **class**: `MEMORY_LINEAGE_CHILD`
- **tranche**: `TRANCHE_4_LINEAGE_AND_ARCHIVAL`
- **rationale**: Primary key is `(memory_id, entity_id)`; both are NOT NULL FKs. The memory is canonical because it is also what the tenant column will be keyed to.
- **row identity**: `memory_id` (surrogate, emitted as-is), `entity_id` (surrogate, emitted as-is)
- **REQUIRED_CURRENT_SCHEMA_CONTRACT** — *Status: DECLARED; runtime verification is not implemented in this checkpoint.*
  - `memory_entity_links_pkey` PRIMARY KEY (memory_id, entity_id), validated
  - `memory_entity_links_memory_id_fkey` FOREIGN KEY (memory_id) REFERENCES memories(id), validated
  - `memory_entity_links_entity_id_fkey` FOREIGN KEY (entity_id) REFERENCES entities(id), validated
- **canonical path**: `row.memory_id -> memories.id -> memories.agent_id -> agents.agent_id -> agents.tenant_id`
- **secondary paths**:
  - `row.agent_id -> agents.agent_id -> agents.id -> agents.tenant_id`
  - `row.entity_id -> entities.id -> entities.agent_id -> tenant`
- **consistency**: Three independent routes to a tenant — the memory, the denormalised agent, and the entity — and all three must agree. This is the table where a silent fallback would be most tempting and most wrong: a link whose memory and entity belong to different tenants is a cross-tenant join, and picking either answer would launder it.
- **added columns**: `tenant_id UUID`
- **initial nullability**: added NULL-able; NOT NULL only in FINAL_CONSTRAINT_TIGHTENING, after backfill verification reports zero blocking findings
- **backfill**: `UPDATE memory_entity_links l SET tenant_id = m.tenant_id FROM memories m WHERE m.id = l.memory_id`
- **must be zero**: `LEGACY_UNMAPPED`, `ORPHANED_MEMORY_REFERENCE`, `ORPHANED_AGENT_REFERENCE`, `UNMAPPED_AGENT`, `UNRESOLVABLE_OWNER`, `NULL_OWNERSHIP_LINK`, `CROSS_TENANT_PARENT_CHILD`, `OWNERSHIP_PATH_DISAGREEMENT`
- **pre-validation indexes**: `idx_memory_entity_links_tenant (tenant_id)`
- **planned composite FKs**: `FOREIGN KEY (memory_id, tenant_id) REFERENCES memories(id, tenant_id)`, `FOREIGN KEY (entity_id, tenant_id) REFERENCES entities(id, tenant_id)`
- **NOT VALID appropriate**: true
- **validate step**: VALIDATE both after backfill
- **lock profile**: join table; two FK validations
- **rollback dependencies**: `memories (tranche 3)`, `entities (tranche 1)`
- **must stay inactive**: `extraction_worker`

### `memory_graph`

- **class**: `DIRECT_AGENT_CHILD`
- **tranche**: `TRANCHE_1_ROOTS_AND_DIRECT_AGENT_CHILDREN`
- **rationale**: Subject/predicate/object triples scoped to one agent. Despite the name it holds no FK to `memories`, so it is not lineage.
- **row identity**: `id` (surrogate, emitted as-is)
- **REQUIRED_CURRENT_SCHEMA_CONTRACT** — *Status: DECLARED; runtime verification is not implemented in this checkpoint.*
  - `memory_graph_pkey` PRIMARY KEY (id), validated
- **canonical path**: `row.agent_id -> agents.agent_id -> agents.id -> agents.tenant_id`
- **added columns**: `agent_uuid UUID`, `tenant_id UUID`
- **initial nullability**: added NULL-able; NOT NULL only in FINAL_CONSTRAINT_TIGHTENING, after backfill verification reports zero blocking findings
- **backfill**: `UPDATE memory_graph g SET agent_uuid = a.id, tenant_id = a.tenant_id FROM agents a WHERE a.agent_id = g.agent_id`
- **must be zero**: `LEGACY_UNMAPPED`, `ORPHANED_AGENT_REFERENCE`, `UNMAPPED_AGENT`, `UNRESOLVABLE_OWNER`, `NULL_OWNERSHIP_LINK`
- **pre-validation indexes**: `idx_memory_graph_tenant (tenant_id, agent_uuid)`
- **planned composite FKs**: `FOREIGN KEY (tenant_id, agent_uuid) REFERENCES agents(tenant_id, id)`
- **NOT VALID appropriate**: true
- **validate step**: VALIDATE CONSTRAINT after backfill
- **lock profile**: ADD COLUMN NULL is metadata-only
- **rollback dependencies**: `tranche 1`
- **must stay inactive**: `extraction_worker`, `GET /graph`

### `memory_retrieval_logs`

- **class**: `DIRECT_AGENT_CHILD`
- **tranche**: `TRANCHE_3_MEMORIES`
- **rationale**: Per-agent retrieval telemetry. Its `candidate_memory_ids` arrays are not foreign keys and are deliberately not treated as an ownership path.
- **row identity**: `id` (surrogate, emitted as-is)
- **REQUIRED_CURRENT_SCHEMA_CONTRACT** — *Status: DECLARED; runtime verification is not implemented in this checkpoint.*
  - `memory_retrieval_logs_pkey` PRIMARY KEY (id), validated
- **session role**: `CONTEXT_ONLY` — Two insert paths exist and one omits session_id entirely (`INSERT INTO memory_retrieval_logs (agent_id, query_hash, injected_memory_ids)`), so a log line is well-formed without a session. No read predicate uses it.
- **canonical path**: `row.agent_id -> agents.agent_id -> agents.id -> agents.tenant_id`
- **consistency**: Measured, then deliberately excluded from ownership. `sessions` rows are created by exactly one code path in the codebase, the working-memory upsert, and it runs at the *end* of successful extraction. A pending job, a permanently failed job, or a memory written before that upsert therefore legitimately references a session that does not exist. Registering the session as an ownership or consistency path would emit blocking ORPHANED_SESSION_REFERENCE findings on entirely normal states, so it is recorded as context only rather than dropped silently: CONTEXT_ONLY says *investigated and non-authoritative*, where an absent conclusion would only say *not investigated*.
- **added columns**: `agent_uuid UUID`, `tenant_id UUID`
- **initial nullability**: added NULL-able; NOT NULL only in FINAL_CONSTRAINT_TIGHTENING, after backfill verification reports zero blocking findings
- **backfill**: `UPDATE memory_retrieval_logs l SET agent_uuid = a.id, tenant_id = a.tenant_id FROM agents a WHERE a.agent_id = l.agent_id`
- **must be zero**: `LEGACY_UNMAPPED`, `ORPHANED_AGENT_REFERENCE`, `UNMAPPED_AGENT`, `UNRESOLVABLE_OWNER`, `NULL_OWNERSHIP_LINK`
- **pre-validation indexes**: `idx_memory_retrieval_logs_tenant (tenant_id, agent_uuid)`
- **planned composite FKs**: `FOREIGN KEY (tenant_id, agent_uuid) REFERENCES agents(tenant_id, id)`
- **NOT VALID appropriate**: true
- **validate step**: VALIDATE CONSTRAINT after backfill
- **lock profile**: append-heavy; also carries `query_text`, which the report must never echo
- **rollback dependencies**: `sessions (tranche 2)`
- **must stay inactive**: `retrieval path`, `GET /memories/search`

### `memory_versions`

- **class**: `MEMORY_LINEAGE_CHILD`
- **tranche**: `TRANCHE_4_LINEAGE_AND_ARCHIVAL`
- **rationale**: `memory_id` is a NOT NULL FK to `memories`; the version's tenant is the memory's tenant by definition.
- **row identity**: `id` (surrogate, emitted as-is)
- **REQUIRED_CURRENT_SCHEMA_CONTRACT** — *Status: DECLARED; runtime verification is not implemented in this checkpoint.*
  - `memory_versions_pkey` PRIMARY KEY (id), validated
  - `memory_versions_memory_id_fkey` FOREIGN KEY (memory_id) REFERENCES memories(id), validated
- **canonical path**: `row.memory_id -> memories.id -> memories.agent_id -> agents.agent_id -> agents.tenant_id`
- **secondary paths**:
  - `row.agent_id -> agents.agent_id -> agents.id -> agents.tenant_id`
- **consistency**: The denormalised `agent_id` must name the same agent the parent memory does. A version that claims a different agent than its memory is a blocking disagreement.
- **added columns**: `tenant_id UUID`
- **initial nullability**: added NULL-able; NOT NULL only in FINAL_CONSTRAINT_TIGHTENING, after backfill verification reports zero blocking findings
- **backfill**: `UPDATE memory_versions v SET tenant_id = m.tenant_id FROM memories m WHERE m.id = v.memory_id`
- **must be zero**: `LEGACY_UNMAPPED`, `ORPHANED_MEMORY_REFERENCE`, `ORPHANED_AGENT_REFERENCE`, `UNMAPPED_AGENT`, `UNRESOLVABLE_OWNER`, `NULL_OWNERSHIP_LINK`, `CROSS_TENANT_PARENT_CHILD`, `OWNERSHIP_PATH_DISAGREEMENT`
- **pre-validation indexes**: `idx_memory_versions_tenant (tenant_id)`
- **planned composite FKs**: `FOREIGN KEY (memory_id, tenant_id) REFERENCES memories(id, tenant_id)`
- **NOT VALID appropriate**: true
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
- **REQUIRED_CURRENT_SCHEMA_CONTRACT** — *Status: DECLARED; runtime verification is not implemented in this checkpoint.*
  - `retrieval_feedback_pkey` PRIMARY KEY (id), validated
  - `retrieval_feedback_memory_id_fkey` FOREIGN KEY (memory_id) REFERENCES memories(id), validated
- **canonical path**: `row.agent_id -> agents.agent_id -> agents.id -> agents.tenant_id`
- **secondary paths**:
  - `row.memory_id -> memories.id -> memories.agent_id -> agents.agent_id -> agents.tenant_id`
- **consistency**: `memory_id` is `ON DELETE SET NULL`, so it is absent for feedback whose memory has been deleted and cannot be canonical. Where present it must agree with the agent.
- **added columns**: `agent_uuid UUID`, `tenant_id UUID`
- **initial nullability**: added NULL-able; NOT NULL only in FINAL_CONSTRAINT_TIGHTENING, after backfill verification reports zero blocking findings
- **backfill**: `UPDATE retrieval_feedback f SET agent_uuid = a.id, tenant_id = a.tenant_id FROM agents a WHERE a.agent_id = f.agent_id`
- **must be zero**: `LEGACY_UNMAPPED`, `ORPHANED_AGENT_REFERENCE`, `UNMAPPED_AGENT`, `UNRESOLVABLE_OWNER`, `NULL_OWNERSHIP_LINK`
- **pre-validation indexes**: `idx_retrieval_feedback_tenant (tenant_id, agent_uuid)`
- **planned composite FKs**: `FOREIGN KEY (tenant_id, agent_uuid) REFERENCES agents(tenant_id, id)`, `FOREIGN KEY (memory_id, tenant_id) REFERENCES memories(id, tenant_id)`
- **NOT VALID appropriate**: true
- **validate step**: VALIDATE both after backfill
- **lock profile**: small; the FK to memories must tolerate a NULL memory_id (MATCH SIMPLE)
- **rollback dependencies**: `memories (tranche 3)`
- **must stay inactive**: `retrieval feedback endpoint`, `rmk_worker`

### `rmk_episodes`

- **class**: `DIRECT_AGENT_CHILD`
- **tranche**: `TRANCHE_5_TENANT_GLOBAL_OPERATIONS`
- **rationale**: Reinforcement episodes recorded per agent; the policy is a secondary link that may be NULL after a policy is deleted.
- **row identity**: `id` (surrogate, emitted as-is)
- **REQUIRED_CURRENT_SCHEMA_CONTRACT** — *Status: DECLARED; runtime verification is not implemented in this checkpoint.*
  - `rmk_episodes_pkey` PRIMARY KEY (id), validated
  - `rmk_episodes_policy_id_fkey` FOREIGN KEY (policy_id) REFERENCES rmk_policies(id), validated
- **session role**: `CONTEXT_ONLY` — Read by `WHERE agent_id = $1 ORDER BY session_id`: the session is a grouping key *within* an agent's episodes, not an addressing key. The owner is the agent.
- **canonical path**: `row.agent_id -> agents.agent_id -> agents.id -> agents.tenant_id`
- **secondary paths**:
  - `row.policy_id -> rmk_policies.id -> rmk_policies.agent_id -> tenant`
- **consistency**: An episode's policy, where set, must belong to the same agent. `policy_id` is `ON DELETE SET NULL`, so it cannot be canonical.
- **added columns**: `agent_uuid UUID`, `tenant_id UUID`
- **initial nullability**: added NULL-able; NOT NULL only in FINAL_CONSTRAINT_TIGHTENING, after backfill verification reports zero blocking findings
- **backfill**: `UPDATE rmk_episodes e SET agent_uuid = a.id, tenant_id = a.tenant_id FROM agents a WHERE a.agent_id = e.agent_id`
- **must be zero**: `LEGACY_UNMAPPED`, `ORPHANED_AGENT_REFERENCE`, `UNMAPPED_AGENT`, `UNRESOLVABLE_OWNER`, `NULL_OWNERSHIP_LINK`
- **pre-validation indexes**: `idx_rmk_episodes_tenant (tenant_id, agent_uuid)`
- **planned composite FKs**: `FOREIGN KEY (tenant_id, agent_uuid) REFERENCES agents(tenant_id, id)`, `FOREIGN KEY (policy_id, tenant_id) REFERENCES rmk_policies(id, tenant_id)`
- **NOT VALID appropriate**: true
- **validate step**: VALIDATE both after backfill
- **lock profile**: append-heavy training log
- **rollback dependencies**: `rmk_policies (tranche 1)`, `sessions (tranche 2)`
- **must stay inactive**: `rmk_worker`, `extraction_worker`, `hnsw_maintenance`

### `rmk_policies`

- **class**: `DIRECT_AGENT_CHILD`
- **tranche**: `TRANCHE_1_ROOTS_AND_DIRECT_AGENT_CHILDREN`
- **rationale**: One policy set per agent; must precede `rmk_episodes`, which references it.
- **row identity**: `id` (surrogate, emitted as-is)
- **REQUIRED_CURRENT_SCHEMA_CONTRACT** — *Status: DECLARED; runtime verification is not implemented in this checkpoint.*
  - `rmk_policies_pkey` PRIMARY KEY (id), validated
- **canonical path**: `row.agent_id -> agents.agent_id -> agents.id -> agents.tenant_id`
- **added columns**: `agent_uuid UUID`, `tenant_id UUID`
- **initial nullability**: added NULL-able; NOT NULL only in FINAL_CONSTRAINT_TIGHTENING, after backfill verification reports zero blocking findings
- **backfill**: `UPDATE rmk_policies p SET agent_uuid = a.id, tenant_id = a.tenant_id FROM agents a WHERE a.agent_id = p.agent_id`
- **must be zero**: `LEGACY_UNMAPPED`, `ORPHANED_AGENT_REFERENCE`, `UNMAPPED_AGENT`, `UNRESOLVABLE_OWNER`, `NULL_OWNERSHIP_LINK`
- **pre-validation indexes**: `idx_rmk_policies_tenant (tenant_id, agent_uuid)`, `rmk_policies_id_tenant_id_key UNIQUE (id, tenant_id) — FK target for rmk_episodes`
- **planned composite FKs**: `FOREIGN KEY (tenant_id, agent_uuid) REFERENCES agents(tenant_id, id)`
- **NOT VALID appropriate**: true
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
- **REQUIRED_CURRENT_SCHEMA_CONTRACT** — *Status: DECLARED; runtime verification is not implemented in this checkpoint.*
  - `sessions_pkey` PRIMARY KEY (id), validated
  - `idx_sessions_agent_session` INDEX (agent_id, session_id) - unique, valid, ready, non-partial, no expressions
  - `sessions_agent_id_fkey` FOREIGN KEY (agent_id) REFERENCES agents(agent_id), validated
- **canonical path**: `row.agent_id -> agents.agent_id -> agents.id -> agents.tenant_id`
- **consistency**: A session's owner is its agent. This table is a direct agent child, not a session child — it is the parent every session child resolves through.
- **added columns**: `agent_uuid UUID`, `tenant_id UUID`
- **initial nullability**: added NULL-able; NOT NULL only in FINAL_CONSTRAINT_TIGHTENING, after backfill verification reports zero blocking findings
- **backfill**: `UPDATE sessions s SET agent_uuid = a.id, tenant_id = a.tenant_id FROM agents a WHERE a.agent_id = s.agent_id`
- **must be zero**: `LEGACY_UNMAPPED`, `ORPHANED_AGENT_REFERENCE`, `UNMAPPED_AGENT`, `UNRESOLVABLE_OWNER`, `NULL_OWNERSHIP_LINK`
- **pre-validation indexes**: `idx_sessions_tenant (tenant_id, agent_uuid)`, `sessions_tenant_session_key — required *before* the uniqueness change below`
- **planned composite FKs**: `FOREIGN KEY (tenant_id, agent_uuid) REFERENCES agents(tenant_id, id)`
- **NOT VALID appropriate**: true
- **validate step**: VALIDATE CONSTRAINT after backfill
- **future uniqueness**: `idx_sessions_agent_session` is UNIQUE (agent_id, session_id). If Step 4B widens session identity to the tenant — UNIQUE (tenant_id, session_id) — then two agents in the same tenant that happen to share a `session_id` collide. That is a real possibility because `session_id` is caller-supplied TEXT, so it is pre-checked as FUTURE_TENANT_UNIQUENESS_COLLISION rather than discovered when the index build fails.
- **lock profile**: ADD COLUMN NULL is metadata-only; a new UNIQUE index should be built CONCURRENTLY outside the migration transaction
- **rollback dependencies**: `working_memory`, `memories`, `memory_retrieval_logs`, `extraction_jobs`, `cognitive_hypervisor_timeline`, `rmk_episodes`
- **must stay inactive**: `POST /sessions`, `every session-scoped write`

### `working_memory`

- **class**: `SESSION_CHILD`
- **tranche**: `TRANCHE_2_SESSIONS`
- **rationale**: The one genuine session child: `session_id` is NOT NULL and the row is meaningless without its session.
- **row identity**: `id` (surrogate, emitted as-is)
- **REQUIRED_CURRENT_SCHEMA_CONTRACT** — *Status: DECLARED; runtime verification is not implemented in this checkpoint.*
  - `working_memory_pkey` PRIMARY KEY (id), validated
  - `working_memory_agent_id_session_id_key` UNIQUE (agent_id, session_id), validated
- **session role**: `CANONICAL` — INSERT carries session_id; rows are deleted by `WHERE agent_id = $1 AND session_id = $2`, i.e. addressed *by* their session; UNIQUE (agent_id, session_id) is exactly the session's own unique key. The session is what identifies the row, so a missing one is a broken row rather than absent context.
- **canonical path**: `(row.agent_id, row.session_id) -> sessions(agent_id, session_id) -> agents.agent_id -> agents.id -> agents.tenant_id`
- **secondary paths**:
  - `row.agent_id -> agents.agent_id -> agents.id -> agents.tenant_id`
- **consistency**: The only table whose session reference is NOT NULL, and whose own unique key `(agent_id, session_id)` is exactly the session's unique key. The denormalised agent must match the session's agent. OPERATIONAL CAVEAT: `working_memory` and its `sessions` row are written by two consecutive statements, not one atomic unit - the upsert writes working memory first and the session second. A live audit can therefore observe the brief state between them and report a session orphan that is about to exist. The finding is deliberately left BLOCKING, because a persistent orphan is a real inconsistency and weakening it would hide that, so a migration-readiness scan must be run with extraction writers quiesced or drained, or re-run once activity settles.
- **added columns**: `agent_uuid UUID`, `tenant_id UUID`
- **initial nullability**: added NULL-able; NOT NULL only in FINAL_CONSTRAINT_TIGHTENING, after backfill verification reports zero blocking findings
- **backfill**: `UPDATE working_memory w SET agent_uuid = s.agent_uuid, tenant_id = s.tenant_id FROM sessions s WHERE s.agent_id = w.agent_id AND s.session_id = w.session_id`
- **must be zero**: `LEGACY_UNMAPPED`, `ORPHANED_AGENT_REFERENCE`, `ORPHANED_SESSION_REFERENCE`, `UNMAPPED_AGENT`, `UNRESOLVABLE_OWNER`, `NULL_OWNERSHIP_LINK`, `OWNERSHIP_PATH_DISAGREEMENT`
- **pre-validation indexes**: `idx_working_memory_tenant (tenant_id, agent_uuid)`
- **planned composite FKs**: `FOREIGN KEY (tenant_id, agent_uuid) REFERENCES agents(tenant_id, id)`
- **NOT VALID appropriate**: true
- **validate step**: VALIDATE CONSTRAINT after backfill
- **future uniqueness**: `working_memory_agent_id_session_id_key` follows whatever `sessions` does; if session identity becomes tenant-scoped this must move with it or the two uniqueness rules disagree.
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
