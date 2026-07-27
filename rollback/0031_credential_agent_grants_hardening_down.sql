-- Rollback for migration 0031 (agent-grant hardening).
--
--     psql "$DATABASE_URL" -v ON_ERROR_STOP=1 \
--       -f rollback/0031_credential_agent_grants_hardening_down.sql
--
-- MIGRATION ORDER. Run this **before** rollback/0030, which now refuses while
-- 0031 is applied. Full unwind order is 0031 -> 0030 -> 0028.
--
-- WHAT THIS REINSTATES, DELIBERATELY. Returning to the 0030 definitions means
-- returning to 0030's behaviour, including the two defects 0031 corrected:
--
--   * the `credentials` trigger becomes unconditional again, so a row that is
--     already in the contradictory state (tenant_wide with grants) cannot be
--     revoked, disabled or otherwise updated until the grants are removed;
--   * both trigger functions lose their pinned `search_path`, so a caller with
--     a hostile session `search_path` can influence how the unqualified names
--     in their bodies resolve.
--
-- That is what a rollback is for — returning to a known previous state, not a
-- better one. It is spelled out here so the cost is visible at the moment
-- someone runs it.
--
-- The column reverts to `agent_id`, matching 0030. Authorization semantics do
-- not change either way: the rename was a naming correction.

BEGIN;

-- Never trust the caller's session search_path: every unqualified name below
-- would otherwise be resolvable to a shadow schema placed ahead of public.
SET LOCAL search_path = pg_catalog, public;

DO $$
BEGIN
    IF to_regclass('public.credential_agent_grants') IS NULL THEN
        RAISE NOTICE 'credential_agent_grants does not exist; nothing for 0031 to unwind';
    END IF;
END $$;

-- ── Restore the 0030 column name ────────────────────────────────────────────
DO $$
BEGIN
    IF EXISTS (
        SELECT 1 FROM information_schema.columns
         WHERE table_schema = 'public'
           AND table_name   = 'credential_agent_grants'
           AND column_name  = 'agent_uuid'
    ) THEN
        ALTER TABLE public.credential_agent_grants RENAME COLUMN agent_uuid TO agent_id;
    END IF;
END $$;

-- ── Restore the 0030 function bodies verbatim ───────────────────────────────
-- Unqualified references and no SET clause, exactly as 0030 wrote them, so a
-- database unwound to 0030 is indistinguishable from one that never had 0031.
CREATE OR REPLACE FUNCTION public.credential_agent_grants_reject_tenant_wide()
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

CREATE OR REPLACE FUNCTION public.credentials_reject_tenant_wide_with_grants()
RETURNS TRIGGER AS $$
BEGIN
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

-- `CREATE OR REPLACE FUNCTION` without a SET clause does not necessarily clear
-- a configuration the previous definition carried, so the reset is explicit.
-- Without it a database unwound to 0030 would keep 0031's pinned search_path
-- and quietly differ from one that never had 0031.
ALTER FUNCTION public.credential_agent_grants_reject_tenant_wide() RESET ALL;
ALTER FUNCTION public.credentials_reject_tenant_wide_with_grants() RESET ALL;

DO $$
BEGIN
    IF to_regclass('public._sqlx_migrations') IS NOT NULL THEN
        DELETE FROM public._sqlx_migrations WHERE version = 31;
    END IF;
END $$;

COMMIT;
