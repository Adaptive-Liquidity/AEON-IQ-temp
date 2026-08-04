-- ============================================================================
-- 0041  Tenancy tranche 1 constraint attachment  (AEON plan §4, decision A-6)
-- ============================================================================
--
-- Step 4B-1 PREPARE, part three and last.  Adopts the unique indexes 0033 built
-- and declares the composite foreign keys TRANCHE_1_ROOTS_AND_DIRECT_AGENT
-- _CHILDREN names in src/tenancy/plan.rs.
--
-- TRANSACTIONAL, deliberately.  This file carries no `-- no-transaction`
-- marker: measured on pgvector/pgvector:pg16, `ADD CONSTRAINT ... USING INDEX`
-- is indifferent to transaction context and succeeds inside an explicit
-- BEGIN/COMMIT.  The "cannot run inside a transaction block" restriction
-- belongs to CREATE INDEX CONCURRENTLY alone, which is why the concurrent
-- builds live in 0033-0040 and the attachments live here.  Keeping them separate is
-- what lets every statement in this file be atomic.
--
-- ADDITIVE AND REVERSIBLE.  `rollback/0041_tenancy_tranche1_constraints_down.sql`
-- drops exactly what this creates.
--
-- LOCKS.  Two distinct profiles, and conflating them is the mistake this split
-- exists to avoid:
--   * ADD CONSTRAINT ... USING INDEX — ACCESS EXCLUSIVE on the table alone, and
--     brief.  It adopts an index that already exists rather than building one,
--     so it examines no rows and names no parent.  Measured: it performs no
--     scan for a UNIQUE over NULL-able columns, and both columns remain
--     attnotnull = f afterwards.  A scan would only arise from NOT NULL
--     marking, which PRIMARY KEY does and UNIQUE does not.
--   * ADD CONSTRAINT ... FOREIGN KEY ... NOT VALID — SHARE ROW EXCLUSIVE on
--     this table AND on agents.  It is brief because NOT VALID skips the
--     historical scan; the scan is FINALIZE's job, under the weaker SHARE
--     UPDATE EXCLUSIVE.
--
-- NOT VALID IS NOT "NOT ENFORCED".  It means existing rows are not scanned.
-- Every INSERT and UPDATE after this migration commits is checked in full.
-- Tranche 1's foreign keys all point at agents, and every writer already
-- supplies a resolvable agent_id, so nothing new can fail here — but this is
-- the reason tranche 2 cannot be scheduled until upsert_working_memory writes
-- its parent first.
--
-- MATCH SIMPLE.  All five keys are composite over NULL-able columns, so
-- PostgreSQL skips the check entirely for any row with a NULL component.  The
-- foreign key is therefore NOT evidence that historical rows are owned during
-- the transition — the audit is.  That is not a defect being tolerated; it is
-- why PREPARE, BACKFILL and FINALIZE are three stages instead of one.
--
-- Idempotency: PostgreSQL has no ADD CONSTRAINT IF NOT EXISTS, so each is
-- wrapped in a pg_constraint existence guard, matching 0028's pattern.
-- ============================================================================

-- ── Refuse to adopt a broken concurrent build ───────────────────────────────
-- A CREATE INDEX CONCURRENTLY that fails leaves the index behind marked
-- INVALID.  It is not usable by the planner and, for a unique index, it
-- enforces nothing — but it exists, so the builds' IF NOT EXISTS skip it and
-- they would otherwise report success over a broken object.
--
-- The guard lives here rather than with the builds because this is the file
-- that ADOPTS them: an INVALID unique index must never become a constraint.
-- Recovery is NOT "drop the index and re-run its build migration". By the time
-- this guard fires, sqlx has typically recorded versions 0033-0040 in
-- `_sqlx_migrations` -- though not necessarily all of them, since a crash can
-- leave a built index with no ledger row. This guard reads
-- `pg_index.indisvalid` directly and is ledger-state-agnostic, so it fires
-- either way. What the ledger decides is whether re-running helps: a failed
-- CONCURRENTLY build leaves the index behind INVALID, and `sqlx migrate run`
-- re-executes any file it has NOT recorded, whose
-- `IF NOT EXISTS` skips the broken index with a notice and lets the migration
-- succeed. The version is then recorded over an index that enforces nothing,
-- and sqlx will never run that file again.
--
-- The authoritative path is to run rollback/0033_tenancy_tranche1_indexes_down
-- .sql, which drops all eight indexes AND deletes ledger versions 33-40, then
-- `sqlx migrate run` to rebuild them. Measured end to end on pg16: guard fires
-- -> rollback leaves 0 indexes and 0 ledger rows for 33-40 -> rerun rebuilds
-- all eight valid -> this migration applies clean.
--
-- The `IF NOT EXISTS` on those builds is deliberate and stays. Removing it
-- would make a failed build louder but would strand the *other* failure mode:
-- measured, a crash after a successful build but before the ledger write leaves
-- a valid index, and re-running without `IF NOT EXISTS` fails with `relation
-- already exists` and no forward path, where `IF NOT EXISTS` yields a notice
-- and completes. This guard is what keeps the permissive build honest.
--
-- The Step 4A verifier reports the same condition as drift, independently.

