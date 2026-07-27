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
-- No data loss: 0028 added columns and a record table but never modified,
-- moved or deleted pre-existing data.  `agents.agent_id`, its global UNIQUE
-- constraint and both dependent foreign keys (`sessions.agent_id`,
-- `archival_batches.agent_id`) are untouched by both the migration and this
-- script, so every V1 row and relationship survives the round trip.
--
-- What IS discarded: tenant assignments held in `agents.tenant_id` and the
-- audit rows in `agent_tenancy_migrations`.  Re-running the backfill after
-- re-applying 0028 reproduces them from the same operator inputs, because the
-- assignment is a pure function of the declared mode and mapping.

BEGIN;

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
