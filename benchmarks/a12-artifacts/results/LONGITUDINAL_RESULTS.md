# Longitudinal Memory Benchmark — Results (smoke-scale + scaled N=40, 2026-07-04)

> **▶ Scaled publication run (N=40, seed 42, untruncated `longmemeval_s`, RMK ON)
> completed 2026-07-04 — see [§8](#8-scaled-publication-run-n40-2026-07-04).**
> Headline (5-seed confidence intervals, A12 — seed-robust): **AMP retains
> far more gold under pressure than gold-blind LRU/random (~2.5–2.9×, 95% CI excludes 0 on all
> 4 pressure arms; exact per-arm figures live in the §8.1 canonical table)**. The
> stress test also **confirmed the §4.5 decay-filter bug is severe** (configured decay
> collapsed recall as memory ages — see the §8.2 before/after curves). That bug was then **fixed and validated** (commit
> `49cd083` + survival test + green store suite (16 store.rs tests) + clean N=40 re-run — §8.2 carries
> the before/after curves), and the AMP headline was **re-confirmed post-fix**
> (§8.1 canonical table, rows 4–6). RMK was found **structurally unmeasurable** by this retrieval-only
> benchmark (0 episodes recorded — its learning is chat-completion-path only, §4.10 /
> §8.4). §§1–7 below are the earlier smoke record.

**Status: smoke-scale (n = 3) in §§1–7; scaled N=40 in §8.** The smoke numbers are a
go/no-go and mechanism-sanity pass, **not** publication-grade. See
[LONGITUDINAL_DESIGN.md](LONGITUDINAL_DESIGN.md) for the pre-registered protocol
and [§6 Caveats](#6-caveats--what-was-and-wasnt-exercised) below for exactly what
was and was not exercised. Do not quote a single number from this file without its
caveat.

Authority: the pre-registration in `LONGITUDINAL_DESIGN.md` is frozen. This file
records the run, every amendment made to the frozen protocol (with reasons,
recorded before the numbers), and the honest findings — including a real
retrieval bug and a benchmark-metric artifact we found in our *own* harness.

---

## 1. One-paragraph summary

The original seed-then-query benchmark showed `aeon-full == cosine-baseline`
because it never aged memories or applied pressure. This harness manufactures
history (SQL-injected timelines, rounds, feedback, memory pressure) so the
differentiators are actually exercised. At smoke scale we find:

- **(a) Ranking layer — provisional, n = 3, noisy:** over an aged corpus, decay /
  importance / graph show **no *measurable* lift** at this scale, but the
  differences are single-query noise — a **provisional observation, not a
  conclusion.**
- **(b) RMK — NOT TESTED:** RMK's learning loop was **off** for this run (background
  workers disabled under `MEMORYOS_ROLE=proxy`, see §6/A5). RMK is an **open
  question**, *not* a negative finding. Do not read "no RMK lift" anywhere in this
  doc.
- **(c) Pressure layer — the one robust result (still smoke-scale):** under memory
  pressure, **AMP eviction retains ~81–95 % of gold vs ~38–50 % for LRU and ~39–47 %
  for random** — a consistent ~2× advantage across **every** agent. Robust in the
  sense that it holds on all agents with a large margin, but still n = 3 and
  **pending the publication-scale run.**

Getting to these honest numbers required fixing four harness bugs and one metric
artifact (§4), one of which had made AMP look like a *perfect* 100 %.

---

## 2. Pre-registered amendments (recorded before results)

Each of these departs from the frozen `LONGITUDINAL_DESIGN.md`; all were forced by
findings at the smoke stage and are logged here for the anti-cherry-pick trail.

| # | Amendment | Reason | When found |
|---|-----------|--------|-----------|
| A1 | Dataset `longmemeval_oracle` → `longmemeval_s` | Oracle is ~all-gold (12–43 gold turns per query over ~23 memories/agent): baseline ceilings at recall 1.0 with **no headroom** for any condition to show a lift. `_s` has a real distractor haystack. | smoke |
| A2 | For the smoke, `_s` haystack **truncated** to gold sessions + 12 distractor sessions/record (~139 turns/agent vs ~492) | Full `_s` = ~492 turns/agent; seeding 3 agents × 2 conditions blows the wall-clock budget. Truncation hits both baseline and every condition **equally**, so relative comparisons are unaffected. The full run must use untruncated `_s`. | smoke |
| A3 | `SIM_EPOCH` anchored to **run-time now** (was fixed `2026-01-01`) | Harness bug — see §4.1. Not a tuning choice; a correctness fix. | smoke |
| A4 | `gold_retention` counts `soft_evicted` as evicted | Metric artifact — see §4.2. Correctness fix. | full run |
| A5 | Kernel run with `MEMORYOS_ROLE=proxy` during the **smoke** run | Serve the forced-sweep endpoint but disable background workers, so forced sweeps are deterministic and the background pressure-sweep job doesn't deadlock against the harness's SQL. **Consequence:** background RMK policy-learning and co-access decay are **not** exercised (see §6). **SUPERSEDED for the scaled run by A9.** | full run |
| A6 | P0 seeding parallelized (16 threads, env `LONGITUDINAL_SEED_CONCURRENCY`) | Serial seeding is ~3/s (synchronous embedding round-trips) → ~10 h for the 5-condition untruncated run. **Perf-only:** creates are order-independent and aging is applied post-seed by memory id (gold-blind), so seed order cannot affect stored state. Integrity-verified: serial == concurrent, byte-identical stored state (mapping keys, per-turn content, importance); 1:1 (100/100). | scaled-run prep |
| A7 | Importance **SQL-injected** post-seed for the importance variant: **gold = 0.95, non-gold = 0.5** (fixed, pre-registered before the run) | The create API silently drops `importance` (`CreateMemoryBody` = {content, memory_type}) and hardcodes `importance_score = 1.0`, so the importance-weighting term is inert. Exactly as the **timeline** is SQL-injected to test decay (the API has no timestamp field either), importance is SQL-injected to test the weighting math. Non-gold **must** be < gold (retrieval penalty = 1 − score). See §4.6. | scaled-run prep |
| A8 | Dedup **disabled** during seeding (`DEDUP_THRESHOLD=0`) | Shipped default `0.05` skips an insert within cosine-distance 0.05 of an existing live memory and returns the **existing** id, silently collapsing near-duplicate turns (N turns → < N memories) and potentially merging a gold turn away. Off ⇒ faithful **1 turn = 1 distinct retrievable memory**. Analogous to the existing `ARCHIVAL_INTERVAL_HOURS=0`. Verified 1:1 (100/100). The smoke run did **not** disable it — see §4.7. | scaled-run prep |
| A9 | Scaled run uses `MEMORYOS_ROLE=all` with **RMK ON** (reverses A5) | The smoke's A5 proxy-role dodge disabled RMK. The deadlock is now fixed properly (per-agent advisory lock serializing the AMP sweep vs harness SQL; §4.8), so the scaled run exercises RMK's learning loop for real. Probe: 112 forced sweeps + 110 aging txns + a real background sweep → zero new deadlocks. | scaled-run prep |
| A10 | **Rounds are a repeated measure**: re-score the SAME unit(s) every round after each clock advance (was: partition an agent's units across rounds) | With ~1 question per LongMemEval record the partition design leaves rounds 2..R **empty at any N** (not a truncation artifact) → no aging curve. Re-scoring the same question at seed-age + (r−1)·6 d yields a genuine **recall-vs-age curve**. **These are repeated measures on a corpus aging underneath the query, NOT independent per-round samples** — any trend analysis must treat them as such (the same unit recurs each round). | scaled-run prep |
| A11 | **RMK compressed learning cadence** for the RMK-isolation test: env-wire `RMK_UPDATE_COOLDOWN_SECS` + `RMK_MIN_EPISODES_BEFORE_UPDATE` (previously **not** wired — only `RMK_ENABLED` was; §4.9) and lock **cooldown = 60 s, min-episodes = 5** (shipped: 3600 s / 20 hardcoded). Compresses RMK's ~1 h update cadence so its learning loop completes many update cycles within one bench run — exactly as the timeline is SQL-injected to test decay. Disclosed in the paper as a compressed cadence, not the shipped real-time one. **Isolation:** compare `amp-rmk` (amp-only toggles + RMK on) vs `amp-only` (identical, RMK off) — only `RMK_ENABLED` differs, so any delta is RMK's and nothing else's. Deviates from the literal "aeon-full vs amp-only" (which is confounded by decay/importance/graph) to isolate RMK cleanly; AMP-on is required because RMK's policy only modulates AMP knobs (`adapter.rs`). **Run once; report helps / hurts / neutral as it lands.** | post-scaled, RMK-isolation prep |

**Scaled run (2026-07-04):** untruncated `longmemeval_s` (drops A2), **N = 40**
conversations, 5 conditions (baseline / decay-only / amp-only / aeon-full /
aeon-full-importance), seed 42, corrected `gold_retention` (A4), **RMK ON** (A9),
**rounds = repeated measure** (A10). Amendments A6–A10 and the §4.6–§4.8 findings
were committed **before** this run (code `2352c3b` + this doc) for the
anti-cherry-pick trail.

Decay stays at the pre-registered **configured** value `MEMORY_DECAY_RATE=0.03`,
`IMPORTANCE_BOOST_FACTOR=0.5` (ships as 0.0 — write-ups must say "configured
decay," not a shipped default). AMP uses shipped defaults (target ≈ 1000 hardcoded;
no tuning PR).

### 2.1 Pre-result expectation: ranking/aging is near-ceiling (logged before results)

Recorded **before** the scaled run completes, from condition 1 (`cosine-baseline`,
the first to finish): at N=40 on untruncated `longmemeval_s`, the **baseline is
already near the recall ceiling** — recall@10 = 1.00, recall@1 = 0.90, recall@5 =
1.00, with a **flat round_trend** (rounds 1–5 identical). Consequences we expect,
stated now so they are not post-hoc excuses:

- **The rounds-ranking comparison will likely show little separation** between
  conditions on recall, because there is almost no recall headroom for decay /
  importance / graph to recover — gold is nearly always already in the top-k.
  This is **dataset difficulty (haystack ≈ 490 turns/agent), not a sample-size
  limitation** — more agents cannot manufacture headroom. MRR/nDCG (baseline
  0.936 / 0.824) retain some room; watch those, not recall@k, for any ranking
  signal.
- **A flat recall-vs-age curve (A10) is the mathematically-expected default**:
  `advance_clock` ages every live memory uniformly, and a uniform age shift scales
  every candidate's decay factor by the *same* constant → ranking is invariant.
  Decay can only move ranking when memories age at *different* rates (e.g. access
  refreshes `last_accessed_at` for retrieved ones) or when the §4.5 filter pushes
  a memory past the distance threshold. So "the aging curve is flat / aeon-full
  tracks baseline" is a **predicted honest outcome**, not a failure to hide.

**Where the real statistical value is:** the **pressure phase (AMP vs LRU vs
random)** — its own SQL-seeded distractor corpus, independent of haystack size,
with 40 per-agent `gold_retention` samples — and the **A10 recall-vs-age curve**
as an honest test of aging even if it lands flat. Lead with AMP-under-pressure;
report ranking/aging/RMK/importance straight, "near-ceiling, little separation"
included where true.

---

## 3. Results

Run: `longmemeval_s` (truncated, A2), seed 42, 3 conversations, 5 rounds,
`AEON_SEMANTIC_THRESHOLD=0.95` (a cosine-*distance* ceiling — permissive),
real OpenAI `text-embedding-3-small`, kernel built from this branch.

### 3.1 Retrieval ranking (rounds phase) — n = 3 queries, HIGH NOISE

"recall@k" here is hit@k (≥1 gold in top-k), the QA convention.

| condition | recall@1 | recall@10 | MRR@10 | nDCG@10 |
|-----------|:-------:|:--------:|:------:|:-------:|
| cosine-baseline | 0.667 | 1.000 | 0.722 | 0.693 |
| decay-only | 1.000 | 1.000 | 1.000 | 0.777 |
| amp-only | 0.667 | 1.000 | 0.722 | 0.693 |
| aeon-full | 0.667 | 1.000 | 0.714 | 0.670 |
| aeon-full-importance | 0.667 | 1.000 | 0.704 | 0.668 |

**Read this as noise, not signal.** At n = 3, one query flipping changes recall@1
by 0.333. `decay-only` scoring 1.000 and `aeon-full` scoring 0.667 differ by a
single query. `amp-only == baseline` exactly (expected: with decay/importance/graph
off, AMP does nothing at retrieval time — it only acts under pressure). **No
condition shows a *measurable* ranking lift or drag over baseline at this scale —
a provisional smoke observation, not a conclusion.** These rows also **do not test
RMK**: its learning loop was off (A5), so its column here is RMK-*absent*, not
RMK-negative.

### 3.2 Memory pressure (eviction) — the robust result

Per agent: ~140 real memories + 2000 SQL-injected far-embedding distractors (§4.4)
→ ~2140 active vs target 1000. The forced-sweep endpoint drives AMP to soft-evict
**K ≈ 1150**; LRU and random comparators evict the **same K** over fresh copies of
the identical aged corpus. Primary metric is `gold_retention` (fraction of gold
still retrievable), corrected per A4.

| condition | **AMP** | LRU | random |
|-----------|:------:|:---:|:------:|
| amp-only | **0.947** | 0.504 | 0.436 |
| aeon-full | **0.855** | 0.445 | 0.467 |
| aeon-full-importance | **0.808** | 0.383 | 0.390 |

Per-agent AMP retention (kept/total): amp-only (23/24, 38/43, 12/12); aeon-full
(22/24, 35/43, 10/12); importance (19/24, 38/43, 9/12). **AMP beats both
comparators on every single agent.** This is the strongest, most consistent finding
in the run — AMP's utility/pheromone pressure preferentially evicts the
never-accessed distractors and largely spares the gold the rounds phase reinforced,
while gold-blind LRU/random destroy roughly half the gold.

**Honest sub-findings:**
- **AMP is good, not perfect.** It soft-evicted real gold on most agents (retention
  0.81–0.95, not 1.0). The perfect 1.0 we first saw was a metric artifact (§4.2).
- **Decay/importance slightly *hurt* retention under pressure**
  (amp-only 0.947 > aeon-full 0.855 > importance 0.808). AMP's pressure term grows
  with `days_stale`, so decay-aged gold gets more eviction pressure; and
  `importance_score` does **not** protect a memory from AMP eviction (AMP's pressure
  ignores importance). Seeding gold at high importance therefore does *not* improve —
  and slightly worsens — pressure survival.
- **The PI controller did not converge.** It hit the 20-sweep cap oscillating
  active ≈ 880–984 around target 1000 (restoring soft-evicted memories when active
  dipped below target), never settling within the deadband for 2 consecutive sweeps.
  The eviction comparison is still valid (all arms evict the same settled K), but
  "convergence" should not be claimed.

---

## 4. Bugs and artifacts found (in our own harness/kernel)

### 4.1 Harness: fixed-past `SIM_EPOCH` over-aged everything (fixed)
`SIM_EPOCH` was hardcoded `2026-01-01`, but the kernel measures decay staleness
from real `NOW()` (`src/memory/store.rs`, `days_stale = EXTRACT(EPOCH FROM (NOW() -
last_accessed_at))/86400`). Run on 2026-07-03 this injected ~183 phantom days;
`exp(0.03·183) ≈ 242×` inflated every distance past the retrieval threshold and
collapsed `aeon-full` recall to **0.0**. Anchored to run-time now; relative aging
stays deterministic (seed-hash).

### 4.2 Harness: `gold_retention` ignored `soft_evicted` (fixed)
`gold_retention` counted only `archived_at IS NULL`, but AMP evicts via
`soft_evicted = TRUE` (never `archived_at`), while LRU/random set `archived_at`.
AMP's evictions were therefore **invisible** to the metric → a guaranteed 1.0 on
every agent regardless of what AMP evicted (empirically: AMP had soft-evicted 25
real memories on one agent while the metric reported 1.0). Fixed to match the
kernel's own retrievability filter (`archived_at IS NULL AND soft_evicted = FALSE`).
Corrected AMP retention: 0.81–0.95 (§3.2).

### 4.3 Harness: two more (fixed)
- `run_rounds` passed raw search-result dicts to the feedback rule (expected string
  ids) → `TypeError`.
- `execute_file` ran multi-statement `.sql` via one psycopg `execute()` → *"cannot
  insert multiple commands into a prepared statement."* Added a string/comment-aware
  statement splitter.

### 4.4 Harness: distractor isolation (design step 1, done)
Pressure distractors were originally seeded through the API with **real** embeddings
into the scored agent, so they could surface in top-k and pollute recall. Replaced
with a direct SQL insert of a fixed **far** (one-hot) embedding: distractors count
toward AMP `active_count` (pressure) but sit at cosine distance ≈ 0.98 (> the 0.95
ceiling) so they are never retrieval candidates. `gold_retention` is the primary
pressure metric; post-eviction recall is over real memories only.

### 4.5 Kernel: decay filters below threshold instead of only re-ranking (CONFIRMED SEVERE at scale; FIXED + validated)
`src/memory/store.rs` applies decay to the distance that is then compared to the
threshold (`WHERE cosine_dist·exp(rate·days_stale)·(1+…) < threshold`), so a
sufficiently-stale memory is **removed** from results, not just down-ranked — even if
highly relevant. The documented intent and the only decay test
(`decay_reorders_stale_memories`, which passes `threshold = 5.0` to avoid the filter)
say decay should **re-rank**. So this is a bug, not a feature.

**Smoke decision was "do not fix now" on the belief the shipped effect was a *mild
ranking drag with zero recall loss.* The scaled run (A10 repeated-measure aging, §8.2)
overturns that belief.** The smoke only ever scored round 1, so aging never
accumulated and the removal never triggered. At N=40 with the corpus aging under
repeated rounds, the filter fires hard: every condition with configured decay
(`0.03`) collapses from near-ceiling recall@10 at round 1 to near-zero by round 5
(the §8.2 before-table), while the two decay-off conditions (baseline, amp-only) stay
pinned at the ceiling across all rounds. That `recall@5 == recall@10` in every collapsed row (§8.2) is the
signature of **removal, not demotion** — aged gold crosses the distance ceiling and
is dropped from the candidate set entirely. So the effect is not a mild drag: **as
shipped-configured, decay actively destroys recall as memory ages.**

**Decision (2026-07-04): FIXED.** Standalone product change (commit `49cd083`): the
threshold now gates **raw** `cosine_dist` (relevance); decay/importance keep
`ORDER BY distance` so they only reorder candidates already inside the ceiling
(matching the `decay_reorders_stale_memories` intent). A relevant-but-stale memory is
never dropped.

**Validated three ways:** (1) new unit test `stale_relevant_memory_survives_decay_filter`
— a cosine-0.10 memory aged 60 days (decayed distance ≈ 2.0 ≫ the 0.80 threshold) is
still retrieved post-fix, and decay still reorders it behind a fresher, less-relevant
memory; pre-fix it was dropped. (2) the store retrieval suite green — **16 tests in `store.rs`** (22 pass under the
`cargo test memory::store::` filter, which also matches 6 `extraction_worker::memory::store`
tests), covering `importance_weighted_retrieval` and `retrieval_plan_uses_hnsw_index` —
**no regression** (run via the Docker `rust:1.96-slim` path — native cargo cannot link on
this host, missing Windows SDK; see repro note at end).
(3) re-run of the three decay conditions at N=40 (§8.2 "after"): the collapse is gone,
curves are flat, decay is now the documented mild reorder. When `decay_rate = 0` the
fix is a provable no-op (`exp(0)=1` ⇒ `cosine_dist == distance`), so baseline/amp-only
are unaffected and were **not** re-run.

### 4.6 Kernel: create API silently ignores importance (DEFERRED known-issue)
`CreateMemoryBody` (`src/api.rs`) accepts only `{content, memory_type}`; the
`importance` field the harness sends is dropped by serde, and `create_memory`
hardcodes stored `importance_score = 1.0`. Because the retrieval formula's
importance factor is `(1.0 + importance_boost·(1.0 − importance_score))`, a uniform
`1.0` makes that factor identically `1.0` — **the importance-weighting mechanism is
inert in production via this API, for any `IMPORTANCE_BOOST_FACTOR`.** This also
means the smoke run's "no importance lift" was **structurally untestable**, not
disproven (same status as RMK under A5). **Decision (owner): do not change shipped
behavior mid-benchmark** (same treatment as §4.5). The scaled run tests the
*weighting math* by SQL-injecting importance (A7), exactly as it tests decay by
SQL-injecting the timeline; the paper reports both the math result **and** this
product gap ("importance is inert in production until the API accepts the field").
Surfaced 2026-07-04 by the A6 integrity check.

### 4.7 Kernel: insert-time dedup silently collapsed the corpus (fixed via A8)
`store_memory` (`src/memory/store.rs`) skips an insert when the nearest live memory
is within `DEDUP_THRESHOLD` (cosine distance, **default 0.05**) and returns the
**existing** id. Seeding N similar turns therefore yields **< N** distinct memories
(measured: 100 near-identical turns → 87 rows serial / 78 concurrent), and a gold
turn can be merged into another memory and become unretrievable under its own id.
The smoke run ran with the default `0.05` **on**, so its corpus was subject to this
collapse (an unquantified confound on the smoke numbers). Fixed for the scaled run
by A8 (`DEDUP_THRESHOLD=0`), verified 1:1 (100/100). Surfaced 2026-07-04 while
validating A6.

### 4.8 Kernel: AMP sweep vs harness SQL deadlock (fixed — enables RMK / A9)
The background AMP pressure sweep and the harness's row mutations (aging, forced
sweeps) both `UPDATE memories` and acquired row locks in opposing orders →
Postgres deadlock. The smoke run dodged it by disabling background workers (A5),
which also disabled RMK. Fixed properly: `run_pressure_sweep_for_agent` now holds a
per-agent advisory lock (`pg_advisory_xact_lock(AMP_SWEEP_LOCK_CLASSIFIER,
hashtext(agent_id))`) across its read→decide→write, and the harness takes the same
lock around its mutations — a total lock order, deadlock-free by construction.
Probe (330 s, dedup/RMK on, `role=all`): 112 forced sweeps + 110 aging txns + one
real autonomous background sweep → **zero** new `pg_stat_database.deadlocks`, zero
errors. This is what lets A9 run RMK for real.

### 4.9 Kernel: RMK tuning knobs were not env-wired (fixed for A11)
`Config::from_env` (`src/config.rs`) built `rmk_config: RmkConfig { enabled:
<RMK_ENABLED>, ..Default::default() }` — only the on/off switch read the
environment; `update_cooldown_secs` (3600 s), `min_episodes_before_update` (20)
and `epsilon` (0.1) were **hardcoded defaults**. The design doc's compressed-run
knob `RMK_UPDATE_COOLDOWN_SECS=1` therefore had **no effect** — setting it would
silently leave RMK at the shipped 1 h / 20-episode cadence (same inert-knob class
as §4.6's dropped `importance` field). Surfaced 2026-07-04 while designing the A11
compressed-cadence test; **without wiring these, the "compressed RMK" run would
have been a false compression reported as a real verdict.** Fix: env-wire
`RMK_UPDATE_COOLDOWN_SECS` + `RMK_MIN_EPISODES_BEFORE_UPDATE`, committed before the
A11 run. (This is also a genuine product improvement — the `RmkConfig` fields exist
to be configurable; they simply were not read from the environment.)

### 4.10 Product/kernel: RMK learning is driven only by chat-completion traffic (KNOWN-ISSUE / future-work)
`insert_episode` (`src/memory/rmk/store.rs`) — the function that records an RMK
learning episode — has exactly **one** production caller: `src/proxy.rs:332`, inside
the **chat-completion proxy**. Episodes are created only when memories are retrieved
*through the LLM chat path* (inject memories → observe the interaction → compute
reward → record an episode); the `/feedback` management endpoint updates AMP
`utility_ema` but does **not** create RMK episodes. Consequence, confirmed empirically
by the A11 run (§8.7): a **retrieval-only** workload records **zero** episodes
(`rmk_episodes = 0`), so the learning loop never reaches `min_episodes`, never writes
a policy (`rmk_policies = 0`), and never learns — **regardless of cadence.**

This is a real product fact worth recording independent of this benchmark:
**RMK's online learning is exercised only by chat-completion traffic.** Any deployment
or evaluation that reaches AEON-IQ via the management/retrieval APIs (not the chat
proxy) gets RMK's *initial* policy and no adaptation. Same inert-by-path class as §4.6
(importance create-API) and §4.9 (phantom knobs). **Not changed here.** Future work:
either (a) record episodes on the retrieval/feedback path too, or (b) build a
proxy-path (chat-completion) benchmark to measure RMK learning. Surfaced 2026-07-04 by
the A11 isolation test.

---

## 5. What this means for AEON-IQ's claims

- **The one robust result (still smoke-scale):** *Under memory pressure, AMP's
  adaptive eviction preserves substantially more task-relevant memory than LRU or
  random* (~1.7–2.2× **at smoke scale**; the scaled N=40 run is larger — see the §8.1
  canonical table). This is the headline worth
  pursuing — but it is **still n = 3 and must be confirmed by the publication-scale
  run** before it is stated as a result.
- **Provisional only (n = 3, noisy):** no *measurable* ranking lift from decay /
  importance / graph. This is a **provisional observation, not a conclusion** — at
  n = 3 a single query moves recall@1 by 0.333. In the pressure phase decay /
  importance mildly *reduce* gold survival; that too is provisional at this scale.
- **UNTESTED — RMK:** RMK's learning loop did not run (proxy role, §6/A5). We have
  **no data** on whether RMK helps or hurts. This is an **open question**, *not* a
  negative finding. Do not write "no RMK lift."
- **Do not claim:** AMP "retains 100 % of gold" (artifact), any RMK effect
  (untested), a ranking *conclusion* (provisional, n = 3), PI-controller
  "convergence" (didn't), or any longitudinal round-over-round trend (LongMemEval
  has one question per record, so only round 1 is scored — see §6).

---

## 6. Caveats — what was and wasn't exercised

- **n = 3 conversations.** Rounds metrics are underpowered; treat §3.1 as noise. The
  pressure result (§3.2) is more robust only because it holds on every agent with a
  large margin — still n = 3, still smoke.
- **Truncated haystack** (A2): ~139 turns/agent, not the full ~492. Relative
  comparisons hold; absolute difficulty is easier than a full run.
- **RMK was NOT tested** (`MEMORYOS_ROLE=proxy`, A5): **RMK policy learning and
  co-access edge decay did not run.** `aeon-full`/`aeon-full-importance` here reflect
  the retrieval-time RMK adapter with a *default* (unlearned) policy. We have **zero
  data** on RMK's effect. This is an **open question**, and nothing in this doc may
  be read as a "no RMK lift" / negative RMK finding.
- **One question per record:** rounds advance the simulated clock but only round 1
  scores a query per agent, so the intended round-over-round *trend* is not measured;
  what §3.1 measures is retrieval over a 0–30-day-aged corpus, scored once.
- **PI controller did not converge** (§3.2).
- **Decay = "configured" (0.03), not shipped** (0.0). Any write-up must say so.

---

## 7. Recommended next steps (for the full/publication run)

1. **Scale up** to ≥ 30–50 conversations for statistical power on the rounds phase;
   the pressure phase especially deserves n large enough for a confidence interval on
   the AMP vs LRU/random gap.
2. **Use untruncated `longmemeval_s`** (drop A2), budget the wall-clock accordingly.
3. **Exercise RMK properly:** run with background workers (`MEMORYOS_ROLE=all`) but
   serialize the forced sweeps against the background job (or add a bench-only lock)
   to avoid the deadlock that forced A5.
4. **Decide the kernel decay-filter question** (§4.5) with real aged-corpus numbers
   at scale before publishing any decay claim.
5. Lead the write-up with the **AMP-under-pressure** result; report the rounds phase
   as "no measurable ranking effect at this scale," honestly.

---

## 8. Scaled publication run (N=40, 2026-07-04)

Untruncated `longmemeval_s` (drops A2), **N = 40** conversations, 5 conditions, seed
42, corrected `gold_retention` (A4), **RMK ON** / `MEMORYOS_ROLE=all` (A9), **dedup
off** (A8), importance **SQL-injected** gold=0.95 / non-gold=0.5 for the variant (A7),
**rounds = repeated measure** (A10). ~19,605 memories seeded per condition (40 agents
× ~490 turns). All amendments + the §4.6–§4.8 findings were committed **before** this
run (anti-cherry-pick trail). All 5 conditions completed `status = ok`.

**This section reports the run straight, exactly as it landed. Nothing was tuned.**
§8.2 keeps the **found-and-fixed arc** visible end-to-end: the PRE-FIX decay collapse
(the *negative* finding that motivated the §4.5 fix — the opposite of a flattering
number) **and** the CORRECTED post-fix curve side by side. The AMP headline (§8.1) was
re-confirmed post-fix and is not contaminated by the decay change.

### 8.1 Memory pressure (eviction) — the robust result, 5-seed confidence intervals (A12)

**▶ HEADLINE (canonical): 5-seed confidence intervals (A12).** Primary metric `gold_retention`
(40-agent mean per seed), N=40, 5 pre-registered seeds (42/7/123/2024/99). CIs = percentile
bootstrap (B=10,000) over the 5 seed-level means (seed-n=5, stated honestly).

| arm | AMP mean [95% CI] | max(LRU,random) | AMP−max diff [95% CI] | ratio range | seed-robust? |
|-----|:-----------------:|:---------------:|:---------------------:|:-----------:|:------------:|
| `amp-only` | 0.890 [0.882, 0.899] | 0.321 | **0.570 [0.556, 0.582]** | 2.59–2.90× | ✅ |
| `aeon-full` | 0.913 [0.907, 0.919] | 0.347 | **0.566 [0.554, 0.575]** | 2.47–2.73× | ✅ |
| `aeon-full-importance` | 0.901 [0.892, 0.908] | 0.335 | **0.566 [0.546, 0.586]** | 2.49–2.86× | ✅ |
| `amp-rmk` | 0.903 [0.892, 0.914] | 0.348 | **0.555 [0.550, 0.564]** | 2.54–2.66× | ✅ |

**Per-seed inputs (all 5 seeds, none dropped) — AMP / max(LRU,random) `gold_retention`:**
- `amp-only`: 42: 0.890/0.343 · 7: 0.901/0.310 · 123: 0.902/0.329 · 2024: 0.878/0.310 · 99: 0.882/0.311
- `aeon-full`: 42: 0.922/0.347 · 7: 0.902/0.336 · 123: 0.915/0.350 · 2024: 0.913/0.369 · 99: 0.914/0.335
- `aeon-full-importance`: 42: 0.886/0.350 · 7: 0.913/0.325 · 123: 0.899/0.319 · 2024: 0.900/0.362 · 99: 0.904/0.317
- `amp-rmk`: 42: 0.885/0.333 · 7: 0.921/0.349 · 123: 0.911/0.358 · 2024: 0.905/0.353 · 99: 0.894/0.346

**Verdict — locked criterion (§A12, `946c01a`):** the across-seed 95% CI of
`AMP − max(LRU, random)` **excludes 0 on all 4 primary pressure arms → seed-robust, 4/4.**
All 5 seeds reported, none dropped; result reported as-is (no reruns, no reframing).

> **⟵ CANONICAL SOURCE (headline).** These four rows are the single source for the pressure
> result and its ratio; all prose (banner, §5, §8.x, DESIGN §13, both papers) points here.
> Derived once: **AMP advantage ~2.5–2.9×** (across-seed per-arm ratio range 2.47–2.90×);
> AMP `gold_retention` 0.878–0.922 across all 20 cells; comparator (LRU/random) band ~0.28–0.37.
> Config: N=40, seeds 42/7/123/2024/99, `text-embedding-3-small`, `longmemeval_s`,
> `pressure_multiplier=2.5`; controller `converged=false` on all runs (same-K comparison valid).

---

**Phase-1 single-seed record (seed 42, N=40) — SUPERSEDED by the 5-seed CIs above.** Retained
for the §4.5 found-and-fixed trail (pre-fix vs post-fix at a single seed); **not the headline.**
Primary metric `gold_retention`, 40 per-agent samples per condition:

| condition | phase | **AMP** gold-ret | LRU | random | ratio | AMP post-evict r@10 / nDCG |
|-----------|:-----:|:------:|:---:|:------:|:----:|:-------------------------:|
| amp-only | decay 0 (fix no-op) | **0.876** | 0.272 | 0.307 | ~3.0× | 1.000 / 0.851 |
| aeon-full | pre-fix | **0.888** | 0.300 | 0.289 | ~3.0× | 0.950 / 0.580 |
| aeon-full-importance | pre-fix | **0.911** | 0.334 | 0.328 | ~2.7× | 0.925 / 0.722 |
| aeon-full | post-fix | **0.899** | 0.309 | 0.330 | ~2.9× | 0.925 / 0.600 |
| aeon-full-importance | post-fix | **0.903** | 0.369 | 0.330 | ~2.4× | 1.000 / 0.820 |
| amp-rmk (A11) | post-fix | **0.895** | 0.327 | 0.347 | ~2.7× | 1.000 / 0.843 |

> **⟵ Phase-1 single-seed figures (SUPERSEDED — the A12 5-seed block above is now canonical).**
> These are the seed-42 point estimates, retained for the found-and-fixed trail. The single
> **whitelisted headline ratio (~2.5–2.9×)** and every exact pressure figure now flow from the
> A12 canonical block above; this block is the Phase-1 record only. Figures *derived* from this
> Phase-1 table:
> - **AMP gold_retention range: 0.876–0.911** (min = `amp-only`; max = `aeon-full-importance`
>   pre-fix). The three post-fix arms (rows 4–6) sit at **0.895–0.903**.
> - **LRU/random range: 0.272–0.369** → reported band **~0.27–0.37**.
> - **AMP advantage (headline, 5-seed CI): ~2.5–2.9×** — see the A12 canonical block above
>   (these Phase-1 single-seed extremes were 2.45×–3.2×).
> - **Controller:** `converged = false` (**all 240 agent-runs**); active ≈ 973, settled K ≈ 2017 (40-agent means; ~2× target 1000).
> - **Run config:** N = 40, seed 42, `text-embedding-3-small`, `longmemeval_s`,
>   `pressure_multiplier = 2.5`.

**Interpretation.** AMP preserves near-all gold under pressure while gold-blind LRU/random
lose it; the gap holds on every arm, **before and after the §4.5 fix** (rows 4–6 are
post-fix — the fix targets the retrieval threshold, which the pressure metric does not
depend on). This is a single-run point estimate, not a seed-averaged effect (see caveats).
Larger than the smoke's ~2× (§3.2). Pressure is measured on its own SQL-seeded distractor
corpus, not the aging haystack, so it is unaffected by the §4.5 decay bug.

**Why the comparators drop from smoke to scale — mechanism, not coincidence.** LRU and
random are gold-blind, so under a fixed eviction budget K they retain gold roughly in
proportion to the surviving fraction, ≈ (pool − K)/pool. The untruncated N=40 corpus is
larger, so the same target forces a *higher* eviction fraction (settled K ≈ 2× target, per
the canonical Controller row) → the gold-blind "blind-dilution floor" falls to the
comparator band above. AMP is insulated because it evicts by learned utility and spares the
gold the rounds phase reinforced. The widening edge is a predictable consequence of higher
pressure at scale, not a lucky baseline shift.

**Caveats — do not overstate precision:**
- **Headline is 5-seed CI-backed (A12); the Phase-1 record is single-seed.** The A12 canonical
  block reports the advantage across 5 pre-registered seeds with 95% CIs (seed-robust, 4/4 arms).
  The Phase-1 table below is a single-seed (42) point estimate, retained for the found-and-fixed
  trail — not a variance-bounded claim on its own.
- **AMP did not converge.** The PI controller hit the 20-sweep cap with `converged: false`
  (canonical Controller row). The comparison stays valid — all three arms evict the *same*
  settled K — but "convergence" must not be implied.
- **Ratio depends on the pressure setpoint** (`pressure_multiplier`, canonical row); a
  different distractor load moves the eviction fraction and hence the absolute ratio. The
  robust, setpoint-independent claim: *AMP preserves gold near-fully while gold-blind
  eviction loses it in proportion to the eviction fraction.*
- **One embedding model, one dataset** (canonical row).

### 8.2 Recall-vs-age curve (A10) — decay bug found → fixed (before / after; §4.5)

**BEFORE the §4.5 fix** — recall@10 by round (corpus ages ~6 d per round underneath a
re-scored query). This is the negative finding that motivated the fix; kept visible as
the "before" half of the found-and-fixed arc:

| condition | r1 | r2 | r3 | r4 | r5 |
|-----------|:--:|:--:|:--:|:--:|:--:|
| cosine-baseline (decay 0) | 1.000 | 1.000 | 1.000 | 1.000 | 1.000 |
| amp-only (decay 0) | 1.000 | 1.000 | 1.000 | 1.000 | 1.000 |
| **decay-only** (decay 0.03) | 0.975 | 0.825 | 0.650 | 0.325 | **0.150** |
| **aeon-full** (decay 0.03) | 0.950 | 0.875 | 0.650 | 0.375 | **0.125** |
| **aeon-full-importance** (decay 0.03) | 0.900 | 0.850 | 0.725 | 0.300 | **0.125** |

nDCG@10 tells the same story (decay-only 0.557→0.035; aeon-full 0.580→0.043;
importance 0.714→0.044). Aggregate over all 200 round-queries: baseline recall@10=1.000
nDCG=0.824; amp-only 1.000 / 0.823; decay-only 0.585 / 0.286; aeon-full 0.595 / 0.298;
importance 0.580 / 0.331.

**Every decay-on condition collapses to ~13–15 % recall by round 5; every decay-off
condition holds at 1.0.** In every collapsed row `recall@5 == recall@10` — the gold is
**removed** from the candidate set (§4.5 filter), not merely demoted. This is the
§4.5 bug manifesting at realistic accumulated aging, and it **overturns the smoke's
"mild drag, zero recall loss"** read (the smoke only scored round 1, so aging never
accumulated). Honest verdict on the pre-fix product: **shipped-configured decay is
harmful, not neutral, under aging.**

**AFTER the §4.5 fix** — the three decay conditions re-run at N=40, seed 42, identical
config, with the fixed kernel (threshold gates raw relevance; decay reorders only).
recall@10 by round:

| condition | r1 | r2 | r3 | r4 | r5 | ndcg@10 |
|-----------|:--:|:--:|:--:|:--:|:--:|:-------:|
| cosine-baseline (decay 0, not re-run — provable no-op) | 1.000 | 1.000 | 1.000 | 1.000 | 1.000 | 0.824 |
| amp-only (decay 0, not re-run — provable no-op) | 1.000 | 1.000 | 1.000 | 1.000 | 1.000 | 0.823 |
| **decay-only** (decay 0.03) | 0.950 | 0.950 | 0.950 | 0.950 | **0.950** | 0.603 |
| **aeon-full** (decay 0.03) | 0.925 | 0.925 | 0.925 | 0.925 | **0.925** | 0.600 |
| **aeon-full-importance** (decay 0.03) | 1.000 | 1.000 | 1.000 | 1.000 | **1.000** | 0.811 |

**The collapse is gone.** Every curve is flat across rounds (the §2.1
mathematically-expected default: uniform aging scales all decayed distances equally, so
ranking is invariant, and the threshold now gates age-independent raw cosine so
membership is invariant too). Decay is now what it was always documented to be — a
**mild ranking reorder** (decay-only nDCG 0.603 vs baseline 0.824; recall 0.950,
occasionally pushing gold below rank 10 but **never removing it**), i.e. the "mild drag,
zero recall loss" the smoke *wrongly assumed* is now actually true. Validated by the
`stale_relevant_memory_survives_decay_filter` unit test and a green store suite (16
store.rs tests; §4.5). See §8.3 for why the importance variant recovers to the recall ceiling.

Note the flat post-fix recall curve is a **definitional** consequence of the fix, not an
aging-robustness demonstration: gating on raw `cosine_dist` makes top-k *membership*
age-independent, so recall cannot vary across rounds by construction. The residual decay
effect lives entirely in *ranking* — decay's nDCG drop vs the baseline (§8.2 after-table) —
which the reorder still produces.

### 8.3 Importance (A7) — a clean ranking win (clearest post-fix)

With importance actually varying (gold 0.95 / non-gold 0.5, SQL-injected per A7 since
the create API drops the field — §4.6), the weighting math helps in both runs; the
**post-fix** run makes it unmistakable. Post-fix, the importance variant is the **best
of the three decay conditions** — highest post-fix recall@10 and nDCG@10 (near the
baseline), with gold pinned to `mean_first_gold_rank ≈ 1.0` in every round (per-condition
figures in the §8.2 after-table). The mechanism: non-gold takes a
larger distance penalty (`1 + 0.5·(1−0.5)=1.25`) than gold (`1 + 0.5·(1−0.95)=1.025`),
so gold is pulled up and recovers the recall the plain decay conditions leave on the
table. Under pressure it also edges the others (§8.1 canonical, `aeon-full-importance` post-fix row). This win
was **masked pre-fix** — decay's removal filter destroyed gold regardless of importance;
only after the §4.5 fix does the weighting benefit show cleanly. The §4.6 product gap
stands: importance is inert in production until the create API accepts the field.

### 8.4 RMK — structurally unmeasurable by this benchmark (0 episodes recorded)

The A11 isolation test (§8.7) resolves the RMK question — but **not** as "neutral" or
"no lift." The honest verdict is that RMK's learning loop is **structurally
unexercisable by a retrieval-only benchmark:** it recorded **zero episodes**, so it
never learned, so there is nothing to measure.

- The compressed cadence was wired and verified live in-kernel (`cooldown_secs=60
  min_episodes=5 epsilon=0.1`), and RMK's workers ran (role=all, §4.8 lock held).
- Yet after the run, **`rmk_episodes = 0` and `rmk_policies = 0`.** With no episodes,
  `min_episodes=5` is never reached; no policy is ever created; no learning occurs — at
  any cadence.
- **Root cause (§4.10):** the only production caller of `insert_episode` is the
  **chat-completion proxy** (`src/proxy.rs:332`). The semantic benchmark seeds/queries
  via the management + retrieval APIs and never traverses the proxy, so no episodes are
  recorded. The `/feedback` endpoint updates AMP `utility_ema`, not RMK episodes.
- The tiny pressure blip between the `amp-rmk` and `amp-only` rows (§8.1 canonical) is
  **noise, not learning** — the LRU/random comparators shifted too, and there was no
  policy in the DB to apply.

**Verdict: RMK learning is not measurable here — 0 episodes recorded — NOT "no lift."**
The distinction is the honest one: we did not test RMK and find it neutral; we found
that this class of benchmark cannot drive it. A real verdict requires a proxy-path
(chat-completion) benchmark (§4.10 future work). RMK is **not** claimed as a second
pillar on this evidence.

### 8.5 Run health

| condition | seeded | errors | error_rate | latency p50 / mean / p99 (ms) |
|-----------|:------:|:------:|:----------:|:-----------------------------:|
| cosine-baseline | 19,605 | 2 / 30,727 | 0.0001 | 271 / 363 / 2249 |
| decay-only | 19,605 | 2 / 30,307 | 0.0001 | 260 / 313 / 1231 |
| amp-only | 19,605 | 2 / 31,647 | 0.0001 | 270 / 420 / 2390 |
| aeon-full | 19,605 | 2 / 31,225 | 0.0001 | 270 / 401 / 3300 |
| aeon-full-importance | 19,605 | 2 / 30,731 | 0.0001 | 262 / 350 / 1571 |

Negligible error rate (2 per ~30 k requests), healthy latency. Seeded corpus identical
across conditions (same seed 42).

### 8.6 What this changes

1. **Headline holds:** AMP's pressure advantage over gold-blind eviction holds at N=40,
   before **and** after the §4.5 fix — the fix does not contaminate it (figures: §8.1
   canonical table).
2. **§4.5 decay bug: found → FIXED → validated** (§4.5, §8.2). Pre-fix collapse
   → post-fix flat curves (§8.2 before/after tables); survival test + green store suite
   (16 store.rs tests). Decay is now the documented mild reorder, zero recall loss.
3. **Importance:** a clean ranking win, clearest post-fix (recall@10=1.000, nDCG 0.811;
   §8.3); production create-API gap (§4.6) unchanged.
4. **RMK:** **structurally unmeasurable** by this benchmark — 0 episodes recorded,
   learning is chat-path-only (§8.4, §4.10). Not "neutral," not a second pillar; a real
   verdict needs a proxy-path benchmark.

### 8.7 RMK compressed-cadence isolation test (A11) — pre-registration + result

**Locked before the run** (anti-cherry-pick, committed ahead of any RMK result):

- **Compressed cadence:** `RMK_UPDATE_COOLDOWN_SECS = 60`, `RMK_MIN_EPISODES_BEFORE_UPDATE
  = 5` (shipped: 3600 s / 20). These are env-wired per §4.9 (they were phantom knobs
  before). Rationale: RMK's ~1 h / 20-episode shipped cadence cannot complete a
  learning trajectory inside a bench run; 60 s / 5 lets it run many update cycles —
  the same "compress an impractical shipped timescale, disclosed" logic used for the
  SQL-injected timeline (decay) and importance. Disclosed in the paper as a compressed
  cadence, **not** the shipped real-time one.
- **Isolation (only `RMK_ENABLED` differs):** `amp-rmk` = amp-only toggles
  (`AMP_ENABLED=true`, decay 0, importance 0, graph off) **+ RMK on**, vs the existing
  `amp-only` (RMK off, decay 0 ⇒ unaffected by the §4.5 fix and by RMK cooldown). Any
  delta is attributable to RMK's learning loop and nothing else. AMP-on is required
  because RMK's policy only modulates AMP knobs — pressure a/b, controller kp/ki,
  graph-bonus weight, retrieval threshold (`src/memory/rmk/adapter.rs`).
- **Metrics watched:** pressure `gold_retention` / nDCG (RMK's primary lever is AMP
  eviction) and rounds nDCG / MRR (recall is at ceiling, so watch ranking not recall).
- **Guardrail:** run **once** at the locked value; report **helps / hurts / neutral**
  exactly as it lands. No re-running with different cooldowns until it "looks good" —
  that would be tuning. "Neutral" or "slightly hurts" is a clean, publishable verdict
  and strictly better than "under-exercised."
- **Sequencing:** runs **after** the decay re-run (PID-tracked) to avoid bench-stack
  collision. Result lands here (§8.7) and in §8.4's verdict.

**Anti-cherry-pick disclosure (harness commit).** The `amp-rmk` condition-registration
(one line in `run_longitudinal.py`) was committed **post-run** (`bcbed06`); the A11
protocol and values (cooldown 60 s, min-episodes 5) were pre-registered at `59c8105`
(06:57), before the amp-rmk result (09:29). We state this plainly rather than hide the
gap: the protocol and values were fixed before the run, but the one-line condition label
itself was not separately committed until afterward.

**Result (ran exactly once, as pledged — no cooldown was re-tried).** The compressed
cadence was verified live in-kernel (`cooldown_secs=60 min_episodes=5 epsilon=0.1`) and
RMK's workers ran. Rounds ranking was byte-identical to `amp-only` (recall@10 = 1.000,
nDCG = 0.8234, MRR = 0.9363) — expected, since RMK modulates AMP *eviction*, not
non-pressure retrieval. Under pressure, the `amp-rmk` vs `amp-only` rows (§8.1 canonical)
differ by only ~+1.9 pp, and the LRU/random comparators shifted too, so the delta is
noise. **Decisive check:** after the run,
**`rmk_episodes = 0` and `rmk_policies = 0`** — the learning loop recorded nothing and
created no policy, so `min_episodes=5` was never reached and no learning happened.
**Verdict: not "neutral" — structurally unmeasurable.** Episodes are recorded only on
the chat-completion proxy path (`src/proxy.rs:332`), which a retrieval-only benchmark
never traverses (§4.10). The compressed-cadence wiring is correct and reusable; a real
RMK verdict requires a proxy-path benchmark. Carried into §8.4.

---

*Generated from `benchmarks/scripts/run_longitudinal.py` runs on 2026-07-04 (smoke §§1–7)
and `run_pubscale.py` (scaled §8). Raw per-condition `longitudinal_quality.json`
artifacts were produced with real OpenAI embeddings; re-run with the committed SQL +
seed 42 to reproduce the aging.*

**Reproduction note (kernel tests).** The `store.rs` retrieval suite (incl.
`stale_relevant_memory_survives_decay_filter`, §4.5) is green **only via the Docker
`rust:1.96-slim` path** on this host: `docker run --rm --network container:aeonbench-postgres
-v <clone>:/app -w /app -e DATABASE_URL=postgres://memoryos:memoryos_secret@localhost:5432/memoryos
rust:1.96-slim bash -lc "cargo test memory::store::"`. **Native `cargo test` cannot link
here** — the host's MSVC toolchain is missing the Windows 10/11 SDK (`LNK1181: cannot
open kernel32.lib`); this is a host-environment limitation, not a code issue.
