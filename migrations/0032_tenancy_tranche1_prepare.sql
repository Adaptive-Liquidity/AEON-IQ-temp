-- ============================================================================
-- 0032  Tenancy tranche 1 PREPARE  (AEON auth/tenancy plan §4, decision A-6)
-- ============================================================================
--
-- Step 4B-1 of the plan accepted at
--   Adaptive-Liquidity/nexus-planning @ b1fe06505d400e435c3ef8d10dc197f15641bebd
-- implementing TRANCHE_1_ROOTS_AND_DIRECT_AGENT_CHILDREN from the typed
-- contract in src/tenancy/plan.rs.  Every object below is declared there; this
-- file creates nothing the contract does not name.
--
-- ADDITIVE AND REVERSIBLE.  Nothing here drops, renames or repurposes an
-- existing column, constraint or index, and no row is rewritten.
-- `rollback/0032_tenancy_tranche1_prepare_down.sql` returns the schema to the
-- 0031 baseline without data loss.
--
-- This is the PREPARE stage only.  It is deliberately split from the concurrent
-- index builds (0033-0040, one statement per file) and the constraint
-- attachments (0041), because CREATE INDEX CONCURRENTLY cannot run inside a
-- transaction block — implicit or explicit — while this file must be
-- transactional: the checkpoint table, the ownership columns and the bridge
-- triggers have to land together or not at all.
--
-- What is deliberately NOT here:
--   * NOT NULL on any ownership column — that is FUTURE STEP 7, which cannot
--     begin until plan steps 5 and 6 merge (src/tenancy/plan.rs, Tranche
--     ::FinalConstraintTightening).  The columns are added NULL-able and the
--     audit, not the foreign key, is the ownership gate during the transition.
--   * Any VALIDATE CONSTRAINT — that is FINALIZE, and it is gated on a
--     COMPLETED backfill checkpoint.  See finalize/tranche1_finalize.sql.
--   * Any backfill of existing rows — that is an operational command, not a
--     migration, precisely so it can be stopped and resumed.
--   * Row-level security.  RLS is deferred to architecture v2 (decision A-4)
--     and is not a guarantee this migration provides or relies on.
--
-- Idempotency: every statement is IF NOT EXISTS or CREATE OR REPLACE, so
-- re-running the whole file is a no-op rather than a duplicate-object error.
-- ============================================================================

-- ── The backfill checkpoint protocol table ──────────────────────────────────
-- Typed as `plan::TENANCY_BACKFILL_CHECKPOINTS`.  `agent_tenancy_migrations`
-- cannot serve this purpose: it has no tranche, no digest, no cursor and no
-- status, so a FINALIZE guard reading it would assert against the wrong
-- evidence and pass.
CREATE TABLE IF NOT EXISTS tenancy_backfill_checkpoints (
    id              UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    tranche         TEXT        NOT NULL,
    contract_digest TEXT        NOT NULL,
    status          TEXT        NOT NULL,
    -- NULL once the tranche completes: there is nothing left to resume.
    resume_cursor   TEXT,
    rows_total      BIGINT      NOT NULL DEFAULT 0,
    rows_backfilled BIGINT      NOT NULL DEFAULT 0,
    blocking_count  BIGINT      NOT NULL DEFAULT 0,
    started_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    completed_at    TIMESTAMPTZ,
    CONSTRAINT tenancy_backfill_checkpoints_status_ck
        CHECK (status IN ('IN_PROGRESS', 'COMPLETED', 'ABANDONED')),
    -- `tranche` is backed by the closed `plan::Tranche` enum exactly as `status`
    -- is backed by `CheckpointStatus`, so it gets the same treatment.  Without
    -- this, a one-character typo produces a row no guard will ever match: the
    -- backfill reports success, FINALIZE keeps refusing, and nothing says why.
    -- It fails closed, which is why this is a legibility fix rather than a
    -- soundness one — but an unmatched checkpoint is a bad way to learn that.
    CONSTRAINT tenancy_backfill_checkpoints_tranche_ck
        CHECK (tranche IN (
            'TRANCHE_1_ROOTS_AND_DIRECT_AGENT_CHILDREN',
            'TRANCHE_2_SESSIONS',
            'TRANCHE_3_MEMORIES',
            'TRANCHE_4_LINEAGE_AND_ARCHIVAL',
            'TRANCHE_5_OPERATIONS',
            'FINAL_CONSTRAINT_TIGHTENING'
        )),
    -- A completion is the only state that carries a completion time, and it is
    -- the only state with nothing left to resume.  Stating both structurally
    -- stops a half-written row from reading as authoritative evidence.
    CONSTRAINT tenancy_backfill_checkpoints_completed_shape_ck
        CHECK ((status = 'COMPLETED') = (completed_at IS NOT NULL)),
    CONSTRAINT tenancy_backfill_checkpoints_completed_cursor_ck
        CHECK (status <> 'COMPLETED' OR resume_cursor IS NULL),
    CONSTRAINT tenancy_backfill_checkpoints_counts_ck
        CHECK (rows_total >= 0 AND rows_backfilled >= 0 AND blocking_count >= 0
               AND rows_backfilled <= rows_total),
    -- FINALIZE_PRECONDITION.max_blocking_count = 0 is the gate the whole
    -- three-stage protocol exists to protect.  Enforcing it here as well means
    -- a COMPLETED row cannot exist while the tranche still has blocking
    -- findings, so the guard is not the only thing standing between a dirty
    -- tranche and a validated constraint.
    CONSTRAINT tenancy_backfill_checkpoints_completed_clean_ck
        CHECK (status <> 'COMPLETED' OR blocking_count = 0),
    -- A completion claims the tranche is finished, so its accounting has to say
    -- so too.  Without this a row can read COMPLETED with 3 of 1000 rows done
    -- and satisfy every other constraint.  The backfill command reconciles
    -- rows_total against the final count before it completes.
    CONSTRAINT tenancy_backfill_checkpoints_completed_accounting_ck
        CHECK (status <> 'COMPLETED' OR rows_backfilled = rows_total)
);

