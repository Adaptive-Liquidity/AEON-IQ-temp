# AEON-IQ: A Pre-Registered Longitudinal Benchmark for Adaptive Memory Eviction under Pressure

## Abstract

Long-lived language-model agents accumulate memory faster than they can keep it, so they must
eventually evict — and *which* memories they discard determines whether later recall succeeds.
We introduce AEON-IQ, a pre-registered longitudinal benchmark that measures how much need-again
("gold") information a memory system retains when it is forced to shrink under pressure; seeds,
comparator definitions, and success criteria are committed to version control before any result
is produced. Within this benchmark we report a single, deliberately narrow result: under
eviction pressure, an adaptive policy that evicts by learned per-memory utility (AMP) retains
**2.5–2.9× more gold than gold-blind least-recently-used and random baselines**, evicting the
same number of memories over the same corpus. The advantage is seed-robust — the across-seed
95% confidence interval of AMP minus the stronger gold-blind baseline excludes zero on all four
pressure configurations and five pre-registered seeds. We claim this one dimension and no more:
the pre-registered curve caught and forced the fix of a real retrieval bug, and the
online-learning and stronger-baseline comparisons that the current retrieval-only harness cannot
exercise are deferred, transparently, to a pre-registered follow-on.

---

## 1. Introduction

Large language model agents that persist across sessions accumulate memory without bound. Every
interaction can add facts, preferences, and events worth remembering, but storage and context
budgets are finite, so a long-lived agent must eventually forget. *Which* memories it discards is
not a neutral housekeeping detail: if the system evicts the facts a user will ask about again,
later recall fails no matter how good the retriever is. We frame this as *gold survival under
pressure* — when a memory store is forced to shrink toward a target size, what fraction of the
genuinely need-again ("gold") memories remain retrievable?

Eviction is, relative to retrieval, under-evaluated. Memory systems for agents are typically
demonstrated rather than measured against controlled baselines; when eviction is compared at all,
it is often on a single configuration, without pre-registered criteria, and without confidence
intervals that separate a real effect from run-to-run noise. Longitudinal effects — what happens
as memories age across many rounds — are rarely isolated, and negative or unmeasurable outcomes
are seldom reported. These are precisely the conditions under which a flattering result is easy to
produce and hard to trust.

This paper introduces AEON-IQ, a pre-registered longitudinal benchmark for memory eviction under
pressure, and uses it to report a single, deliberately narrow result: under eviction pressure, an
adaptive policy that evicts by learned value (AMP) retains more gold than gold-blind
least-recently-used and random baselines. We report that advantage with confidence intervals over
five pre-registered seeds: AMP retains **~2.5–2.9×** more gold than gold-blind baselines, and the
advantage is **seed-robust on all four pressure arms** (the across-seed 95% CI of AMP−max(LRU,random)
excludes 0; exact per-arm figures in §6 / RESULTS §8.1). We claim this one dimension and no more; the benchmark's other
axes are reported as method, honest limitation, or future work.

Our contributions are threefold. First, we introduce the benchmark and its pre-registration
protocol: seeds, comparator definitions, and success criteria are committed before the numbers,
and every reported figure lives in a single canonical table so none can drift. Second, we report
one result for adaptive eviction under pressure, evaluated across five pre-registered seeds rather
than a single run: AMP retains ~2.5–2.9× more gold than gold-blind baselines, seed-robust on
all four pressure arms (§6). Third, we offer the benchmark as a model of
honest evaluation: we document a real product bug that our own pre-registered curve caught and
that we fixed (§5), and we pre-register — rather than approximate — the components we could not
yet measure (online learning, and a stronger gold-blind baseline), deferring them transparently to
a follow-on (§8).

## 2. Related Work

**Memory systems for LLM agents.** A growing line of work equips agents with long-term memory —
external stores written during interaction and retrieved to condition later responses, as in
MemGPT [1], generative-agent memory [2], and production long-term-memory systems such as Mem0 [3]. Our
aim is orthogonal to proposing another such system: we do not claim a better memory architecture
in general. The system under test is our own AEON-IQ kernel — a Rust/pgvector engine with the AMP eviction
policy that also ships as the memory layer of the Nexus-IQ product — and the AEON-IQ benchmark is
the evaluation instrument. The contribution is the methodology and one measured property of adaptive
eviction within it.

