-- Rollback for migration 0032 (tenancy tranche 1 PREPARE).
--
-- Apply manually:
--     psql "$DATABASE_URL" -v ON_ERROR_STOP=1 \
--       -f rollback/0032_tenancy_tranche1_prepare_down.sql
--
-- MIGRATION ORDER.  Full unwind order is 0041 -> 0033-0040 -> 0032, and this is
-- the last step.  The guard below refuses while any later part of the tranche
-- is still applied, naming what to run first.
--
-- WHAT IS LOST.  This is the one destructive step in the tranche, and it is
-- destructive in exactly two ways, both stated rather than discovered:
--
--   1. Dropping agent_uuid and tenant_id discards every ownership value the
--      bridge triggers and the backfill wrote.  Re-running the tranche
--      re-derives all of it from `agents`, which remains the authority, so
--      nothing unrecoverable is lost -- but the backfill must run again, and on
--      a large deployment that is hours, not seconds.
--
--   1b. Any COMPLETED checkpoint for this tranche is transitioned to ABANDONED
--      before the columns go. That is a loss of *authority*, not of history:
--      the row, its counts and its digest all survive, but it can no longer
--      satisfy a FINALIZE guard describing ownership this script just deleted.
--      See the block that does it, below, for why leaving it COMPLETED breaks
--      in both directions.
--
--   2. Dropping tenancy_backfill_checkpoints discards the backfill history,
--      including ABANDONED attempts.  That history is diagnostic evidence about
--      what an operator did and when.  This script therefore does NOT drop it
--      by default: the table is preserved, and the guarded block at the foot
--      explains how to remove it deliberately.  A rollback that silently
--      destroys the record of why a rollback was needed is a bad trade.
--
-- Deliberately NOT resolved with `DROP ... CASCADE` anywhere.
--
-- DROPS ONLY WHAT IT OWNS.  A name is not a claim of ownership, and every drop
-- below is by name.  Migration 0032 stamps each of the twenty objects it creates
-- -- ten ownership columns, five bridge functions, five bridge triggers -- with an
-- ownership marker in `pg_description`, inside the transaction that creates it.
-- This script drops an object only while that marker is present, and refuses
-- outright if any of the twenty names is occupied by something without it: a
-- pre-existing `entities.tenant_id` was skipped by 0032's `ADD COLUMN IF NOT
-- EXISTS` with a notice, and the `DROP COLUMN` statements below then destroyed it
-- and its data as though the tranche had created it.  The refusal comes before the
-- first mutation and names every offender at once.  See the guard for what it does
-- and does not prove, and for the one state it deliberately refuses rather than
-- resolves: a database that applied 0032 before the marker existed.
--
-- SAFE TO RE-RUN.  Every statement is IF EXISTS, and an object that is simply
-- absent is skipped rather than refused -- only a PRESENT unmarked one is an
-- ownership dispute.
BEGIN;

SET LOCAL search_path = pg_catalog, public;

DO $$
DECLARE
    remaining   TEXT;
    constraints TEXT;
BEGIN
    SELECT string_agg(c.relname, ', ' ORDER BY c.relname)
      INTO remaining
      FROM pg_catalog.pg_class c
      JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
     WHERE n.nspname = 'public'
       AND c.relkind = 'i'
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

    -- The five composite foreign keys reference the very columns this script
    -- drops. Checking them here, alongside the indexes, is what makes the
    -- refusal complete: an operator who unwound 0033-0040 but not 0041 would
    -- otherwise get past the index check and only discover the dependency when
    -- ALTER TABLE ... DROP COLUMN failed partway down.
    --
    -- Reached from the parent's unique key by OID rather than by (name,
    -- table-name). `conname` is unique only per relation, so a bare-name match
    -- would refuse over a constraint on some unrelated table -- but a literal
    -- table name is defeated by `ALTER TABLE ... RENAME`, which leaves
    -- constraint names untouched, so a renamed child reported "none" while its
    -- key was still attached. `conindid` is the OID of
    -- `agents_tenant_id_id_key`, which all five depend on and which no rename of
    -- a child can change; `conrelid::regclass` then reports where each one
    -- actually is.
    --
    -- SCOPED BY TABLE OID, NOT BY CONSTRAINT NAME, and that half was still a hole
    -- after `conindid` replaced the literal table name: `ALTER TABLE ... RENAME
    -- CONSTRAINT` leaves the key attached and pointing at the same parent index
    -- while hiding it from a `conname IN (...)` filter. This guard then reported
    -- "none" and the script proceeded -- and it does not fail late, which is what
    -- makes the miss quiet: `ALTER TABLE ... DROP COLUMN` drops the foreign-key
    -- constraints that use the column without needing CASCADE, so a renamed
    -- ownership key was silently destroyed by the column drops below while the
    -- ledger recorded 41 as still applied. Sibling script 0033 was rewritten to
    -- find the same five keys by the same two axes; this is that test, one script
    -- along.
    --
    -- The table scope is what keeps it precise, and dropping it would be a
    -- regression rather than a simplification: measured on pg16, migration 0030's
    -- `credential_agent_grants_agent_fkey` has the identical shape --
    -- `(tenant_id, agent_uuid)` referencing `agents (tenant_id, id)` through the
    -- same `conindid` -- so a shape-only test would report it as a tranche 1 key
    -- and refuse this rollback forever. A renamed or schema-moved TABLE escapes
    -- this arm because its scope is the thing that moved; that is caught by the
    -- bridge-trigger guard below, which refuses before anything is touched.
    SELECT string_agg(format('%s on %s', c.conname, c.conrelid::regclass), ', '
                      ORDER BY c.conname)
      INTO constraints
      FROM pg_catalog.pg_constraint c
     WHERE c.contype = 'f'
       AND c.conindid = (
               SELECT p.conindid
                 FROM pg_catalog.pg_constraint p
                WHERE p.conname = 'agents_tenant_id_id_key'
                  AND p.conrelid = 'public.agents'::regclass
                  AND p.contype  = 'u')
       AND c.conrelid IN (to_regclass('public.archival_batches'),
                          to_regclass('public.audit_logs'),
                          to_regclass('public.entities'),
                          to_regclass('public.memory_graph'),
                          to_regclass('public.rmk_policies'));

    IF remaining IS NOT NULL OR constraints IS NOT NULL THEN
        RAISE EXCEPTION
            'tranche 1 is not fully unwound. Indexes still present: %. '
            'Constraints still attached: %. Run '
            'rollback/0041_tenancy_tranche1_constraints_down.sql then '
            'rollback/0033_tenancy_tranche1_indexes_down.sql first.',
            COALESCE(remaining, 'none'),
            COALESCE(constraints, 'none')
            USING ERRCODE = 'dependent_objects_still_exist';
    END IF;
END $$;

-- Refuse if a bridged table has been renamed out from under this script.
--
-- Every DROP TRIGGER and DROP COLUMN below names its table literally, so a
-- renamed child turns this rollback into a bare "relation ... does not exist"
-- partway through -- after the block that retires completion evidence has
-- already run. The enclosing transaction makes that atomic rather than
-- corrupting, but it leaves the operator holding a raw catalog error instead of
-- a next action, and it is indistinguishable from the script being broken.
--
-- The rename-proof handle is the bridge trigger: `pg_trigger.tgrelid` is an OID,
-- and `ALTER TABLE ... RENAME` does not rename triggers. Resolving each bridge to
-- the table it is actually on, and comparing that against the table this script
-- names, catches the rename before anything is touched.
--
-- Reached through the trigger's FUNCTION, not its name. Trigger names are unique
-- only per table, so a name-only join classifies any unrelated table's
-- `trg_entities_tenancy_bridge` as a moved tranche trigger and refuses this
-- rollback permanently, even with all five real bridges correctly in place. The
-- zero-argument `trigger`-returning `fn_*_tenancy_bridge` functions that
-- migration 0032 installs are the thing that is actually ours, and `tgfoid`
-- points at them by OID.
DO $$
DECLARE
    moved TEXT;
BEGIN
    SELECT string_agg(format('%s is on %s (expected public.%s)',
                             t.tgname, t.tgrelid::regclass, expected.tbl),
                      ', ' ORDER BY t.tgname)
      INTO moved
      FROM (VALUES
              ('fn_archival_batches_tenancy_bridge', 'archival_batches'),
              ('fn_audit_logs_tenancy_bridge',       'audit_logs'),
              ('fn_entities_tenancy_bridge',         'entities'),
              ('fn_memory_graph_tenancy_bridge',     'memory_graph'),
              ('fn_rmk_policies_tenancy_bridge',     'rmk_policies')
           ) AS expected(fn, tbl)
      JOIN pg_catalog.pg_proc p
        ON p.proname      = expected.fn
       AND p.pronamespace = 'public'::regnamespace
       AND p.pronargs     = 0
       AND p.prorettype   = 'pg_catalog.trigger'::regtype
      JOIN pg_catalog.pg_trigger t
        ON t.tgfoid = p.oid
       AND NOT t.tgisinternal
     WHERE t.tgrelid IS DISTINCT FROM to_regclass(format('public.%I', expected.tbl));

    IF moved IS NOT NULL THEN
        RAISE EXCEPTION
            'a tranche 1 bridge trigger is on a table this script does not name: %. The table was '
            'renamed after migration 0032 was applied, so the DROP TRIGGER and DROP COLUMN '
            'statements below would not reach it. Nothing has been changed. Rename the table back '
            'to what migration 0032 created, then re-run.',
            moved
            USING ERRCODE = 'object_not_in_prerequisite_state';
    END IF;
END $$;

-- ── Refuse to drop any of the twenty objects this tranche does not own ───────
--
-- A NAME IS NOT A CLAIM OF OWNERSHIP, and every DROP below is by name. Migration
-- 0032 creates its columns with `ADD COLUMN IF NOT EXISTS` and its functions and
-- triggers with `CREATE OR REPLACE`, none of which says whether the object it
-- found was already there. A pre-existing `entities.tenant_id` was skipped by the
-- build with a notice, version 32 was recorded, and the `DROP COLUMN` statements
-- below then destroyed it and its data as though the tranche had created it. The
-- same held for a zero-argument `trigger`-returning function of a bridge's name --
-- whose OID `CREATE OR REPLACE` preserves, so 0032 silently rewrote what every
-- existing trigger on it executed -- and for a same-named trigger on a bridged
-- table.
--
-- Migration 0032 therefore stamps each of those twenty objects with an ownership
-- marker in `pg_description` inside the transaction that creates it, and this
-- script drops an object only while the marker is present. Absent is fine and is
-- not an error: this file promises to be re-runnable, so an object that is already
-- gone is simply skipped, exactly as the `IF EXISTS` on every drop intends. What
-- is refused is an object that is PRESENT and unmarked.
--
-- Refusal is transaction-wide and comes before the first mutation -- ahead of the
-- checkpoint retirement below, which is the earliest write this script performs --
-- so a refused rollback changes nothing at all, not even the ledger. All twenty
-- are checked in one pass and reported together.
--
-- `pg_description` rows are dropped with the object they describe, so a marker
-- cannot outlive its column and be inherited by a later column that happens to
-- reuse the name or the attnum.
--
-- A marker is evidence about 0032, not a defence against someone who can
-- `COMMENT ON` these objects; anyone with that privilege can already do worse.
--
-- The remediation for a legacy database is stated in the refusal itself, because
-- there is one state this cannot distinguish and should not silently resolve: a
-- database that applied 0032 BEFORE the marker existed has the tranche's own
-- twenty objects, unmarked. Guessing in either direction is wrong -- adopting them
-- is the very defect this guard closes, and refusing without a next action strands
-- the operator -- so it refuses and names the fix.
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
    -- The ten columns the ALTER TABLE statements below drop.
    FOR rec IN
        SELECT expected.tbl, expected.col, d.description AS comment
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
            unowned := unowned || format('column public.%s.%s (comment: %s)',
                                         rec.tbl, rec.col,
                                         COALESCE(quote_literal(rec.comment), 'none'));
        END IF;
    END LOOP;

    -- The five functions the DROP FUNCTION statements below drop, discovered by
    -- the same zero-argument `trigger`-returning contract those statements name.
    -- An unrelated overload is neither dropped nor reported.
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
            unowned := unowned || format('function public.%s() (comment: %s)',
                                         rec.fn, COALESCE(quote_literal(rec.comment), 'none'));
        END IF;
    END LOOP;

    -- The five triggers, scoped to (name, table) exactly as the DROP TRIGGER
    -- statements below are. A same-named trigger on any other table is not
    -- reachable by those statements and must not be reported here.
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
            unowned := unowned || format('trigger %s on public.%s (comment: %s)',
                                         rec.trg, rec.tbl,
                                         COALESCE(quote_literal(rec.comment), 'none'));
        END IF;
    END LOOP;

    IF cardinality(unowned) > 0 THEN
        RAISE EXCEPTION
            'refusing to drop tranche 1 objects this rollback does not own: %. Each holds a name '
            'migration 0032 uses but does not carry the ownership marker that migration writes '
            'when it creates the object, so dropping it would destroy something else -- a column '
            'together with its data. Nothing has been changed. Rename or remove the occupant, '
            'then re-run. If these ARE the tranche''s own objects in a database that applied 0032 '
            'before the marker existed, re-stamp them with the COMMENT statements migration 0032 '
            'uses and re-run; verify first, because the marker is exactly what authorises this '
            'script to drop them.',
            array_to_string(unowned, ' | ')
            USING ERRCODE = 'duplicate_object';
    END IF;
END $$;

-- Retire completion evidence before destroying what it describes.
--
-- This script drops every ownership value the backfill wrote, so any COMPLETED
-- checkpoint for this tranche stops being true the moment it runs. Leaving such
-- a row authoritative is wrong in both directions, and both were reachable:
-- this script deletes migration version 32, so re-applying recreates the
-- columns with historical rows NULL and a FINALIZE guard would validate over a
-- table nobody backfilled; and if the operator re-runs the backfill instead,
-- the partial unique index rejects the replacement COMPLETED row.
--
-- ABANDONED is the schema's own word for "superseded, retained for history,
-- never treated as completion". `completed_at` is cleared in the same statement
-- because `tenancy_backfill_checkpoints_completed_shape_ck` ties it to the
-- COMPLETED status. Counts and digest are preserved -- they are the diagnostic
-- content.
--
-- Scoped to this tranche, and deliberately NOT to a contract digest: this
-- script destroys every tranche-1 ownership value, so a completion recorded
-- against a superseded digest is equally untrue.
--
-- Guarded on the table's existence, because this file promises every statement
-- tolerates the object already being gone, and it documents a manual DROP of
-- that very table.
--
-- SHARE ROW EXCLUSIVE is load-bearing, not defensive: the UPDATE sees one READ
-- COMMITTED snapshot, and measured on pg16 a row that became COMPLETED after it
-- ran but before COMMIT survived the column drops without it.
DO $$
BEGIN
    IF to_regclass('public.tenancy_backfill_checkpoints') IS NULL THEN
        RETURN;
    END IF;

    LOCK TABLE public.tenancy_backfill_checkpoints IN SHARE ROW EXCLUSIVE MODE;

    UPDATE public.tenancy_backfill_checkpoints
       SET status       = 'ABANDONED',
           completed_at = NULL,
           updated_at   = NOW()
     WHERE status  = 'COMPLETED'
       AND tranche = 'TRANCHE_1_ROOTS_AND_DIRECT_AGENT_CHILDREN';
END $$;

-- Triggers before functions: a function cannot be dropped while a trigger
-- references it.
DROP TRIGGER IF EXISTS trg_archival_batches_tenancy_bridge ON public.archival_batches;
DROP TRIGGER IF EXISTS trg_audit_logs_tenancy_bridge       ON public.audit_logs;
DROP TRIGGER IF EXISTS trg_entities_tenancy_bridge         ON public.entities;
DROP TRIGGER IF EXISTS trg_memory_graph_tenancy_bridge     ON public.memory_graph;
DROP TRIGGER IF EXISTS trg_rmk_policies_tenancy_bridge     ON public.rmk_policies;

DROP FUNCTION IF EXISTS public.fn_archival_batches_tenancy_bridge();
DROP FUNCTION IF EXISTS public.fn_audit_logs_tenancy_bridge();
DROP FUNCTION IF EXISTS public.fn_entities_tenancy_bridge();
DROP FUNCTION IF EXISTS public.fn_memory_graph_tenancy_bridge();
DROP FUNCTION IF EXISTS public.fn_rmk_policies_tenancy_bridge();

ALTER TABLE public.archival_batches DROP COLUMN IF EXISTS agent_uuid;
ALTER TABLE public.archival_batches DROP COLUMN IF EXISTS tenant_id;
ALTER TABLE public.audit_logs       DROP COLUMN IF EXISTS agent_uuid;
ALTER TABLE public.audit_logs       DROP COLUMN IF EXISTS tenant_id;
ALTER TABLE public.entities         DROP COLUMN IF EXISTS agent_uuid;
ALTER TABLE public.entities         DROP COLUMN IF EXISTS tenant_id;
ALTER TABLE public.memory_graph     DROP COLUMN IF EXISTS agent_uuid;
ALTER TABLE public.memory_graph     DROP COLUMN IF EXISTS tenant_id;
ALTER TABLE public.rmk_policies     DROP COLUMN IF EXISTS agent_uuid;
ALTER TABLE public.rmk_policies     DROP COLUMN IF EXISTS tenant_id;

-- tenancy_backfill_checkpoints is deliberately RETAINED.  See "WHAT IS LOST"
-- above.  To remove it as well, run this separately and knowingly:
--
--     DROP TABLE IF EXISTS public.tenancy_backfill_checkpoints;
--
-- Leaving it in place is safe because the block above already retired this
-- tranche's completions to ABANDONED.  It is worth being exact about why that
-- block is what makes retention safe, rather than the absence of the columns:
-- this script deletes migration version 32, so the very next `sqlx migrate run`
-- recreates every ownership column with historical rows NULL again.  A retained
-- COMPLETED row would at that point describe ownership that no longer exists,
-- and a FINALIZE guard reading it would validate constraints over a table
-- nobody had backfilled.  "The columns are gone, so nothing can validate them"
-- is true only until the next migrate, which is why it is not the argument.

DO $$
BEGIN
    IF to_regclass('public._sqlx_migrations') IS NOT NULL THEN
        DELETE FROM public._sqlx_migrations WHERE version = 32;
    END IF;
END $$;

COMMIT;
