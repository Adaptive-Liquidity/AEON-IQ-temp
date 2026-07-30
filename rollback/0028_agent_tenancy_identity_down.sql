-- Rollback for migration 0028 (agent identity rework).
--
-- Returns the schema to the 0027 baseline.  Kept outside `migrations/` on
-- purpose: this repository uses sqlx's non-reversible migration naming
-- (`NNNN_name.sql`), so a `.down.sql` sibling inside that directory would make
-- the whole set inconsistent.  Apply manually:
--
--     psql "$DATABASE_URL" -v ON_ERROR_STOP=1 \
--       -f rollback/0028_agent_tenancy_identity_down.sql
--
-- No identifier is lost.  0028 added columns and a record table but never
-- modified, moved or deleted pre-existing data, and it left `agents.agent_id`
-- and its global UNIQUE constraint alone, so every V1 row and relationship
-- survives the round trip untouched.
--
-- Rows created through the tenant-aware path need one extra step.  Their
-- `agent_id` is a compatibility key (the row's UUID), not the caller-facing
-- identifier, which lives in `external_agent_id`.  Dropping that column without
-- restoring it first would leave the agent reachable only by a UUID it never
-- advertised, and re-applying 0028 would then backfill `external_agent_id` from
-- the UUID — an identifier no operator input reproduces.  The first block below
-- therefore moves the caller-facing identifier back into `agent_id`, which also
-- means it briefly drops and re-creates the two foreign keys that reference
-- that column (`sessions.agent_id`, `archival_batches.agent_id`) — see the
-- comment on that block for why.  Both come back with their original
-- definitions, and the block is skipped entirely when no identifier has moved,
-- which is every step-1 deployment.
--
-- Where the baseline schema genuinely cannot hold the data — two tenants
-- sharing one `external_agent_id`, which is exactly what the global UNIQUE on
-- `agent_id` forbids — this script raises instead of picking a winner.
--
-- What IS discarded: tenant assignments held in `agents.tenant_id` and the
-- audit rows in `agent_tenancy_migrations`.  Re-running the backfill after
-- re-applying 0028 reproduces them from the same operator inputs, because the
-- assignment is a pure function of the declared mode and mapping.

BEGIN;

-- Never trust the caller's session search_path.  Without this a schema placed
-- ahead of `public` could capture every unqualified name below, including the
-- function drop, which names no schema at all.  `SET LOCAL` confines it to this
-- transaction, so the operator's session is unchanged afterwards.
SET LOCAL search_path = pg_catalog, public;

-- ── Migration order ──────────────────────────────────────────────────────────
-- Migration 0030 (agent grants) hangs a composite foreign key off
-- `agents_tenant_id_id_key`, which this script drops.  Unwinding out of order
-- fails on a dependency error partway through — after the identifier restore
-- block below has already run.  Refusing up front, with the remedy named, is
-- the difference between "run this first" and "work out what state your
-- database is in now".
--
-- Deliberately NOT resolved with `DROP ... CASCADE`: cascading would silently
-- delete every agent grant, which is authorization data, as a side effect of a
-- rollback the operator asked for on a different migration.
DO $$
BEGIN
    IF to_regclass('public.credential_agent_grants') IS NOT NULL THEN
        RAISE EXCEPTION
            'migration 0030 (agent grants) is still applied and depends on constraints this '
            'rollback removes. Run rollback/0030_credential_agent_grants_down.sql first.'
            USING ERRCODE = 'dependent_objects_still_exist';
    END IF;
END $$;

-- ── Restore caller-facing identifiers ────────────────────────────────────────
-- Runs before anything is dropped.  Skipped entirely when 0028 was never
-- applied; plpgsql plans the statements lazily, so the guard is enough to keep
-- the references to `external_agent_id` from being resolved in that case.
DO $$
DECLARE
    unrepresentable BIGINT;
    fk              RECORD;
BEGIN
    IF NOT EXISTS (
        SELECT 1
        FROM information_schema.columns
        WHERE table_schema = 'public'
          AND table_name   = 'agents'
          AND column_name  = 'external_agent_id'
    ) THEN
        RETURN;
    END IF;

    -- Nothing was created through the tenant-aware path, so no identifier moved
    -- and the foreign keys below never need to be touched.  This is the only
    -- case a deployment of step 1 can actually be in, because `insert_agent` is
    -- not yet reachable from request handling.
    IF NOT EXISTS (
        SELECT 1 FROM public.agents WHERE agent_id IS DISTINCT FROM external_agent_id
    ) THEN
        RETURN;
    END IF;

    SELECT COUNT(*) INTO unrepresentable FROM (
        -- Several agents share one caller-facing identifier: the baseline's
        -- global UNIQUE on agent_id can hold at most one of them.
        SELECT external_agent_id
          FROM public.agents
         GROUP BY external_agent_id
        HAVING COUNT(*) > 1
        UNION ALL
        -- Or the identifier we would restore is already held by another row.
        SELECT a.external_agent_id
          FROM public.agents a
         WHERE a.agent_id IS DISTINCT FROM a.external_agent_id
           AND EXISTS (
               SELECT 1 FROM public.agents b
                WHERE b.agent_id = a.external_agent_id
                  AND b.id <> a.id
           )
    ) AS conflicts;

    IF unrepresentable > 0 THEN
        RAISE EXCEPTION
            'rollback would lose caller-facing identifiers for % agent(s): the 0027 '
            'schema keeps agents.agent_id globally unique and cannot represent them. '
            'Remove or rename the conflicting tenant-scoped agents, then retry.',
            unrepresentable;
    END IF;

    -- `sessions.agent_id` (0001_initial.sql:23) and `archival_batches.agent_id`
    -- (0006_archival_versioning.sql:10) reference the value about to change, and
    -- both are ON UPDATE NO ACTION -- PostgreSQL's default, which neither
    -- declaration overrides.  Renaming the parent while a child still points at
    -- the old value is therefore rejected outright and aborts the rollback.
    -- Drop those keys, repoint the children, rename the parents, then restore
    -- each constraint from `pg_get_constraintdef`, so the baseline definition
    -- comes back exactly as it was rather than as something re-typed here.
    CREATE TEMP TABLE _agents_fk_snapshot ON COMMIT DROP AS
        SELECT format('%I.%I', n.nspname, cl.relname) AS child_table,
               c.conname                   AS constraint_name,
               pg_get_constraintdef(c.oid) AS definition,
               a.attname                   AS child_column
          FROM pg_constraint c
          JOIN pg_class cl ON cl.oid = c.conrelid
          JOIN pg_namespace n ON n.oid = cl.relnamespace
          JOIN unnest(c.conkey) AS k(attnum) ON TRUE
          JOIN pg_attribute a
            ON a.attrelid = c.conrelid AND a.attnum = k.attnum
         WHERE c.contype  = 'f'
           AND c.confrelid = 'public.agents'::regclass
           -- Only keys that target agents(agent_id).  Once §10 step 4 repoints
           -- dependants at agents(id), those must not be caught by this.
           AND c.confkey = ARRAY[(
               SELECT attnum FROM pg_attribute
                WHERE attrelid = 'public.agents'::regclass AND attname = 'agent_id'
           )];

    FOR fk IN SELECT * FROM _agents_fk_snapshot LOOP
        EXECUTE format('ALTER TABLE %s DROP CONSTRAINT %I',
                       fk.child_table, fk.constraint_name);
    END LOOP;

    FOR fk IN SELECT * FROM _agents_fk_snapshot LOOP
        EXECUTE format(
            'UPDATE %s AS c SET %I = a.external_agent_id FROM public.agents a '
            'WHERE c.%I = a.agent_id '
            '  AND a.agent_id IS DISTINCT FROM a.external_agent_id',
            fk.child_table, fk.child_column, fk.child_column);
    END LOOP;

    UPDATE public.agents
       SET agent_id = external_agent_id
     WHERE agent_id IS DISTINCT FROM external_agent_id;

    FOR fk IN SELECT * FROM _agents_fk_snapshot LOOP
        EXECUTE format('ALTER TABLE %s ADD CONSTRAINT %I %s',
                       fk.child_table, fk.constraint_name, fk.definition);
    END LOOP;
END $$;

DROP TRIGGER  IF EXISTS agents_bridge_identity_columns_trg ON public.agents;
DROP FUNCTION IF EXISTS public.agents_bridge_identity_columns();

ALTER TABLE public.agents DROP CONSTRAINT IF EXISTS agents_tenant_id_external_agent_id_key;
ALTER TABLE public.agents DROP CONSTRAINT IF EXISTS agents_tenant_id_id_key;

DROP INDEX IF EXISTS public.idx_agents_unmapped;

ALTER TABLE public.agents DROP COLUMN IF EXISTS external_agent_id;
ALTER TABLE public.agents DROP COLUMN IF EXISTS tenant_id;

DROP INDEX IF EXISTS public.idx_agent_tenancy_migrations_applied;
DROP TABLE IF EXISTS public.agent_tenancy_migrations;

-- Let `sqlx migrate run` re-apply 0028 cleanly afterwards.  Guarded because the
-- ledger table does not exist on a database whose migrations were applied by
-- some other tool.
DO $$
BEGIN
    IF to_regclass('public._sqlx_migrations') IS NOT NULL THEN
        DELETE FROM public._sqlx_migrations WHERE version = 28;
    END IF;
END $$;

COMMIT;
