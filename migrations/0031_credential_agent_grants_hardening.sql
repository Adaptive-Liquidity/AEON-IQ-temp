-- ============================================================================
-- 0031  Agent-grant hardening  (post-merge corrective for 0030)
-- ============================================================================
--
-- 0030 has merged and may already be recorded by sqlx in deployed databases, so
-- it is not edited. Everything here is additive and forward-only.
--
-- Three corrections:
--
--   1. RENAME `agent_id` -> `agent_uuid`. The column references `agents.id`
--      (UUID), never `agents.agent_id` (the legacy caller-facing TEXT
--      identifier). The two names differing only by a suffix is a footgun the
--      plan's §2.2 sketch avoided by calling it `agent_uuid` in the first
--      place.
--
--   2. Make the `credentials` trigger TRANSITION-aware. 0030's version fired on
--      every UPDATE and refused whenever `tenant_wide` was true and grants
--      existed. That is right for the write that *creates* the contradiction
--      and wrong for every other write: a row that reached the contradictory
--      state (triggers disabled for a bulk load, restored from a drifted dump)
--      could not then be revoked, disabled, or even have `last_used_at`
--      stamped. The control that exists to prevent a bad state was preventing
--      its repair.
--
--   3. Pin name resolution on both trigger functions. They are SECURITY
--      INVOKER, so a caller with a hostile `search_path` could otherwise make
--      `credential_agent_grants` resolve to a shadow table and have the check
--      inspect the wrong rows. Bodies are fully qualified AND the functions
--      carry `SET search_path`, so neither alone is load-bearing.
--
-- Semantics are otherwise unchanged: the two authorization modes, what denies,
-- and what the composite foreign keys forbid are all exactly as 0030 left them.
-- ============================================================================

-- ── 1. Rename ───────────────────────────────────────────────────────────────
-- `ALTER TABLE ... RENAME COLUMN` has no IF EXISTS, so the guard supplies it.
-- Constraint and index NAMES are unaffected (none of them contains `agent_id`);
-- their *definitions* re-render against the new column, which the schema
-- contract in `store.rs` expects.
DO $$
BEGIN
    IF EXISTS (
        SELECT 1 FROM information_schema.columns
         WHERE table_schema = 'public'
           AND table_name   = 'credential_agent_grants'
           AND column_name  = 'agent_id'
    ) THEN
        ALTER TABLE public.credential_agent_grants RENAME COLUMN agent_id TO agent_uuid;
    END IF;
END $$;

COMMENT ON COLUMN public.credential_agent_grants.agent_uuid IS
    'agents.id (UUID). Renamed from agent_id in 0031 so it cannot be confused with '
    'agents.agent_id, the legacy caller-facing TEXT identifier.';

-- ── 2. Transition-aware mutual exclusion ────────────────────────────────────
--
-- Refuses exactly two writes:
--
--   * an INSERT that creates a tenant_wide credential while grants already
--     exist for that id;
--   * an UPDATE that is a real false -> true transition while grants exist.
--
-- Everything else is allowed, including every write on a row that is *already*
-- contradictory. That is deliberate: those writes are how an operator revokes,
-- disables or repairs it. `authorize_agent` denies the credential throughout,
-- so permitting the repair path costs no authority.
--
-- Concurrency is unchanged. A false -> true transition is an UPDATE, which
-- already holds FOR UPDATE on the credential row, and the grants trigger takes
-- FOR SHARE on the same row — so the two still serialise. The INSERT case needs
-- no lock: no grant can reference a credential that is not yet visible, because
-- the grant's foreign key would have nothing to match.
CREATE OR REPLACE FUNCTION public.credentials_reject_tenant_wide_with_grants()
RETURNS TRIGGER
LANGUAGE plpgsql
SECURITY INVOKER
SET search_path = pg_catalog, public
AS $$
BEGIN
    -- Turning tenant_wide off, or leaving it off. Always allowed: this is the
    -- repair path.
    IF NOT NEW.tenant_wide THEN
        RETURN NEW;
    END IF;

    -- Already tenant_wide before this UPDATE, so this write did not create the
    -- state. Allowed even when it is contradictory — revocation, disabling and
    -- last_used_at all land here.
    IF TG_OP = 'UPDATE' AND OLD.tenant_wide THEN
        RETURN NEW;
    END IF;

    -- What remains: an INSERT with tenant_wide set, or a real false -> true
    -- transition. Either creates the contradiction, so either is refused.
    IF to_regclass('public.credential_agent_grants') IS NOT NULL
       AND EXISTS (SELECT 1
                     FROM public.credential_agent_grants g
                    WHERE g.credential_id = NEW.id)
    THEN
        RAISE EXCEPTION
            'credential % holds per-agent grants; tenant_wide cannot be set while they exist',
            NEW.id
            USING ERRCODE = 'check_violation';
    END IF;

    RETURN NEW;
END;
$$;

-- ── 3. Pin the grant-side function too ──────────────────────────────────────
-- Unchanged in behaviour; re-created only to qualify its references and pin
-- `search_path`, so both functions satisfy the same contract.
CREATE OR REPLACE FUNCTION public.credential_agent_grants_reject_tenant_wide()
RETURNS TRIGGER
LANGUAGE plpgsql
SECURITY INVOKER
SET search_path = pg_catalog, public
AS $$
DECLARE
    is_tenant_wide BOOLEAN;
BEGIN
    SELECT c.tenant_wide INTO is_tenant_wide
      FROM public.credentials c
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
$$;

-- The triggers themselves are untouched: `CREATE OR REPLACE FUNCTION` keeps
-- them bound to the same functions, and `pg_get_triggerdef` renders identically
-- either way. Re-creating them would change nothing and risk a window with no
-- trigger at all.