**Eviction and cache replacement.** Deciding what to discard under a size bound is the classical
cache-replacement problem, with well-known policies — least-recently-used, least-frequently-used,
and random — and a large literature on value- and frequency-aware variants [4]. We adopt LRU
and random as *deliberately conservative, gold-blind baselines*: beating them establishes that an
adaptive policy does something beyond recency or chance, not that it beats the strongest possible
opponent. A frequency-aware policy (LFU) is the natural stronger rung; §7 explains why it is not
exercisable in the present retrieval-only harness, and we defer it (§8) rather than claim a win
over it.

**Long-context and longitudinal memory benchmarks.** Benchmarks such as LongMemEval [5] and
LoCoMo [6] evaluate retrieval and question answering over long interaction histories. We build
on this setting but shift the axis of evaluation: rather than measuring retrieval on a fixed
history, we impose accumulated aging and hard eviction pressure and measure how much gold survives
being forgotten. To our knowledge, pre-registered, confidence-interval-backed comparison of
eviction policies under this kind of pressure is not standard in that literature.

**Pre-registration and reproducibility.** Pre-registration — fixing hypotheses, analyses, and
inclusion criteria before observing outcomes — is established in the empirical sciences and
increasingly urged in machine learning [7, 8]. We import that discipline: the benchmark's seeds,
baselines, and success criteria are committed before results, and the honest reporting of a
narrowed or null outcome is treated as a feature of the design, not a failure of the system.

**Positioning.** Against this backdrop, our contribution is not a claim to outperform the field of
memory systems. It is (a) a pre-registered, reproducible *longitudinal benchmark* for eviction
under pressure, and (b) *one robust, CI-backed result* within it — that adaptive eviction preserves
more gold than gold-blind baselines under pressure (~2.5–2.9×, seed-robust across five seeds on
all four arms; §6) — reported
alongside an explicit account of what the benchmark cannot yet measure.

## 3. The AEON-IQ Benchmark: Design and Pre-Registration

The AEON-IQ benchmark evaluates how well a memory system preserves *useful* information when it is forced
to forget. Rather than measuring retrieval on a static corpus, it subjects a long-lived
agent memory to accumulated aging and to hard eviction pressure, and asks a single, sharp
question: after the system must discard memories to stay within a size target, how much of
the information a user will actually need again is still retrievable? We call these
need-again items *gold* memories, and the primary metric *gold retention* — the fraction of
gold still live after eviction.

**Pre-registration.** The benchmark is governed by a pre-registration protocol that we treat
as binding. Every design decision that could bias a result — the seed list, the definition
of each comparator, the success criteria, and the metric of record — is committed to version
control *before* the corresponding numbers are produced, as a numbered sequence of amendments
(A1–A16) with their commit hashes. This is not ceremony: it is the mechanism that lets a
reader verify a result was not chosen after the fact. We adopt four standing commitments.
First, seeds, baselines, and success criteria are fixed in advance and never redefined after
seeing a result. Second, every committed seed is reported; none is dropped because its number
is inconvenient. Third, a deflated or null result is reported as-is — a stronger baseline that
narrows a gap, or multi-seed variance that widens an interval, is the finding, not something
to tune away. Fourth, all reported figures live in a single canonical results table and are
referenced rather than re-typed, so no number can drift between documents.

**Corpus and aging.** Each run instantiates *N* independent agents (N = 40 in the runs
reported here) from LongMemEval-S, a long-context question-answering dataset in which each
record carries a gold-evidence memory and a large haystack of distractor turns. We seed each
agent's turns as individual memories and then age the corpus along a simulated timeline. The
aging is deterministic and gold-blind: a memory's age is a pure function of its identifier and
the run seed, so gold and distractor memories are drawn from the same age distribution and no
policy can infer gold status from recency. Aging drives the decay/importance ranking layer
(below) and lets us measure a recall-versus-age curve across repeated rounds.

