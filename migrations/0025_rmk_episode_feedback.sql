-- Migration 0025: RMK episode ↔ feedback correlation.
--
-- Episodes are logged synchronously per proxied turn, but task-success
-- feedback (/api/v1/feedback) arrives asynchronously and optionally.  To let
-- a background job attribute feedback to the episodes whose injected
-- memories earned it, each episode now records its session and the exact set
-- of memory IDs that were injected for that turn.  `task_success_source`
-- distinguishes the default assumption (1.0, "assumed") from a
-- feedback-derived value.

ALTER TABLE rmk_episodes
    ADD COLUMN IF NOT EXISTS session_id TEXT,
    ADD COLUMN IF NOT EXISTS injected_memory_ids UUID[],
    ADD COLUMN IF NOT EXISTS task_success_source TEXT NOT NULL DEFAULT 'assumed'
        CHECK (task_success_source IN ('assumed', 'feedback'));

-- Serves the aggregation job's recent-episode window scan.
CREATE INDEX IF NOT EXISTS idx_rmk_episodes_agent_created
    ON rmk_episodes(agent_id, created_at DESC);
