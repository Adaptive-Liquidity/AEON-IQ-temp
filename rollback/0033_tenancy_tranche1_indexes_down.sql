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
-- Discovered by OID, not by (name, table-name). `conname` is unique only per
-- relation, so an unrelated table -- or another schema entirely -- may
-- legitimately hold a constraint called `entities_id_tenant_id_key`, and
-- matching on the bare name would block a rollback that has nothing to do with
-- it. But pinning each name to a literal table name has the opposite failure:
-- `ALTER TABLE ... RENAME` moves a table and leaves its constraint names alone,
-- so a renamed child stops matching and the guard reports "none" while 0041 is
-- still fully applied.
--
-- That was not a cosmetic inaccuracy here. PostgreSQL does not rename indexes
-- when their table is renamed, so every `DROP INDEX` below still resolved. A
-- rollback run against a renamed child therefore passed the guard, dropped the
-- lookup indexes -- which no constraint owns, so nothing objected -- and deleted
-- ledger versions 33-40, while 0041's foreign keys stayed attached. The tranche
-- was left half-unwound, with a ledger claiming the indexes had never been
-- built.
--
-- Both kinds are now found through something a rename does not disturb:
--   * the five foreign keys through `conindid`, the OID of
--     `agents_tenant_id_id_key` on the parent -- the one key all five depend on,
--     whatever their own tables are called now;
--   * the three unique constraints through the OID of the index they adopted,
--     which keeps its name across a table rename.
-- Each is reported with `conrelid::regclass`, so the message names where the
-- constraint actually is rather than where this script expected it.
--
-- Both kinds must be checked. The three unique constraints own indexes this
-- script drops. The five foreign keys do not own them, but they are 0041's work
-- and unwinding out of order would leave them referencing columns that
-- rollback/0032 is about to remove -- so this script refuses rather than letting
-- the operator discover that two steps later.
DO $$
DECLARE
    remaining  TEXT;
    agents_idx oid;
BEGIN
    SELECT c.conindid INTO agents_idx
      FROM pg_catalog.pg_constraint c
     WHERE c.conname = 'agents_tenant_id_id_key'
       AND c.conrelid = 'public.agents'::regclass
       AND c.contype = 'u';

    SELECT string_agg(found.label, ', ' ORDER BY found.label) INTO remaining FROM (
        -- The five ownership keys, reached from the parent's unique key.
        SELECT format('%s on %s', c.conname, c.conrelid::regclass) AS label
          FROM pg_catalog.pg_constraint c
         WHERE c.contype = 'f'
           AND agents_idx IS NOT NULL
           AND c.conindid = agents_idx
           AND c.conname IN ('archival_batches_tenant_agent_fkey',
                             'audit_logs_tenant_agent_fkey',
                             'entities_tenant_agent_fkey',
                             'memory_graph_tenant_agent_fkey',
                             'rmk_policies_tenant_agent_fkey')
        UNION ALL
        -- The three adopted unique constraints, reached from their own index.
        SELECT format('%s on %s', c.conname, c.conrelid::regclass) AS label
          FROM (VALUES
                  ('archival_batches_id_tenant_id_key'),
                  ('entities_id_tenant_id_key'),
                  ('rmk_policies_id_tenant_id_key')
               ) AS expected(name)
          JOIN pg_catalog.pg_constraint c
            ON c.conname = expected.name
           AND c.contype = 'u'
           AND c.conindid = to_regclass(format('public.%I', expected.name))
    ) AS found;

    IF remaining IS NOT NULL THEN
        RAISE EXCEPTION
            'migration 0041 is still applied: %. '
            'Run rollback/0041_tenancy_tranche1_constraints_down.sql first.',
            remaining
            USING ERRCODE = 'dependent_objects_still_exist';
    END IF;
END $$;

-- Refuse if any index this script drops now belongs to a table it does not name.
--
-- The drops below are by index name, and an index keeps its name when its table
-- is renamed, so without this check a rollback scoped to `public.rmk_policies`
-- would drop an index that is now serving `rmk_policies_old`. Resolved by
-- comparing each index's `indrelid` against the OID of the table this script
-- believes it is unwinding.
DO $$
DECLARE
    moved TEXT;
