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
-- That is only safe because of the provenance guards immediately below: they
-- prove that anything this file is about to skip-or-replace is this file's own
-- earlier work, and refuse otherwise.
-- ============================================================================

-- ── Provenance: this tranche creates, or it refuses.  It never adopts. ──────
--
-- `ADD COLUMN IF NOT EXISTS`, `CREATE OR REPLACE FUNCTION` and `CREATE OR
-- REPLACE TRIGGER` are what make this file re-runnable, and each of them is
-- also a silent adoption of somebody else's object:
--
--   * A pre-existing `entities.tenant_id` is skipped by IF NOT EXISTS and then
--     dropped unconditionally by rollback/0032, destroying data this tranche
--     never created.  The column holds rows, so an identically-shaped column is
--     NOT interchangeable -- this is the one place where the tranche's usual
--     identity-by-shape rule is insufficient and provenance is required.
--   * `CREATE OR REPLACE FUNCTION` preserves the OID, so replacing a
--     pre-existing `fn_entities_tenancy_bridge()` silently changes the behaviour
--     of every trigger that already calls it, and rollback/0032 then drops it.
--   * `CREATE OR REPLACE TRIGGER` replaces a same-named trigger on the same
--     table, and rollback/0032 then drops that too.  Same defect, third object
--     kind; fixed here rather than left for the next reviewer to find.
--
-- The handle is `pg_description`.  Every object this file creates is stamped
-- with the marker below, in the same transaction that creates it, and
-- rollback/0032 destroys only objects carrying exactly that comment.  A comment
-- is keyed by (objoid, objsubid), so it survives ALTER TABLE RENAME of either
-- the table or the column -- which is more than a name-based handle manages.
-- It cannot be created by accident, so it yields no false adoption.  Stripping
-- it yields a false REFUSAL, which preserves data; that direction is the safe
-- one, and rollback/0032 says how to resolve it.
--
-- The three guards run before anything is created, so a refusal leaves the
-- schema untouched rather than half-built.
--
-- WHAT THE MARKER IS AND IS NOT.  It is a provenance handle: it stops this
-- migration adopting an object it did not create, and so stops rollback/0032
-- destroying data belonging to whatever did.  It is NOT authentication.  A role
-- that can create objects in `public` can also write the marker onto them, so
-- each guard additionally requires `pg_has_role(current_user, <owner>,
-- 'USAGE')` -- an object owned by a role this migration cannot vouch for is
-- refused whatever its comment says.  Note honestly that this ownership test is
-- VACUOUS when migrations run as a superuser, because a superuser is a member
-- of every role; where that is the deployment, the load-bearing control is
-- PostgreSQL 15+'s default of not granting CREATE on `public` to PUBLIC, not
-- anything in this file.
--
-- The marker text appears three times in the tranche: the guard below, the
-- stamping block at the foot of this file, and the guard in
-- rollback/0032_tenancy_tranche1_prepare_down.sql.  All three must stay
-- byte-identical; `the_provenance_marker_is_identical_everywhere_it_appears`
-- fails if they drift.

DO $provenance_guard$
DECLARE
    -- Declared once.  Duplicated verbatim in the stamping block at the foot of
    -- this file and in rollback/0032_tenancy_tranche1_prepare_down.sql; the test
    -- `the_provenance_marker_is_identical_everywhere_it_appears` fails on drift.
    marker CONSTANT TEXT :=
        'AEON tenancy tranche 1 (migration 0032). Provenance marker: rollback/0032 '
        'destroys only objects carrying exactly this comment, and refuses to touch '
        'any that do not.';
    occupied TEXT;
