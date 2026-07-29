-- ============================================================================
-- 0041  Tenancy tranche 1 constraint attachment  (AEON plan §4, decision A-6)
-- ============================================================================
--
-- Step 4B-1 PREPARE, part three and last.  Adopts the unique indexes 0033 built
-- and declares the composite foreign keys TRANCHE_1_ROOTS_AND_DIRECT_AGENT
-- _CHILDREN names in src/tenancy/plan.rs.
--
-- TRANSACTIONAL, deliberately.  This file carries no `-- no-transaction`
-- marker: measured on pgvector/pgvector:pg16, `ADD CONSTRAINT ... USING INDEX`
-- is indifferent to transaction context and succeeds inside an explicit
-- BEGIN/COMMIT.  The "cannot run inside a transaction block" restriction
-- belongs to CREATE INDEX CONCURRENTLY alone, which is why the concurrent
-- builds live in 0033-0040 and the attachments live here.  Keeping them separate is
-- what lets every statement in this file be atomic.
--
-- ADDITIVE AND REVERSIBLE.  `rollback/0041_tenancy_tranche1_constraints_down.sql`
-- drops exactly what this creates.
--
-- LOCKS.  Two distinct profiles, and conflating them is the mistake this split
-- exists to avoid:
--   * ADD CONSTRAINT ... USING INDEX — ACCESS EXCLUSIVE on the table alone, and
--     brief.  It adopts an index that already exists rather than building one,
--     so it examines no rows and names no parent.  Measured: it performs no
--     scan for a UNIQUE over NULL-able columns, and both columns remain
--     attnotnull = f afterwards.  A scan would only arise from NOT NULL
--     marking, which PRIMARY KEY does and UNIQUE does not.
--   * ADD CONSTRAINT ... FOREIGN KEY ... NOT VALID — SHARE ROW EXCLUSIVE on
--     this table AND on agents.  It is brief because NOT VALID skips the
--     historical scan; the scan is FINALIZE's job, under the weaker SHARE
--     UPDATE EXCLUSIVE.
--
-- NOT VALID IS NOT "NOT ENFORCED".  It means existing rows are not scanned.
-- Every INSERT and UPDATE after this migration commits is checked in full.
-- Tranche 1's foreign keys all point at agents, and every writer already
-- supplies a resolvable agent_id, so nothing new can fail here — but this is
-- the reason tranche 2 cannot be scheduled until upsert_working_memory writes
-- its parent first.
--
-- MATCH SIMPLE.  All five keys are composite over NULL-able columns, so
-- PostgreSQL skips the check entirely for any row with a NULL component.  The
-- foreign key is therefore NOT evidence that historical rows are owned during
-- the transition — the audit is.  That is not a defect being tolerated; it is
-- why PREPARE, BACKFILL and FINALIZE are three stages instead of one.
--
-- Idempotency: PostgreSQL has no ADD CONSTRAINT IF NOT EXISTS, so each is
-- wrapped in a pg_constraint existence guard, matching 0028's pattern.
-- ============================================================================

-- ── Refuse to adopt a broken concurrent build ───────────────────────────────
-- A CREATE INDEX CONCURRENTLY that fails leaves the index behind marked
-- INVALID.  It is not usable by the planner and, for a unique index, it
-- enforces nothing — but it exists, so the builds' IF NOT EXISTS skip it and
-- they would otherwise report success over a broken object.
--
-- The guard lives here rather than with the builds because this is the file
-- that ADOPTS them: an INVALID unique index must never become a constraint.
-- Recovery is NOT "drop the index and re-run its build migration". By the time
-- this guard fires, sqlx has typically recorded versions 0033-0040 in
-- `_sqlx_migrations` -- though not necessarily all of them, since a crash can
-- leave a built index with no ledger row. This guard reads
-- `pg_index.indisvalid` directly and is ledger-state-agnostic, so it fires
-- either way. What the ledger decides is whether re-running helps: a failed
-- CONCURRENTLY build leaves the index behind INVALID, and `sqlx migrate run`
-- re-executes any file it has NOT recorded, whose
-- `IF NOT EXISTS` skips the broken index with a notice and lets the migration
-- succeed. The version is then recorded over an index that enforces nothing,
-- and sqlx will never run that file again.
--
-- The authoritative path is to run rollback/0033_tenancy_tranche1_indexes_down
-- .sql, which drops all eight indexes AND deletes ledger versions 33-40, then
-- `sqlx migrate run` to rebuild them. Measured end to end on pg16: guard fires
-- -> rollback leaves 0 indexes and 0 ledger rows for 33-40 -> rerun rebuilds
-- all eight valid -> this migration applies clean.
--
-- The `IF NOT EXISTS` on those builds is deliberate and stays. Removing it
-- would make a failed build louder but would strand the *other* failure mode:
-- measured, a crash after a successful build but before the ledger write leaves
-- a valid index, and re-running without `IF NOT EXISTS` fails with `relation
-- already exists` and no forward path, where `IF NOT EXISTS` yields a notice
-- and completes. This guard is what keeps the permissive build honest.
--
-- The Step 4A verifier reports the same condition as drift, independently.
DO $$
DECLARE
    broken TEXT;
