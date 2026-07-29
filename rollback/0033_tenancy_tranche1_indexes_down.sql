-- Rollback for migrations 0033-0040 (tenancy tranche 1 concurrent index builds).
--
-- Apply manually:
--     psql "$DATABASE_URL" -v ON_ERROR_STOP=1 \
--       -f rollback/0033_tenancy_tranche1_indexes_down.sql
--
-- One script for eight migrations, because the eight exist as separate files
-- only to satisfy sqlx: a migration file is sent as one simple-query message,
-- and PostgreSQL wraps a multi-statement message in an implicit transaction
-- that CREATE INDEX CONCURRENTLY refuses.  Dropping has no such constraint, so
-- splitting the rollback eight ways would add files without adding safety.
--
-- MIGRATION ORDER.  Full unwind order is 0041 -> 0033-0040 -> 0032.  Run
-- rollback/0041_tenancy_tranche1_constraints_down.sql first; the guard below
-- refuses otherwise.
--
-- WHAT THIS REVERSES.  The five (tenant_id, agent_uuid) lookup indexes, plus
-- the three unique indexes if 0041 never adopted them.  Once adopted they are
-- owned by their constraints and 0041's rollback removes them, which is why
-- all eight drops are IF EXISTS rather than unconditional.
--
-- Plain DROP INDEX, not DROP INDEX CONCURRENTLY: CONCURRENTLY cannot run inside
-- a transaction block, and this script is transactional so a partial unwind
-- cannot leave half the indexes gone.  The trade is a brief ACCESS EXCLUSIVE
-- lock per index, which is acceptable for an operator-initiated rollback and is
-- not acceptable for the forward build -- hence the asymmetry.
--
-- NO DATA IS LOST.
--
-- SAFE TO RE-RUN.  Every drop is IF EXISTS.
BEGIN;

SET LOCAL search_path = pg_catalog, public;

-- Refuse while any tranche-1 constraint from 0041 is still attached.
--
-- Scoped by table, not by name alone. `conname` is unique only per relation, so
-- an unrelated table -- or another schema entirely -- may legitimately hold a
-- constraint called `entities_id_tenant_id_key`, and matching on the bare name
-- would block a rollback that has nothing to do with it. The join pins each
-- name to the exact public table 0041 attaches it to.
--
-- Both kinds are checked. The three unique constraints own indexes this script
-- drops. The five foreign keys do not own them, but they are 0041's work and
-- unwinding out of order would leave them referencing columns that
-- rollback/0032 is about to remove -- so this script refuses rather than
-- letting the operator discover that two steps later.
DO $$
DECLARE
    remaining TEXT;
BEGIN
    SELECT string_agg(format('%s on %s', c.conname, expected.tbl), ', '
                      ORDER BY c.conname)
      INTO remaining
      FROM (VALUES
              ('archival_batches_id_tenant_id_key',  'archival_batches', 'u'),
              ('entities_id_tenant_id_key',          'entities',         'u'),
              ('rmk_policies_id_tenant_id_key',      'rmk_policies',     'u'),
              ('archival_batches_tenant_agent_fkey', 'archival_batches', 'f'),
              ('audit_logs_tenant_agent_fkey',       'audit_logs',       'f'),
              ('entities_tenant_agent_fkey',         'entities',         'f'),
              ('memory_graph_tenant_agent_fkey',     'memory_graph',     'f'),
              ('rmk_policies_tenant_agent_fkey',     'rmk_policies',     'f')
           ) AS expected(name, tbl, kind)
      JOIN pg_catalog.pg_class t
        ON t.relname = expected.tbl
      JOIN pg_catalog.pg_namespace n
        ON n.oid = t.relnamespace AND n.nspname = 'public'
      JOIN pg_catalog.pg_constraint c
        ON c.conname = expected.name AND c.conrelid = t.oid
       AND c.contype = expected.kind;

    IF remaining IS NOT NULL THEN
        RAISE EXCEPTION
            'migration 0041 is still applied: %. '
            'Run rollback/0041_tenancy_tranche1_constraints_down.sql first.',
            remaining
            USING ERRCODE = 'dependent_objects_still_exist';
    END IF;
END $$;

DROP INDEX IF EXISTS public.idx_archival_batches_tenant;
DROP INDEX IF EXISTS public.idx_audit_logs_tenant;
DROP INDEX IF EXISTS public.idx_entities_tenant;
DROP INDEX IF EXISTS public.idx_memory_graph_tenant;
DROP INDEX IF EXISTS public.idx_rmk_policies_tenant;

DROP INDEX IF EXISTS public.archival_batches_id_tenant_id_key;
DROP INDEX IF EXISTS public.entities_id_tenant_id_key;
DROP INDEX IF EXISTS public.rmk_policies_id_tenant_id_key;

DO $$
BEGIN
    IF to_regclass('public._sqlx_migrations') IS NOT NULL THEN
        DELETE FROM public._sqlx_migrations WHERE version BETWEEN 33 AND 40;
    END IF;
END $$;

COMMIT;