BEGIN
    -- Ten ownership columns.
    SELECT pg_catalog.string_agg(
               pg_catalog.format('%s.%s', expected.tbl, expected.col),
               ', ' ORDER BY expected.tbl, expected.col)
      INTO occupied
      FROM (VALUES
              ('archival_batches', 'agent_uuid'), ('archival_batches', 'tenant_id'),
              ('audit_logs',       'agent_uuid'), ('audit_logs',       'tenant_id'),
              ('entities',         'agent_uuid'), ('entities',         'tenant_id'),
              ('memory_graph',     'agent_uuid'), ('memory_graph',     'tenant_id'),
              ('rmk_policies',     'agent_uuid'), ('rmk_policies',     'tenant_id')
           ) AS expected(tbl, col)
      JOIN pg_catalog.pg_attribute a
        ON a.attrelid = pg_catalog.to_regclass(pg_catalog.format('public.%I', expected.tbl))
       AND a.attname  = expected.col
       AND a.attnum   > 0
       AND NOT a.attisdropped
      JOIN pg_catalog.pg_class ct ON ct.oid = a.attrelid
     WHERE pg_catalog.col_description(a.attrelid, a.attnum::int) IS DISTINCT FROM marker
        OR NOT pg_catalog.pg_has_role(current_user, ct.relowner, 'USAGE');

    IF occupied IS NOT NULL THEN
        RAISE EXCEPTION
            'these ownership columns already exist and tenancy tranche 1 did not create '
            'them: %. Migration 0032 will not adopt a column it did not create, because '
            'rollback/0032 drops the ownership columns and would destroy data belonging to '
            'whatever does own them. Nothing has been changed. Either drop or rename the '
            'pre-existing columns, or -- if they really are this tranche''s and their '
            'provenance comment was removed -- restore the comment this migration stamps.',
            occupied
            USING ERRCODE = 'duplicate_column';
    END IF;

    -- Five bridge functions, matched by SIGNATURE rather than by name:
    -- PostgreSQL allows overloading, and only the zero-argument
    -- `trigger`-returning form is the one this file installs and rollback/0032
    -- drops.
    SELECT pg_catalog.string_agg(
               pg_catalog.format('public.%s()', expected.fn), ', ' ORDER BY expected.fn)
      INTO occupied
      FROM (VALUES
              ('fn_archival_batches_tenancy_bridge'),
              ('fn_audit_logs_tenancy_bridge'),
              ('fn_entities_tenancy_bridge'),
              ('fn_memory_graph_tenancy_bridge'),
              ('fn_rmk_policies_tenancy_bridge')
           ) AS expected(fn)
      JOIN pg_catalog.pg_proc p
        ON p.proname      = expected.fn
       AND p.pronamespace = 'public'::regnamespace
       AND p.pronargs     = 0
       AND p.prorettype   = 'pg_catalog.trigger'::regtype
     WHERE pg_catalog.obj_description(p.oid, 'pg_proc') IS DISTINCT FROM marker
        OR NOT pg_catalog.pg_has_role(current_user, p.proowner, 'USAGE');

    IF occupied IS NOT NULL THEN
        RAISE EXCEPTION
            'these bridge functions already exist and tenancy tranche 1 did not create '
            'them: %. CREATE OR REPLACE FUNCTION preserves the OID, so replacing them would '
            'silently change the behaviour of every trigger that already calls them, and '
            'rollback/0032 would then drop them outright. Nothing has been changed.',
            occupied
            USING ERRCODE = 'duplicate_function';
    END IF;

    -- Five bridge triggers, scoped to the table each one belongs on: trigger
    -- names are unique only per table.
    SELECT pg_catalog.string_agg(
               pg_catalog.format('%s on public.%s', expected.trg, expected.tbl),
               ', ' ORDER BY expected.trg)
      INTO occupied
      FROM (VALUES
              ('trg_archival_batches_tenancy_bridge', 'archival_batches'),
              ('trg_audit_logs_tenancy_bridge',       'audit_logs'),
              ('trg_entities_tenancy_bridge',         'entities'),
              ('trg_memory_graph_tenancy_bridge',     'memory_graph'),
              ('trg_rmk_policies_tenancy_bridge',     'rmk_policies')
           ) AS expected(trg, tbl)
      JOIN pg_catalog.pg_trigger t
        ON t.tgname  = expected.trg
       AND t.tgrelid = pg_catalog.to_regclass(pg_catalog.format('public.%I', expected.tbl))
       AND NOT t.tgisinternal
      JOIN pg_catalog.pg_class tt ON tt.oid = t.tgrelid
     WHERE pg_catalog.obj_description(t.oid, 'pg_trigger') IS DISTINCT FROM marker
        OR NOT pg_catalog.pg_has_role(current_user, tt.relowner, 'USAGE');

    IF occupied IS NOT NULL THEN
        RAISE EXCEPTION
            'these bridge triggers already exist and tenancy tranche 1 did not create '
            'them: %. CREATE OR REPLACE TRIGGER would replace them and rollback/0032 would '
            'then drop them, leaving nothing to restore. Nothing has been changed.',
            occupied
            USING ERRCODE = 'duplicate_object';
    END IF;

    -- The checkpoint protocol table.
    --
    -- `CREATE TABLE IF NOT EXISTS` adopts exactly as silently as `ADD COLUMN IF
    -- NOT EXISTS` does, and this is the worst object in the tranche to adopt:
    -- skipping the CREATE also skips every CHECK inside it, so a pre-existing
    -- table of this name enforces neither the `status` vocabulary, nor the
    -- `tranche` vocabulary, nor `rows_backfilled <= rows_total`, nor
    -- `COMPLETED => blocking_count = 0`, nor `COMPLETED => rows_backfilled =
    -- rows_total`. A forged or merely malformed COMPLETED row then satisfies
    -- FINALIZE_PRECONDITION -- the one piece of evidence FINALIZE trusts before
    -- it validates constraints over data nobody backfilled.
    --
    -- Checked by SHAPE rather than by the provenance marker used above, and the
    -- difference is deliberate. The marker exists because dropping a column
    -- destroys data, so this migration must know whose column it is; this table
    -- is never dropped by rollback/0032 at all, so adopting it destroys
    -- nothing. What adoption costs here is the constraints, and those are
    -- exactly what shape can see. The table also already carries a documented
    -- `COMMENT ON TABLE`, which a marker would have to overwrite.
    --
    -- Ownership is checked alongside, for the same reason it is checked above:
    -- an object this migration cannot vouch for is not this migration's.
    SELECT pg_catalog.string_agg(missing.what, ', ' ORDER BY missing.what)
      INTO occupied
      FROM (
            -- Compared by EXPRESSION, not by name.
            --
            -- Codex, P1 on 9a6c279: matching `contype` and `conname` alone
            -- establishes none of the invariants this guard exists to protect.
            -- Seven constraints carrying these exact names and defined as
            -- `CHECK (true)` satisfied a name-only test, `CREATE TABLE IF NOT
            -- EXISTS` adopted the table, and a forged COMPLETED row still
            -- reached FINALIZE. That is identity-by-name at an eighth object
            -- kind, and the tranche's rule is that identity is by shape.
            --
            -- The right-hand sides are PostgreSQL's own normalised rendering,
            -- read back out of `pg_get_expr` after this migration builds the
            -- table -- not the source text above, which deparses differently.
            -- Exact comparison is pinned to pg16, the only version in compose
            -- and CI, exactly as `IndexPredicate::Exactly` already is. A major
            -- upgrade that changed deparse output would fail closed here, with
            -- a spurious refusal naming the constraint, rather than silently
            -- passing.
            SELECT pg_catalog.format('CHECK %s is missing or does not match', expected.con) AS what
              FROM (VALUES
                      ('tenancy_backfill_checkpoints_status_ck',
                       '(status = ANY (ARRAY[''IN_PROGRESS''::text, ''COMPLETED''::text, ''ABANDONED''::text]))'),
                      ('tenancy_backfill_checkpoints_tranche_ck',
                       '(tranche = ANY (ARRAY[''TRANCHE_1_ROOTS_AND_DIRECT_AGENT_CHILDREN''::text, ''TRANCHE_2_SESSIONS''::text, ''TRANCHE_3_MEMORIES''::text, ''TRANCHE_4_LINEAGE_AND_ARCHIVAL''::text, ''TRANCHE_5_OPERATIONS''::text, ''FINAL_CONSTRAINT_TIGHTENING''::text]))'),
                      ('tenancy_backfill_checkpoints_completed_shape_ck',
                       '((status = ''COMPLETED''::text) = (completed_at IS NOT NULL))'),
                      ('tenancy_backfill_checkpoints_completed_cursor_ck',
                       '((status <> ''COMPLETED''::text) OR (resume_cursor IS NULL))'),
                      ('tenancy_backfill_checkpoints_counts_ck',
                       '((rows_total >= 0) AND (rows_backfilled >= 0) AND (blocking_count >= 0) AND (rows_backfilled <= rows_total))'),
                      ('tenancy_backfill_checkpoints_completed_clean_ck',
                       '((status <> ''COMPLETED''::text) OR (blocking_count = 0))'),
                      ('tenancy_backfill_checkpoints_completed_accounting_ck',
                       '((status <> ''COMPLETED''::text) OR (rows_backfilled = rows_total))')
                   ) AS expected(con, expr)
             WHERE pg_catalog.to_regclass('public.tenancy_backfill_checkpoints') IS NOT NULL
               AND NOT EXISTS (
                     SELECT 1
                       FROM pg_catalog.pg_constraint k
                      WHERE k.conrelid =
                            pg_catalog.to_regclass('public.tenancy_backfill_checkpoints')
                        AND k.contype  = 'c'
                        AND k.conname  = expected.con
                        AND pg_catalog.pg_get_expr(k.conbin, k.conrelid) = expected.expr
                        -- Codex, on 1f5cdda: the expressions match even when the
                        -- constraint was added NOT VALID after malformed rows were
                        -- already in the table, so `convalidated` is what makes this
                        -- an assertion about the DATA and not just about the schema.
                        -- 0032 declares these inline in CREATE TABLE, where they are
                        -- always validated, so requiring it costs a clean apply nothing.
                        AND k.convalidated)

            UNION ALL

            -- The partial unique index, by SHAPE.
            --
            -- Found by self-audit after Codex's P1, enumerating every
            -- `IF NOT EXISTS` in the tranche and asking what identifies the
            -- object. This one was checked for ownership and nothing else, and
            -- it is not decorative: it is what makes "at most one authoritative
            -- completion per tranche and digest" true, and its
            -- `WHERE status = 'COMPLETED'` scoping is what keeps ABANDONED
            -- history retained rather than overwritten. A non-unique occupant
            -- lets two COMPLETED rows coexist, which is the evidence FINALIZE
            -- reads; a total occupant blocks the ABANDONED history the rollback
            -- deliberately keeps; different columns key it on the wrong thing.
            SELECT pg_catalog.format(
                       'index tenancy_backfill_checkpoints_completed_key is not the '
                       'partial unique index this migration builds '
                       '(indisunique=%s indisvalid=%s predicate=%L columns=%L)',
                       i.indisunique, i.indisvalid,
                       COALESCE(pg_catalog.pg_get_expr(i.indpred, i.indrelid), '<total>'),
                       COALESCE((SELECT pg_catalog.string_agg(a.attname, ',' ORDER BY k.ord)
                                   FROM pg_catalog.unnest(i.indkey::int2[])
                                        WITH ORDINALITY AS k(attnum, ord)
                                   JOIN pg_catalog.pg_attribute a
                                     ON a.attrelid = i.indrelid AND a.attnum = k.attnum),
                                '<none>'))
              FROM pg_catalog.pg_index i
             WHERE i.indexrelid =
                   pg_catalog.to_regclass('public.tenancy_backfill_checkpoints_completed_key')
               AND (NOT i.indisunique
                    -- An INVALID index enforces nothing, so it must never be
                    -- adopted as the object that guarantees uniqueness. 0041's
                    -- adoption branch already refuses INVALID for the same
                    -- reason; note this is the opposite of the rollback guards,
                    -- which deliberately ignore `indisvalid` so an interrupted
                    -- concurrent build stays droppable. Adopting and dropping
                    -- want different answers here.
                    OR NOT i.indisvalid
                    OR pg_catalog.pg_get_expr(i.indpred, i.indrelid)
                       IS DISTINCT FROM '(status = ''COMPLETED''::text)'
                    OR (SELECT pg_catalog.string_agg(a.attname, ',' ORDER BY k.ord)
                          FROM pg_catalog.unnest(i.indkey::int2[])
                               WITH ORDINALITY AS k(attnum, ord)
                          JOIN pg_catalog.pg_attribute a
                            ON a.attrelid = i.indrelid AND a.attnum = k.attnum)
                       IS DISTINCT FROM 'tranche,contract_digest')

            UNION ALL

            SELECT pg_catalog.format('%s is owned by a role this migration cannot vouch for',
                                     expected.rel)
              FROM (VALUES
                      ('tenancy_backfill_checkpoints'),
                      ('tenancy_backfill_checkpoints_completed_key'),
                      ('idx_tenancy_backfill_checkpoints_tranche')
                   ) AS expected(rel)
              JOIN pg_catalog.pg_class c
                ON c.oid = pg_catalog.to_regclass(pg_catalog.format('public.%I', expected.rel))
             WHERE NOT pg_catalog.pg_has_role(current_user, c.relowner, 'USAGE')
           ) AS missing;

    IF occupied IS NOT NULL THEN
        RAISE EXCEPTION
            'tenancy_backfill_checkpoints already exists but is not the table this migration '
            'creates: %. CREATE TABLE IF NOT EXISTS would adopt it, and adopting it skips '
            'every CHECK this migration puts on it -- after which a COMPLETED row that no '
            'backfill ever earned satisfies the FINALIZE precondition. Nothing has been '
            'changed. Drop or rename the occupant, then re-run.',
            occupied
            USING ERRCODE = 'duplicate_table';
    END IF;