BEGIN
    SELECT string_agg(c.relname, ', ' ORDER BY c.relname)
      INTO broken
      FROM pg_catalog.pg_index i
      JOIN pg_catalog.pg_class c ON c.oid = i.indexrelid
      JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
     WHERE n.nspname = 'public'
       AND NOT i.indisvalid
       AND c.relname IN (
           'idx_archival_batches_tenant',
           'idx_audit_logs_tenant',
           'idx_entities_tenant',
           'idx_memory_graph_tenant',
           'idx_rmk_policies_tenant',
           'archival_batches_id_tenant_id_key',
           'entities_id_tenant_id_key',
           'rmk_policies_id_tenant_id_key'
       );

    IF broken IS NOT NULL THEN
        RAISE EXCEPTION
            'tranche 1 concurrent index build left INVALID indexes: %. '
            'Re-running the build migration will NOT fix this: sqlx may already '
            'have recorded some of versions 33-40, and those files skip an '
            'existing index. '
            'Run rollback/0033_tenancy_tranche1_indexes_down.sql (it drops all '
            'eight indexes and deletes ledger versions 33-40), then re-run '
            'sqlx migrate. An INVALID unique index enforces nothing and must '
            'not be adopted as a constraint below.',
            broken
            USING ERRCODE = 'object_not_in_prerequisite_state';
    END IF;
END $$;

-- ── Adopt the concurrent unique builds as constraints ───────────────────────
-- The constraint takes the index's name, so these names match the build
-- migrations' exactly.
-- Guarded on pg_constraint rather than pg_class: after adoption the index still
-- exists under the same name, so an index-existence check would be satisfied by
-- 0033 alone and would skip the adoption entirely.

DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM pg_catalog.pg_constraint
        WHERE conname = 'archival_batches_id_tenant_id_key'
          AND conrelid = 'public.archival_batches'::regclass
    ) THEN
        ALTER TABLE archival_batches
            ADD CONSTRAINT archival_batches_id_tenant_id_key
            UNIQUE USING INDEX archival_batches_id_tenant_id_key;
    END IF;

    IF NOT EXISTS (
        SELECT 1 FROM pg_catalog.pg_constraint
        WHERE conname = 'entities_id_tenant_id_key'
          AND conrelid = 'public.entities'::regclass
    ) THEN
        ALTER TABLE entities
            ADD CONSTRAINT entities_id_tenant_id_key
            UNIQUE USING INDEX entities_id_tenant_id_key;
    END IF;

    IF NOT EXISTS (
        SELECT 1 FROM pg_catalog.pg_constraint
        WHERE conname = 'rmk_policies_id_tenant_id_key'
          AND conrelid = 'public.rmk_policies'::regclass
    ) THEN
        ALTER TABLE rmk_policies
            ADD CONSTRAINT rmk_policies_id_tenant_id_key
            UNIQUE USING INDEX rmk_policies_id_tenant_id_key;
    END IF;
END $$;

-- ── Composite ownership foreign keys ────────────────────────────────────────
-- FOREIGN KEY (tenant_id, agent_uuid) REFERENCES agents (tenant_id, id).
--
-- This is decision A-6 made structural: a row whose tenant_id does not match
-- its agent's tenant becomes unrepresentable, rather than relying on every
-- future write path remembering to keep the two aligned.  agents
-- (tenant_id, id) is the already-current unique target created by 0028; this
-- migration does not recreate it.

DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM pg_catalog.pg_constraint
        WHERE conname = 'archival_batches_tenant_agent_fkey'
          AND conrelid = 'public.archival_batches'::regclass
    ) THEN
        ALTER TABLE archival_batches
            ADD CONSTRAINT archival_batches_tenant_agent_fkey
            FOREIGN KEY (tenant_id, agent_uuid)
            REFERENCES agents (tenant_id, id) NOT VALID;
    END IF;

    IF NOT EXISTS (
        SELECT 1 FROM pg_catalog.pg_constraint
        WHERE conname = 'audit_logs_tenant_agent_fkey'
          AND conrelid = 'public.audit_logs'::regclass
    ) THEN
        ALTER TABLE audit_logs
            ADD CONSTRAINT audit_logs_tenant_agent_fkey
            FOREIGN KEY (tenant_id, agent_uuid)
            REFERENCES agents (tenant_id, id) NOT VALID;
    END IF;

    IF NOT EXISTS (
        SELECT 1 FROM pg_catalog.pg_constraint
        WHERE conname = 'entities_tenant_agent_fkey'
          AND conrelid = 'public.entities'::regclass
    ) THEN
        ALTER TABLE entities
            ADD CONSTRAINT entities_tenant_agent_fkey
            FOREIGN KEY (tenant_id, agent_uuid)
            REFERENCES agents (tenant_id, id) NOT VALID;
    END IF;

    IF NOT EXISTS (
        SELECT 1 FROM pg_catalog.pg_constraint
        WHERE conname = 'memory_graph_tenant_agent_fkey'
          AND conrelid = 'public.memory_graph'::regclass
    ) THEN
        ALTER TABLE memory_graph
            ADD CONSTRAINT memory_graph_tenant_agent_fkey
            FOREIGN KEY (tenant_id, agent_uuid)
            REFERENCES agents (tenant_id, id) NOT VALID;
    END IF;

    IF NOT EXISTS (
        SELECT 1 FROM pg_catalog.pg_constraint
        WHERE conname = 'rmk_policies_tenant_agent_fkey'
          AND conrelid = 'public.rmk_policies'::regclass
    ) THEN
        ALTER TABLE rmk_policies
            ADD CONSTRAINT rmk_policies_tenant_agent_fkey
            FOREIGN KEY (tenant_id, agent_uuid)
            REFERENCES agents (tenant_id, id) NOT VALID;
    END IF;
END $$;

COMMENT ON CONSTRAINT audit_logs_tenant_agent_fkey ON audit_logs IS
    'MATCH SIMPLE: rows with a NULL key component are not checked. audit_logs ownership stays '
    'permanently NULL-able because agentless events are legitimate, so this key never becomes '
    'evidence that every row is owned. The audit is that evidence.';
