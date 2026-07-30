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
-- Neither kind is looked up by its own name any more, and that was still a hole
-- at the previous head: renaming a foreign key hid it from the `conname` filter
-- even though `conindid` still identified it, and renaming a unique constraint
-- renames its backing index too, so the index-OID lookup found nothing either.
-- Either way this guard reported "0041 is not applied", the drops below ran, and
-- versions 33-40 were retired while 0041 was still attached.
--
-- Found instead by what cannot be renamed away:
--   * the five foreign keys through `conindid`, the OID of
--     `agents_tenant_id_id_key` on the parent -- the one key all five depend on;
--   * the three unique constraints by their column shape.
--
-- BOTH ARMS STAY SCOPED TO THE TRANCHE'S TABLES BY OID, and dropping that scope
-- would be a real regression rather than a simplification. Measured on pg16,
-- migration 0030's `credential_agent_grants_agent_fkey` has the identical shape
-- -- `(tenant_id, agent_uuid)` referencing `agents (tenant_id, id)` through the
-- same `conindid` -- and migration 0029's `credentials_id_tenant_id_key` is
-- likewise `UNIQUE (id, tenant_id)`. A shape-only test would report both as
-- tranche 1 objects and refuse this rollback forever.
--
-- A renamed or schema-moved TABLE escapes both arms, because their scope is
-- exactly the thing that moved. That is covered, before any drop, by the
-- missing-table guard further down; the refusal simply comes from there instead,
-- and says the table is not in `public`.
--
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
        -- The five ownership keys: any foreign key backed by the parent's unique
        -- key that sits on a tranche table, whatever it is called now.
        SELECT format('%s on %s', c.conname, c.conrelid::regclass) AS label
          FROM pg_catalog.pg_constraint c
         WHERE c.contype = 'f'
           AND agents_idx IS NOT NULL
           AND c.conindid = agents_idx
           AND c.conrelid IN (to_regclass('public.archival_batches'),
                              to_regclass('public.audit_logs'),
                              to_regclass('public.entities'),
                              to_regclass('public.memory_graph'),
                              to_regclass('public.rmk_policies'))
        UNION ALL
        -- The three adopted unique constraints, by shape on a tranche table
        -- rather than by their own name, because renaming a unique constraint
        -- renames its backing index too and leaves nothing to look it up by.
        SELECT format('%s on %s', c.conname, c.conrelid::regclass) AS label
          FROM pg_catalog.pg_constraint c
         WHERE c.contype = 'u'
           AND NOT c.condeferrable AND NOT c.condeferred AND c.conparentid = 0
           AND c.conrelid IN (to_regclass('public.archival_batches'),
                              to_regclass('public.entities'),
                              to_regclass('public.rmk_policies'))
           AND (SELECT array_agg(a.attname::TEXT ORDER BY k.ord)
                  FROM unnest(c.conkey) WITH ORDINALITY AS k(attnum, ord)
                  JOIN pg_catalog.pg_attribute a
                    ON a.attrelid = c.conrelid AND a.attnum = k.attnum)
               = ARRAY['id', 'tenant_id']
    ) AS found;

    IF remaining IS NOT NULL THEN
        RAISE EXCEPTION
            'migration 0041 is still applied: %. '
            'Run rollback/0041_tenancy_tranche1_constraints_down.sql first.',
            remaining
            USING ERRCODE = 'dependent_objects_still_exist';
    END IF;
END $$;

-- Refuse unless each index this script drops is the one the tranche built.
--
-- Two distinct ways a name-based drop goes wrong, and both are checked here.
--
-- The table moved. The drops below are by index name, and an index keeps its
-- name when its table is renamed, so without this a rollback scoped to
-- `public.rmk_policies` would drop an index now serving `rmk_policies_old`.
-- Resolved by comparing `indrelid` against the OID of the table this script
-- believes it is unwinding.
--
-- The index is not ours. `indrelid` alone is not ownership: migrations
-- 0033-0040 build with `CREATE INDEX ... IF NOT EXISTS`, so a pre-existing index
-- holding a reserved name on the expected table is skipped by the build with a
-- notice, and a guard checking only the table would then let this script drop an
-- index the tranche never created and retire versions 33-40 over it. This is the
-- same defect the constraint guards in 0041 were rewritten to close, one object
-- kind further down. Columns are resolved by name through pg_attribute for the
-- same reason they are there: attnums differ per table and shift after any
-- rollback/re-apply cycle.
--
-- Expected shape, as migrations 0033-0040 build it: the five lookup indexes are
-- non-unique btree over (tenant_id, agent_uuid); the three unique indexes are
-- unique btree over (id, tenant_id); none is partial, none is an expression
-- index, all use plain ASC/NULLS LAST ordering, the default operator class and
-- no collation.
--
-- `indisvalid` is deliberately NOT part of this test. An interrupted
-- CREATE INDEX CONCURRENTLY leaves the index behind INVALID, and dropping it is
-- exactly what this script exists to do -- requiring validity here would make
-- the one state that needs recovery the one state that cannot be recovered.
-- Validity is checked where it actually matters, in migration 0041, which must
-- never ADOPT an invalid index as a constraint.
DO $$
DECLARE
    spec     RECORD;
    idx      RECORD;
    problems TEXT[];
    bad      TEXT[] := ARRAY[]::TEXT[];
    cols     TEXT[];