END $provenance_guard$;

-- ── The backfill checkpoint protocol table ──────────────────────────────────
-- Typed as `plan::TENANCY_BACKFILL_CHECKPOINTS`.  `agent_tenancy_migrations`
-- cannot serve this purpose: it has no tranche, no digest, no cursor and no
-- status, so a FINALIZE guard reading it would assert against the wrong
-- evidence and pass.
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

-- ── Provenance stamps ───────────────────────────────────────────────────────
-- Every object created above is stamped here, in the same transaction that
-- created it, so no window exists in which one of them is unattributed.
--
-- Written as a loop over the object lists rather than as twenty literal COMMENT
-- statements so the marker appears exactly once more in this file, and so a
-- stamp cannot be forgotten when the lists change: the same lists the guard at
-- the head of the file checks are the lists stamped here.
--
-- Re-running this file re-issues identical comments, which is a no-op.
DO $provenance_stamp$
DECLARE
    -- Byte-identical to the guard at the head of this file and to the guard in
    -- rollback/0032_tenancy_tranche1_prepare_down.sql.
    marker CONSTANT TEXT :=
        'AEON tenancy tranche 1 (migration 0032). Provenance marker: rollback/0032 '
        'destroys only objects carrying exactly this comment, and refuses to touch '
        'any that do not.';
    target RECORD;