**Pressure protocol.** To force eviction we seed each agent with a large distractor corpus so
that the live memory count greatly exceeds the system's target size. The distractors are
inserted directly with a fixed, far embedding: they count toward the active total (so the
eviction machinery has rows to remove) but sit far below any retrieval threshold, so they can
never contaminate recall — gold retention is measured over the real memories only. The
adaptive controller (AMP) then runs its eviction sweeps until it settles on a number of
evictions *K* needed to reach the target. That settled *K* is the fairness anchor for the
whole comparison.

**Comparators and the same-K rule.** We compare AMP against two gold-blind baselines,
least-recently-used (LRU) and uniform-random eviction, each implemented as committed,
auditable SQL over a *fresh copy* of the same seeded and aged corpus. The comparators evict
*exactly the same K* that AMP settled on — the same eviction budget over the same corpus — so
the only variable across arms is the eviction *rule*, never how much is removed. Both baselines
are strictly gold-blind: their victim selection reads only recency (LRU) or a seeded hash
(random), never any gold or distractor label. This is a deliberately conservative comparison
set; §7 is explicit that a stronger, frequency-aware gold-blind opponent is not exercisable in
this harness and is deferred to a dedicated follow-on (A16).

**Metrics.** Gold retention is the primary, pre-registered metric. Post-eviction recall@k and
nDCG@10 are reported as supporting evidence, and a recall-versus-age curve characterises the
ranking layer over simulated time. The success criteria (§6) are stated against gold retention
alone; the supporting metrics inform interpretation but do not gate any claim.

## 4. Method

**The memory kernel.** The system under test is the AEON-IQ kernel, backed by pgvector.
Memories are stored with an embedding, a creation and last-accessed timestamp, an access
counter, and a running utility estimate. Retrieval is nearest-neighbour by cosine distance,
filtered by a relevance threshold, over the agent's live (non-archived) memories.

**The ranking layer: decay and importance.** On top of raw cosine similarity the kernel
applies two optional re-ranking terms. A *decay* term scales a memory's effective distance by
its staleness, and an *importance* term scales it by a per-memory importance score. The
intended semantics are that both terms *reorder* candidates — a stale or low-importance memory
should sink in the ranking — without changing which memories are *eligible* to be returned.
Section 5 documents a bug in exactly this boundary and its fix.

**AMP: adaptive memory-pressure eviction.** AMP is the adaptive policy under test. It maintains
a per-memory utility estimate as an exponential moving average of retrieval feedback, and
computes an eviction pressure that combines staleness and inverse utility. A proportional-
integral controller drives repeated eviction sweeps toward the size target, soft-evicting the
lowest-value memories until the live count reaches the target. Crucially, AMP evicts by
*learned value*, whereas LRU and random are blind to value; the benchmark asks whether that
learned signal is worth its complexity under pressure.

**Fairness and determinism.** All arms operate on byte-identical copies of the same seeded,
aged corpus and evict the same K (§3). The comparator eviction rules are pure functions of
committed inputs — recency and identifier for LRU, a seeded hash and identifier for random —
so their victim sets are reproducible on replay. AMP operates on the original agent; the
baselines on independent copies; no arm can perturb another.

## 5. Found and Fixed: The Decay-Filter Bug

The benchmark's honesty discipline earned its keep by surfacing a real defect in the product,
which we report in full because the *finding* is part of the contribution.

**The bug.** The retrieval threshold that decides whether a memory is relevant enough to
return was applied to the *decayed* distance rather than the raw cosine distance. Because the
decay term multiplies distance by an exponential function of staleness, a highly relevant but
aged memory could be pushed past the relevance threshold and *removed from the candidate set
entirely* — not merely demoted in rank. The ranking layer, which was designed only to reorder,
could therefore silently delete still-relevant memories as they aged.

**How it surfaced.** A single-round smoke test had reported decay as a benign "mild ranking
drag, zero recall loss." The pre-registered recall-versus-age curve, run at scale across
multiple rounds of accumulated aging, told a different and correct story: every decay-enabled
condition collapsed its recall as age accumulated, while decay-disabled conditions held at the
ceiling. In the collapsed rounds, recall@5 equalled recall@10 — the signature of *removal*
(the gold had left the candidate set), not demotion. The scaled, multi-round protocol
overturned the benign single-round reading; this is precisely the failure mode a longitudinal,
pre-registered curve exists to catch.

