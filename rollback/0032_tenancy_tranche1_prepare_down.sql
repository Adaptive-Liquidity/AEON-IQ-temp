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
