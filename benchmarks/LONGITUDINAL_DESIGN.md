# Longitudinal Memory Benchmark — Design & Pre-Registration

**Status:** design for review — no harness built or run yet.
**Purpose:** measure whether AEON-IQ's ranking/memory mechanisms (time-decay, importance,
AMP co-access, AMP eviction, RMK feedback learning) produce a *measurable, mechanism-attributable*
improvement over a pure-cosine baseline **when memories have history** — age, access patterns,
feedback, and memory pressure.

## 0. Motivation — why the first live run showed nothing

The existing `run_semantic_quality.py` seeds every memory at once: age ≈ 0, uniform
importance, no co-access edges, no feedback, no pressure. In that state the retrieval math
collapses to cosine:

```
adjusted_distance = cosine_dist
                  × exp(MEMORY_DECAY_RATE × days_stale)      # days_stale ≈ 0 → ×1
                  × (1 + IMPORTANCE_BOOST × (1 − importance)) # uniform importance → constant
                  − amp_bonus                                 # no edges → 0
```

Result (2026-07-03, 50 LoCoMo queries, `text-embedding-3-small`): **aeon-full == cosine-baseline
to every decimal** (recall@10 = 0.72 both). That is the correct result for a fresh-seed design;
it simply doesn't exercise the differentiators. This benchmark fixes that.

## 1. Go/No-Go finding — eviction sweep forceability (verified first, per gate)

- Eviction is performed by `run_pressure_sweep_for_agent(state, agent_id)` in `src/rmk_worker.rs`
  (writes `soft_evicted=TRUE` at ~L332, restores at ~L345). It runs the PI controller and per-memory
  pressure `pressure = a·days_stale + b·(1 − utility_ema)`; evicts when `pressure > threshold_high`
  and `age ≥ AMP_MIN_AGE_SECONDS`.
- The function is **already unit-tested by direct call** (rmk_worker.rs L541/L554), proving it is
  drivable deterministically outside the background loop.
- **Only trigger today:** a hardcoded 5-minute background loop (`run_pressure_sweep_job`,
  `sleep(5*60)`). No interval env var, no HTTP trigger.
- Eviction ramps gradually (PI controller, ≤ +0.1 aggressiveness/cycle), so N sweeps are needed →
  5 min/cycle is impractical.

**Required prerequisite kernel change (ships in the harness PR, not this design PR):** add a gated
management endpoint `POST /api/v1/agents/:id/amp/sweep` that calls the existing
`run_pressure_sweep_for_agent`. ~15–25 lines, mirrors `trigger_archival` (api.rs:1054), no change to
sweep logic. This is the go/no-go dependency; everything else is harness-side.

## 2. Pre-registered protocol (FIXED before any run — anti-cherry-pick)

The impressive results depend on SQL-injecting `created_at` / `last_accessed_at` (the API has no
timestamp field — `CreateMemoryBody` is content-only, always `NOW()`). To prevent "the timeline was
tuned to win," **these parameters are frozen here before running and the exact SQL is committed**:

| Parameter | Fixed value | Rationale |
|---|---|---|
| Dataset | LongMemEval (`longmemeval_oracle.json`) | purpose-built for long-horizon memory |
| Sample | `--sample-size` fixed, `--seed 42`, stratified by question category | deterministic, reproducible |
| Simulated timeline | memories aged **uniformly at random** over a fixed [0, 30] simulated-day window, seeded RNG | NOT hand-placed; the aging is blind to whether a memory is gold or distractor |
| Which memories age | **ALL** memories (gold + distractor) drawn from the *same* distribution | gold answers get no timeline advantage |
| `last_accessed_at` | = `created_at` at seed (no pre-warmed access history) | no head start |
| Feedback values | fixed: `1.0` on the memory the query's gold evidence points to, `0.0` on retrieved distractors; applied every round by the same rule | no per-run tuning |
| Round count / clock step | fixed R rounds, fixed Δ simulated-days per round | frozen |
| **Eviction sweep count** (pressure phase) | **fixed deterministic rule, committed before running: drive forced sweeps until `active_count` is within `AMP_DEADBAND` of `AMP_TARGET_ACTIVE_COUNT` for 2 consecutive sweeps, OR 20 sweeps — whichever comes first.** No hand-picking. | the PI controller ramps aggressiveness gradually (≤ +0.1/cycle), so sweep count directly changes how much is evicted. The rule is the controller's *own* convergence criterion (not a number chosen to flatter the result) with a hard cap, and it is identical across AMP / LRU / random (LRU & random evict the *same count* AMP settled on) |