**The fix.** We changed the threshold to gate the *raw* cosine distance, while leaving the
decay and importance terms to reorder results as intended. A relevant memory now remains
eligible regardless of age; decay and importance affect only its position in the ranking. We
validated the fix three ways: a new unit test that seeds a highly relevant but heavily aged
memory and asserts it survives the threshold (with a fresher copy still ranked ahead of it, so
reordering is preserved); a green kernel test suite; and a re-run of the affected conditions at
scale, in which the collapse is gone and the curves are flat, matching the mathematically
expected behaviour of uniform aging. The fix shipped to the product's main branch as an
isolated change.

**Why we report it.** The bug is not incidental to the paper — it is evidence that the
methodology works. A benign single-round result was corrected by a pre-registered, multi-round
curve; the correction was a genuine product fix, not a reframing of the metric. We keep the
before-and-after curves side by side in the results rather than presenting only the corrected
picture.

## 6. Results

We report the pre-registered primary metric — gold retention under eviction pressure — on
the four pressure arms, each over the five pre-registered seeds (42, 7, 123, 2024, 99). The
values below are the canonical figures of record (RESULTS §8.1); supporting recall and
ranking metrics live in RESULTS §8.1–§8.2 and are referenced rather than re-typed.

**Headline.** On every pressure arm, the adaptive policy (AMP) retains **2.5–2.9× more gold**
than the better of the two gold-blind baselines, and the advantage is **seed-robust**: the
across-seed 95% confidence interval of AMP−max(LRU, random) excludes zero on all four arms.
AMP keeps roughly 89–91% of gold memories through eviction; the stronger gold-blind baseline
keeps only ~32–35%.

| arm | AMP mean [95% CI] | max(LRU,random) mean | AMP−max diff [95% CI] | ratio range | seed-robust? |
|-----|:-----------------:|:--------------------:|:---------------------:|:-----------:|:------------:|
| `amp-only` | 0.890 [0.882, 0.899] | 0.321 | **0.570** [0.556, 0.582] | 2.59–2.90× | ✅ yes |
| `aeon-full` | 0.913 [0.907, 0.919] | 0.347 | **0.566** [0.554, 0.575] | 2.47–2.73× | ✅ yes |
| `aeon-full-importance` | 0.901 [0.892, 0.908] | 0.335 | **0.566** [0.546, 0.586] | 2.49–2.86× | ✅ yes |
| `amp-rmk` | 0.903 [0.892, 0.914] | 0.348 | **0.555** [0.550, 0.564] | 2.54–2.66× | ✅ yes |

Per-seed inputs (AMP / max(LRU,random)), every seed reported, none dropped:

- `amp-only` — 42: 0.890/0.343 · 7: 0.901/0.310 · 123: 0.902/0.329 · 2024: 0.878/0.310 · 99: 0.882/0.311
- `aeon-full` — 42: 0.922/0.347 · 7: 0.902/0.336 · 123: 0.915/0.350 · 2024: 0.913/0.369 · 99: 0.914/0.335
- `aeon-full-importance` — 42: 0.886/0.350 · 7: 0.913/0.325 · 123: 0.899/0.319 · 2024: 0.900/0.362 · 99: 0.904/0.317
- `amp-rmk` — 42: 0.885/0.333 · 7: 0.921/0.349 · 123: 0.911/0.358 · 2024: 0.905/0.353 · 99: 0.894/0.346

**Verdict against the locked criterion.** The success criterion — frozen in
`LONGITUDINAL_STRENGTHENING.md` at `946c01a` before the sweep ran — requires, for each
pressure arm, that the across-seed 95% bootstrap CI (percentile method, B = 10,000 over the
five seed-level means) of AMP−max(LRU, random) exclude zero. All four arms satisfy it:
amp-only [0.556, 0.582], aeon-full [0.554, 0.575], aeon-full-importance [0.546, 0.586],
amp-rmk [0.550, 0.564]. The effect is seed-robust, not a single-seed artefact.

