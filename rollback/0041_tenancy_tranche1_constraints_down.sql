-- Rollback for migration 0041 (tenancy tranche 1 constraint attachment).
--
-- Apply manually:
--     psql "$DATABASE_URL" -v ON_ERROR_STOP=1 \
--       -f rollback/0041_tenancy_tranche1_constraints_down.sql
--
-- MIGRATION ORDER.  Full unwind order for tranche 1 is 0041 -> 0033-0040 -> 0032.
-- This script must run first: migrations 0033-0040 build indexes that 0041
-- adopts as constraints, and 0032's columns cannot be dropped while any of
-- these constraints reference them.
--
-- WHAT THIS REVERSES, AND WHAT IT DELIBERATELY DOES NOT.
-- Drops the five composite ownership foreign keys and the three unique
-- constraints 0041 attached.  Dropping a UNIQUE constraint also drops the index
-- it adopted -- that is PostgreSQL's behaviour for an adopted index, not an
-- oversight here, and it is why 0033-0040's rollback drops those three names
-- only IF EXISTS.
--
-- Deliberately NOT resolved with `DROP ... CASCADE`: nothing here needs it, and
-- cascading from an ownership constraint would reach into whatever a later
-- tranche has since attached.
--
-- NO DATA IS LOST.  Every ownership value written by the bridge triggers stays
-- in its column; only the constraints enforcing consistency are removed.
--
-- SAFE TO RE-RUN.  Every drop is IF EXISTS.
BEGIN;

-- Never trust the caller's session search_path.  Without this a schema placed
-- ahead of `public` could capture every unqualified name below.  SET LOCAL
-- confines it to this transaction, so the operator's session is unchanged
-- afterwards.
SET LOCAL search_path = pg_catalog, public;

ALTER TABLE public.archival_batches DROP CONSTRAINT IF EXISTS archival_batches_tenant_agent_fkey;
ALTER TABLE public.audit_logs       DROP CONSTRAINT IF EXISTS audit_logs_tenant_agent_fkey;
ALTER TABLE public.entities         DROP CONSTRAINT IF EXISTS entities_tenant_agent_fkey;
ALTER TABLE public.memory_graph     DROP CONSTRAINT IF EXISTS memory_graph_tenant_agent_fkey;
ALTER TABLE public.rmk_policies     DROP CONSTRAINT IF EXISTS rmk_policies_tenant_agent_fkey;

ALTER TABLE public.archival_batches DROP CONSTRAINT IF EXISTS archival_batches_id_tenant_id_key;
ALTER TABLE public.entities         DROP CONSTRAINT IF EXISTS entities_id_tenant_id_key;
ALTER TABLE public.rmk_policies     DROP CONSTRAINT IF EXISTS rmk_policies_id_tenant_id_key;

DO $$
BEGIN
    IF to_regclass('public._sqlx_migrations') IS NOT NULL THEN
        DELETE FROM public._sqlx_migrations WHERE version = 41;
    END IF;
END $$;

COMMIT;
