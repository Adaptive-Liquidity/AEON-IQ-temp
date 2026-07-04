# Longitudinal Memory Benchmark — Design & Pre-Registration

**Status:** pre-registration — EXECUTED at smoke scale (2026-07-04). What actually
happened (results, amendments to this frozen protocol, and the bugs/artifacts found)
lives in **[LONGITUDINAL_RESULTS.md](LONGITUDINAL_RESULTS.md)** (PR #42). See §12.
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
- The **kernel force-sweep endpoint** (small, gated) — needed for the eviction comparison. **DONE** (PR #42): `POST /api/v1/agents/:id/amp/sweep`, empirically verified (1500-memory aged corpus → 517 soft-evicted, converged to target in 23 sweeps).

## 10. Verification findings (2026-07-03) — read before running

### 10a. Go/no-go: eviction is forceable — CONFIRMED
The only pre-existing sweep trigger is a hardcoded 5-min loop; the force-sweep endpoint (PR #42) exposes the existing `run_pressure_sweep_for_agent` and was proven to drive eviction to convergence deterministically.

### 10b. Q1 — all four conditions are cleanly producible (verified in `src/config.rs`)
Every toggle is env-wired: `MEMORY_DECAY_RATE` (L239), `IMPORTANCE_BOOST_FACTOR` (L258), `GRAPH_RETRIEVAL_ENABLED` (L305), `AMP_ENABLED` (L323), `RMK_ENABLED` (L330). baseline / decay-only / amp-only / aeon-full each map to a clean env recipe. The only hardcoded knobs are AMP *internals* (`target_active_count`=1000, pressure/controller coefficients) — but AMP on/off is clean, so the ablation still separates mechanisms.

### 10c. Locked (pre-registered) config — decay/importance
`MEMORY_DECAY_RATE` and `IMPORTANCE_BOOST_FACTOR` **ship as 0.0 (off)**. So decay/importance are NOT shipped defaults; the positive values below are a **pre-registered choice**, frozen here like the timeline, and must be described in the white paper as **"configured decay,"** not a shipped default:

| Param | Locked value | Used in | Rationale |
|---|---|---|---|
| `MEMORY_DECAY_RATE` | **0.03** | decay-only, aeon-full, aeon-full-importance | `exp(0.03·days)` → ~1.35× penalty at 10d, ~2.5× at 30d (window edge); distance "doubles" at ~23 days. Meaningful but not crushing. |
| `IMPORTANCE_BOOST_FACTOR` | **0.5** | decay-only, aeon-full(+importance) | inert under uniform importance at seed; becomes active via `IMPORTANCE_REFRESH_BOOST` as accessed memories accrue importance (spacing effect) |
| `IMPORTANCE_REFRESH_BOOST` | **0.05** (shipped default) | same | per-retrieval importance bump |
| gold importance (variant only) | **0.95** | aeon-full-importance | reported as its own line, never folded into the primary uniform claim |
AMP runs at **shipped defaults** (target=1000, etc.) in amp-only/aeon-full — the more defensible "works out of the box" claim.

### 10d. Q2 — distractor isolation is NOT yet clean → FIX IS STEP 1 OF THE RUN
Verified in `run_longitudinal.py::run_pressure_phase`: pressure distractors are currently seeded via the real API (`seed_memory`, real embeddings) **into the same agent** that post-eviction recall re-queries. Consequences: (1) post-eviction recall can be distorted by distractors as retrieval candidates; (2) ~2500 real embeddings/pressure-condition (needless cost/time). The rounds recall/MRR/nDCG are **already clean** (computed before the pressure phase); `gold_retention` is **already clean** (set-membership).

**Required fix, before the smoke pass:** SQL-seed pressure distractors with a **fixed far/dummy embedding** (guaranteed to rank last → they create pressure without being retrieval candidates), and make **`gold_retention` the primary pressure metric** with post-eviction recall **secondary/caveated**. Contained change to `run_pressure_phase`.

## 11. Fresh-session run order
1. **Distractor-isolation fix** (§10d) — before anything spends credits.
2. **Smoke pass:** aeon-full vs cosine-baseline, rounds-only (`--skip-pressure`), 3–4 conv subset, 4–6 rounds. **If they don't diverge at all, STOP and report** — no-divergence is itself a finding.
3. If they diverge: full 4-way ablation + importance variant + pressure phase, at the approved bounds (3–4 conv, 4–6 rounds, pressure corpus ≈2.5×1000). $ cost is trivial (embeddings ≈ pennies); wall-clock ~30–45 min run condition-by-condition to respect the 10-min command limit.
4. Fold **differentiated** results into the white paper as a PR — honest caveats (configured decay; shipped-default AMP; simulated time; sampled subset), surfacing any weakness or no-lift finding rather than filtering it.

## 12. What actually happened → LONGITUDINAL_RESULTS.md (PR #42)

This pre-registration was executed at smoke scale on 2026-07-04. The full record —
run, every amendment to the frozen protocol (recorded before the numbers), and the
honest findings — is in **[LONGITUDINAL_RESULTS.md](LONGITUDINAL_RESULTS.md)** on the
harness branch (PR #42). Quick trail so this doc is complete from either side:

- **Amendments** (see RESULTS §2): dataset `oracle` → `longmemeval_s` (oracle was
  ~all-gold, no headroom); smoke haystack truncated; `SIM_EPOCH` anchored to run-time
  now; `gold_retention` made `soft_evicted`-aware; kernel run `MEMORYOS_ROLE=proxy`.
- **Bugs/artifacts found in our own harness+kernel** (RESULTS §4): fixed-past epoch
  (collapsed recall to 0.0), the AMP `gold_retention` artifact (had pinned AMP at a
  false **1.000**), a feedback `TypeError`, a psycopg multi-statement failure, the
  distractor-isolation fix (§10d, done), and a **latent** kernel bug — decay filters
  below the retrieval threshold instead of only re-ranking (owner: don't fix now).
- **Findings** (RESULTS §3/§5): the one robust result is **AMP under pressure**
  (gold retention 0.81–0.95 vs LRU 0.38–0.50 / random 0.39–0.47, every agent — still
  smoke-scale). Ranking-layer lift is **provisional/none at n = 3** (noisy, not a
  conclusion). **RMK was NOT tested** (proxy role disabled its learning loop) — an
  open question, not a negative finding.
- **Next:** publication-scale run (≥30–50 conv, untruncated `_s`, RMK enabled with
  sweep serialization) in a fresh session.