-- Never trust the caller's session search_path, matching this migration's own
-- rollback and the 0028/0032/0033 scripts.
--
-- Schema-qualifying every relation name is NOT sufficient on its own, and the
-- asymmetry is easy to miss: `'public.agents'::regclass` is a type-input cast and
-- cannot be shadowed, but `to_regclass(...)` is an ordinary, overridable catalog
-- function. Qualifying the string ARGUMENT does nothing to protect the FUNCTION
-- CALL. Demonstrated on pg16 with `search_path = hostile, public, pg_catalog` and
-- a `hostile.to_regclass(text)`: the cast still resolved `agents`, the function
-- call returned an attacker-chosen decoy. Since the guard below resolves table
-- OIDs through `to_regclass` and then reads pg_constraint against them, a
-- poisoned resolver could make a wrong-shape occupant look correct and defeat the
-- very check this migration adds. The DDL itself still targets literal
-- `public.<table>`, so the target cannot be redirected -- only the guard fooled.
--
-- SET LOCAL is confined to this transaction, which this file has because it
-- carries no `-- no-transaction` marker.
SET LOCAL search_path = pg_catalog, public;

DO $$
DECLARE
    broken TEXT;
BEGIN
    SELECT string_agg(c.relname, ', ' ORDER BY c.relname)
      INTO broken
      FROM pg_catalog.pg_index i
      JOIN pg_catalog.pg_class c ON c.oid = i.indexrelid
      JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
     WHERE n.nspname = 'public'
       AND NOT i.indisvalid
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

    IF broken IS NOT NULL THEN
        RAISE EXCEPTION
            'tranche 1 concurrent index build left INVALID indexes: %. '
            'Re-running the build migration will NOT fix this: sqlx may already '
            'have recorded some of versions 33-40, and those files skip an '
            'existing index. '
            'Run rollback/0033_tenancy_tranche1_indexes_down.sql (it drops all '
            'eight indexes and deletes ledger versions 33-40), then re-run '
            'sqlx migrate. An INVALID unique index enforces nothing and must '
            'not be adopted as a constraint below.',
            broken
            USING ERRCODE = 'object_not_in_prerequisite_state';
    END IF;
END $$;

