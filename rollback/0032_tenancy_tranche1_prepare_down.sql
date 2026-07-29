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
    remaining TEXT;
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

    IF remaining IS NOT NULL THEN
        RAISE EXCEPTION
            'tranche 1 indexes are still present (%). Run '
            'rollback/0041_tenancy_tranche1_constraints_down.sql then '
            'rollback/0033_tenancy_tranche1_indexes_down.sql first.',
            remaining
            USING ERRCODE = 'dependent_objects_still_exist';
    END IF;
END $$;

-- ── Retire completion evidence before destroying what it describes ──────────
-- This script drops every ownership value the backfill wrote, so any COMPLETED
-- checkpoint for this tranche stops being true the moment it runs. Leaving such
-- a row authoritative is wrong in both directions, and both were reachable:
--
--   * This script also deletes migration version 32, so re-applying the
--     migrations recreates the columns with historical rows NULL again. A
--     FINALIZE guard would then find a COMPLETED row for this tranche at the
--     current contract digest and validate constraints over a table nobody has
--     backfilled.
--   * And if the operator instead re-runs the backfill, the partial unique
--     index rejects the new COMPLETED row as a duplicate, so fresh evidence
--     cannot be recorded at all.
--
-- The fix uses the schema's own state model rather than deleting anything:
-- ABANDONED means "superseded, retained for history, never treated as
-- completion", which is exactly what these rows now are. The diagnostic record
-- of what ran and when survives; its authority does not.
--
-- `completed_at` is cleared in the same statement because
-- `tenancy_backfill_checkpoints_completed_shape_ck` ties it to the COMPLETED
-- status; leaving it set would make the row unrepresentable. The row counts and
-- the digest are deliberately preserved — they are the diagnostic content.
--
-- Scoped to this tranche: a completion for some other tranche describes
-- ownership this script does not touch, and retiring it would be a lie in the
-- other direction.
-- Guarded on the table's existence, because this file's own contract is that
-- every statement tolerates the object already being gone — and this file also
-- documents a manual `DROP TABLE tenancy_backfill_checkpoints` as a legitimate
-- follow-up. Without the guard, re-running this script after that DROP, or
-- against a database that never applied 0032, aborts on a missing relation
-- instead of being the no-op the header promises.
--
-- SHARE ROW EXCLUSIVE is taken first, and it is load-bearing rather than
-- defensive. The UPDATE sees one READ COMMITTED snapshot: a row that becomes
-- COMPLETED after it runs but before this transaction commits is invisible to
-- it, and the column drops below would then commit alongside a surviving
-- COMPLETED row describing ownership that no longer exists — exactly the state
-- this block exists to prevent. Measured on pg16: with no lock, a concurrent
-- INSERT committed mid-transaction survived as COMPLETED. The lock blocks
-- writers for the rest of the transaction without blocking readers; moving the
-- UPDATE nearer the COMMIT would only narrow the window, not close it.
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
-- Leaving it in place is harmless: nothing reads it unless a FINALIZE runs, and
-- a FINALIZE cannot run while the columns it validates no longer exist.

DO $$
BEGIN
    IF to_regclass('public._sqlx_migrations') IS NOT NULL THEN
        DELETE FROM public._sqlx_migrations WHERE version = 32;
    END IF;
END $$;

COMMIT;