-- The partial scope is load-bearing and cannot be left implicit.  A plain
-- UNIQUE (tranche, contract_digest) would reject the second row an operator
-- creates when restarting a tranche against the same digest — but ABANDONED
-- exists precisely so the superseded attempt is retained rather than
-- overwritten.  Only WHERE status = 'COMPLETED' gives "at most one
-- authoritative completion" while leaving the history alone.
CREATE UNIQUE INDEX IF NOT EXISTS tenancy_backfill_checkpoints_completed_key
    ON tenancy_backfill_checkpoints (tranche, contract_digest)
    WHERE status = 'COMPLETED';

CREATE INDEX IF NOT EXISTS idx_tenancy_backfill_checkpoints_tranche
    ON tenancy_backfill_checkpoints (tranche, started_at DESC);

COMMENT ON TABLE tenancy_backfill_checkpoints IS
    'Per-tranche, per-contract backfill evidence (plan §4 step 4B). BACKFILL writes progress '
    'here and FINALIZE refuses to proceed without a COMPLETED row for its own tranche against '
    'the current contract digest with blocking_count = 0. ABANDONED rows are retained as '
    'history and are never treated as completion.';

COMMENT ON COLUMN tenancy_backfill_checkpoints.contract_digest IS
    'report::inventory_digest() at the time the backfill ran. A backfill that ran against a '
    'superseded plan proves nothing about the current one, so FINALIZE compares this exactly.';

COMMENT ON COLUMN tenancy_backfill_checkpoints.blocking_count IS
    'Blocking findings the authoritative audit reported for THIS tranche at completion time. '
    'Per-tranche rather than global: a later tranche is not evidence about this one.';

-- ── Ownership columns ───────────────────────────────────────────────────────
-- Added NULL-able.  ADD COLUMN with no default is metadata-only on PostgreSQL
-- 11+: catalog update, no table rewrite, no scan.
--
-- archival_batches, entities, memory_graph and rmk_policies are
-- NullableThenTightened — NULL only until FUTURE STEP 7.  audit_logs is
-- RemainsNullable: it records agentless events (startup, configuration change,
-- administrative action) which are legitimate rows with no owning agent, so
-- tightening it would make the schema reject valid audit history.

ALTER TABLE archival_batches
    ADD COLUMN IF NOT EXISTS agent_uuid UUID,
    ADD COLUMN IF NOT EXISTS tenant_id  UUID;