BEGIN
    FOR target IN
        SELECT * FROM (VALUES
              ('archival_batches', 'agent_uuid'), ('archival_batches', 'tenant_id'),
              ('audit_logs',       'agent_uuid'), ('audit_logs',       'tenant_id'),
              ('entities',         'agent_uuid'), ('entities',         'tenant_id'),
              ('memory_graph',     'agent_uuid'), ('memory_graph',     'tenant_id'),
              ('rmk_policies',     'agent_uuid'), ('rmk_policies',     'tenant_id')
        ) AS c(tbl, col)
    LOOP
        EXECUTE pg_catalog.format('COMMENT ON COLUMN public.%I.%I IS %L',
                                  target.tbl, target.col, marker);
    END LOOP;

    FOR target IN
        SELECT * FROM (VALUES
              ('fn_archival_batches_tenancy_bridge'),
              ('fn_audit_logs_tenancy_bridge'),
              ('fn_entities_tenancy_bridge'),
              ('fn_memory_graph_tenancy_bridge'),
              ('fn_rmk_policies_tenancy_bridge')
        ) AS f(fn)
    LOOP
        EXECUTE pg_catalog.format('COMMENT ON FUNCTION public.%I() IS %L',
                                  target.fn, marker);
    END LOOP;

    FOR target IN
        SELECT * FROM (VALUES
              ('trg_archival_batches_tenancy_bridge', 'archival_batches'),
              ('trg_audit_logs_tenancy_bridge',       'audit_logs'),
              ('trg_entities_tenancy_bridge',         'entities'),
              ('trg_memory_graph_tenancy_bridge',     'memory_graph'),
              ('trg_rmk_policies_tenancy_bridge',     'rmk_policies')
        ) AS t(trg, tbl)
    LOOP
        EXECUTE pg_catalog.format('COMMENT ON TRIGGER %I ON public.%I IS %L',
                                  target.trg, target.tbl, marker);
    END LOOP;
END $provenance_stamp$;
