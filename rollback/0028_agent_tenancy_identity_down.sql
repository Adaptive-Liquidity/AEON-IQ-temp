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
-- modified, moved or deleted pre-existing data.  `agents.agent_id`, its global
-- UNIQUE constraint and both dependent foreign keys (`sessions.agent_id`,
-- `archival_batches.agent_id`) are untouched by both the migration and this
-- script, so every V1 row and relationship survives the round trip.
--
-- Rows created through the tenant-aware path need one extra step.  Their
-- `agent_id` is a compatibility key (the row's UUID), not the caller-facing
-- identifier, which lives in `external_agent_id`.  Dropping that column without
-- restoring it first would leave the agent reachable only by a UUID it never
-- advertised, and re-applying 0028 would then backfill `external_agent_id` from
-- the UUID — an identifier no operator input reproduces.  The first statement
-- below therefore moves the caller-facing identifier back into `agent_id`.
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

-- ── Restore caller-facing identifiers ────────────────────────────────────────
-- Runs before anything is dropped.  Skipped entirely when 0028 was never
-- applied; plpgsql plans the statements lazily, so the guard is enough to keep
-- the references to `external_agent_id` from being resolved in that case.
DO $$
DECLARE
    unrepresentable BIGINT;
BEGIN
    IF NOT EXISTS (
        SELECT 1
        FROM information_schema.columns
        WHERE table_schema = current_schema()
          AND table_name   = 'agents'
          AND column_name  = 'external_agent_id'
    ) THEN
        RETURN;
    END IF;

    SELECT COUNT(*) INTO unrepresentable FROM (
        -- Several agents share one caller-facing identifier: the baseline's
        -- global UNIQUE on agent_id can hold at most one of them.
        SELECT external_agent_id
          FROM agents
         GROUP BY external_agent_id
        HAVING COUNT(*) > 1
        UNION ALL
        -- Or the identifier we would restore is already held by another row.
        SELECT a.external_agent_id
          FROM agents a
         WHERE a.agent_id IS DISTINCT FROM a.external_agent_id
           AND EXISTS (
               SELECT 1 FROM agents b
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

    UPDATE agents
       SET agent_id = external_agent_id
     WHERE agent_id IS DISTINCT FROM external_agent_id;
END $$;

DROP TRIGGER  IF EXISTS agents_bridge_identity_columns_trg ON agents;
DROP FUNCTION IF EXISTS agents_bridge_identity_columns();

ALTER TABLE agents DROP CONSTRAINT IF EXISTS agents_tenant_id_external_agent_id_key;
ALTER TABLE agents DROP CONSTRAINT IF EXISTS agents_tenant_id_id_key;

DROP INDEX IF EXISTS idx_agents_unmapped;

ALTER TABLE agents DROP COLUMN IF EXISTS external_agent_id;
ALTER TABLE agents DROP COLUMN IF EXISTS tenant_id;

DROP INDEX IF EXISTS idx_agent_tenancy_migrations_applied;
DROP TABLE IF EXISTS agent_tenancy_migrations;

-- Let `sqlx migrate run` re-apply 0028 cleanly afterwards.  Guarded because the
-- ledger table does not exist on a database whose migrations were applied by
-- some other tool.
DO $$
BEGIN
    IF to_regclass('_sqlx_migrations') IS NOT NULL THEN
        DELETE FROM _sqlx_migrations WHERE version = 28;
    END IF;
END $$;

COMMIT;
