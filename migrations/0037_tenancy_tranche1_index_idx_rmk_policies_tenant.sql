-- no-transaction
-- ============================================================================
-- 0037  Tenancy tranche 1 concurrent index build 5/8  (AEON plan §4, A-6)
-- ============================================================================
--
-- Step 4B-1 PREPARE. ONE statement, deliberately.
--
-- `-- no-transaction` is the literal first line, which is what makes sqlx skip
-- its own BEGIN/COMMIT (sqlx-core-0.9.0 src/migrate/source.rs:234 tests
-- `sql.starts_with("-- no-transaction")`).  That alone is NOT sufficient:
-- sqlx sends a migration file as a single simple-query message
-- (sqlx-postgres-0.9.0 src/migrate.rs:321, `conn.execute(migration.sql)`), and
-- PostgreSQL wraps a multi-statement simple query in an IMPLICIT transaction
-- block, which CREATE INDEX CONCURRENTLY refuses just as firmly as an explicit
-- one.  Measured: eight builds in one file failed with SQLSTATE 25001,
-- "CREATE INDEX CONCURRENTLY cannot run inside a transaction block", under both
-- `#[sqlx::test]` and the kernel's own startup migrator.
--
-- Hence one statement per file.  Do not merge these back together, and do not
-- add a second statement here.
-- ============================================================================

CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_rmk_policies_tenant
    ON public.rmk_policies (tenant_id, agent_uuid);
