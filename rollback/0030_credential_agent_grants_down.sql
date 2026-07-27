-- Rollback for migration 0030 (agent grants).
--
-- Kept outside `migrations/` for the same reason as the 0028 rollback: this
-- repository uses sqlx's non-reversible migration naming, so a `.down.sql`
-- sibling in that directory would make the whole set inconsistent. Apply
-- manually:
--
--     psql "$DATABASE_URL" -v ON_ERROR_STOP=1 \
--       -f rollback/0030_credential_agent_grants_down.sql
--
-- MIGRATION ORDER. Unwind 0031 -> 0030 -> 0028.
--
--   * This script refuses while **0031** is applied. 0031 renamed a column and
--     redefined both trigger functions; dropping them from underneath it would
--     leave the ledger claiming 0031 while nothing it describes exists.
--   * The **0028** rollback refuses while this migration is applied, because
--     0030's agent-side foreign key depends on `agents_tenant_id_id_key`, which
--     0028's rollback drops.
--
-- Each script refuses up front and names the one to run first, rather than
-- failing partway through and leaving the operator to work out what state the
-- database is in.
--
-- WHAT IS LOST. Grant rows are authorization data and they are discarded: this
-- migration is what gives them somewhere to live. Re-applying 0030 restores an
-- empty table, not the grants. Nothing depends on them at step 3 — route
-- enforcement is step 5 — so no request behaviour changes either way, but the
-- count is reported below so it is not lost silently.
--
-- The triggers on `credentials` are dropped too. `credentials.tenant_wide`
-- itself is left alone: it is a 0029 column, not a 0030 one, and after this
-- runs it goes back to being the inert flag it was between steps 2 and 3.

BEGIN;

-- Never trust the caller's session search_path. Without this, a schema placed
-- ahead of `public` could capture every unqualified name below — and the
-- function drops in particular, which name no schema at all. `SET LOCAL`
-- confines it to this transaction, so the operator's session is unchanged
-- afterwards.
SET LOCAL search_path = pg_catalog, public;

-- ── Migration order ─────────────────────────────────────────────────────────
DO $$
BEGIN
    IF EXISTS (
        SELECT 1 FROM information_schema.columns
         WHERE table_schema = 'public'
           AND table_name   = 'credential_agent_grants'
           AND column_name  = 'agent_uuid'
    ) OR (
        to_regclass('public._sqlx_migrations') IS NOT NULL
        AND EXISTS (SELECT 1 FROM public._sqlx_migrations WHERE version = 31)
    ) THEN
        RAISE EXCEPTION
            'migration 0031 (agent-grant hardening) is still applied. Run '
            'rollback/0031_credential_agent_grants_hardening_down.sql first.'
            USING ERRCODE = 'dependent_objects_still_exist';
    END IF;
END $$;

DO $$
DECLARE
    grant_count BIGINT := 0;
BEGIN
    IF to_regclass('public.credential_agent_grants') IS NULL THEN
        RAISE NOTICE 'credential_agent_grants does not exist; 0030 was never applied';
        RETURN;
    END IF;

    SELECT count(*) INTO grant_count FROM public.credential_agent_grants;
    IF grant_count > 0 THEN
        RAISE NOTICE
            'discarding % agent grant(s); re-applying 0030 restores an empty table, not these rows',
            grant_count;
    END IF;
END $$;

-- Triggers first: the one on `credentials` outlives the grants table, and
-- leaving it behind would leave a trigger calling a function about a table that
-- no longer exists.
DROP TRIGGER IF EXISTS credentials_not_tenant_wide_with_grants ON public.credentials;
DROP TRIGGER IF EXISTS credential_agent_grants_not_tenant_wide
    ON public.credential_agent_grants;

DROP FUNCTION IF EXISTS public.credentials_reject_tenant_wide_with_grants();
DROP FUNCTION IF EXISTS public.credential_agent_grants_reject_tenant_wide();

-- Drops the table's own index and both foreign keys with it.
DROP TABLE IF EXISTS public.credential_agent_grants;

-- Guarded so this script is still safe to run against a database where sqlx
-- never recorded the migration.
DO $$
BEGIN
    IF to_regclass('public._sqlx_migrations') IS NOT NULL THEN
        DELETE FROM public._sqlx_migrations WHERE version = 30;
    END IF;
END $$;

COMMIT;
