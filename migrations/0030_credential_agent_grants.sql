-- ============================================================================
-- 0030  Agent grants  (AEON auth/tenancy plan §2.2, §10 step 3)
-- ============================================================================
--
-- Step 3 of the plan accepted at
--   Adaptive-Liquidity/nexus-planning @ b1fe06505d400e435c3ef8d10dc197f15641bebd
--
-- WHY THIS TABLE EXISTS
--
-- Tenant isolation and agent authorization are different controls, and the
-- first does not imply the second. Without this table, any valid `memory:read`
-- credential could name ANY agent in its own tenant and satisfy every route's
-- tenant+agent predicate — which is exactly the cross-agent hole plan test #5
-- forbids.
--
-- ADDITIVE AND INERT. Creates one table and two triggers. It alters no existing
-- column and rewrites no data. Nothing consults it on any request path: route
-- enforcement is plan step 5.
--
-- COLUMN NAMING — worth reading before extending this.
--
-- `agent_id` here is the UUID `agents.id`. It is NOT `agents.agent_id`, which
-- is the legacy caller-facing TEXT identifier that step 1 preserved. The plan's
-- §2.2 sketch calls this column `agent_uuid` precisely to keep the two apart;
-- the name `agent_id` is used here as specified for this step. The types differ
-- (UUID vs TEXT), so a mistaken join fails loudly rather than silently matching
-- the wrong thing — but a reader skimming the schema can still be misled, so:
-- this column joins `agents.id`, never `agents.agent_id`.
-- ============================================================================

CREATE TABLE IF NOT EXISTS credential_agent_grants (
    credential_id UUID        NOT NULL,

    -- Denormalised deliberately. It is not redundant: it is the single value
    -- that both foreign keys below must simultaneously satisfy, which is what
    -- makes a cross-tenant grant unrepresentable rather than merely unwritten.
    tenant_id     UUID        NOT NULL,

    -- `agents.id` (UUID), not `agents.agent_id` (TEXT). See the note above.
    agent_id      UUID        NOT NULL,

    created_at    TIMESTAMPTZ NOT NULL DEFAULT NOW(),

    -- Duplicate grants are impossible: granting the same agent twice to the
    -- same credential is the same grant. `tenant_id` is deliberately NOT part
    -- of the key — including it would let one credential hold two rows for the
    -- same agent under different tenants, which is the state the composite
    -- foreign keys exist to forbid.
    CONSTRAINT credential_agent_grants_pkey
        PRIMARY KEY (credential_id, agent_id),

    -- Ties the grant to the CREDENTIAL's tenant. An earlier plan draft
    -- referenced `credentials (id)` alone, which left `tenant_id` free to name
    -- any tenant at all.
    CONSTRAINT credential_agent_grants_credential_fkey
        FOREIGN KEY (credential_id, tenant_id)
        REFERENCES credentials (id, tenant_id)
        ON DELETE CASCADE,

    -- …and to the AGENT's tenant. One `tenant_id` value must satisfy both
    -- sides, so a grant naming an agent in another tenant cannot be written
    -- even by direct SQL.
    --
    -- ON DELETE CASCADE: removing an agent removes the grants that named it.
    -- ON UPDATE is left at NO ACTION on purpose — moving an agent to another
    -- tenant while grants exist is refused rather than silently repointed,
    -- because a silent repoint would hand another tenant's credential a live
    -- grant.
    CONSTRAINT credential_agent_grants_agent_fkey
        FOREIGN KEY (tenant_id, agent_id)
        REFERENCES agents (tenant_id, id)
        ON DELETE CASCADE
);

-- The primary key already serves "which agents may this credential act for".
-- This serves the other direction — "which credentials name this agent" — and
-- backs the agent-side foreign key, which PostgreSQL does not index for you.
CREATE INDEX IF NOT EXISTS idx_credential_agent_grants_agent
    ON credential_agent_grants (tenant_id, agent_id);

