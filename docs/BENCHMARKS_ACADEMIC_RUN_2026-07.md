# Academic-Parameter Benchmark Run — July 2026

This document records a re-run of the benchmark suite with parameters upgraded
for the Nexus-IQ white paper, plus the two findings the upgrade surfaced. Raw
artifacts live in `benchmarks/results/academic-run/` (committed, exception to
the usual results gitignore).

## What changed vs the standard proof run

| Dimension | Standard proof run | This run |
|---|---|---|
| Latency sample size | 30 requests/scenario | **200 requests/scenario** |
| Retrieval scale tiers | 100, 1,000 | **100, 1,000, 10,000** (`BENCHMARK_INCLUDE_10000=true`) |
| Recall corpus | 7 memories, 14 queries | **+ extended benchmark: 120 targets among 1,000 distractors (1,120 memories), 360 queries** |
| Recall metrics | recall@{1,3,5}, precision@5 | **+ recall@10, MRR@10, nDCG@10, per-query-type and per-cluster breakdowns** |
| New script | — | `benchmarks/scripts/run_recall_extended.py` |

Environment: 4-vCPU Intel Xeon @ 2.80 GHz cloud container, 15 GiB RAM,
Linux 6.18.5, PostgreSQL 16.13 + pgvector 0.6.0 (native), rustc 1.94.1
release build, AEON-IQ `852f7cb`, mock upstream (`MOCK_EMBEDDING_MODE=hash`),
`RETRIEVAL_THRESHOLD=0.95`, `AMP_ENABLED=true`, `RMK_ENABLED=true`.
No CPU-governor control (shared tenancy) — treat absolute numbers as
indicative and the within-run deltas as the robust signal.

## Headline results

**Proxy latency (n=200/scenario):** direct mock p95 0.65 ms; proxy empty-memory
p50 3.39 / p95 6.18 ms; proxy 100-seeded p50 10.08 / p95 29.48 ms. Server-side
retrieval (audit log): p50 4.0 / p95 7.75 ms.

**Retrieval scale (search endpoint, k=5):** 100 → p50 2.85 ms; 1,000 → p50
10.72 ms; 10,000 → p50 95.07 ms.

**Extended recall (360 queries, 1,120-memory corpus):** recall@1 0.9306,
recall@10 0.9611, MRR@10 0.9454, nDCG@10 0.9495. By query type: full
paraphrase 1.000 across all metrics; keyword recall@1 0.9917; reduced-overlap
paraphrase recall@1 0.800. Search latency during evaluation: p50 8.5 /
p95 12.2 ms. Base 7-memory benchmark for continuity: recall@1 0.9286,
injected-expected rate 1.0.

**Correctness proofs:** temporal correctness 14/14 pass; narrative archival
8/8 pass. Suite gate: `proof_status: pass` (k6 optional, not run — no k6
binary in the environment).

**Token accounting (cl100k_base):** profile recall −37.0%; archival question
−23.0%; small-context overhead +231% (the suite's intentional negative case —
savings depend on history length vs injection size).

## Finding 1 — heavy tail invisible at n=30

At 200 requests, the seeded proxy path shows p99 ≈ 1,031 ms and max ≈ 3,023 ms
against a p50 of ~10 ms. The tail correlates with background work sharing the
4-vCPU host: per-turn extraction spawns, the AMP pressure sweep (scoring the
~11k seeded memories every 5 minutes), and RMK episode writes. Action:
isolate background sweeps from the hot path (worker role / pool partitioning)
and re-measure.

## Finding 2 — decay ORDER BY bypasses the HNSW index

At the 10,000-memory tier, `EXPLAIN (ANALYZE)` on the two-CTE retrieval query
shows a **sequential scan with top-N heapsort** (see
`benchmarks/results/academic-run/explain_10000_seqscan.txt`). pgvector can
only serve ANN queries whose sort key is the bare `embedding <=> $q`; the
decay/importance-modified expression prevents index use at any corpus size,
making retrieval latency linear in corpus size (2.85 → 10.72 → 95.07 ms p50).

Remedy (tracked): two-stage retrieval — ANN top-K (K≈100) ordered by raw
cosine (HNSW-served), then apply the decay/importance re-rank over the K
candidates. Exact whenever the modifier is cohort-constant (always true at the
neutral defaults λ=β=0), bounded-error otherwise.

## Failure-mode localization (extended recall)

All 14/360 misses are reduced-overlap paraphrases; 10 of 14 concentrate in one
cluster where the query's inflected token ("rotation") shares no token with
the stored fact ("rotate") — hash embeddings perform no stemming. Under a real
embedding model this miss class should disappear; this is a falsifiable
prediction for the semantic-evaluation roadmap item.

## Scope disclaimer

Identical to `docs/BENCHMARKS.md`: hash embeddings measure the retrieval
pipeline (ranking, thresholds, injection, audit) deterministically — not
semantic retrieval quality. No cross-system comparison is made or implied.
