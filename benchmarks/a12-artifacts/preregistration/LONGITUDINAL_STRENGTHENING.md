# Longitudinal Benchmark — Strengthening Pre-Registration (Phase 2)

**Status:** PRE-REGISTRATION — frozen before any Phase-2 run. Purpose: raise the
evidence bar from "one proven mechanism, single seed, naive baselines" to a result
that survives peer review, so the arXiv/research paper is defensible and the white
paper is built on the same stronger numbers.

**Authority:** the original protocol in `LONGITUDINAL_DESIGN.md` (amendments A1–A11)
remains frozen and unchanged. This document adds Phase-2 amendments **A12–A15**,
committed here *before* the numbers, exactly as A1–A11 were. Everything in `LONGITUDINAL_RESULTS.md`
Phase-1 stays as the honest smoke+N=40 record; Phase-2 results append, they do not overwrite.

**Sequencing (no external timing pressure):** both papers wait for Phase-2. Tier 1 is
the research paper's critical path; Tier 2 is a gated bet that does **not** block Tier 1;
Tier 3 is product work off the paper critical path.

---

## 0. Anti-cherry-pick commitments (re-affirmed for Phase 2)

The same discipline that governed A1–A11 governs Phase 2:

1. **Seeds, baseline definition, and success criteria are committed in this doc before any run.** A seed or baseline is never chosen, dropped, or redefined after seeing a result.
2. **All committed seeds are reported.** No seed is excluded because its number is inconvenient.
3. **A deflated or null result is reported as-is.** If a stronger baseline narrows the gap, or multi-seed reveals wide variance, that is the finding — it is not tuned away. A better test shrinking a result means the test worked.
4. **One canonical source.** Phase-2 numbers land in the RESULTS §8.1 canonical table (extended, not duplicated). Both papers reference that block; neither hand-types figures. This preserves the single-source structure the Phase-1 audit established.
5. **Only the seed and the added baseline change.** All simulated-timeline / aging / rounds / feedback / pressure parameters are identical to the frozen A1–A11 protocol. The point is to vary *what should not matter* (seed) and *the comparator*, holding the method fixed.

---

## TIER 1 — must-have for the research paper (run these)

### A12 — Multi-seed confidence intervals

**What.** Re-run the existing N=40 pipeline (seed → SQL-age → rounds → feedback →
pressure) end-to-end under **five pre-registered seeds**, so every headline figure
becomes a seed-robust effect with a confidence interval instead of a single-run point
estimate. This directly retires the single strongest caveat in the current write-up
("single seed (42), no confidence intervals, the ratio is a one-run point estimate").

**Pre-registered seed list (locked, chosen arbitrarily before any run — NOT selected to flatter):**

```
42   (original Phase-1 seed, retained for continuity)
7
123
2024
99
```

**Scope.** All 6 canonical pressure arms, plus the recall-vs-age curves (§8.2), per seed.
No parameter changes; `--seed` is the only variable. Each seed re-samples the LongMemEval
subset and re-draws the gold-blind aging RNG, exactly as designed.