ALTER TABLE audit_logs
    ADD COLUMN IF NOT EXISTS agent_uuid UUID,
    ADD COLUMN IF NOT EXISTS tenant_id  UUID;

ALTER TABLE entities
    ADD COLUMN IF NOT EXISTS agent_uuid UUID,
    ADD COLUMN IF NOT EXISTS tenant_id  UUID;

ALTER TABLE memory_graph
    ADD COLUMN IF NOT EXISTS agent_uuid UUID,
    ADD COLUMN IF NOT EXISTS tenant_id  UUID;

ALTER TABLE rmk_policies
    ADD COLUMN IF NOT EXISTS agent_uuid UUID,
    ADD COLUMN IF NOT EXISTS tenant_id  UUID;

-- ── Transitional write bridges ──────────────────────────────────────────────
-- A database bridge trigger is chosen over exhaustive application dual-write.
-- Dual-write requires enumerating every writer and keeping that enumeration
-- exhaustive; it is defeated by a rollback to the previous release, by a second
-- service, by a maintenance script, and by any psql session.  The trigger is
-- attached to the table, so it holds for every writer including ones nobody
-- enumerated, and it survives an application rollback because it is schema
-- rather than code.  It is installed here, in PREPARE and before any backfill,
-- so no window exists in which new rows are unowned.
--
-- All five are SECURITY INVOKER with a pinned search_path and fully-qualified
-- bodies: a caller with a hostile search_path could otherwise make `agents`
-- resolve to a shadow table and have the bridge resolve ownership from it.
-- Neither the pin nor the qualification is load-bearing alone.

CREATE OR REPLACE FUNCTION public.fn_archival_batches_tenancy_bridge()
RETURNS TRIGGER
LANGUAGE plpgsql
SECURITY INVOKER
SET search_path = pg_catalog, public
AS $$
DECLARE
    resolved_uuid   UUID;
    resolved_tenant UUID;
BEGIN
    SELECT a.id, a.tenant_id
      INTO resolved_uuid, resolved_tenant
      FROM public.agents a
     WHERE a.agent_id = NEW.agent_id;

    IF NEW.agent_uuid IS NOT NULL AND resolved_uuid IS NOT NULL
       AND NEW.agent_uuid <> resolved_uuid THEN
        RAISE EXCEPTION
            'archival_batches row for agent % supplies an agent_uuid that disagrees with agents',
            NEW.agent_id
            USING ERRCODE = 'check_violation';
    END IF;

    IF NEW.tenant_id IS NOT NULL AND resolved_tenant IS NOT NULL
       AND NEW.tenant_id <> resolved_tenant THEN
        RAISE EXCEPTION
            'archival_batches row for agent % supplies a tenant_id that disagrees with agents',
            NEW.agent_id
            USING ERRCODE = 'check_violation';
    END IF;

    -- `agents` is the sole authority, so assign unconditionally rather than
    -- only filling NULLs.
    --
    -- Filling only NULLs left a cross-tenant injection path, measured on pg16:
    -- name an agent that does not exist and supply any tenant_id, and the row
    -- was accepted carrying that tenant. Neither guard above can fire in that
    -- case — both contradiction checks require the RESOLVED side to be
    -- non-NULL, and the composite foreign key is skipped entirely because
    -- MATCH SIMPLE ignores a key with a NULL component. Only archival_batches
    -- was protected, and only because it happens to carry an agent_id foreign
    -- key from migration 0006; entities, memory_graph, rmk_policies and
    -- audit_logs carry none.
    --
    -- Unconditional assignment closes it: when the agent does not resolve,
    -- resolved_* are NULL and any caller-supplied ownership is overwritten with
    -- NULL, which is exactly the PRESERVED_UNRESOLVED outcome the contract
    -- names and which the audit then reports. When the agent does resolve, the
    -- checks above have already refused any value that disagrees, so this
    -- overwrite is a no-op for honest writers.
    NEW.agent_uuid := resolved_uuid;
    NEW.tenant_id  := resolved_tenant;

    RETURN NEW;
END;
$$;

CREATE OR REPLACE TRIGGER trg_archival_batches_tenancy_bridge
    BEFORE INSERT OR UPDATE ON archival_batches
    FOR EACH ROW EXECUTE FUNCTION public.fn_archival_batches_tenancy_bridge();

