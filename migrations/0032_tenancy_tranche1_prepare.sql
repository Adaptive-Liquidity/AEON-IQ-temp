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
--
-- IDEMPOTENCY IS NOT OWNERSHIP, and that gap is what the guard below closes.
-- `IF NOT EXISTS` and `CREATE OR REPLACE` are silent about *whose* object they
-- found, and this migration's rollback later destroys all twenty unconditionally:
--
--   * `ADD COLUMN IF NOT EXISTS agent_uuid UUID` skips a pre-existing column of
--     that name with a notice.  Version 32 is then recorded, and
--     `rollback/0032` drops the column -- and its data -- as though the tranche
--     had created it.
--   * `CREATE OR REPLACE FUNCTION public.fn_<t>_tenancy_bridge()` replaces a
--     pre-existing zero-argument `trigger`-returning function of that name while
--     PRESERVING ITS OID, so every trigger already calling it silently starts
--     running this file's body instead.  Rollback then drops it outright.
--   * `CREATE OR REPLACE TRIGGER trg_<t>_tenancy_bridge ... ON <t>` replaces a
--     pre-existing trigger of that name on the same table (PostgreSQL 14+), so an
--     unrelated trigger is repointed at this file's bridge and then dropped by
--     rollback.  Same defect as the two above, one object kind further along; it
--     is fixed here rather than left for a later round.
--
-- So every object this file creates is stamped with an ownership marker in
-- `pg_description`, and the guard below refuses the WHOLE MIGRATION if any of the
-- twenty names is already occupied by something not carrying it.  Refusing is the
-- only defensible answer: adopting records version 32 over objects the tranche
-- does not own, and replacing destroys them.
--
-- The refusal is atomic in both directions that matter.  It runs before the first
-- mutation, so nothing is half-applied; and this file carries no
-- `-- no-transaction` marker, so sqlx wraps it -- including its own
-- `_sqlx_migrations` insert -- in one transaction that the RAISE rolls back.  A
-- refused migration therefore leaves neither schema change nor ledger drift.  All
-- twenty are checked in one pass and reported together, so an operator sees the
-- full collision rather than fixing them one RAISE at a time.
--
-- A marker is proof that THIS MIGRATION created the object, not proof against a
-- privileged adversary: anyone who can `COMMENT ON` these objects can forge it,
-- and anyone in that position can already do worse.  The threat it addresses is
-- the realistic one -- a name this file assumes is free is already taken.
-- ============================================================================

-- Never trust the caller's session search_path, matching 0041 and the three
-- rollback scripts.
--
-- This is a prerequisite for the guard below rather than general hygiene. The
-- guard resolves each table through `to_regclass`, which -- unlike a
-- `'public.agents'::regclass` cast -- is an ordinary, overridable catalog
-- function: 0041's header records that a `hostile.to_regclass(text)` ahead of
-- `public` returned an attacker-chosen decoy while the cast still resolved
-- correctly. A guard that can be pointed at a different table than the DDL
-- mutates is not a proof, so the resolver is pinned here and every DDL target
-- below is schema-qualified. Neither is load-bearing alone.
SET LOCAL search_path = pg_catalog, public;

-- ── Ownership proof: refuse to adopt what this migration did not create ──────
DO $$
DECLARE
    col_marker TEXT := 'AEON-IQ tenancy tranche 1 ownership column. Created and owned by '
                       'migration 0032; rollback/0032_tenancy_tranche1_prepare_down.sql drops it '
                       'only while this exact comment is present. '
                       'aeon-iq:tenancy:tranche1:0032:ownership-column';
    fn_marker  TEXT := 'AEON-IQ tenancy tranche 1 bridge function. Created and owned by '
                       'migration 0032; rollback/0032_tenancy_tranche1_prepare_down.sql drops it '
                       'only while this exact comment is present. '
                       'aeon-iq:tenancy:tranche1:0032:bridge-function';
    trg_marker TEXT := 'AEON-IQ tenancy tranche 1 bridge trigger. Created and owned by '
                       'migration 0032; rollback/0032_tenancy_tranche1_prepare_down.sql drops it '
                       'only while this exact comment is present. '
                       'aeon-iq:tenancy:tranche1:0032:bridge-trigger';
    rec        RECORD;
    unowned    TEXT[] := ARRAY[]::TEXT[];
