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
-- DROPS ONLY WHAT IT OWNS.  A name is not a claim of ownership.  Every drop
-- below is preceded by proving that the constraint holding the name IS the
-- object migration 0041 creates -- same kind, same ordered local columns, same
-- target table and target columns, same referential actions, same match type,
-- same deferrability, not partition-inherited.  If a same-named object is
-- present but different, this script raises and drops NOTHING: an unrelated
-- CHECK constraint called `entities_tenant_agent_fkey`, or a foreign key an
-- operator added by hand, belongs to whoever created it.  `DROP CONSTRAINT IF
-- EXISTS` by name alone would have deleted it silently.
--
-- The same identity test lives in migration 0041.  It is written out in both
-- files rather than shared, because sqlx has no include mechanism and a rollback
-- that depended on an object the forward migration installs could not run after
-- that object had been removed.
--
-- RENAME-PROOF DISCOVERY.  The tranche's foreign keys are found from the parent
-- side, by OID: every one of them is backed by `agents_tenant_id_id_key`, so
-- `conindid` identifies them whatever their own table is called now.  That
-- matters because `ALTER TABLE ... RENAME` moves a table without touching its
-- constraint names, while the drops below name tables literally.  Discovering by
-- (name, table-name) finds nothing for a renamed child, lets the script proceed,
-- and fails later on a raw "relation does not exist" -- or, in the sibling 0033
-- script, drops indexes that survived the rename.  Detected here, it refuses up
-- front and names the table that moved.
--
-- SAFE TO RE-RUN.  A constraint that is already gone is skipped.
BEGIN;

-- Never trust the caller's session search_path.  Without this a schema placed
-- ahead of `public` could capture every unqualified name below.  SET LOCAL
-- confines it to this transaction, so the operator's session is unchanged
-- afterwards.
SET LOCAL search_path = pg_catalog, public;

DO $$
DECLARE
    spec        RECORD;
    con         RECORD;
    moved       TEXT;
    stmt        TEXT;
    tbl_oid     oid;
    agents_oid  oid := 'public.agents'::regclass;
    agents_idx  oid;
    actual      TEXT[];
    problems    TEXT[];
    foreigners  TEXT[] := ARRAY[]::TEXT[];
    absent      TEXT[] := ARRAY[]::TEXT[];
    to_drop     TEXT[] := ARRAY[]::TEXT[];