CREATE OR REPLACE FUNCTION public.fn_entities_tenancy_bridge()
RETURNS TRIGGER
LANGUAGE plpgsql
SECURITY INVOKER
SET search_path = pg_catalog, public
AS $$
DECLARE
    resolved_uuid   UUID;
    resolved_tenant UUID;
BEGIN
    SELECT a.id, a.tenant_id
      INTO resolved_uuid, resolved_tenant
      FROM public.agents a
     WHERE a.agent_id = NEW.agent_id;

    IF NEW.agent_uuid IS NOT NULL AND resolved_uuid IS NOT NULL
       AND NEW.agent_uuid <> resolved_uuid THEN
        RAISE EXCEPTION
            'entities row for agent % supplies an agent_uuid that disagrees with agents',
            NEW.agent_id
            USING ERRCODE = 'check_violation';
    END IF;

    IF NEW.tenant_id IS NOT NULL AND resolved_tenant IS NOT NULL
       AND NEW.tenant_id <> resolved_tenant THEN
        RAISE EXCEPTION
            'entities row for agent % supplies a tenant_id that disagrees with agents',
            NEW.agent_id
            USING ERRCODE = 'check_violation';
    END IF;

    -- `agents` is the sole authority, so assign unconditionally rather than
    -- only filling NULLs.
    --
    -- Filling only NULLs left a cross-tenant injection path, measured on pg16:
    -- name an agent that does not exist and supply any tenant_id, and the row
    -- was accepted carrying that tenant. Neither guard above can fire in that
    -- case — both contradiction checks require the RESOLVED side to be
    -- non-NULL, and the composite foreign key is skipped entirely because
    -- MATCH SIMPLE ignores a key with a NULL component. Only archival_batches
    -- was protected, and only because it happens to carry an agent_id foreign
    -- key from migration 0006; entities, memory_graph, rmk_policies and
    -- audit_logs carry none.
    --
    -- Unconditional assignment closes it: when the agent does not resolve,
    -- resolved_* are NULL and any caller-supplied ownership is overwritten with
    -- NULL, which is exactly the PRESERVED_UNRESOLVED outcome the contract
    -- names and which the audit then reports. When the agent does resolve, the
    -- checks above have already refused any value that disagrees, so this
    -- overwrite is a no-op for honest writers.
    NEW.agent_uuid := resolved_uuid;
    NEW.tenant_id  := resolved_tenant;

    RETURN NEW;
END;
$$;

CREATE OR REPLACE TRIGGER trg_entities_tenancy_bridge
    BEFORE INSERT OR UPDATE ON entities
    FOR EACH ROW EXECUTE FUNCTION public.fn_entities_tenancy_bridge();

CREATE OR REPLACE FUNCTION public.fn_memory_graph_tenancy_bridge()
RETURNS TRIGGER
LANGUAGE plpgsql
SECURITY INVOKER
SET search_path = pg_catalog, public
AS $$
DECLARE
    resolved_uuid   UUID;
    resolved_tenant UUID;
BEGIN
    SELECT a.id, a.tenant_id
      INTO resolved_uuid, resolved_tenant
      FROM public.agents a
     WHERE a.agent_id = NEW.agent_id;

    IF NEW.agent_uuid IS NOT NULL AND resolved_uuid IS NOT NULL
       AND NEW.agent_uuid <> resolved_uuid THEN
        RAISE EXCEPTION
            'memory_graph row for agent % supplies an agent_uuid that disagrees with agents',
            NEW.agent_id
            USING ERRCODE = 'check_violation';
    END IF;

    IF NEW.tenant_id IS NOT NULL AND resolved_tenant IS NOT NULL
       AND NEW.tenant_id <> resolved_tenant THEN
        RAISE EXCEPTION
            'memory_graph row for agent % supplies a tenant_id that disagrees with agents',
            NEW.agent_id
            USING ERRCODE = 'check_violation';
    END IF;

    -- `agents` is the sole authority, so assign unconditionally rather than
    -- only filling NULLs.
    --
    -- Filling only NULLs left a cross-tenant injection path, measured on pg16:
    -- name an agent that does not exist and supply any tenant_id, and the row
    -- was accepted carrying that tenant. Neither guard above can fire in that
    -- case — both contradiction checks require the RESOLVED side to be
    -- non-NULL, and the composite foreign key is skipped entirely because
    -- MATCH SIMPLE ignores a key with a NULL component. Only archival_batches
    -- was protected, and only because it happens to carry an agent_id foreign
    -- key from migration 0006; entities, memory_graph, rmk_policies and
    -- audit_logs carry none.
    --
    -- Unconditional assignment closes it: when the agent does not resolve,
    -- resolved_* are NULL and any caller-supplied ownership is overwritten with
    -- NULL, which is exactly the PRESERVED_UNRESOLVED outcome the contract
    -- names and which the audit then reports. When the agent does resolve, the
    -- checks above have already refused any value that disagrees, so this
    -- overwrite is a no-op for honest writers.
    NEW.agent_uuid := resolved_uuid;
    NEW.tenant_id  := resolved_tenant;

    RETURN NEW;