BEGIN
    FOR spec IN
        SELECT * FROM (VALUES
            ('idx_archival_batches_tenant',       'archival_batches', FALSE,
                 ARRAY['tenant_id', 'agent_uuid']),
            ('idx_audit_logs_tenant',             'audit_logs',       FALSE,
                 ARRAY['tenant_id', 'agent_uuid']),
            ('idx_entities_tenant',               'entities',         FALSE,
                 ARRAY['tenant_id', 'agent_uuid']),
            ('idx_memory_graph_tenant',           'memory_graph',     FALSE,
                 ARRAY['tenant_id', 'agent_uuid']),
            ('idx_rmk_policies_tenant',           'rmk_policies',     FALSE,
                 ARRAY['tenant_id', 'agent_uuid']),
            ('archival_batches_id_tenant_id_key', 'archival_batches', TRUE,
                 ARRAY['id', 'tenant_id']),
            ('entities_id_tenant_id_key',         'entities',         TRUE,
                 ARRAY['id', 'tenant_id']),
            ('rmk_policies_id_tenant_id_key',     'rmk_policies',     TRUE,
                 ARRAY['id', 'tenant_id'])
        ) AS s(idx_name, tbl, is_unique, cols)
        ORDER BY s.idx_name
    LOOP
        SELECT i.indrelid, i.indisunique, i.indnkeyatts, i.indnatts, i.indkey,
               i.indpred IS NOT NULL AS is_partial,
               i.indexprs IS NOT NULL AS is_expression,
               i.indnullsnotdistinct AS nulls_not_distinct,
               am.amname AS access_method,
               i.indoption::int2[] AS options,
               i.indcollation::oid[] AS collations,
               (SELECT bool_and(oc.opcdefault AND oc.opcmethod = ic.relam)
                  FROM unnest(i.indclass::oid[]) AS k(cls)
                  JOIN pg_catalog.pg_opclass oc ON oc.oid = k.cls) AS default_opclasses
          INTO idx
          FROM pg_catalog.pg_index i
          JOIN pg_catalog.pg_class ic ON ic.oid = i.indexrelid
          JOIN pg_catalog.pg_am    am ON am.oid = ic.relam
         WHERE i.indexrelid = to_regclass(format('public.%I', spec.idx_name));

        CONTINUE WHEN NOT FOUND;   -- already dropped, or moved with its table

        problems := ARRAY[]::TEXT[];

        IF idx.indrelid IS DISTINCT FROM to_regclass(format('public.%I', spec.tbl)) THEN
            problems := problems
                || format('it is on %s, expected public.%s', idx.indrelid::regclass, spec.tbl);
        END IF;
        IF idx.indisunique <> spec.is_unique THEN
            problems := problems
                || format('indisunique=%s, expected %s', idx.indisunique, spec.is_unique);
        END IF;
        IF idx.is_partial THEN
            problems := problems || 'it is partial; the tranche builds no predicate'::TEXT;
        END IF;
        IF idx.is_expression THEN
            problems := problems || 'it indexes an expression, not plain columns'::TEXT;
        END IF;
        IF idx.nulls_not_distinct THEN
            problems := problems
                || 'it is NULLS NOT DISTINCT; the tranche builds plain unique indexes, '
                   'which treat NULLs as distinct'::TEXT;
        END IF;
        IF idx.access_method IS DISTINCT FROM 'btree' THEN
            problems := problems
                || format('its access method is %L, expected %L', idx.access_method, 'btree');
        END IF;
        -- indoption is a per-column bitmask: bit 0 is DESC, bit 1 is NULLS FIRST.
        -- Zero is the plain ASC/NULLS LAST ordering these migrations build. Checked
        -- because `indkey` is blind to it: `(tenant_id DESC, agent_uuid)` has the
        -- same indkey, arity, uniqueness, predicate and expression state as the
        -- canonical index, and was accepted as tranche-owned.
        IF EXISTS (SELECT 1 FROM unnest(idx.options) AS o(v) WHERE o.v <> 0) THEN
            problems := problems
                || format('its column ordering is %L, expected all-zero '
                          '(ASC NULLS LAST); bit 0 is DESC, bit 1 is NULLS FIRST',
                          idx.options);
        END IF;
        IF EXISTS (SELECT 1 FROM unnest(idx.collations) AS c(v) WHERE c.v <> 0) THEN
            problems := problems
                || format('it carries collations %L; the uuid columns these '
                          'migrations index have none', idx.collations);
        END IF;
        IF idx.default_opclasses IS NOT TRUE THEN
            problems := problems
                || 'it uses a non-default operator class'::TEXT;
        END IF;
        IF idx.indnatts <> 2 OR idx.indnkeyatts <> 2 THEN
            problems := problems
                || format('it has %s column(s) (%s key), expected 2', idx.indnatts,
                          idx.indnkeyatts);
        ELSE
            SELECT array_agg(a.attname::TEXT ORDER BY k.ord)
              INTO cols
              FROM unnest(idx.indkey::int2[]) WITH ORDINALITY AS k(attnum, ord)
              JOIN pg_catalog.pg_attribute a
                ON a.attrelid = idx.indrelid AND a.attnum = k.attnum;
            IF cols IS DISTINCT FROM spec.cols THEN
                problems := problems
                    || format('columns are %L, expected %L', cols, spec.cols);
            END IF;
        END IF;

        IF cardinality(problems) > 0 THEN
            bad := bad || format('%s (%s)', spec.idx_name, array_to_string(problems, '; '));
        END IF;
    END LOOP;

    IF cardinality(bad) > 0 THEN
        RAISE EXCEPTION
            'refusing to drop indexes this tranche does not own: %. Each holds a name migrations '
            '0033-0040 use but is not the object they build, so dropping it would destroy '
            'something else and retire versions 33-40 over work that was never done. Nothing has '
            'been dropped. Rename or remove the occupant, or restore the table to the name '
            'migration 0032 created, then re-run.',
            array_to_string(bad, ' | ')
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
