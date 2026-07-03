-- Migration 0027: persist AMP PI-controller state per agent.
--
-- The pressure sweep previously reconstructed a fresh PIController every
-- cycle, so integral action never accumulated across sweeps and convergence
-- to the target active count was proportional-only (white paper §5.4 scope
-- note).  State is now saved after each sweep and restored on the next.

CREATE TABLE IF NOT EXISTS amp_controller_state (
    agent_id       TEXT             PRIMARY KEY,
    aggressiveness DOUBLE PRECISION NOT NULL DEFAULT 0.0,
    integral_error DOUBLE PRECISION NOT NULL DEFAULT 0.0,
    updated_at     TIMESTAMPTZ      NOT NULL DEFAULT NOW()
);