BEGIN
    SELECT string_agg(format('%s is on %s (expected public.%s)',
                             ic.relname, i.indrelid::regclass, expected.tbl),
                      ', ' ORDER BY ic.relname)
      INTO moved
      FROM (VALUES
              ('idx_archival_batches_tenant',        'archival_batches'),
              ('idx_audit_logs_tenant',              'audit_logs'),
              ('idx_entities_tenant',                'entities'),
              ('idx_memory_graph_tenant',            'memory_graph'),
              ('idx_rmk_policies_tenant',            'rmk_policies'),
              ('archival_batches_id_tenant_id_key',  'archival_batches'),
              ('entities_id_tenant_id_key',          'entities'),
              ('rmk_policies_id_tenant_id_key',      'rmk_policies')
           ) AS expected(idx, tbl)
      JOIN pg_catalog.pg_class ic
        ON ic.oid = to_regclass(format('public.%I', expected.idx))
      JOIN pg_catalog.pg_index i
        ON i.indexrelid = ic.oid
     WHERE i.indrelid IS DISTINCT FROM to_regclass(format('public.%I', expected.tbl));

    IF moved IS NOT NULL THEN
        RAISE EXCEPTION
            'a tranche 1 index belongs to a table this script does not name: %. The table was '
            'renamed after the tranche was applied, and an index keeps its name across a rename, '
            'so dropping by name would unwind a table this rollback is not scoped to. Nothing '
            'has been dropped. Rename the table back to what migration 0032 created, then '
            're-run.',
            moved
            USING ERRCODE = 'object_not_in_prerequisite_state';
    END IF;
END $$;

-- Refuse if a tranche table is not in `public` at all.
--
-- The index check below starts from `to_regclass('public.<index>')`, which is
-- rename-proof only because a rename leaves the index where it was. It is NOT
-- move-proof: `ALTER TABLE ... SET SCHEMA` takes the table's indexes with it, so
-- that lookup yields NULL, the join produces no row, and the guard reports
-- nothing. The `DROP INDEX IF EXISTS public.<name>` statements then no-op
-- silently and the ledger delete still runs. Measured on pg16: with `audit_logs`
-- moved to another schema, this script emitted one "does not exist, skipping"
-- notice and COMMITted, reporting success while the table, its bridge trigger and
-- its lookup index were all still live somewhere else.
--
-- Checking that each expected table is present in `public` is the unambiguous
-- form of that question, and it subsumes moved, renamed and dropped in one test.
-- Detecting the moved object directly is not reliable: an unrelated
-- `decoy.entities` may legitimately carry an index of the same name -- the
-- existing `identically_named_constraints_elsewhere_do_not_block_rollback` test
-- requires exactly that not to block -- and nothing distinguishes the two by
-- catalog shape alone. All five tables are created by migrations 0001, 0006 and
-- 0017, so a healthy database never trips this.
DO $$
DECLARE
    missing TEXT;
BEGIN
    SELECT string_agg(expected.tbl, ', ' ORDER BY expected.tbl)
      INTO missing
      FROM (VALUES
              ('archival_batches'), ('audit_logs'), ('entities'),
              ('memory_graph'), ('rmk_policies')
           ) AS expected(tbl)
     WHERE to_regclass(format('public.%I', expected.tbl)) IS NULL;

    IF missing IS NOT NULL THEN
        RAISE EXCEPTION
            'tranche 1 table(s) missing from schema public: %. This rollback drops indexes and '
            'retires ledger versions 33-40 by name, so it cannot prove those objects are gone '
            'while the table they belong to is not where it is expected -- it was renamed, moved '
            'to another schema, or dropped. Nothing has been dropped. Restore the table to '
            'public under the name migration 0032 created, then re-run.',
            missing
            USING ERRCODE = 'object_not_in_prerequisite_state';
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