BEGIN
    SELECT c.conindid INTO agents_idx
      FROM pg_catalog.pg_constraint c
     WHERE c.conname = 'agents_tenant_id_id_key'
       AND c.conrelid = agents_oid
       AND c.contype = 'u';

    -- ── Refuse if any tranche constraint has moved under another table name ──
    -- Both kinds are checked, and the unique half is not redundant.  Checking
    -- only the five foreign keys looks sufficient -- every table carrying an
    -- adopted unique constraint carries an ownership key too -- but that holds
    -- only while 0041 is fully applied.  Measured on pg16: drop the five keys
    -- (the state an interrupted rollback leaves), rename `rmk_policies`, and a
    -- foreign-key-only rename check finds nothing; the drop loop below then
    -- resolves `public.rmk_policies` to NULL, skips it as "already gone", drops
    -- the other two unique constraints, deletes ledger version 41 and reports
    -- success -- leaving `rmk_policies_id_tenant_id_key` attached to the renamed
    -- table with a ledger claiming 0041 was never applied.  That is the exact
    -- failure this guard exists to prevent, reachable through the half-unwound
    -- state rather than the fully-applied one.
    --
    -- Each kind is reached through a handle a rename does not disturb: the keys
    -- through `conindid` on the parent's unique key, the unique constraints
    -- through the OID of the index they adopted, which keeps its own name.
    -- `agents_idx` is NULL only when 0028 is already unwound, in which case no
    -- ownership key can exist either, so that arm simply yields nothing.
    SELECT string_agg(found.label, ', ' ORDER BY found.label) INTO moved FROM (
        SELECT format('%s is now on %s (expected public.%s)',
                      c.conname, c.conrelid::regclass, expected.tbl) AS label
          FROM pg_catalog.pg_constraint c
          JOIN (VALUES
                  ('archival_batches_tenant_agent_fkey', 'archival_batches'),
                  ('audit_logs_tenant_agent_fkey',       'audit_logs'),
                  ('entities_tenant_agent_fkey',         'entities'),
                  ('memory_graph_tenant_agent_fkey',     'memory_graph'),
                  ('rmk_policies_tenant_agent_fkey',     'rmk_policies')
               ) AS expected(name, tbl) ON expected.name = c.conname
         WHERE c.contype = 'f'
           AND agents_idx IS NOT NULL
           AND c.conindid = agents_idx
           AND c.conrelid IS DISTINCT FROM to_regclass(format('public.%I', expected.tbl))
        UNION ALL
        SELECT format('%s is now on %s (expected public.%s)',
                      c.conname, c.conrelid::regclass, expected.tbl) AS label
          FROM (VALUES
                  ('archival_batches_id_tenant_id_key', 'archival_batches'),
                  ('entities_id_tenant_id_key',         'entities'),
                  ('rmk_policies_id_tenant_id_key',     'rmk_policies')
               ) AS expected(name, tbl)
          JOIN pg_catalog.pg_constraint c
            ON c.conname = expected.name
           AND c.contype = 'u'
           AND c.conindid = to_regclass(format('public.%I', expected.name))
         WHERE c.conrelid IS DISTINCT FROM to_regclass(format('public.%I', expected.tbl))
    ) AS found;

    IF moved IS NOT NULL THEN
        RAISE EXCEPTION
            'a tranche 1 constraint is attached to a table this script does not name: %. '
            'The table was renamed after the tranche was applied, so dropping by the names '
            'below would miss the constraint and leave the tranche half-attached. Nothing has '
            'been dropped. Rename the table back to what migration 0032 created, then re-run.',
            moved
            USING ERRCODE = 'object_not_in_prerequisite_state';
    END IF;

    -- ── Classify every name this script would drop ───────────────────────────
    FOR spec IN
        SELECT * FROM (VALUES
            ('f', 'archival_batches', 'archival_batches_tenant_agent_fkey',
                 ARRAY['tenant_id', 'agent_uuid'], ARRAY['tenant_id', 'id']),
            ('f', 'audit_logs',       'audit_logs_tenant_agent_fkey',
                 ARRAY['tenant_id', 'agent_uuid'], ARRAY['tenant_id', 'id']),
            ('f', 'entities',         'entities_tenant_agent_fkey',
                 ARRAY['tenant_id', 'agent_uuid'], ARRAY['tenant_id', 'id']),
            ('f', 'memory_graph',     'memory_graph_tenant_agent_fkey',
                 ARRAY['tenant_id', 'agent_uuid'], ARRAY['tenant_id', 'id']),
            ('f', 'rmk_policies',     'rmk_policies_tenant_agent_fkey',
                 ARRAY['tenant_id', 'agent_uuid'], ARRAY['tenant_id', 'id']),
            ('u', 'archival_batches', 'archival_batches_id_tenant_id_key',
                 ARRAY['id', 'tenant_id'],         NULL::TEXT[]),
            ('u', 'entities',         'entities_id_tenant_id_key',
                 ARRAY['id', 'tenant_id'],         NULL::TEXT[]),
            ('u', 'rmk_policies',     'rmk_policies_id_tenant_id_key',
                 ARRAY['id', 'tenant_id'],         NULL::TEXT[])
        ) AS s(kind, tbl, con_name, local_cols, ref_cols)
        ORDER BY s.kind, s.tbl
    LOOP
        tbl_oid := to_regclass(format('public.%I', spec.tbl));

        -- A tranche table missing from `public` is refused, not skipped.
        --
        -- Skipping it was a hole with the same shape as the rename one above, but
        -- reached differently: `ALTER TABLE ... SET SCHEMA` moves the table AND
        -- its indexes, so the rename guard's `to_regclass('public.<index>')`
        -- lookup yields NULL and finds nothing, and this loop then treated the
        -- table as already gone. Measured: with the sibling foreign key already
        -- dropped by hand and `archival_batches` moved to another schema, the
        -- script dropped what it could reach and committed, leaving
        -- `archival_batches_id_tenant_id_key` attached in the new schema.
        --
        -- Detecting the move itself is not reliable -- an unrelated
        -- `decoy.entities` may legitimately hold a constraint of the same name,
        -- and no reverse dependency distinguishes the two -- but the absence of
        -- the table this script is scoped to is unambiguous, and it covers being
        -- moved, renamed and dropped at once. All five tables are created by
        -- migrations 0001, 0006 and 0017, so a healthy database never reaches
        -- this branch.
        IF tbl_oid IS NULL THEN
            absent := absent || format('%s (for %s)', spec.tbl, spec.con_name);
            CONTINUE;
        END IF;

        SELECT c.contype, c.conkey, c.confrelid, c.confkey, c.confupdtype,
               c.confdeltype, c.confmatchtype, c.condeferrable, c.condeferred,
               c.conparentid
          INTO con
          FROM pg_catalog.pg_constraint c
         WHERE c.conname = spec.con_name
           AND c.conrelid = tbl_oid;

        CONTINUE WHEN NOT FOUND;         -- already dropped; re-running is a no-op

        problems := ARRAY[]::TEXT[];

        IF con.contype IS DISTINCT FROM spec.kind THEN
            problems := problems
                || format('constraint type is %L, expected %L', con.contype, spec.kind);
        END IF;

        SELECT array_agg(a.attname::TEXT ORDER BY k.ord)
          INTO actual
          FROM unnest(con.conkey) WITH ORDINALITY AS k(attnum, ord)
          JOIN pg_catalog.pg_attribute a
            ON a.attrelid = tbl_oid AND a.attnum = k.attnum;
        IF actual IS DISTINCT FROM spec.local_cols THEN
            problems := problems
                || format('local columns are %L, expected %L', actual, spec.local_cols);
        END IF;

        IF con.condeferrable OR con.condeferred THEN
            problems := problems
                || format('deferrability is (condeferrable=%s, condeferred=%s), expected (f, f)',
                          con.condeferrable, con.condeferred);
        END IF;

        IF con.conparentid <> 0 THEN
            problems := problems || 'constraint is inherited from a partitioned parent'::TEXT;
        END IF;

        IF spec.kind = 'f' THEN
            IF con.confrelid IS DISTINCT FROM agents_oid THEN
                problems := problems
                    || format('references %s, expected public.agents',
                              COALESCE(con.confrelid::regclass::TEXT, 'no table'));
            ELSE
                SELECT array_agg(a.attname::TEXT ORDER BY k.ord)
                  INTO actual
                  FROM unnest(con.confkey) WITH ORDINALITY AS k(attnum, ord)
                  JOIN pg_catalog.pg_attribute a
                    ON a.attrelid = con.confrelid AND a.attnum = k.attnum;
                IF actual IS DISTINCT FROM spec.ref_cols THEN
                    problems := problems
                        || format('referenced columns are %L, expected %L',
                                  actual, spec.ref_cols);
                END IF;
            END IF;
            IF con.confupdtype IS DISTINCT FROM 'a' THEN
                problems := problems
                    || format('ON UPDATE action is %L, expected %L (NO ACTION)',
                              con.confupdtype, 'a');
            END IF;
            IF con.confdeltype IS DISTINCT FROM 'a' THEN
                problems := problems
                    || format('ON DELETE action is %L, expected %L (NO ACTION)',
                              con.confdeltype, 'a');
            END IF;
            IF con.confmatchtype IS DISTINCT FROM 's' THEN
                problems := problems
                    || format('match type is %L, expected %L (MATCH SIMPLE)',
                              con.confmatchtype, 's');
            END IF;
        END IF;

        IF cardinality(problems) > 0 THEN
            foreigners := foreigners
                || format('%s on public.%s (%s)',
                          spec.con_name, spec.tbl, array_to_string(problems, '; '));
        ELSE
            to_drop := to_drop || format('ALTER TABLE public.%I DROP CONSTRAINT %I',
                                         spec.tbl, spec.con_name);
        END IF;
    END LOOP;

    -- ── Refuse before dropping anything, or drop everything verified ─────────
    -- A missing table is a prerequisite-state problem, not an ownership dispute,
    -- so it raises 55000 with the rename guard's wording rather than 42710's
    -- "this is somebody else's object".
    IF cardinality(absent) > 0 THEN
        RAISE EXCEPTION
            'tranche 1 table(s) missing from schema public: %. This script drops constraints and '
            'retires ledger version 41 by name, so it cannot prove those constraints are gone '
            'while the table holding them is not where it is expected -- it was renamed, moved to '
            'another schema, or dropped. Nothing has been dropped. Restore the table to public '
            'under the name migration 0032 created, then re-run.',
            array_to_string(absent, ', ')
            USING ERRCODE = 'object_not_in_prerequisite_state';
    END IF;

    IF cardinality(foreigners) > 0 THEN
        RAISE EXCEPTION
            'refusing to drop constraints this tranche does not own: %. Each of these holds a '
            'name migration 0041 uses but is a different object, so dropping it would destroy '
            'something else. Nothing has been dropped. Rename the occupant, or drop it '
            'deliberately yourself, then re-run.',
            array_to_string(foreigners, ' | ')
            USING ERRCODE = 'duplicate_object';
    END IF;

    FOREACH stmt IN ARRAY to_drop LOOP
        EXECUTE stmt;
    END LOOP;
END $$;

DO $$
BEGIN
    IF to_regclass('public._sqlx_migrations') IS NOT NULL THEN
        DELETE FROM public._sqlx_migrations WHERE version = 41;
    END IF;
END $$;

COMMIT;