END;
$$;

CREATE OR REPLACE TRIGGER trg_memory_graph_tenancy_bridge
    BEFORE INSERT OR UPDATE ON memory_graph
    FOR EACH ROW EXECUTE FUNCTION public.fn_memory_graph_tenancy_bridge();

CREATE OR REPLACE FUNCTION public.fn_rmk_policies_tenancy_bridge()
RETURNS TRIGGER
LANGUAGE plpgsql
SECURITY INVOKER
SET search_path = pg_catalog, public
AS $$
DECLARE
    resolved_uuid   UUID;
    resolved_tenant UUID;
BEGIN
    SELECT a.id, a.tenant_id
      INTO resolved_uuid, resolved_tenant
      FROM public.agents a
     WHERE a.agent_id = NEW.agent_id;

    IF NEW.agent_uuid IS NOT NULL AND resolved_uuid IS NOT NULL
       AND NEW.agent_uuid <> resolved_uuid THEN
        RAISE EXCEPTION
            'rmk_policies row for agent % supplies an agent_uuid that disagrees with agents',
            NEW.agent_id
            USING ERRCODE = 'check_violation';
    END IF;

    IF NEW.tenant_id IS NOT NULL AND resolved_tenant IS NOT NULL
       AND NEW.tenant_id <> resolved_tenant THEN
        RAISE EXCEPTION
            'rmk_policies row for agent % supplies a tenant_id that disagrees with agents',
            NEW.agent_id
            USING ERRCODE = 'check_violation';
    END IF;

    -- `agents` is the sole authority, so assign unconditionally rather than
    -- only filling NULLs.
    --
    -- Filling only NULLs left a cross-tenant injection path, measured on pg16:
    -- name an agent that does not exist and supply any tenant_id, and the row
    -- was accepted carrying that tenant. Neither guard above can fire in that
    -- case — both contradiction checks require the RESOLVED side to be
    -- non-NULL, and the composite foreign key is skipped entirely because
    -- MATCH SIMPLE ignores a key with a NULL component. Only archival_batches
    -- was protected, and only because it happens to carry an agent_id foreign
    -- key from migration 0006; entities, memory_graph, rmk_policies and
    -- audit_logs carry none.
    --
    -- Unconditional assignment closes it: when the agent does not resolve,
    -- resolved_* are NULL and any caller-supplied ownership is overwritten with
    -- NULL, which is exactly the PRESERVED_UNRESOLVED outcome the contract
    -- names and which the audit then reports. When the agent does resolve, the
    -- checks above have already refused any value that disagrees, so this
    -- overwrite is a no-op for honest writers.
    NEW.agent_uuid := resolved_uuid;
    NEW.tenant_id  := resolved_tenant;

    RETURN NEW;
END;
$$;

CREATE OR REPLACE TRIGGER trg_rmk_policies_tenancy_bridge
    BEFORE INSERT OR UPDATE ON rmk_policies
    FOR EACH ROW EXECUTE FUNCTION public.fn_rmk_policies_tenancy_bridge();

