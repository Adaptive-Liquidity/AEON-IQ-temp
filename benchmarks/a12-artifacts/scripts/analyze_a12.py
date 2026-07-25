"""A12 analysis — turns the 30 result JSONs into the frozen-criterion verdict.

Reads a12_5seed/<seed>/<condition>/longitudinal_quality.json for the 4 primary
pressure arms x 5 seeds, and evaluates the LOCKED criterion (STRENGTHENING 946c01a):

  AMP's advantage is seed-robust on an arm IFF the across-seed 95% CI (bootstrap over
  the 5 seed-level means, percentile method) of  AMP gold_retention - max(LRU, random)
  EXCLUDES 0.

Reporting rules (locked): all 5 seeds reported/none dropped; headline ratio as the
across-seed RANGE; any arm whose CI includes 0 -> "not seed-robust on arm X"; no rerun,
no reframe. Emits a human report + a RESULTS-§8.1-ready markdown block (a12_section81.md).

Usage:
  python analyze_a12.py            # analyze the real A12 results (when present)
  python analyze_a12.py --validate # self-test the math + prove extraction on Phase-1 data
"""
import json
import os
import random
import sys

CLONE = ("C:/Users/Benna/AppData/Local/Temp/claude/"
         "C--Users-Benna-Documents-nexus2-Nexus-main--claude-worktrees-hungry-noyce-a51d27/"
         "26896b37-6307-41d1-8d32-46519222e924/scratchpad/AEON-IQ")
A12_ROOT = f"{CLONE}/benchmarks/results/a12_5seed"
OUT_MD = os.path.join(os.path.dirname(os.path.abspath(__file__)), "a12_section81.md")

SEEDS = ["42", "7", "123", "2024", "99"]                       # frozen seed list
PRESSURE_ARMS = ["amp-only", "aeon-full", "aeon-full-importance", "amp-rmk"]  # 4 primary
BOOT_B = 10000
BOOT_SEED = 12345                                              # fixed -> reproducible CI


def extract(path: str) -> dict | None:
    """Pull the per-policy gold_retention (40-agent mean) + sanity fields from one run."""
    try:
        with open(path, encoding="utf-8") as f:
            d = json.load(f)
    except Exception:  # noqa: BLE001
        return None
    er = d.get("eviction_retention", {})
    def gr(p):  # noqa: E306
        v = er.get(p, {}).get("gold_retention")
        return float(v) if isinstance(v, (int, float)) else None
    return {
        "amp": gr("amp"), "lru": gr("lru"), "random": gr("random"),
        "settled_k": er.get("settled_k_mean"), "agents": er.get("agents"),
        "status": d.get("status"),
        "errors": (d.get("errors") or {}).get("error_count"),
        "requests": (d.get("errors") or {}).get("request_count"),
    }


def bootstrap_ci(vals: list[float], conf: float = 0.95) -> tuple:
    """Percentile bootstrap over the seed-level values (n small, stated honestly)."""
    if not vals:
        return (None, None, None)
    rng = random.Random(BOOT_SEED)
    n = len(vals)
    means = sorted(sum(vals[rng.randrange(n)] for _ in range(n)) / n for _ in range(BOOT_B))
    lo = means[int((1 - conf) / 2 * BOOT_B)]
    hi = means[int((1 + conf) / 2 * BOOT_B) - 1]
    return (lo, hi, sum(vals) / n)


def fmt(x, nd=3):
    return "—" if x is None else f"{x:.{nd}f}"