All timeline/eviction SQL lives in `benchmarks/sql/longitudinal_*.sql` (committed), invoked verbatim
by the harness. A reviewer can replay the exact protocol. **The RNG that assigns ages is seeded and
gold-blind** — this is the core "we didn't engineer the timeline" defense.

## 3. Conditions (identical seed / queries / timeline across all)

Primary (importance **uniform** — conservative, avoids "you hand-boosted the answers"):
1. `cosine-baseline` — decay 0, importance 0, AMP off, RMK off
2. `decay-only` — decay + importance-formula on (uniform importance ⇒ importance factor ≈ constant), AMP/RMK off
3. `amp-only` — AMP (co-access + pressure/eviction) on, RMK off
4. `aeon-full` — AMP + RMK on

Secondary (one extra variant): `aeon-full-importance` — gold memories seeded high importance, to
measure the importance×decay synergy **separately** and transparently (reported as its own line, not
folded into the primary claim).

## 4. Scenario phases

- **P0 Seed-with-history:** seed all turns via API (real embeddings); then SQL-age per §2; build
  no edges yet.
- **P1…PR Sessions:** each round at simulated time `t_r`: (a) run that round's QA subset → recall;
  (b) issue reinforcement queries that co-retrieve correlated gold sets → AMP edges grow;
  (c) `POST /feedback` per the fixed rule → utility_ema + RMK episodes; (d) advance the simulated
  clock (SQL). RMK knobs set for compressed runs: `RMK_UPDATE_COOLDOWN_SECS=1`,
  `RMK_MIN_EPISODES_BEFORE_UPDATE` low.
- **PP Pressure:** seed a fixed distractor corpus so `active_count ≫ AMP_TARGET_ACTIVE_COUNT`; drive
  the force-sweep endpoint a fixed N cycles; re-query.

## 5. Eviction comparison (the headline) — AMP vs LRU vs random

The kernel has **no** native LRU/random mode, so the harness implements the comparators as committed
SQL over the *same* seeded+aged corpus, evicting the same *count* the AMP controller chose:
- **LRU:** archive the oldest-K by `last_accessed_at`.
- **Random:** archive K uniformly (seeded RNG).
- **AMP:** the kernel's own `soft_evicted` set after N forced sweeps.
Then re-query each policy's surviving corpus and compare **post-eviction recall@k** and
**useful-memory retention** (fraction of gold/high-utility memories kept). AEON's claim is AMP retains
the useful ones → higher post-eviction recall. If it doesn't, that is a real finding and gets reported.

## 6. Metrics (per-round trend + aggregate)

recall@{1,3,5,10}, MRR@10, nDCG@10, precision@5, per round (to show divergence over time);
AMP co-access: co-retrieval rate of high-edge pairs vs random; post-eviction recall + useful-retention
(§5); RMK: mean reward per policy version + policy-param drift + recall trend as policy learns.

## 7. Threats to validity (stated up front)

- Simulated time is SQL-injected, not real aging — mitigated by the gold-blind seeded RNG (§2) and
  committed SQL. Documented as a limitation, not hidden.
- Sampled subset, not full dataset; single embedding model; no mem0/production side-by-side (future).
- **Honest-weakness commitment:** this is a *fair* test, not a rigged win. If AMP/RMK show no lift, a
  regression, or a failure mode (e.g. eviction discarding useful memories, RMK failing to converge),
  it is reported in the results and the white paper, not filtered out.

## 8. Work plan & sizing

1. **[go/no-go, first]** kernel force-sweep endpoint (§1) + prove it evicts on a toy corpus.
2. `benchmarks/sql/longitudinal_*.sql` (aging, LRU, random, counts) — committed, auditable.
3. `benchmarks/scripts/run_longitudinal.py` — reuses recall/nDCG/seed/query code from
   `run_semantic_quality.py`; adds rounds, feedback, clock advance, condition matrix.
4. Runtime: seeding embeddings dominate (bounded subset as before); rounds/feedback cheap.
   Estimate build+iterate a few hours; each full 5-condition run ≈ 30–60 min on a bounded subset.

## 9. Requires your approval

- The **full run** (burns OpenAI credits) — scope/subset-size to be approved before spending.
- The **kernel force-sweep endpoint** (small, gated) — needed for the eviction comparison.
