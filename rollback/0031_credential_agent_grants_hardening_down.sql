-- Rollback for migration 0031 (agent-grant hardening).
--
--     psql "$DATABASE_URL" -v ON_ERROR_STOP=1 \
--       -f rollback/0031_credential_agent_grants_hardening_down.sql
--
-- MIGRATION ORDER. Run this **before** rollback/0030, which refuses while 0031
-- is applied. Full unwind order is 0031 -> 0030 -> 0028.
--
-- ── WHAT THIS REVERSES, AND WHAT IT DELIBERATELY DOES NOT ───────────────────
--
-- 0031 did two unrelated things, and only one of them is reversed here.
--
--   REVERSIBLE — the naming correction. `agent_uuid` goes back to `agent_id`,
--   matching what 0030 created and what rollback/0030 and the 0030-era schema
--   expect. No authorization semantics ride on the name: it is the same column,
--   the same composite foreign key, the same primary key. Renaming it back
--   changes nothing a caller can observe.
--
--   IRREVERSIBLE, BY DESIGN — the authorization hardening. The transition-aware
--   `credentials` trigger, SECURITY INVOKER, the pinned function-level
--   `search_path`, and the fully `public`-qualified relation references all
--   stay exactly as 0031 left them. This script contains no CREATE, no ALTER
--   FUNCTION, and in particular no `ALTER FUNCTION ... RESET ALL`.
--
-- An earlier revision restored 0030's function bodies verbatim and then reset
-- their configuration, on the reasoning that a rollback returns the database to
-- a previous state rather than a better one. That is the wrong trade here,
-- because the previous state is a known-defective one, and a supported rollback
-- path must not be a way to re-arm a fixed vulnerability:
--
--   * 0030's `credentials` trigger fired on every UPDATE and refused whenever
--     `tenant_wide` was set and grants existed. A row already in that state
--     could not then be revoked, disabled, or even have `last_used_at` stamped.
--     Restoring it re-arms a denial-of-remedy on precisely the credential an
--     operator is most likely to be trying to revoke.
--   * 0030's bodies name relations unqualified and carry no `search_path`, so
--     inside a SECURITY INVOKER trigger the caller's session `search_path`
--     decides what `credential_agent_grants` and `credentials` mean.
--
-- Nothing outside the functions depends on those bodies. What 0030's rollback
-- and the 0030-era schema reference is the column *name*; the bodies are
-- internal to the two triggers, and both hardened bodies work unchanged against
-- either name because neither one mentions the renamed column. So the security
-- fix is not removed here merely to reproduce a byte-identical historical
-- schema.
--
-- CONSEQUENCE TO BE AWARE OF. After this script the database is at 0030 by
-- ledger and by column name, with 0031's hardened trigger functions still in
-- place. That difference is benign and short-lived:
--
--   * re-applying 0031 over it is idempotent — the rename is guarded and the
--     two `CREATE OR REPLACE FUNCTION`s land on identical definitions;
--   * re-applying 0030 over it replaces both functions with 0030's, exactly as
--     a fresh 0030 would;
--   * running rollback/0030 next drops both functions outright, so a full
--     unwind to 0028 leaves no trace of either version.
--
-- SAFE TO RE-RUN. Every step is guarded and the script creates nothing, so a
-- second run — or a run against a database that never had 0031, or one already
-- unwound past it — cannot resurrect an object that should not exist. That was
-- the concrete failure of the earlier revision: its `CREATE OR REPLACE
-- FUNCTION` calls recreated both 0030 functions on a database where 0030 had
-- already been rolled back.

BEGIN;

-- Never trust the caller's session search_path: a schema placed ahead of
-- `public` would otherwise capture the unqualified names below. `SET LOCAL`
-- confines it to this transaction, so the operator's session is unchanged
-- afterwards.
SET LOCAL search_path = pg_catalog, public;

-- ── Restore the 0030 column name ────────────────────────────────────────────
-- `ALTER TABLE ... RENAME COLUMN` has no IF EXISTS, so the guard supplies it,
-- and the table check covers a database where 0030 itself was never applied.
DO $$
BEGIN
    IF to_regclass('public.credential_agent_grants') IS NULL THEN
        RAISE NOTICE
            'credential_agent_grants does not exist; 0031 is not applied and there is '
            'no column to rename';
    ELSIF EXISTS (
        SELECT 1 FROM information_schema.columns
         WHERE table_schema = 'public'
           AND table_name   = 'credential_agent_grants'
           AND column_name  = 'agent_uuid'
    ) THEN
        ALTER TABLE public.credential_agent_grants RENAME COLUMN agent_uuid TO agent_id;

        -- A comment follows its column through a rename, so without this the
        -- reverted column would still be documented as the one 0031 renamed.
        -- Restores 0030's wording verbatim.
        COMMENT ON COLUMN public.credential_agent_grants.agent_id IS
            'agents.id (UUID). NOT agents.agent_id, which is the legacy caller-facing TEXT identifier.';
    END IF;
END $$;

-- ── Drop the ledger row ─────────────────────────────────────────────────────
-- Guarded so the script is still safe against a database where sqlx never
-- recorded the migration. Deliberately outside the block above: a stale row 31
-- with no grants table is exactly what a partial unwind leaves behind, and it
-- should be cleared either way.
DO $$
BEGIN
    IF to_regclass('public._sqlx_migrations') IS NOT NULL THEN
        DELETE FROM public._sqlx_migrations WHERE version = 31;
    END IF;
END $$;

COMMIT;