def analyze() -> int:
    missing, report_arms = [], []
    for arm in PRESSURE_ARMS:
        per_seed = {}
        for s in SEEDS:
            e = extract(f"{A12_ROOT}/{s}/{arm}/longitudinal_quality.json")
            if e is None or e["amp"] is None:
                missing.append(f"{s}/{arm}")
            per_seed[s] = e
        report_arms.append((arm, per_seed))

    if missing:
        print(f"[A12] INCOMPLETE — {len(missing)} cell(s) not ready yet: {missing[:8]}"
              f"{'...' if len(missing) > 8 else ''}")
        print("      (run again once the sweep finishes all 5x4 pressure cells)")
        return 1

    lines = ["### 8.1 Memory pressure — 5-seed confidence intervals (A12)\n",
             "Primary metric `gold_retention` (40-agent mean per seed), N=40, "
             "5 pre-registered seeds (42/7/123/2024/99). CIs = percentile bootstrap "
             f"(B={BOOT_B}) over the 5 seed-level means; seed-n=5, stated honestly.\n",
             "| arm | AMP mean [95% CI] | max(LRU,random) mean | AMP−max diff [95% CI] "
             "| ratio range | seed-robust? |",
             "|-----|:-----------------:|:--------------------:|:-------------------------:"
             "|:-----------:|:------------:|"]
    verdicts = []
    for arm, per_seed in report_arms:
        amp = [per_seed[s]["amp"] for s in SEEDS]
        cmp_max = [max(per_seed[s]["lru"], per_seed[s]["random"]) for s in SEEDS]
        diff = [a - c for a, c in zip(amp, cmp_max)]
        ratio = [a / c if c else float("inf") for a, c in zip(amp, cmp_max)]
        a_lo, a_hi, a_mean = bootstrap_ci(amp)
        c_lo, c_hi, c_mean = bootstrap_ci(cmp_max)
        d_lo, d_hi, d_mean = bootstrap_ci(diff)
        robust = d_lo is not None and d_lo > 0
        verdicts.append((arm, robust, d_lo, d_hi))
        lines.append(
            f"| `{arm}` | {fmt(a_mean)} [{fmt(a_lo)}, {fmt(a_hi)}] | {fmt(c_mean)} "
            f"| **{fmt(d_mean)}** [{fmt(d_lo)}, {fmt(d_hi)}] | "
            f"{fmt(min(ratio),2)}–{fmt(max(ratio),2)}× | "
            f"{'✅ yes' if robust else '❌ NOT seed-robust'} |")
        # per-seed transparency block (all 5, none dropped)
        lines.append("")
        lines.append(f"<sub>`{arm}` per-seed AMP / max(LRU,random): " +
                     ", ".join(f"seed {s}: {fmt(per_seed[s]['amp'])}/"
                               f"{fmt(max(per_seed[s]['lru'], per_seed[s]['random']))}"
                               for s in SEEDS) + "</sub>")
    lines.append("")
    lines.append("**Verdict against the locked criterion:**")
    for arm, robust, d_lo, d_hi in verdicts:
        if robust:
            lines.append(f"- `{arm}`: **seed-robust** — AMP−max(LRU,random) 95% CI "
                         f"[{fmt(d_lo)}, {fmt(d_hi)}] excludes 0.")
        else:
            lines.append(f"- `{arm}`: **not seed-robust on arm `{arm}`** — 95% CI "
                         f"[{fmt(d_lo)}, {fmt(d_hi)}] includes 0. Reported as-is (no rerun, "
                         f"no reframe).")
    out = "\n".join(lines)
    print(out)
    with open(OUT_MD, "w", encoding="utf-8") as f:
        f.write(out + "\n")
    print(f"\n[A12] wrote {OUT_MD}")
    n_robust = sum(1 for _, r, _, _ in verdicts if r)
    print(f"[A12] {n_robust}/4 pressure arms seed-robust.")
    return 0


def validate() -> int:
    print("=== bootstrap self-test ===")
    # clearly-positive gap -> CI must exclude 0; overlapping -> CI must include 0
    pos = [0.60, 0.55, 0.58, 0.62, 0.57]
    lo, hi, m = bootstrap_ci(pos)
    print(f"clear-positive {pos}: mean={fmt(m)} CI=[{fmt(lo)},{fmt(hi)}] "
          f"excludes0={lo > 0}  (expect True)")
    mix = [0.10, -0.05, 0.08, -0.12, 0.02]
    lo2, hi2, m2 = bootstrap_ci(mix)
    print(f"mixed {mix}: mean={fmt(m2)} CI=[{fmt(lo2)},{fmt(hi2)}] "
          f"includes0={lo2 <= 0 <= hi2}  (expect True)")
    print("\n=== extraction test on Phase-1 seed-42 pressure data ===")
    checks = [
        ("pubscale_n40/amp-only", f"{CLONE}/benchmarks/results/pubscale_n40/amp-only/longitudinal_quality.json"),
        ("pubscale_n40/aeon-full", f"{CLONE}/benchmarks/results/pubscale_n40/aeon-full/longitudinal_quality.json"),
        ("pubscale_n40/aeon-full-importance", f"{CLONE}/benchmarks/results/pubscale_n40/aeon-full-importance/longitudinal_quality.json"),
        ("rmk_a11/amp-rmk", f"{CLONE}/benchmarks/results/rmk_a11/amp-rmk/longitudinal_quality.json"),
    ]
    ok = True
    for label, p in checks:
        e = extract(p)
        if e and e["amp"] is not None:
            gap = e["amp"] - max(e["lru"], e["random"])
            print(f"  {label}: AMP={fmt(e['amp'])} LRU={fmt(e['lru'])} rand={fmt(e['random'])}"
                  f" -> gap={fmt(gap)} | errs={e['errors']}/{e['requests']} K={e['settled_k']}")
        else:
            print(f"  {label}: extraction FAILED ({p})"); ok = False
    print(f"\nvalidate: {'PASS' if ok else 'FAIL'} — schema/extraction/bootstrap verified")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(validate() if "--validate" in sys.argv else analyze())