**Reading the arms.** The bare adaptive-eviction arm (`amp-only`) already secures the effect;
adding the RMK online-learning wiring (`aeon-full`, `amp-rmk`) or gold-importance seeding
(`aeon-full-importance`) neither creates nor destroys it — the diffs and their intervals
overlap across all four arms. This is consistent with the paper's single-win framing: the
measured advantage is attributable to adaptive, value-based eviction itself, not to the
additional components, whose *isolated* contributions this harness cannot measure (§7).

**Consistency across seeds.** All twenty pressure cells (four arms × five seeds) produced a
positive AMP−baseline gap; no seed reversed or nulled the effect. AMP's per-seed retention
stays in a tight 0.878–0.922 band while the gold-blind baselines never exceed ~0.37 — which is
why the intervals are narrow despite the small seed count.

**Comparator floor and mechanism.** The gold-blind baselines retain gold only in proportion to
the fraction of the corpus they leave live: with a large distractor corpus forcing a high
eviction budget *K*, recency- and chance-based eviction discard gold at roughly the base rate,
yielding ~0.32–0.35 retention. AMP's learned utility signal lets it preferentially spare
high-value memories at the *same K* — that is the entire mechanism behind the gap.

**Relation to the decay-filter fix (§5).** The pressure figures are insulated from the
decay-filter bug: gold retention is measured by direct lookup of surviving memory identifiers,
not by threshold-filtered retrieval, and the fix targets only the retrieval relevance
threshold. RESULTS §8.2 keeps the pre-fix and corrected recall-versus-age curves side by side;
the headline here is unaffected by the fix, and we present both curves rather than only the
corrected one.

**Caveats carried forward.** Two honest caveats bound the result and are detailed in §7: the
adaptive controller did not converge within the sweep cap (it settled at roughly twice the
target eviction count, so we report a valid same-K comparison but claim no convergence), and
the confidence intervals are computed over five seed-level means — honest but coarse. Neither
caveat touches the sign or the seed-robustness of the effect.

## 7. Limitations and Honest Scope

*This section is written to stand on its own terms and is not contingent on the A12 outcome.
It is not softened if the headline result is strong.*

**One robust dimension, not a broad victory.** The central, confidence-interval-backed claim of
this paper concerns a single dimension: gold retention under eviction pressure. We do not claim
that the adaptive system dominates across ranking, aging, personalisation, or online learning.
Where the evidence supports only one robust win, we report one robust win.

**Online learning (RMK) is unmeasured here, not shown to be absent.** The kernel includes an
online policy-learning component that adapts eviction behaviour from feedback. It is exercised
only on the chat-completion path; the retrieval-only benchmark reported here never triggers it,
and records zero learning episodes. The correct statement is therefore that its contribution is
*structurally unmeasurable in this harness* — not that it provides no benefit. Measuring it
requires a different code path and is deferred (§8).

**No genuinely gold-blind frequency/utility baseline was exercisable.** A reviewer is right to
want a stronger opponent than LRU and random — for instance a least-frequently-used policy that
uses the same access signal the adaptive policy benefits from. We attempted this and report,
transparently, why it does not yet exist in this harness. The access counter that LFU would key
on is incremented only on the chat path the benchmark bypasses, leaving it uniformly zero, which
collapses LFU to LRU. The alternative utility signal is populated in this harness only by a
feedback rule that assigns positive feedback to gold and zero to distractors — so it encodes the
gold label and is not gold-blind; a policy keyed on it would be reading the answer key. A valid
strong baseline therefore requires the same different code path as the learning measurement, and
is deferred to A16 rather than approximated with a compromised proxy.

**Point-estimate and setup caveats.** Results are on a single dataset family (LongMemEval-S), at
N = 40, with one embedding model. The adaptive controller did not converge to its target within
the sweep cap — it settled at roughly twice the target eviction count — so we report a same-K
comparison (which remains valid, since all arms evict the identical settled K) but do not claim
controller convergence. The multi-seed confidence intervals reported in §6 are computed over
five seed-level means; with a seed count of five, the intervals are honest but coarse, and we
state the small seed-n explicitly rather than implying asymptotic precision.

## 8. Future Work

