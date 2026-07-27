-- Migration 0028: agent identity rework — tenant-scoped agent identity.
--
-- Step 1 of the AEON authentication & tenancy plan (§10 step 1), accepted at
-- Adaptive-Liquidity/nexus-planning @ b1fe06505d400e435c3ef8d10dc197f15641bebd.
--
-- ADDITIVE AND REVERSIBLE.  Nothing here drops, renames or repurposes an
-- existing column or constraint; `rollback/0028_agent_tenancy_identity_down.sql`
-- returns the schema to the baseline without data loss.
--
-- Establishes:
--   * agents.tenant_id          — tenant owner.  NULL means "not yet mapped".
--   * agents.external_agent_id  — the caller-facing string identifier.
--   * UNIQUE (tenant_id, external_agent_id) — the per-tenant name-space.
--   * UNIQUE (tenant_id, id)    — the FK target that §2.2's
--     `credential_agent_grants` and §4.1's `memories`/`entities` composite
--     foreign keys require.  Added here because those tables cannot reference
--     it before it exists (§10: "agent identity must precede the grant table").
--
-- Deliberately NOT done here — see docs/AGENT_IDENTITY_MIGRATION.md:
--   * `agents.agent_id` and its global UNIQUE constraint are retained.
--     `sessions.agent_id` (0001_initial.sql:23) and `archival_batches.agent_id`
--     (0006_archival_versioning.sql:10) are declared REFERENCES
--     agents(agent_id), and PostgreSQL requires a unique target, so dropping
--     that constraint now would break both foreign keys.  Its removal is
--     §4 migration-order step 7 / §10 step 7, after §10 step 4 repoints the
--     dependent tables at agents(id).
--   * No row is assigned a tenant.  Backfill is an explicit operator action
--     driven by src/tenancy.rs; there is no implicit or default tenant.

-- ── agents.id ────────────────────────────────────────────────────────────────
-- Already present at the baseline as `id UUID PRIMARY KEY DEFAULT
-- gen_random_uuid()` (0001_initial.sql:11), so every existing agent already
-- carries a stable UUID and no identifier has to be generated here.  The plan's
-- §3 table marks `agents.id` "(new)"; at commit 35e77ba it is not.  The
-- assertion below means a schema that somehow lacks it fails loudly rather than
-- silently skipping the guarantee this step is supposed to establish.
DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1
        FROM information_schema.columns
        WHERE table_schema = current_schema()
          AND table_name   = 'agents'
          AND column_name  = 'id'
          AND data_type    = 'uuid'
    ) THEN
        RAISE EXCEPTION
            'agents.id (uuid) is missing — 0001_initial.sql must be applied first';
    END IF;
END $$;

-- ── New identity columns ─────────────────────────────────────────────────────
ALTER TABLE agents ADD COLUMN IF NOT EXISTS tenant_id         UUID;
ALTER TABLE agents ADD COLUMN IF NOT EXISTS external_agent_id TEXT;

-- Preserve existing caller-facing identifiers verbatim.
UPDATE agents
   SET external_agent_id = agent_id
 WHERE external_agent_id IS NULL;

-- ── Legacy/new column bridge ─────────────────────────────────────────────────
-- V1 writes the legacy column only (`INSERT INTO agents (agent_id) VALUES ($1)`
-- — src/memory/store.rs:63, src/archival.rs:461).  A BEFORE ROW trigger fires
-- ahead of the NOT NULL check, so mirroring here keeps every existing write path
-- working unchanged while `external_agent_id` is still NOT NULL.
--
-- The reverse direction lets a tenant-aware caller supply only the new column.
-- The two values are NOT required to be equal: once two tenants may share an
-- `external_agent_id`, the globally-unique legacy column can no longer hold the
-- same string for both, so it becomes a compatibility key (see
-- src/tenancy.rs::legacy_compat_agent_id).  Supplying both explicitly is
-- honoured as given.
CREATE OR REPLACE FUNCTION agents_bridge_identity_columns() RETURNS TRIGGER AS $$
BEGIN
    IF NEW.external_agent_id IS NULL THEN
        NEW.external_agent_id := NEW.agent_id;
    END IF;
    IF NEW.agent_id IS NULL THEN
        NEW.agent_id := NEW.external_agent_id;
    END IF;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