BEGIN
    -- The ten ownership columns.
    --
    -- `attnum > 0 AND NOT attisdropped` excludes system columns and the
    -- placeholder rows a previous DROP COLUMN leaves behind; a dropped column's
    -- name is released, so its corpse must not read as an occupant.
    --
    -- The type is checked only for columns that ARE ours: a marked column whose
    -- type is no longer `uuid` is not the object this file created, and the
    -- backfill and foreign keys below would both be reasoning about the wrong
    -- thing. Nullability is deliberately NOT checked -- FUTURE STEP 7 sets NOT
    -- NULL on four of these five tables, and a re-run after that step must still
    -- recognise the tranche's own column.
    FOR rec IN
        SELECT expected.tbl, expected.col, d.description AS comment,
               format_type(a.atttypid, a.atttypmod) AS actual_type
          FROM (VALUES
                  ('archival_batches', 'agent_uuid'), ('archival_batches', 'tenant_id'),
                  ('audit_logs',       'agent_uuid'), ('audit_logs',       'tenant_id'),
                  ('entities',         'agent_uuid'), ('entities',         'tenant_id'),
                  ('memory_graph',     'agent_uuid'), ('memory_graph',     'tenant_id'),
                  ('rmk_policies',     'agent_uuid'), ('rmk_policies',     'tenant_id')
               ) AS expected(tbl, col)
          JOIN pg_catalog.pg_attribute a
            ON a.attrelid = to_regclass(format('public.%I', expected.tbl))
           AND a.attname  = expected.col
           AND a.attnum   > 0
           AND NOT a.attisdropped
          LEFT JOIN pg_catalog.pg_description d
            ON d.objoid   = a.attrelid
           AND d.classoid = 'pg_catalog.pg_class'::regclass
           AND d.objsubid = a.attnum
         ORDER BY expected.tbl, expected.col
    LOOP
        IF rec.comment IS DISTINCT FROM col_marker THEN
            unowned := unowned || format(
                'column public.%s.%s exists without this migration''s ownership marker '
                '(comment: %s)',
                rec.tbl, rec.col, COALESCE(quote_literal(rec.comment), 'none'));
        ELSIF rec.actual_type IS DISTINCT FROM 'uuid' THEN
            unowned := unowned || format(
                'column public.%s.%s carries this migration''s ownership marker but its type is '
                '%s, not uuid', rec.tbl, rec.col, rec.actual_type);
        END IF;
    END LOOP;

    -- The five bridge functions.
    --
    -- Discovered by the full contract `CREATE OR REPLACE` would replace and
    -- `rollback/0032` would drop -- namespace, name, zero input arguments and a
    -- `trigger` return type -- rather than by name alone. Both halves matter:
    -- name alone refuses over an unrelated `fn_entities_tenancy_bridge(integer)`
    -- overload that neither statement can touch (the existing
    -- `an_overloaded_bridge_function_name_does_not_block_step_one_rollback` test
    -- requires exactly that not to happen), while anything less than the full
    -- contract misses the one shape that IS silently replaced. Language is
    -- deliberately not part of the contract: `CREATE OR REPLACE` changes it
    -- freely, so an `sql` function of this signature is just as replaceable as a
    -- `plpgsql` one.
    FOR rec IN
        SELECT expected.fn, d.description AS comment
          FROM (VALUES
                  ('fn_archival_batches_tenancy_bridge'), ('fn_audit_logs_tenancy_bridge'),
                  ('fn_entities_tenancy_bridge'),         ('fn_memory_graph_tenancy_bridge'),
                  ('fn_rmk_policies_tenancy_bridge')
               ) AS expected(fn)
          JOIN pg_catalog.pg_proc p
            ON p.proname      = expected.fn
           AND p.pronamespace = 'public'::regnamespace
           AND p.pronargs     = 0
           AND p.prorettype   = 'pg_catalog.trigger'::regtype
          LEFT JOIN pg_catalog.pg_description d
            ON d.objoid   = p.oid
           AND d.classoid = 'pg_catalog.pg_proc'::regclass
           AND d.objsubid = 0
         ORDER BY expected.fn
    LOOP
        IF rec.comment IS DISTINCT FROM fn_marker THEN
            unowned := unowned || format(
                'function public.%s() exists without this migration''s ownership marker '
                '(comment: %s). CREATE OR REPLACE would preserve its OID, so every trigger '
                'already calling it would start running this migration''s body',
                rec.fn, COALESCE(quote_literal(rec.comment), 'none'));
        END IF;
    END LOOP;

    -- The five bridge triggers, scoped to (name, table) because trigger names are
    -- unique only per relation. A same-named trigger on any other table is
    -- untouched by this file and must not be reported -- the existing
    -- `an_unrelated_trigger_of_the_same_name_does_not_block_the_prepare_rollback`
    -- test pins that.
    FOR rec IN
        SELECT expected.trg, expected.tbl, d.description AS comment
          FROM (VALUES
                  ('trg_archival_batches_tenancy_bridge', 'archival_batches'),
                  ('trg_audit_logs_tenancy_bridge',       'audit_logs'),
                  ('trg_entities_tenancy_bridge',         'entities'),
                  ('trg_memory_graph_tenancy_bridge',     'memory_graph'),
                  ('trg_rmk_policies_tenancy_bridge',     'rmk_policies')
               ) AS expected(trg, tbl)
          JOIN pg_catalog.pg_trigger t
            ON t.tgname  = expected.trg
           AND t.tgrelid = to_regclass(format('public.%I', expected.tbl))
           AND NOT t.tgisinternal
          LEFT JOIN pg_catalog.pg_description d
            ON d.objoid   = t.oid
           AND d.classoid = 'pg_catalog.pg_trigger'::regclass
           AND d.objsubid = 0
         ORDER BY expected.trg
    LOOP
        IF rec.comment IS DISTINCT FROM trg_marker THEN
            unowned := unowned || format(
                'trigger %s on public.%s exists without this migration''s ownership marker '
                '(comment: %s)',
                rec.trg, rec.tbl, COALESCE(quote_literal(rec.comment), 'none'));
        END IF;
    END LOOP;

    IF cardinality(unowned) > 0 THEN
        RAISE EXCEPTION
            'migration 0032 refuses to adopt tranche 1 objects it did not create: %. Each holds a '
            'name this migration uses but is not the object it creates, and rollback/0032 drops '
            'all of them unconditionally -- so adopting them would record version 32 over '
            'somebody else''s schema and arm a rollback that destroys it. Nothing has been '
            'changed and no version has been recorded. Rename or remove the occupant, then '
            're-run. If these ARE this tranche''s objects from a database that applied 0032 '
            'before the ownership marker existed, stamp them yourself with the COMMENT '
            'statements this migration uses -- verify first, because the marker is exactly what '
            'authorises rollback/0032 to drop them.',
            array_to_string(unowned, ' | ')
            USING ERRCODE = 'duplicate_object';
    END IF;