**Reporting rules (LOCKED before any run):**
- **All 5 seeds are reported — none dropped for any reason** (never excluded because a
  seed's number, variance, or result is inconvenient).
- Per-arm `gold_retention`: report **all 5 per-seed means** **and** the across-seed
  **mean ± 95% CI**, where the CI is computed by **bootstrap over the 5 seed-level means,
  percentile method** (seed-n = 5, stated honestly). Also note the within-seed agent
  spread (40 agents/seed).
- Headline ratio: report as the **across-seed range** (e.g. "2.x–3.y× across 5 seeds"),
  **never a single value**.
- Comparators (LRU/random): same treatment. **No strong gold-blind baseline this phase** —
  deferred to the A16 proxy-path benchmark (below).
- If any pressure arm's CI includes 0, report it explicitly as **"not seed-robust on
  arm X"** — **no rerun, no reframe, no tuning.**

**Primary success criterion (LOCKED before any run).** AMP's pressure advantage is confirmed
seed-robust **iff** the across-seed **95% CI (bootstrap over the 5 seed-level means, percentile
method)** of `AMP gold_retention − max(LRU, random) gold_retention` **excludes 0 on each of the
4 primary pressure arms**: `amp-only`, `aeon-full`, `aeon-full-importance`, `amp-rmk`.
- The **4 pressure arms are primary**; the recall@k / nDCG curves (§8.2) are **supporting,
  not gating**.
- Arm excludes 0 → "seed-robust on arm X; ratio X–Y×."
- Arm includes 0 → report exactly **"not seed-robust on arm X"** — as the finding, no rerun,
  no reframe, no tuning.
- The strong gold-blind baseline is **deferred to a future proxy-path benchmark (A16, with
  RMK)** — it is not exercisable in this retrieval-only harness.

**Cost / gate (measured).** API $ is negligible — retrieval-only, `text-embedding-3-small`
only, **no generation calls**: **< $1 total** across all 5 seeds. The real cost is wall-clock:
measured per-seed ≈ **3.6–4.5 h** over the 6 conditions (from the Phase-1 N=40 per-condition
timings), so **~18–22 h total, run sequentially** on the single bench stack (one postgres +
kernel, fixed host ports — no parallel stacks). All 5 seeds run **fresh on the fixed kernel**
(decay fix `49cd083` + RMK env-wiring `89ef009`). **Human gate before spending**, per your
standing rule.

---

### A13 — One stronger, gold-blind baseline: LFU (least-frequently-used)

> **⛔ SUPERSEDED / DEFERRED (2026-07-05) — NOT run in A12.** LFU-on-`access_count` proved
> **degenerate** in this retrieval-only harness: `access_count` is incremented only on the
> chat/`retrieval.rs:173` path, which the benchmark bypasses (`/api/v1/memories/search` →
> `store::search_memories_all_sensitivities`, no bump) — verified, **0 rows populated** (an
> N=2 smoke caught it), so LFU collapses to LRU. A `utility_ema`-greedy variant was also
> rejected: the harness feeds feedback **1.0/gold, 0.0/distractor**, so `utility_ema` encodes
> the gold label — **not gold-blind** (it reads the answer key). **The strong gold-blind
> baseline is deferred to the A16 proxy-path benchmark (with RMK), below.** The original A13
> text is kept as the honest record of the approach and why it was dropped.

**Why LFU specifically.** The current comparators (LRU, random) are *importance-blind and
access-blind* — beating them is expected and a reviewer discounts it. LFU is the honest
next rung: gold-blind (it does not know which memories are gold), standard, and — critically —
it **uses the same access signal AMP benefits from**, because the rounds phase reinforces
gold via co-retrieval, so gold accrues higher access counts. **Expect LFU to narrow AMP's
gap.** That narrowing is the informative result: it measures how much of AMP's headline win
is "learned utility policy" versus "uses access frequency at all." If AMP still beats LFU,
the claim is strong. If it does not, the honest finding is "AMP's advantage over naive
eviction is largely explained by access-awareness" — still publishable, just more modest.

**Definition (locked, committed as SQL like the existing LRU/random comparators):**
- **LFU:** archive the K memories with the **lowest access count** (`last_accessed_at`-derived
  retrieval count, or the utility/access counter the kernel already tracks — use whichever
  the schema exposes; state which in the results). **Ties broken by oldest `last_accessed_at`**
  (deterministic), then by memory id (fully deterministic).
- **Same-K discipline (unchanged fairness rule):** LFU evicts the **same settled K** the AMP
  PI controller landed on — identical to how LRU/random already evict the same count. No arm
  gets a different eviction budget.
- Gold-blind: the eviction rule may not read gold/distractor labels.

**Scope.** Add LFU as a comparator column to the §8.1 canonical table, run under all five
A12 seeds, all applicable arms.

**Success criterion (committed now).** "AMP's learned policy beats standard access-aware
eviction" holds **iff** the AMP across-seed CI lower bound > the LFU across-seed CI upper
bound (non-overlapping). If they overlap: report "vs a frequency-aware baseline the margin
is X; much of the LRU/random headline is attributable to access-signal use" — as the
finding, not a failure to hide.

**Optional (only if scope allows, else skip):** a second recency+frequency hybrid
(e.g. LRU-2). One baseline (LFU) is the requirement; the hybrid is a nice-to-have. Do not
let it delay A12/A13.

---

## TIER 2 — gated bet, does NOT block Tier 1 (RMK proxy-path)

### A14 — RMK learning measured on the chat-completion path (go/no-go gated)

**Why it's separate.** RMK is the most novel-sounding component and currently the least
proven — 0 episodes in Phase 1 because `insert_episode` fires only on the chat-completion
proxy path (`proxy.rs:332`), which a retrieval-only benchmark bypasses (§4.10). Measuring it
requires a **new harness on a different code path**, plausibly weeks, and it **may return no
measurable learning**. So it runs as its own bet with a hard go/no-go, never as a Tier-1
prerequisite.

**What it requires (sketch, to be fully pre-registered before the full run):**
- Drive **chat-completion requests through the proxy** (`MEMORYOS_ROLE=all`, `RMK_ENABLED`,
  compressed cadence per A11: `RMK_UPDATE_COOLDOWN_SECS`/`RMK_MIN_EPISODES_BEFORE_UPDATE`
  wired via `89ef009`) over a sequence carrying gold/feedback structure across rounds.
- Isolate RMK's contribution with an RMK-on vs RMK-off pair on the *same* proxy workload
  (mirrors the A11 amp-rmk vs amp-only isolation), so any lift is attributable to `RMK_ENABLED`
  alone.
- Measure: `rmk_episodes` / `rmk_policies` > 0 (does it fire at all), policy-param drift over
  rounds (does it *learn*), and downstream recall/nDCG improvement attributable to that drift
  (does learning *help*).

**Go/no-go gate (committed now) — run a cheap smoke FIRST:**
- Smoke: drive a small batch of chat-completion requests; confirm `rmk_episodes > 0` **and**
  `rmk_policies > 0` **and** any observable policy-param movement.
- **GREEN** (episodes fire + drift observed) → pre-register the full A14 protocol (seeds,
  workload, success criteria) and run it.
- **RED** (episodes stay 0, or fire but no drift) → **stop**; report "RMK learning not
  exercised / not measurable even on the proxy path under these conditions" as the honest
  bounded finding, and keep the existing §4.10 future-work framing. Do not open the
  multi-week build.

This gate is the whole point: it spends ~an afternoon to decide whether a multi-week
project has any signal, instead of committing to it blind.

---

## TIER 3 — product track, NOT paper-blocking

### A15 — Wire importance through the shipped create API (product, not paper)

The create API silently drops `importance` (§4.6), so importance is currently a validated
**mechanism** (via SQL injection) but not a shipped **feature**. For the *papers*, keep the
pre-registered SQL-injection methodology — it is honest and already disclosed. Exposing
importance via the create API matters for the *product* (turns a mechanism into a feature)
and belongs on the product roadmap, off the research critical path. Tracked here so it is
not forgotten, explicitly deferred.

---

## Execution order & gates

1. **Commit this pre-registration** (A12–A15) to the repo **before any Phase-2 run** — same
   discipline as A1–A11.
2. **Human gate** on spend/scope before the A12/A13 runs (your standing rule).
3. **Run A12 alone** — one 5-seed sweep, comparators **AMP vs LRU vs random** only. The
   strong gold-blind baseline (A13) is **dropped from this phase** and deferred to A16.
4. **Extend the RESULTS §8.1 canonical table** with the multi-seed CIs (AMP/LRU/random) and
   update the derived-figures line. **No LFU column** (deferred to A16). Single source preserved.
5. **A16** (proxy-path: strong baseline + RMK, absorbing A14) is the committed **next phase**,
   run after A12 — see the A16 sketch below.
6. **Then, and only then, draft both papers** from the strengthened canonical numbers —
   research (arXiv/LaTeX) and white paper (product/investor) sharing one dataset.
7. **No result is folded into either paper before the canonical table is updated and you've
   signed off** — the Phase-1 gate discipline carries forward.

## What success and failure both look like (so neither is a surprise)

- **A12 likely succeeds** and strengthens the claim (the ~0.5 absolute margin is very
  unlikely to vanish across seeds); its job is to convert a point estimate into a CI-bounded
  effect.
- **The strong gold-blind baseline (A13) is deferred to A16** — LFU-on-`access_count` was found
  degenerate in this retrieval-only harness (the signal is chat-path-only). A12 reports the
  missing strong opponent as a known limitation; **A16 delivers it on the proxy path.**
- **A16 may return a modest or empty result** (a strong baseline that matches AMP, or RMK with
  no measurable learning). Those are legitimate, pre-committed outcomes that bound the claim
  honestly — not failed experiments.

---

## PHASE 3 — committed next phase (proxy-path benchmark)

### A16 — Proxy-path benchmark: one harness unlocks the strong baseline + RMK (SKETCH)

**Status:** **committed next phase — not vague future work.** Sketch + rationale only; **no
build, no run, no locked design yet.** A12 (5-seed CIs) ships first and independently; A16 is
the arXiv paper's smart-opponent + RMK section.

**Why (the gap A12 leaves, root-caused).** The A12 harness is retrieval-only — it scores via
`POST /api/v1/memories/search`, which bypasses the chat/`retrieval.rs` path. **Two signals live
only on that path:** `access_count` (bumped in `bump_access_counts`, `retrieval.rs:173`) and RMK
episodes (`insert_episode`, `proxy.rs:332`). A retrieval-only harness structurally cannot populate
either — which makes a real frequency baseline degenerate (access_count all-0 → LFU≡LRU) **and**
RMK unmeasurable (0 episodes). **One code path is the single root cause of both gaps.**

**What A16 does.** Drive the workload through the **chat-completion proxy** (`MEMORYOS_ROLE=all`)
instead of the raw search endpoint, over the same seeded/aged/gold-structured corpus. One harness,
one code path, two results:
1. **A real gold-blind frequency/utility baseline** — LFU on genuinely-populated `access_count`
   (and/or a utility-greedy variant). Because `access_count` is driven by **actual retrieval
   frequency**, not the gold-labeled `/feedback` signal, the baseline is **gold-blind by
   construction** — the "smart opponent" a reviewer demands.
2. **RMK learning measurement** (absorbs A14) — episodes fire, so RMK policy drift and any
   downstream recall/nDCG lift become measurable (RMK-on vs RMK-off on the same proxy workload).

**Open design questions (A16's own pre-registration, drafted when greenlit — not now):** how the
proxy workload maps to LongMemEval turns; how pressure/eviction is measured on this path; how to
keep the frequency baseline **gold-blind** (feedback must **not** encode gold here, unlike A12);
cost (the chat path adds LLM generation calls — materially more than A12's embed-only <$1).

**Sequencing.** A12 first and independently. A16 is the committed follow-on that converts "strong
baseline + RMK deferred" from an A12 limitation into the next paper section; it **supersedes and
absorbs A14** (RMK proxy-path) into one unified harness.