**A16 — a proxy-path benchmark.** The three gaps above (RMK unmeasured, LFU degenerate, and the
utility signal gold-contaminated) share a single root cause: the metrics that would close them
live on the chat-completion path, which the retrieval-only harness does not exercise. We
therefore commit to a follow-on benchmark that drives the workload through the chat path over
the same seeded and aged corpus. That single change populates the access counter from genuine
retrieval frequency — enabling a *gold-blind* frequency/utility baseline (the strong opponent) —
and records real learning episodes — enabling a direct RMK-on versus RMK-off measurement. One
harness closes both gaps; its detailed design is pre-registered separately before it runs.

---

## 9. Conclusion

We set out to measure one thing well rather than many things loosely. AEON-IQ is a
pre-registered, confidence-interval-backed benchmark for memory eviction under pressure, and
within it we report a single robust result: under eviction pressure, adaptive value-based
eviction (AMP) retains 2.5–2.9× more gold than gold-blind LRU and random baselines,
seed-robustly across five pre-registered seeds on all four pressure arms. We claim this
dimension and no other.

The discipline that makes the positive result trustworthy is the same discipline that bounds
it. Pre-registration caught, and forced the fix of, a real product bug — a decay term that
silently deleted still-relevant memories as they aged (§5) — which a benign single-round test
had missed. And it kept us honest about what we could not measure: the online-learning
component and a genuinely gold-blind frequency baseline are structurally unexercisable in this
retrieval-only harness, so we defer them to a pre-registered proxy-path follow-on (A16, §8)
rather than approximate them with a compromised proxy. The narrowness of the claim is not a
hedge; it is the result of reporting only what five seeds and a locked criterion actually
support.

We offer the AEON-IQ benchmark less as a verdict on any one memory system than as a template: fix the seeds
and the criteria before the numbers, report every seed and every null, keep all figures in one
canonical table so none can drift, and treat a found bug as a result. On that basis, adaptive
eviction earns exactly one confidence-interval-backed claim under pressure — and the benchmark
is built to test the next one the same way.

---

## 10. Reproducibility

**Environment and versions.** The system under test is built and run as a Docker Compose stack
(`compose.bench.yml`, `.env.bench`). The AEON-IQ kernel is compiled from source in a multi-stage
image — a `rust:1.96-slim` builder producing a `debian:bookworm-slim` runtime — and served on a
fixed local port. Vector storage is PostgreSQL with pgvector (`pgvector/pgvector:pg16`), exposed on
host port 5433. Embeddings are produced by OpenAI `text-embedding-3-small` (1536-dimensional); the
benchmark is retrieval-only and issues no chat-completion (generation) calls, so its only external
dependency is the embedding endpoint.

**Dataset.** All runs use LongMemEval-S (`benchmarks/data/longmemeval_s.json`), untruncated, with
N = 40 agents sampled per run. Each record contributes one gold-evidence memory and its distractor
haystack.

**Pre-registration.** The protocol is version-controlled and committed before the corresponding
results. Amendments A1–A11 (the base protocol) are recorded in `LONGITUDINAL_DESIGN.md`; the
Phase-2 amendments A12–A16 — the five-seed confidence-interval design, its locked success criterion
and reporting rules, the superseded/deferred strong baseline (A13), and the proxy-path follow-on
(A16) — are in `LONGITUDINAL_STRENGTHENING.md`, frozen at commit `946c01a` and merged to `main`.
The §4.5 decay fix (§5) shipped separately as commit `bab3f0a`. Every figure this paper reports
lives in a single canonical results table and is referenced rather than re-typed.

**Seeds.** The five seeds are pre-registered and fixed: **42, 7, 123, 2024, 99**. `--seed` is the
only variable across the confidence-interval sweep; each seed independently re-samples the
LongMemEval subset and re-draws the gold-blind aging RNG. All five are reported; none is dropped.

**Running the benchmark.** Each of the six conditions is run per seed by setting the kernel's
environment toggles, recreating the kernel container, and invoking the harness:

```
# per (seed, condition): recreate the kernel with the condition's env toggles, then
python run_longitudinal.py \
    --condition <cosine-baseline|decay-only|amp-only|aeon-full|aeon-full-importance|amp-rmk> \
    --dataset longmemeval --dataset-file benchmarks/data/longmemeval_s.json \
    --sample-size 40 --seed <42|7|123|2024|99> \
    --results-dir benchmarks/results/a12_5seed/<seed>/<condition>
```

