-- Migration 0026: RMK policy acceptance status.
--
-- Enables a real hill-climbing accept/reject loop: a policy whose measured
-- mean episode reward regresses against its predecessor is marked
-- 'rejected' and stops serving (get_latest_policy only considers accepted
-- rows); the next exploration step re-rolls from the last known-good
-- policy instead of compounding the regression.  Existing rows backfill as
-- 'accepted'.

ALTER TABLE rmk_policies
    ADD COLUMN IF NOT EXISTS status TEXT NOT NULL DEFAULT 'accepted'
        CHECK (status IN ('accepted', 'rejected'));

CREATE INDEX IF NOT EXISTS idx_rmk_policies_agent_status
    ON rmk_policies(agent_id, status, created_at DESC);