-- ── tenant_wide and grants are mutually exclusive ───────────────────────────
--
-- §2.2 requires a credential to be EITHER tenant-wide OR restricted to an
-- explicit agent set, never both. A credential holding `tenant_wide = true`
-- *and* grant rows asserts both "all agents" and "these agents"; resolving it
-- either way is worse than refusing, because resolving toward tenant-wide lets
-- a stray flag quietly widen a restricted credential, and resolving toward the
-- grants lets a flag intended as tenant-wide be silently narrowed.
--
-- The plan notes a CHECK cannot express this (a CHECK cannot subquery) and
-- suggests "a trigger or an application invariant plus a periodic audit".
-- Triggers are used here so the contradictory state is genuinely
-- unrepresentable rather than merely detected after the fact — the application
-- invariant in `grants.rs` is then a second line, not the only one.
--
-- CONCURRENCY, STATED RATHER THAN ASSUMED: under READ COMMITTED, two
-- concurrent transactions — one inserting a grant, one setting tenant_wide —
-- could each see a state the other is about to invalidate. The grant trigger
-- therefore takes `FOR SHARE` on the credential row, which conflicts with the
-- `FOR UPDATE` lock an `UPDATE credentials` already holds, so the two
-- serialise. (The foreign key's own `FOR KEY SHARE` is not enough: it does not
-- conflict with a non-key column update, and `tenant_wide` is a non-key
-- column.)

CREATE OR REPLACE FUNCTION credential_agent_grants_reject_tenant_wide()
RETURNS TRIGGER AS $$
DECLARE
    is_tenant_wide BOOLEAN;
BEGIN
    SELECT c.tenant_wide INTO is_tenant_wide
      FROM credentials c
     WHERE c.id = NEW.credential_id
       FOR SHARE;

    IF is_tenant_wide THEN
        RAISE EXCEPTION
            'credential % is tenant_wide; tenant-wide and per-agent grants are mutually exclusive',
            NEW.credential_id
            USING ERRCODE = 'check_violation';
    END IF;

    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE OR REPLACE TRIGGER credential_agent_grants_not_tenant_wide
    BEFORE INSERT OR UPDATE ON credential_agent_grants
    FOR EACH ROW EXECUTE FUNCTION credential_agent_grants_reject_tenant_wide();

CREATE OR REPLACE FUNCTION credentials_reject_tenant_wide_with_grants()
RETURNS TRIGGER AS $$
BEGIN
    -- Guarded on `to_regclass` so migration 0029 can still be applied to a
    -- database where 0030 has not run: the trigger function is created here,
    -- but a restore or a partial replay could invoke it against a schema with
    -- no grants table.
    IF NEW.tenant_wide
       AND to_regclass('public.credential_agent_grants') IS NOT NULL
       AND EXISTS (SELECT 1
                     FROM credential_agent_grants g
                    WHERE g.credential_id = NEW.id)
    THEN
        RAISE EXCEPTION
            'credential % holds per-agent grants; tenant_wide cannot be set while they exist',
            NEW.id
            USING ERRCODE = 'check_violation';
    END IF;

    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE OR REPLACE TRIGGER credentials_not_tenant_wide_with_grants
    BEFORE INSERT OR UPDATE ON credentials
    FOR EACH ROW EXECUTE FUNCTION credentials_reject_tenant_wide_with_grants();

COMMENT ON TABLE credential_agent_grants IS
    'Which agents a credential may act for (plan §2.2). A credential is agent-restricted '
    '(>=1 row here) or tenant-wide (credentials.tenant_wide, zero rows here) — never both, '
    'and never neither. Absence of a grant on a non-tenant-wide credential denies.';

COMMENT ON COLUMN credential_agent_grants.agent_id IS
    'agents.id (UUID). NOT agents.agent_id, which is the legacy caller-facing TEXT identifier.';

COMMENT ON COLUMN credential_agent_grants.tenant_id IS
    'The one tenant value both foreign keys must satisfy; this is what makes a cross-tenant '
    'grant unrepresentable rather than merely unwritten.';
