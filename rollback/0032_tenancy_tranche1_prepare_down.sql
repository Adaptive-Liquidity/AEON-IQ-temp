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
-- SAFE TO RE-RUN.  Every statement is IF EXISTS.
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
       AND c.conname IN ('archival_batches_tenant_agent_fkey',
                         'audit_logs_tenant_agent_fkey',
                         'entities_tenant_agent_fkey',
                         'memory_graph_tenant_agent_fkey',
                         'rmk_policies_tenant_agent_fkey');

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
