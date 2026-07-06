-- random_evict.sql — Random eviction comparator (§5).
--
-- Authority: benchmarks/LONGITUDINAL_DESIGN.md §5 (Eviction comparison).
--
-- Archives K memories chosen UNIFORMLY AT RANDOM (seeded, reproducible) over a
-- FRESH COPY (separate agent) of the same seeded+aged corpus.  K is supplied by
-- the harness and equals the count the AMP PI controller settled on, identical to
-- the LRU arm — the three policies evict the SAME count so only the *selection
-- rule* differs (§2, "Eviction sweep count").
--
-- Determinism: like longitudinal_age.sql we avoid session-scoped random()/
-- setseed(); each memory gets a stable pseudo-random key from
-- md5(id || ':' || :seed), and we take the lowest-K keys.  Pure function of
-- (id, seed) → identical victim set on every replay.
--
-- FAIRNESS: gold-blind.  The key depends only on id and seed, never on gold
-- status.  We set archived_at = NOW() and DO NOT touch soft_evicted /
-- soft_evicted_at (those are AMP's columns; migration 0014).
--
-- Parameters (bound by the harness):
--   :agent_id  TEXT    — the random comparator agent (a fresh copy of the corpus).
--   :k         INTEGER — number of memories to evict (= AMP's settled count).
--   :seed      INTEGER — RNG seed (pre-registered default 42).

UPDATE memories
SET archived_at = NOW()
WHERE id IN (
    SELECT id
    FROM memories
    WHERE agent_id = :agent_id
      AND archived_at IS NULL
    ORDER BY md5(id::text || ':' || :seed::text) ASC, id ASC
    LIMIT :k
);