-- ── Attach the tranche's constraints, or refuse ──────────────────────────────
-- Three UNIQUE constraints adopting the concurrent builds from 0038-0040, and
-- five composite ownership foreign keys.
--
-- The UNIQUE names match the build migrations' index names exactly, because
-- `ADD CONSTRAINT ... UNIQUE USING INDEX` takes the index's name.  Guarded on
-- pg_constraint rather than pg_class: after adoption the index still exists
-- under the same name, so an index-existence check would be satisfied by the
-- build alone and would skip the adoption entirely.
--
-- The foreign keys are decision A-6 made structural: a row whose tenant_id does
-- not match its agent's tenant becomes unrepresentable, rather than relying on
-- every future write path remembering to keep the two aligned.  agents
-- (tenant_id, id) is the already-current unique target created by 0028; this
-- migration does not recreate it.
--
-- A NAME IS NOT AN IDENTITY.  This guard does not ask "does a constraint of this
-- name exist on this table"; it asks "is the constraint of this name on this
-- table the one this migration would have created".  The distinction is not
-- academic, because a name-and-table test is satisfied exactly by:
--   * a CHECK constraint called `entities_tenant_agent_fkey`,
--   * a foreign key over the wrong local columns,
--   * a foreign key pointing at the wrong table or the wrong target columns,
--   * a foreign key with ON DELETE CASCADE instead of NO ACTION,
--   * a UNIQUE constraint over the wrong pair of columns.
-- In every one of those cases the old guard skipped the real object, sqlx
-- recorded version 41 as applied over a schema that never received its
-- constraint, and `rollback/0041` would later drop the unrelated occupant by
-- name.
--
-- So every axis that distinguishes the intended object is compared, and any
-- difference RAISES before this migration changes anything.  Refusing is the
-- only defensible answer: adopting the occupant records a constraint the
-- database does not have, and replacing it destroys an object this tranche does
-- not own.  The exception names the table, the constraint and every axis that
-- differs, because "constraint already exists" is not a next action.
--
-- Ordered so all three UNIQUE adoptions are considered before any foreign key.
-- The keys do not depend on them -- they depend on `agents_tenant_id_id_key`
-- from 0028 -- but a stable order makes the first failure reproducible.
--
-- `convalidated` is deliberately NOT compared.  FINALIZE validates these five
-- foreign keys, which flips it from f to t on the tranche's own object, and a
-- guard treating that as a mismatch would refuse to recognise the constraint
-- this migration created itself.  Every other axis already pins identity; the
-- observed value is reported in the diagnostic when some other axis fails.
--
-- Every identifier below is schema-qualified, and every column is resolved
-- through pg_attribute by name.  Neither is decoration: `conkey` holds attnums,
-- and the same logical column sits at a different attnum on each of the five
-- tables (measured on pg16: 8,7 / 7,6 / 10,9 / 9,8 / 13,12), so a guard written
-- against literal attnum arrays would be wrong on four tables out of five.
DO $$
DECLARE
    spec       RECORD;
    con        RECORD;
    idx        RECORD;
    tbl_oid    oid;
    agents_oid oid := 'public.agents'::regclass;
    actual     TEXT[];
    problems   TEXT[];
