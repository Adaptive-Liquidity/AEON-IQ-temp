### 8.1 Memory pressure — 5-seed confidence intervals (A12)

Primary metric `gold_retention` (40-agent mean per seed), N=40, 5 pre-registered seeds (42/7/123/2024/99). CIs = percentile bootstrap (B=10000) over the 5 seed-level means; seed-n=5, stated honestly.

| arm | AMP mean [95% CI] | max(LRU,random) mean | AMP−max diff [95% CI] | ratio range | seed-robust? |
|-----|:-----------------:|:--------------------:|:-------------------------:|:-----------:|:------------:|
| `amp-only` | 0.890 [0.882, 0.899] | 0.321 | **0.570** [0.556, 0.582] | 2.59–2.90× | ✅ yes |

<sub>`amp-only` per-seed AMP / max(LRU,random): seed 42: 0.890/0.343, seed 7: 0.901/0.310, seed 123: 0.902/0.329, seed 2024: 0.878/0.310, seed 99: 0.882/0.311</sub>
| `aeon-full` | 0.913 [0.907, 0.919] | 0.347 | **0.566** [0.554, 0.575] | 2.47–2.73× | ✅ yes |

<sub>`aeon-full` per-seed AMP / max(LRU,random): seed 42: 0.922/0.347, seed 7: 0.902/0.336, seed 123: 0.915/0.350, seed 2024: 0.913/0.369, seed 99: 0.914/0.335</sub>
| `aeon-full-importance` | 0.901 [0.892, 0.908] | 0.335 | **0.566** [0.546, 0.586] | 2.49–2.86× | ✅ yes |

<sub>`aeon-full-importance` per-seed AMP / max(LRU,random): seed 42: 0.886/0.350, seed 7: 0.913/0.325, seed 123: 0.899/0.319, seed 2024: 0.900/0.362, seed 99: 0.904/0.317</sub>
| `amp-rmk` | 0.903 [0.892, 0.914] | 0.348 | **0.555** [0.550, 0.564] | 2.54–2.66× | ✅ yes |

<sub>`amp-rmk` per-seed AMP / max(LRU,random): seed 42: 0.885/0.333, seed 7: 0.921/0.349, seed 123: 0.911/0.358, seed 2024: 0.905/0.353, seed 99: 0.894/0.346</sub>

**Verdict against the locked criterion:**
- `amp-only`: **seed-robust** — AMP−max(LRU,random) 95% CI [0.556, 0.582] excludes 0.
- `aeon-full`: **seed-robust** — AMP−max(LRU,random) 95% CI [0.554, 0.575] excludes 0.
- `aeon-full-importance`: **seed-robust** — AMP−max(LRU,random) 95% CI [0.546, 0.586] excludes 0.
- `amp-rmk`: **seed-robust** — AMP−max(LRU,random) 95% CI [0.550, 0.564] excludes 0.