-- ── The conditional bridge: audit_logs ──────────────────────────────────────
-- audit_logs.agent_id is NULL-able and legitimately so.  Declining to resolve
-- ownership at all would also decline it for the rows that DO name a resolvable
-- agent, which would then stay NULL for no reason and be indistinguishable from
-- genuinely agentless ones.  The bridge is therefore conditional rather than
-- absent, and it decides all four states named by
-- `plan::ConditionalOwnership::ALL`:
--
--   AGENTLESS_ALLOWED      agent_id IS NULL.  Ownership stays NULL and nothing
--                          is reported, because nothing is wrong.  Ownership
--                          arriving on an agentless row is dropped rather than
--                          trusted: `agents` says nothing about a row that
--                          names no agent, so accepting it would let a caller
--                          write history under any tenant it liked.
--   RESOLVED_AND_VERIFIED  agent_id resolves.  Both columns are populated from
--                          agents and any supplied values are verified.
--   PRESERVED_UNRESOLVED   agent_id names an agent that does not resolve, or an
--                          agent whose own tenant_id is NULL.  The row is
--                          written with NULL ownership rather than rejected, so
--                          the audit reports ORPHANED_AGENT_REFERENCE or
--                          UNMAPPED_AGENT against it.  Losing audit history is
--                          a worse outcome than an unowned audit row, and a
--                          silently dropped event is worse than both.
--   CONTRADICTION_REJECTED The row supplies ownership contradicting agents.
--                          Refused: a contradicted audit row is not evidence of
--                          anything.
CREATE OR REPLACE FUNCTION public.fn_audit_logs_tenancy_bridge()
RETURNS TRIGGER
LANGUAGE plpgsql
SECURITY INVOKER
SET search_path = pg_catalog, public
AS $$
DECLARE
    resolved_uuid   UUID;
    resolved_tenant UUID;
BEGIN
    -- AGENTLESS_ALLOWED.
    IF NEW.agent_id IS NULL THEN
        NEW.agent_uuid := NULL;
        NEW.tenant_id  := NULL;
        RETURN NEW;
    END IF;

    SELECT a.id, a.tenant_id
      INTO resolved_uuid, resolved_tenant
      FROM public.agents a
     WHERE a.agent_id = NEW.agent_id;

    -- CONTRADICTION_REJECTED.
    IF NEW.agent_uuid IS NOT NULL AND resolved_uuid IS NOT NULL
       AND NEW.agent_uuid <> resolved_uuid THEN
        RAISE EXCEPTION
            'audit_logs row for agent % supplies an agent_uuid that disagrees with agents',
            NEW.agent_id
            USING ERRCODE = 'check_violation';
    END IF;

    IF NEW.tenant_id IS NOT NULL AND resolved_tenant IS NOT NULL
       AND NEW.tenant_id <> resolved_tenant THEN
        RAISE EXCEPTION
            'audit_logs row for agent % supplies a tenant_id that disagrees with agents',
            NEW.agent_id
            USING ERRCODE = 'check_violation';
    END IF;

    -- RESOLVED_AND_VERIFIED when the agent resolves; PRESERVED_UNRESOLVED when
    -- it does not, in which case both assignments below write NULL and the
    -- audit is left to report the row.
    -- `agents` is the sole authority, so assign unconditionally rather than
    -- only filling NULLs.
    --
    -- Filling only NULLs left a cross-tenant injection path, measured on pg16:
    -- name an agent that does not exist and supply any tenant_id, and the row
    -- was accepted carrying that tenant. Neither guard above can fire in that
    -- case — both contradiction checks require the RESOLVED side to be
    -- non-NULL, and the composite foreign key is skipped entirely because
    -- MATCH SIMPLE ignores a key with a NULL component. Only archival_batches
    -- was protected, and only because it happens to carry an agent_id foreign
    -- key from migration 0006; entities, memory_graph, rmk_policies and
    -- audit_logs carry none.
    --
    -- Unconditional assignment closes it: when the agent does not resolve,
    -- resolved_* are NULL and any caller-supplied ownership is overwritten with
    -- NULL, which is exactly the PRESERVED_UNRESOLVED outcome the contract
    -- names and which the audit then reports. When the agent does resolve, the
    -- checks above have already refused any value that disagrees, so this
    -- overwrite is a no-op for honest writers.
    NEW.agent_uuid := resolved_uuid;
    NEW.tenant_id  := resolved_tenant;

    RETURN NEW;
END;
$$;

CREATE OR REPLACE TRIGGER trg_audit_logs_tenancy_bridge
    BEFORE INSERT OR UPDATE ON audit_logs
    FOR EACH ROW EXECUTE FUNCTION public.fn_audit_logs_tenancy_bridge();