DROP TRIGGER IF EXISTS agents_bridge_identity_columns_trg ON agents;
CREATE TRIGGER agents_bridge_identity_columns_trg
    BEFORE INSERT OR UPDATE ON agents
    FOR EACH ROW EXECUTE FUNCTION agents_bridge_identity_columns();

ALTER TABLE agents ALTER COLUMN external_agent_id SET NOT NULL;

-- ── Constraints ──────────────────────────────────────────────────────────────
-- `tenant_id` is nullable in this step and NULL is the explicit "unmapped"
-- state.  The plan's §6 target model is NOT NULL, and that is where this ends
-- up — but only once every row carries a tenant.  Encoding "unmapped" as NULL
-- is what makes migration Mode 2 fail closed: NULL never satisfies
-- `WHERE tenant_id = $1`, so an unmapped agent is unreachable from every
-- tenant-scoped query rather than being served to a default tenant.  Any
-- sentinel UUID would instead be a real, claimable tenant value — precisely the
-- implicit default assignment the plan forbids.  The NOT NULL tightening is
-- listed in docs/AGENT_IDENTITY_MIGRATION.md.
DO $$
BEGIN
    -- Per-tenant name-space: the same external identifier may exist once in
    -- each tenant, and at most once within any single tenant.
    IF NOT EXISTS (
        SELECT 1 FROM pg_constraint
        WHERE conname = 'agents_tenant_id_external_agent_id_key'
          AND conrelid = 'agents'::regclass
    ) THEN
        ALTER TABLE agents
            ADD CONSTRAINT agents_tenant_id_external_agent_id_key
            UNIQUE (tenant_id, external_agent_id);
    END IF;

    -- FK target for credential_agent_grants (§2.2) and the tenant-consistent
    -- child FKs of §4.1.  Named exactly as the plan specifies.
    IF NOT EXISTS (
        SELECT 1 FROM pg_constraint
        WHERE conname = 'agents_tenant_id_id_key'
          AND conrelid = 'agents'::regclass
    ) THEN
        ALTER TABLE agents
            ADD CONSTRAINT agents_tenant_id_id_key UNIQUE (tenant_id, id);
    END IF;
END $$;

-- ── Indexes ──────────────────────────────────────────────────────────────────
-- Tenant + external-agent lookup (`WHERE tenant_id = $1 AND external_agent_id =
-- $2`) and tenant-scoped listing (`WHERE tenant_id = $1`) are both served by the
-- index backing agents_tenant_id_external_agent_id_key, whose leading column is
-- `tenant_id`.  A separate single-column index on `tenant_id` would be a
-- redundant prefix and is deliberately not created.
--
-- The partial index below backs the multi-tenant enablement gate, which counts
-- rows still awaiting a tenant assignment.
CREATE INDEX IF NOT EXISTS idx_agents_unmapped ON agents (id) WHERE tenant_id IS NULL;

-- ── Migration record ─────────────────────────────────────────────────────────
-- §6 requires the selected mode (and, in Mode 1, the operator-declared tenant)
-- to be recorded in the migration record and not only in config.  The CHECK
-- makes the two modes structurally distinct: Mode 1 must name a tenant, Mode 2
-- must not, so a record cannot describe a mode it did not run in.
--
-- Column semantics: `agents_assigned` counts rows this run moved from unmapped
-- to a tenant; `agents_unmapped` is the number still unmapped after the run.
CREATE TABLE IF NOT EXISTS agent_tenancy_migrations (
    id               UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    mode             TEXT        NOT NULL CHECK (mode IN ('single_tenant', 'mapped')),
    legacy_tenant_id UUID,
    agents_total     BIGINT      NOT NULL,
    agents_assigned  BIGINT      NOT NULL,
    agents_unmapped  BIGINT      NOT NULL,
    applied_at       TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    CONSTRAINT agent_tenancy_migrations_mode_tenant_ck
        CHECK ((mode = 'single_tenant') = (legacy_tenant_id IS NOT NULL))
);

CREATE INDEX IF NOT EXISTS idx_agent_tenancy_migrations_applied
    ON agent_tenancy_migrations (applied_at DESC);
