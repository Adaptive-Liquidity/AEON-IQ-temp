-- lru_evict.sql — LRU eviction comparator (the "headline" comparison, §5).
--
-- Authority: benchmarks/LONGITUDINAL_DESIGN.md §5 (Eviction comparison).
--
-- The kernel has no native LRU mode; the harness implements it here as committed,
-- auditable SQL over a FRESH COPY (separate agent) of the same seeded+aged corpus.
-- It archives the oldest-K memories by last_accessed_at — the classic LRU victim
-- set — where K is supplied by the harness and equals the count the AMP PI
-- controller settled on after its forced sweeps (§2, "Eviction sweep count").
--
-- FAIRNESS: this policy is gold-BLIND.  It orders purely by last_accessed_at and
-- never inspects whether a memory is gold or distractor.  The only difference
-- from the AMP arm is the eviction rule itself; the corpus, seed, and K are
-- identical.
--
-- We set archived_at = NOW() to remove the victim from live retrieval.  We do NOT
-- touch soft_evicted / soft_evicted_at — those belong exclusively to AMP's own
-- eviction path (migration 0014).  Search already filters archived_at IS NULL
-- (see src/api.rs), so archiving is sufficient to make a memory invisible.
--
-- Parameters (bound by the harness):
--   :agent_id  TEXT    — the LRU comparator agent (a fresh copy of the corpus).
--   :k         INTEGER — number of memories to evict (= AMP's settled count).
--
-- Ties on last_accessed_at are broken deterministically by id so the victim set
-- is reproducible.

UPDATE memories
SET archived_at = NOW()
WHERE id IN (
    SELECT id
    FROM memories
    WHERE agent_id = :agent_id
      AND archived_at IS NULL
    ORDER BY last_accessed_at ASC NULLS FIRST, id ASC
    LIMIT :k
);
