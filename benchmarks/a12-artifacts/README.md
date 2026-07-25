# AEON-IQ A12 Benchmark Artifacts

Reproducibility bundle for the **A12** result of the AEON-IQ longitudinal memory-eviction
benchmark: under memory pressure, adaptive value-based eviction (**AMP**) retains **~2.5–2.9×
more gold** than gold-blind LRU/random baselines, and the advantage is **seed-robust** across
5 pre-registered seeds on all four pressure arms.

Release: **v0.1.0-a12** · Benchmarked system: **AEON-IQ** (Rust kernel + pgvector + AMP eviction).

---

## Headline (canonical: `results/LONGITUDINAL_RESULTS.md` §8.1)

Primary metric `gold_retention` (fraction of gold memories surviving eviction), N=40 agents,
`longmemeval_s`, seeds 42/7/123/2024/99. CIs = across-seed 95% percentile bootstrap
(B=10,000, over the 5 seed-level means) of AMP − max(LRU, random).

| arm | AMP mean [95% CI] | max(LRU,random) | AMP−max diff [95% CI] | seed-robust? |
|-----|:-----------------:|:---------------:|:---------------------:|:------------:|
| `amp-only` | 0.890 [0.882, 0.899] | 0.321 | 0.570 [0.556, 0.582] | ✅ |
| `aeon-full` | 0.913 [0.907, 0.919] | 0.347 | 0.566 [0.554, 0.575] | ✅ |
| `aeon-full-importance` | 0.901 [0.892, 0.908] | 0.335 | 0.566 [0.546, 0.586] | ✅ |
| `amp-rmk` | 0.903 [0.892, 0.914] | 0.348 | 0.555 [0.550, 0.564] | ✅ |

The across-seed 95% CI excludes 0 on every pressure arm → the effect is not a single-seed artefact.

## Contents

```
results/
  a12_5seed/<seed>/<condition>/longitudinal_quality.{json,csv}   raw per-cell outputs (30 cells)
  a12_5seed/_summary.json                                        run summary
  LONGITUDINAL_RESULTS.md                                        canonical results doc (§8.1 = source of truth)
tables/
  canonical_results_section_8_1.md                              the §8.1 table snapshot used in the papers
scripts/
  analyze_a12.py        reads the 20 pressure JSONs -> per-seed means + bootstrap 95% CIs + per-arm verdict
  run_a12.py            the 5-seed sweep launcher (kernel-reuse, real-data validation, resume-guard)
  watchdog_a12.py       hourly run monitor used during the sweep
preregistration/
  LONGITUDINAL_STRENGTHENING.md   frozen Phase-2 pre-registration (amendments A12–A16, locked criterion)
  LONGITUDINAL_DESIGN.md          base protocol + amendment trail
papers/
  research_paper_draft.md   the AEON-IQ research paper (DRAFT)
  white_paper_draft.md      the Nexus-IQ white paper (DRAFT)
MANIFEST.sha256             SHA-256 of every file in this bundle
```

## Reproduce the analysis

The verdict is deterministic from the raw JSONs:

```bash
PYTHONIOENCODING=utf-8 PYTHONUTF8=1 python scripts/analyze_a12.py
```

It reads the 20 pressure-arm `longitudinal_quality.json` files, computes each arm's per-seed
`gold_retention` means and the across-seed percentile bootstrap 95% CI (B=10,000, fixed
resampling seed 12345 → identical on replay), and prints the per-arm seed-robust verdict. The
numbers reproduce the §8.1 table above.

To re-run the sweep itself, see `scripts/run_a12.py` and the harness/kernel on `main` (below).

## Provenance

- Repository: `github.com/adaptiveliquidity/AEON-IQ` (branch `main`).
- Harness + `LONGITUDINAL_RESULTS.md` §8.1 + force-sweep endpoint: PR #42 (squash `3cc6784`).
- `LONGITUDINAL_DESIGN.md` §13d caveat: PR #45 (squash `cd7632a`).
- §4.5 decay-filter fix (independent): PR #43 (`bab3f0a`).
- Pre-registration frozen before the sweep: `LONGITUDINAL_STRENGTHENING.md` at commit `946c01a`.

## Honest scope (see the papers' Limitations §7)

Single benchmark family (`longmemeval_s`), N=40, one embedding model. The AMP controller did
**not** converge within the sweep cap (settled at ~2× the target eviction count → we report a
valid same-K comparison, not a convergence claim). CIs are over 5 seed-level means (honest but
coarse). The online-learning (RMK) component and a genuinely gold-blind frequency baseline are
**structurally unmeasurable** in this retrieval-only harness (0 learning episodes; the access
counter and utility signal are chat-path-only) and are deferred to a pre-registered proxy-path
follow-on (A16).

## Note on the papers

`papers/*.md` are **drafts**. The research paper's §10 "results-artifact" link points to *this*
release; that URL is filled in the draft once this release is published (it is the only
remaining placeholder). The white paper's two figures are specified inline for a designer to
render. Every figure/number traces to `results/LONGITUDINAL_RESULTS.md` §8.1.

## Integrity

```bash
sha256sum -c MANIFEST.sha256
```