BEGIN
    FOR spec IN
        SELECT * FROM (VALUES
            ('u', 'archival_batches', 'archival_batches_id_tenant_id_key',
                 ARRAY['id', 'tenant_id'],         NULL::TEXT[]),
            ('u', 'entities',         'entities_id_tenant_id_key',
                 ARRAY['id', 'tenant_id'],         NULL::TEXT[]),
            ('u', 'rmk_policies',     'rmk_policies_id_tenant_id_key',
                 ARRAY['id', 'tenant_id'],         NULL::TEXT[]),
            ('f', 'archival_batches', 'archival_batches_tenant_agent_fkey',
                 ARRAY['tenant_id', 'agent_uuid'], ARRAY['tenant_id', 'id']),
            ('f', 'audit_logs',       'audit_logs_tenant_agent_fkey',
                 ARRAY['tenant_id', 'agent_uuid'], ARRAY['tenant_id', 'id']),
            ('f', 'entities',         'entities_tenant_agent_fkey',
                 ARRAY['tenant_id', 'agent_uuid'], ARRAY['tenant_id', 'id']),
            ('f', 'memory_graph',     'memory_graph_tenant_agent_fkey',
                 ARRAY['tenant_id', 'agent_uuid'], ARRAY['tenant_id', 'id']),
            ('f', 'rmk_policies',     'rmk_policies_tenant_agent_fkey',
                 ARRAY['tenant_id', 'agent_uuid'], ARRAY['tenant_id', 'id'])
        ) AS s(kind, tbl, con_name, local_cols, ref_cols)
        ORDER BY s.kind DESC, s.tbl
    LOOP
        tbl_oid := to_regclass(format('public.%I', spec.tbl));
        IF tbl_oid IS NULL THEN
            RAISE EXCEPTION
                'tranche 1 table public.% does not exist, so constraint % cannot be attached.',
                spec.tbl, spec.con_name
                USING ERRCODE = 'undefined_table';
        END IF;

        SELECT c.contype, c.conkey, c.confrelid, c.confkey, c.confupdtype,
               c.confdeltype, c.confmatchtype, c.convalidated, c.condeferrable,
               c.condeferred, c.conparentid, c.conindid
          INTO con
          FROM pg_catalog.pg_constraint c
         WHERE c.conname = spec.con_name
           AND c.conrelid = tbl_oid;

        -- ── Absent: create it ────────────────────────────────────────────────
        IF NOT FOUND THEN
            IF spec.kind = 'u' THEN
                -- Adopting an index that is not there fails with a bare
                -- "relation does not exist"; name what builds it instead.
                IF to_regclass(format('public.%I', spec.con_name)) IS NULL THEN
                    RAISE EXCEPTION
                        'unique index % is missing, so constraint % cannot be adopted. Run the '
                        'concurrent unique builds (migrations 0038-0040) first.',
                        spec.con_name, spec.con_name
                        USING ERRCODE = 'object_not_in_prerequisite_state';
                END IF;

                -- The index EXISTING is not the index being OURS, and this is
                -- the branch where that distinction bites hardest.
                -- `USING INDEX` derives the constraint's columns from whatever
                -- index it is handed, so adopting a wrong-shaped one produces a
                -- constraint that is not the promised key while sqlx records 41
                -- as applied. Measured: a pre-existing
                -- `archival_batches_id_tenant_id_key ON (id, agent_uuid)` --
                -- which 0038-0040 skip via IF NOT EXISTS -- was adopted as
                -- `UNIQUE (id, agent_uuid)` without complaint. The identity test
                -- further down only ever runs when a CONSTRAINT already exists,
                -- so it could not catch this.
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
                 WHERE i.indexrelid = to_regclass(format('public.%I', spec.con_name));

                problems := ARRAY[]::TEXT[];
                IF idx.indrelid IS DISTINCT FROM tbl_oid THEN
                    problems := problems
                        || format('it is on %s, expected public.%s',
                                  idx.indrelid::regclass, spec.tbl);
                END IF;
                IF NOT idx.indisunique THEN
                    problems := problems || 'it is not a unique index'::TEXT;
                END IF;
                IF idx.is_partial THEN
                    problems := problems || 'it is partial'::TEXT;
                END IF;
                IF idx.is_expression THEN
                    problems := problems || 'it indexes an expression'::TEXT;
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
                        || format('it has %s column(s) (%s key), expected 2',
                                  idx.indnatts, idx.indnkeyatts);
                ELSE
                    SELECT array_agg(a.attname::TEXT ORDER BY k.ord)
                      INTO actual
                      FROM unnest(idx.indkey::int2[]) WITH ORDINALITY AS k(attnum, ord)
                      JOIN pg_catalog.pg_attribute a
                        ON a.attrelid = idx.indrelid AND a.attnum = k.attnum;
                    IF actual IS DISTINCT FROM spec.local_cols THEN
                        problems := problems
                            || format('its columns are %L, expected %L',
                                      actual, spec.local_cols);
                    END IF;
                END IF;

                IF cardinality(problems) > 0 THEN
                    RAISE EXCEPTION
                        'index % is not the object migrations 0038-0040 build, so adopting it '
                        'would create a constraint this migration never promised: %. '
                        'ADD CONSTRAINT ... USING INDEX takes its columns from the index it is '
                        'given, so the wrong index silently becomes the wrong key. Nothing has '
                        'been changed. Rename or remove the occupying index, then re-run.',
                        spec.con_name, array_to_string(problems, '; ')
                        USING ERRCODE = 'object_not_in_prerequisite_state';
                END IF;

                EXECUTE format(
                    'ALTER TABLE public.%I ADD CONSTRAINT %I UNIQUE USING INDEX %I',
                    spec.tbl, spec.con_name, spec.con_name);
            ELSE
                EXECUTE format(
                    'ALTER TABLE public.%I ADD CONSTRAINT %I FOREIGN KEY (%I, %I) '
                    'REFERENCES public.agents (%I, %I) NOT VALID',
                    spec.tbl, spec.con_name,
                    spec.local_cols[1], spec.local_cols[2],
                    spec.ref_cols[1],   spec.ref_cols[2]);
            END IF;
            CONTINUE;
        END IF;

        -- ── Present: prove it is this tranche's object before skipping ───────
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
        ELSE
            -- A UNIQUE constraint is only as good as the index it owns, and an
            -- INVALID or partial one enforces nothing.
            SELECT i.indisunique, i.indisvalid, i.indpred IS NULL AS is_total,
                   i.indnkeyatts, i.indrelid, i.indnullsnotdistinct AS nulls_not_distinct,
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
             WHERE i.indexrelid = con.conindid;

            -- Held to the same axes as the adoption branch above. They diverged
            -- once -- adoption checked ordering, access method, collation and
            -- operator class while this branch did not -- and a constraint
            -- repointed at a DESC-ordered index was accepted here as the
            -- tranche's own. Reaching that state needs catalog surgery, because
            -- PostgreSQL refuses to ADD CONSTRAINT over a non-default-sorted
            -- index, so this is consistency rather than a reachable hole; the
            -- two checks living apart is the actual defect.
            -- Reported one axis at a time, for the same reason the adoption
            -- branch does. A single summary line printing five values while the
            -- condition tests ten is worse than no diagnostic: a DESC-ordered or
            -- BRIN occupant fails on an axis the line does not print, so every
            -- value in the message reads as correct and the operator is told the
            -- index is wrong in a way the evidence appears to contradict.
            IF NOT FOUND THEN
                problems := problems || 'constraint owns no index'::TEXT;
            ELSE
                IF NOT idx.indisunique THEN
                    problems := problems || 'its backing index is not unique'::TEXT;
                END IF;
                IF NOT idx.indisvalid THEN
                    problems := problems
                        || 'its backing index is INVALID, so it enforces nothing'::TEXT;
                END IF;
                IF NOT idx.is_total THEN
                    problems := problems || 'its backing index is partial'::TEXT;
                END IF;
                IF idx.indrelid <> tbl_oid THEN
                    problems := problems
                        || format('its backing index is on %s, expected public.%I',
                                  idx.indrelid::regclass, spec.tbl);
                END IF;
                IF idx.nulls_not_distinct THEN
                    problems := problems
                        || 'its backing index is NULLS NOT DISTINCT; the tranche builds plain '
                           'unique indexes, which treat NULLs as distinct'::TEXT;
                END IF;
                IF idx.access_method IS DISTINCT FROM 'btree' THEN
                    problems := problems
                        || format('its backing index access method is %L, expected %L',
                                  idx.access_method, 'btree');
                END IF;
                IF EXISTS (SELECT 1 FROM unnest(idx.options) AS o(v) WHERE o.v <> 0) THEN
                    problems := problems
                        || format('its backing index column ordering is %L, expected all-zero '
                                  '(ASC NULLS LAST); bit 0 is DESC, bit 1 is NULLS FIRST',
                                  idx.options);
                END IF;
                IF EXISTS (SELECT 1 FROM unnest(idx.collations) AS c(v) WHERE c.v <> 0) THEN
                    problems := problems
                        || format('its backing index carries collations %L; the uuid columns '
                                  'these migrations index have none', idx.collations);
                END IF;
                IF idx.default_opclasses IS NOT TRUE THEN
                    problems := problems
                        || 'its backing index uses a non-default operator class'::TEXT;
                END IF;
                IF idx.indnkeyatts <> 2 THEN
                    problems := problems
                        || format('its backing index has %s key column(s), expected 2',
                                  idx.indnkeyatts);
                END IF;
            END IF;
        END IF;

        IF cardinality(problems) > 0 THEN
            RAISE EXCEPTION
                'constraint % on public.% already exists but is NOT the tranche 1 object this '
                'migration creates: %. Migration 0041 refuses rather than adopting it, which '
                'would record version 41 over a constraint the database does not have, or '
                'replacing it, which would destroy an object this tranche does not own. Rename '
                'or remove the occupant, then re-run. Its validation state, which is not part of '
                'this test, is convalidated=%.',
                spec.con_name, spec.tbl, array_to_string(problems, '; '), con.convalidated
                USING ERRCODE = 'duplicate_object';
        END IF;
    END LOOP;
END $$;

COMMENT ON CONSTRAINT audit_logs_tenant_agent_fkey ON audit_logs IS
    'MATCH SIMPLE: rows with a NULL key component are not checked. audit_logs ownership stays '
    'permanently NULL-able because agentless events are legitimate, so this key never becomes '
    'evidence that every row is owned. The audit is that evidence.';