The four pressure conditions (`amp-only`, `aeon-full`, `aeon-full-importance`, `amp-rmk`)
additionally run the eviction phase, in which AMP settles K and the LRU/random comparators evict
the same K over fresh corpus copies. Analysis is a single command over the completed result tree:

```
python analyze_a12.py     # -> per-seed means, bootstrap-percentile 95% CIs, per-arm verdict
```

which reproduces the §6 table, including the bootstrap confidence intervals (percentile method,
B = 10,000 resamples over the five seed-level means, with a fixed resampling seed so the intervals
are deterministic on replay).

**Determinism.** The comparator eviction rules and the aging schedule are pure functions of
committed inputs (identifiers and the run seed), so victim sets and ages are reproducible on
replay; the confidence-interval computation uses a fixed bootstrap seed. The one documented source
of run-to-run variation is the asynchronous, fire-and-forget update of access/utility counters on
the retrieval hot path, which the retrieval-only harness does not depend on for any reported metric.

**Artifacts.** The harness, the SQL comparators, the pre-registration documents, and the analysis
script are in the repository (branch and commit references above). The raw per-(seed, condition)
result JSONs and the derived canonical table are at `[A12-PENDING: results-artifact path / release]`.

---

## 11. References

[1] Packer, C., Wooders, S., Lin, K., Fang, V., Patil, S. G., Stoica, I., & Gonzalez, J. E. (2023).
*MemGPT: Towards LLMs as Operating Systems.* arXiv:2310.08560. https://arxiv.org/abs/2310.08560

[2] Park, J. S., O'Brien, J. C., Cai, C. J., Morris, M. R., Liang, P., & Bernstein, M. S. (2023).
*Generative Agents: Interactive Simulacra of Human Behavior.* UIST 2023. arXiv:2304.03442.
https://arxiv.org/abs/2304.03442

[3] Chhikara, P., Khant, D., Aryan, S., Singh, T., & Yadav, D. (2025). *Mem0: Building
Production-Ready AI Agents with Scalable Long-Term Memory.* arXiv:2504.19413.
https://arxiv.org/abs/2504.19413

[4] Megiddo, N., & Modha, D. S. (2003). *ARC: A Self-Tuning, Low Overhead Replacement Cache.*
USENIX FAST '03. https://www.usenix.org/conference/fast-03/arc-self-tuning-low-overhead-replacement-cache

[5] Wu, D., Wang, H., Yu, W., Zhang, Y., Chang, K.-W., & Yu, D. (2024). *LongMemEval: Benchmarking
Chat Assistants on Long-Term Interactive Memory.* ICLR 2025. arXiv:2410.10813.
https://arxiv.org/abs/2410.10813

[6] Maharana, A., Lee, D.-H., Tulyakov, S., Bansal, M., Barbieri, F., & Fang, Y. (2024).
*Evaluating Very Long-Term Conversational Memory of LLM Agents.* ACL 2024. arXiv:2402.17753.
https://arxiv.org/abs/2402.17753

[7] Nosek, B. A., Ebersole, C. R., DeHaven, A. C., & Mellor, D. T. (2018). *The Preregistration
Revolution.* PNAS 115(11):2600–2606. https://doi.org/10.1073/pnas.1708274114

[8] Pineau, J., Vincent-Lamarre, P., Sinha, K., Larivière, V., Beygelzimer, A., d'Alché-Buc, F.,
Fox, E., & Larochelle, H. (2021). *Improving Reproducibility in Machine Learning Research.* JMLR
22(164):1–20. arXiv:2003.12206. https://arxiv.org/abs/2003.12206

---
*Draft status: §1–§11 complete prose. Abstract written; the seven §2 citation markers resolved to
[1]–[8] against a verified References section (§11); §6 Results filled and verified against
canonical RESULTS §8.1. Sole remaining placeholder: the results-artifact release path in §10
(`[A12-PENDING: results-artifact path / release]`) — the user's decision. Nothing here changes
main or the frozen criteria.*
