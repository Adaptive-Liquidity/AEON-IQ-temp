-- longitudinal_age.sql — pre-registered, gold-blind timeline injection.
--
-- Authority: benchmarks/LONGITUDINAL_DESIGN.md §2 (Pre-registered protocol).
--
-- The public API has no timestamp field (CreateMemoryBody is content-only and
-- always stamps NOW()).  To exercise AEON-IQ's history-dependent mechanisms
-- (time-decay, AMP pressure, LRU comparators) we must SQL-inject a simulated
-- timeline.  To prevent "the timeline was tuned to win," this aging is:
--
--   * UNIFORM AT RANDOM over a fixed [0, :window_days] simulated-day window;
--   * SEEDED and DETERMINISTIC — a per-memory age is derived from a stable hash
--     of (memory id, :seed), so a reviewer replays the exact same timeline;
--   * GOLD-BLIND — the hash depends ONLY on the memory id and the seed, NEVER on
--     whether a memory is gold or distractor.  Gold answers get no timeline
--     advantage; gold and distractor draw from the SAME distribution.
--   * last_accessed_at = created_at at seed — no pre-warmed access history, no
--     head start (§2, "last_accessed_at" row).
--
-- Parameters (bound by the harness, never interpolated as strings):
--   :agent_id     TEXT   — the agent whose memories are aged.
--   :seed         INTEGER— fixed RNG seed (pre-registered default 42).
--   :window_days  INTEGER— fixed aging window in simulated days (default 30).
--   :as_of        TIMESTAMPTZ — the simulated "now" at seed time (P0 clock).
--
-- Determinism note: we do NOT use random()/setseed() (session-scoped and easy to
-- get non-reproducible across connections).  Instead we compute a stable
-- fraction in [0,1) from md5(memory_id || seed): take the first 8 hex chars as a
-- 32-bit integer and divide by 2^32.  This is a pure function of (id, seed) and
-- is identical on every replay, every connection, every host.

-- Statement 1: set created_at = as_of - (fraction * window_days), for ALL of the
-- agent's live (non-archived) memories.  fraction is the gold-blind seeded draw.
-- last_accessed_at is pinned equal to created_at (no access history at seed).
UPDATE memories m
SET created_at = :as_of::timestamptz
                 - make_interval(
                     days => 0,
                     -- fractional days expressed as seconds for sub-day precision
                     secs => (
                       ( ('x' || substr(md5(m.id::text || ':' || :seed::text), 1, 8))::bit(32)::bigint
                         & x'ffffffff'::bigint  -- force unsigned 0 .. 2^32-1
                       )::double precision / 4294967296.0        -- fraction in [0,1)
                     ) * (:window_days::double precision * 86400.0)
                   ),
    last_accessed_at = created_at
WHERE m.agent_id = :agent_id
  AND m.archived_at IS NULL;

-- Statement 2 (defensive): guarantee the invariant last_accessed_at = created_at
-- even for any row where created_at was already at :as_of and the expression
-- above evaluated last_accessed_at against the pre-update value.  Re-pinning is a
-- no-op when statement 1 already set it, and keeps the seed state unambiguous.
UPDATE memories m
SET last_accessed_at = created_at
WHERE m.agent_id = :agent_id
  AND m.archived_at IS NULL;
