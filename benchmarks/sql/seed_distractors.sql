-- seed_distractors.sql — pressure-only distractors with a FIXED FAR embedding.
--
-- Authority: benchmarks/LONGITUDINAL_DESIGN.md §4 (pressure phase) and §10d
-- (Q2 distractor-isolation finding).  This is the fix for the contamination bug:
-- the original harness seeded pressure distractors through the API, which gave
-- each one a REAL OpenAI embedding and inserted it into the SAME agent that is
-- then scored.  Those distractors became live retrieval candidates and could
-- surface in the top-k, polluting recall@k / MRR / nDCG in a way that has nothing
-- to do with the mechanism under test.
--
-- The fix: insert distractors DIRECTLY via SQL with a fixed, far, constant
-- embedding so they:
--   * COUNT toward active_count (AMP pressure is a row-count vs target problem),
--     so the PI controller still has something to soft-evict; but
--   * are NEVER a retrieval candidate — the embedding is a one-hot unit vector
--     e_1 = [1, 0, 0, ...].  For any L2-normalized OpenAI query q, cosine
--     similarity(e_1, q) = q_1 ≈ N(0, 1/dim) in magnitude (~0.02 for dim=1536),
--     i.e. essentially orthogonal.  The benchmark's search threshold is 0.95
--     similarity, so a distractor (sim ≈ 0.02, cosine distance ≈ 0.98) sits at
--     the extreme far end and is filtered under ANY sane threshold semantics.
--
-- Because the embedding is a pure function of :dim (not of content, not of gold),
-- this is fully gold-blind and deterministic: every distractor is identical, so
-- none can ever out-rank a real memory, and a reviewer replays the exact corpus.
--
-- Retrieval isolation is therefore a STRUCTURAL guarantee, not a tuning choice:
-- gold_retention (archived_at survival) is the primary pressure metric; any
-- post-eviction recall number is measured over real memories only.
--
-- Parameters (bound by the harness, never string-interpolated):
--   :agent_id      TEXT        — the agent under pressure (same one being scored).
--   :n_distractors INTEGER     — how many filler rows to insert.
--   :dim           INTEGER     — embedding dimension (detected from a live real
--                                memory of this agent; 1536 for OpenAI small).
--   :as_of         TIMESTAMPTZ — the simulated clock; created_at/last_accessed_at
--                                are set here and then re-aged gold-blind by
--                                longitudinal_age.sql over the whole live corpus.

WITH farvec AS (
    -- Build the one-hot far vector ONCE ([1,0,0,...,0] of length :dim) and reuse
    -- it for every distractor row.  array_to_string keeps the literal compact.
    SELECT (
        '[' || array_to_string(
            array(
                SELECT CASE WHEN i = 1 THEN '1' ELSE '0' END
                FROM generate_series(1, :dim::int) AS g(i)
            ), ','
        ) || ']'
    )::vector AS v
)
INSERT INTO memories
    (agent_id, content, memory_type, confidence, embedding,
     importance_score, created_at, last_accessed_at, utility_ema)
SELECT
    :agent_id,
    'pressure distractor #' || s
        || ' — far-embedding filler (pressure only; non-retrievable)',
    'episodic',
    1.0,
    (SELECT v FROM farvec),
    0.5,
    :as_of::timestamptz,
    :as_of::timestamptz,
    0.0
FROM generate_series(1, :n_distractors::int) AS s;