END $$;

-- ── The backfill checkpoint protocol table ──────────────────────────────────
-- Typed as `plan::TENANCY_BACKFILL_CHECKPOINTS`.  `agent_tenancy_migrations`
-- cannot serve this purpose: it has no tranche, no digest, no cursor and no
-- status, so a FINALIZE guard reading it would assert against the wrong
-- evidence and pass.
--
-- Deliberately OUTSIDE the ownership guard above, which covers the twenty objects
-- `rollback/0032` destroys.  `CREATE TABLE IF NOT EXISTS` neither replaces nor
-- rewrites an existing table, and this rollback does not drop this one -- it
-- retains it on purpose and documents a manual DROP.  A pre-existing table of
-- this name would therefore be adopted as the checkpoint store rather than
-- destroyed, which is a FINALIZE-evidence problem for a later step to state, not
-- a data-loss path for this one to guard.
CREATE TABLE IF NOT EXISTS public.tenancy_backfill_checkpoints (
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
    ON public.tenancy_backfill_checkpoints (tranche, contract_digest)
    WHERE status = 'COMPLETED';

CREATE INDEX IF NOT EXISTS idx_tenancy_backfill_checkpoints_tranche
    ON public.tenancy_backfill_checkpoints (tranche, started_at DESC);

COMMENT ON TABLE public.tenancy_backfill_checkpoints IS
    'Per-tranche, per-contract backfill evidence (plan §4 step 4B). BACKFILL writes progress '
    'here and FINALIZE refuses to proceed without a COMPLETED row for its own tranche against '
    'the current contract digest with blocking_count = 0. ABANDONED rows are retained as '
    'history and are never treated as completion.';

COMMENT ON COLUMN public.tenancy_backfill_checkpoints.contract_digest IS
    'report::inventory_digest() at the time the backfill ran. A backfill that ran against a '
    'superseded plan proves nothing about the current one, so FINALIZE compares this exactly.';

COMMENT ON COLUMN public.tenancy_backfill_checkpoints.blocking_count IS
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

ALTER TABLE public.archival_batches
    ADD COLUMN IF NOT EXISTS agent_uuid UUID,
    ADD COLUMN IF NOT EXISTS tenant_id  UUID;

ALTER TABLE public.audit_logs
    ADD COLUMN IF NOT EXISTS agent_uuid UUID,
    ADD COLUMN IF NOT EXISTS tenant_id  UUID;

ALTER TABLE public.entities
    ADD COLUMN IF NOT EXISTS agent_uuid UUID,
    ADD COLUMN IF NOT EXISTS tenant_id  UUID;

ALTER TABLE public.memory_graph
    ADD COLUMN IF NOT EXISTS agent_uuid UUID,
    ADD COLUMN IF NOT EXISTS tenant_id  UUID;

ALTER TABLE public.rmk_policies
    ADD COLUMN IF NOT EXISTS agent_uuid UUID,
    ADD COLUMN IF NOT EXISTS tenant_id  UUID;

-- Stamp the ten columns as this migration's own.
--
-- Written in the same transaction that creates them, so a column and its
-- ownership marker land together or not at all -- there is no window in which a
-- column this migration created is indistinguishable from one it found.  Applied
-- unconditionally rather than only to new columns: on an idempotent re-run the
-- guard above has already proved every existing column is ours, so re-stating the
-- marker is a no-op that also repairs a comment somebody overwrote.
--
-- `COMMENT ON` writes `pg_description`, which is catalog state and is dropped
-- with the column, so the marker cannot outlive what it describes and be
-- inherited by a later same-named column.
--
-- The marker text is repeated from the guard above rather than shared: SQL has no
-- cross-statement constants, and it appears a third time in `rollback/0032`.
-- `the_ownership_markers_do_not_drift` in src/tenancy/tranche1_db_tests.rs pins
-- all three to the same literal without needing a database.
DO $$
DECLARE
    col_marker TEXT := 'AEON-IQ tenancy tranche 1 ownership column. Created and owned by '
                       'migration 0032; rollback/0032_tenancy_tranche1_prepare_down.sql drops it '
                       'only while this exact comment is present. '
                       'aeon-iq:tenancy:tranche1:0032:ownership-column';
    rec        RECORD;
BEGIN
    FOR rec IN
        SELECT * FROM (VALUES
                  ('archival_batches', 'agent_uuid'), ('archival_batches', 'tenant_id'),
                  ('audit_logs',       'agent_uuid'), ('audit_logs',       'tenant_id'),
                  ('entities',         'agent_uuid'), ('entities',         'tenant_id'),
                  ('memory_graph',     'agent_uuid'), ('memory_graph',     'tenant_id'),
                  ('rmk_policies',     'agent_uuid'), ('rmk_policies',     'tenant_id')
               ) AS t(tbl, col)
         ORDER BY 1, 2
    LOOP
        EXECUTE format('COMMENT ON COLUMN public.%I.%I IS %L', rec.tbl, rec.col, col_marker);
    END LOOP;
END $$;

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
    BEFORE INSERT OR UPDATE ON public.archival_batches
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
    BEFORE INSERT OR UPDATE ON public.entities
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
    BEFORE INSERT OR UPDATE ON public.memory_graph
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
    BEFORE INSERT OR UPDATE ON public.rmk_policies
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
    BEFORE INSERT OR UPDATE ON public.audit_logs
    FOR EACH ROW EXECUTE FUNCTION public.fn_audit_logs_tenancy_bridge();

-- ── Stamp the bridges as this migration's own ───────────────────────────────
-- Same protocol as the ownership columns above, and the same reasoning: the
-- marker is written in the transaction that creates the object, so the two land
-- together, and `pg_description` rows are dropped with the object so a marker
-- cannot be inherited by a later same-named one.
--
-- Both kinds are stamped AFTER all five triggers exist, not interleaved with
-- them, because the guard at the head of this file is what makes the ordering
-- safe: it has already proved every one of these twenty names is either free or
-- this tranche's own, so nothing here can stamp somebody else's object.
--
-- The functions are stamped by their zero-argument signature, which is the same
-- contract the guard discovers them by and the one `rollback/0032` drops -- an
-- unrelated overload of the same name is neither stamped nor dropped.
DO $$
DECLARE
    fn_marker  TEXT := 'AEON-IQ tenancy tranche 1 bridge function. Created and owned by '
                       'migration 0032; rollback/0032_tenancy_tranche1_prepare_down.sql drops it '
                       'only while this exact comment is present. '
                       'aeon-iq:tenancy:tranche1:0032:bridge-function';
    trg_marker TEXT := 'AEON-IQ tenancy tranche 1 bridge trigger. Created and owned by '
                       'migration 0032; rollback/0032_tenancy_tranche1_prepare_down.sql drops it '
                       'only while this exact comment is present. '
                       'aeon-iq:tenancy:tranche1:0032:bridge-trigger';
    rec        RECORD;
BEGIN
    FOR rec IN
        SELECT * FROM (VALUES
                  ('fn_archival_batches_tenancy_bridge', 'trg_archival_batches_tenancy_bridge',
                       'archival_batches'),
                  ('fn_audit_logs_tenancy_bridge',       'trg_audit_logs_tenancy_bridge',
                       'audit_logs'),
                  ('fn_entities_tenancy_bridge',         'trg_entities_tenancy_bridge',
                       'entities'),
                  ('fn_memory_graph_tenancy_bridge',     'trg_memory_graph_tenancy_bridge',
                       'memory_graph'),
                  ('fn_rmk_policies_tenancy_bridge',     'trg_rmk_policies_tenancy_bridge',
                       'rmk_policies')
               ) AS t(fn, trg, tbl)
         ORDER BY 1
    LOOP
        EXECUTE format('COMMENT ON FUNCTION public.%I() IS %L', rec.fn, fn_marker);
        EXECUTE format('COMMENT ON TRIGGER %I ON public.%I IS %L',
                       rec.trg, rec.tbl, trg_marker);
    END LOOP;
END $$;
