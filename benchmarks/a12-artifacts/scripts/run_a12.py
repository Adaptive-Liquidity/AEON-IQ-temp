"""A12 — 5-seed confidence-interval sweep (frozen pre-registration 946c01a).

HARDENED re-run (v2) after the first run silently skipped 19 cells on a transient kernel
outage. Robustness fixes ONLY — the pre-registration and criteria are unchanged:

  FIX 1 (reachability): recreate_kernel now confirms the kernel actually answers
    GET http://localhost:8080/api/v1/stats from the host (not just docker "healthy"),
    and RETRIES the whole recreate if it can't — a transient outage makes a cell WAIT,
    never silently skip.
  FIX 2 (real-data success test): a cell counts as done ONLY if its JSON has real data
    (units_available > 0, seeded_memory_count > 0, request_count > 0, and for pressure
    arms eviction_retention.amp.gold_retention present) — NOT the process exit code.
    An empty/"not_run" cell is retried once, then recorded FAILED-EMPTY.
  FIX 3 (fail-loud): any cell that lands empty is logged with a visible banner and the
    run ends with an explicit FAILED-CELLS summary instead of a hollow "all ok".

Seeds 42/7/123/2024/99, N=40, 6 conditions each, sequential, fresh on the fixed kernel.
Comparators AMP vs LRU vs random (harness at origin e811ef0 — no LFU). Resume-guarded on
REAL data (a stub JSON never counts as done). Launched DETACHED via PowerShell Start-Process.
"""
import json
import os
import subprocess
import sys
import time
import urllib.request

CLONE = ("C:/Users/Benna/AppData/Local/Temp/claude/"
         "C--Users-Benna-Documents-nexus2-Nexus-main--claude-worktrees-hungry-noyce-a51d27/"
         "26896b37-6307-41d1-8d32-46519222e924/scratchpad/AEON-IQ")
SCRIPTS = f"{CLONE}/benchmarks/scripts"
HARNESS = f"{SCRIPTS}/run_longitudinal.py"
DATASET = f"{CLONE}/benchmarks/data/longmemeval_s.json"
RESULTS = f"{CLONE}/benchmarks/results/a12_5seed"
COMPOSE = "compose.bench.yml"
ENVFILE = ".env.bench"
SAMPLE = "40"
SEEDS = ["42", "7", "123", "2024", "99"]
COND_TIMEOUT_S = 7200
MGMT_KEY = "sk-bench-local-mgmt-key"
STATS_URL = "http://localhost:8080/api/v1/stats"
PRESSURE = {"amp-only", "aeon-full", "aeon-full-importance", "amp-rmk"}

BASE_ENV = {"MEMORYOS_ROLE": "all", "DEDUP_THRESHOLD": "0"}

CONDITIONS = [
    ("cosine-baseline", {"MEMORY_DECAY_RATE": "0.0", "IMPORTANCE_BOOST_FACTOR": "0.0",
                         "GRAPH_RETRIEVAL_ENABLED": "false", "AMP_ENABLED": "false",
                         "RMK_ENABLED": "false"}),
    ("decay-only", {"MEMORY_DECAY_RATE": "0.03", "IMPORTANCE_BOOST_FACTOR": "0.5",
                    "GRAPH_RETRIEVAL_ENABLED": "false", "AMP_ENABLED": "false",
                    "RMK_ENABLED": "false"}),
    ("amp-only", {"MEMORY_DECAY_RATE": "0.0", "IMPORTANCE_BOOST_FACTOR": "0.0",
                  "GRAPH_RETRIEVAL_ENABLED": "false", "AMP_ENABLED": "true",
                  "RMK_ENABLED": "false"}),
    ("aeon-full", {"MEMORY_DECAY_RATE": "0.03", "IMPORTANCE_BOOST_FACTOR": "0.5",
                   "GRAPH_RETRIEVAL_ENABLED": "true", "AMP_ENABLED": "true",
                   "RMK_ENABLED": "true"}),
    ("aeon-full-importance", {"MEMORY_DECAY_RATE": "0.03", "IMPORTANCE_BOOST_FACTOR": "0.5",
                              "GRAPH_RETRIEVAL_ENABLED": "true", "AMP_ENABLED": "true",
                              "RMK_ENABLED": "true"}),
    ("amp-rmk", {"MEMORY_DECAY_RATE": "0.0", "IMPORTANCE_BOOST_FACTOR": "0.0",
                 "GRAPH_RETRIEVAL_ENABLED": "false", "AMP_ENABLED": "true",
                 "RMK_ENABLED": "true", "RMK_UPDATE_COOLDOWN_SECS": "60",
                 "RMK_MIN_EPISODES_BEFORE_UPDATE": "5"}),
]

HARNESS_ENV = {
    "AEON_SEMANTIC_BENCHMARK": "true", "MANAGEMENT_API_KEY": MGMT_KEY,
    "AEON_BASE_URL": "http://localhost:8080",
    "DATABASE_URL": "postgresql://memoryos:memoryos_secret@localhost:5433/memoryos",
    "LONGITUDINAL_SEED_CONCURRENCY": "16", "PYTHONUTF8": "1", "PYTHONIOENCODING": "utf-8",
}


def log(msg: str) -> None:
    print(f"[{time.strftime('%Y-%m-%d %H:%M:%S')}] {msg}", flush=True)


def build_kernel_once() -> bool:
    log("building fixed kernel image from clone source (once)...")
    r = subprocess.run(
        ["docker", "compose", "-f", COMPOSE, "--env-file", ENVFILE, "build", "memoryos"],
        cwd=CLONE, env={**os.environ, **BASE_ENV}, capture_output=True, text=True)
    if r.returncode != 0:
        log(f"  BUILD FAILED: {r.stderr.strip()[-500:]}"); return False
    log("  build ok"); return True


def kernel_reachable(timeout: int = 6) -> bool:
    """FIX 1: real host-side reachability — the exact check the harness makes at startup."""
    try:
        req = urllib.request.Request(STATS_URL, headers={"X-Management-Key": MGMT_KEY})
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            return 200 <= getattr(resp, "status", resp.getcode()) < 300
    except Exception:  # noqa: BLE001
        return False


def recreate_kernel(cond_env: dict) -> bool:
    """FIX 1: recreate, wait for docker health, THEN confirm the kernel answers /stats.
    Retries the whole recreate up to 3x; a transient outage makes the cell wait, not skip."""
    env = {**os.environ, **BASE_ENV, **cond_env}
    for attempt in range(1, 4):
        r = subprocess.run(
            ["docker", "compose", "-f", COMPOSE, "--env-file", ENVFILE,
             "up", "-d", "--force-recreate", "memoryos"],
            cwd=CLONE, env=env, capture_output=True, text=True)
        if r.returncode != 0:
            log(f"  docker recreate FAILED (attempt {attempt}): {r.stderr.strip()[:300]}")
            time.sleep(5); continue
        healthy = False
        for _ in range(40):
            h = subprocess.run(["docker", "inspect", "-f", "{{.State.Health.Status}}",
                                "aeonbench-memoryos"], capture_output=True, text=True)
            if h.stdout.strip() == "healthy":
                healthy = True; break
            time.sleep(3)
        if not healthy:
            log(f"  kernel not healthy in 120s (attempt {attempt}) — retrying recreate")
            continue
        # NEW: docker says healthy — now confirm the HOST can actually reach /api/v1/stats.
        for _ in range(20):  # up to ~60s for the port/app to actually serve
            if kernel_reachable():
                return True
            time.sleep(3)
        log(f"  healthy but /api/v1/stats UNREACHABLE (attempt {attempt}) — retrying recreate")
    log("  kernel never became reachable after 3 recreate attempts")
    return False


_KERNEL_ENV = None  # env dict currently applied to the RUNNING kernel (for reuse)


def ensure_kernel(cond_env: dict) -> bool:
    """Reuse the running kernel across cells; force-recreate ONLY when the condition's
    kernel env differs from what's already running. This removes the per-cell recreate
    churn (~40 recreates) that wedged the Docker port-forward. A cell needing a different
    decay/importance/AMP/RMK config gets a fresh kernel; a cell whose config already
    matches the running kernel reuses it (verified reachable first)."""
    global _KERNEL_ENV
    target = {**BASE_ENV, **cond_env}
    if _KERNEL_ENV == target and kernel_reachable():
        log("  reuse running kernel — same env, reachable (no recreate)")
        return True
    if not recreate_kernel(cond_env):
        return False
    _KERNEL_ENV = target
    return True


def validate_result(jp: str, name: str) -> tuple[bool, str]:
    """FIX 2: a cell is done ONLY with real data — not the process exit code."""
    try:
        with open(jp, encoding="utf-8") as f:
            d = json.load(f)
    except Exception as e:  # noqa: BLE001
        return False, f"no/invalid JSON ({e})"
    units = d.get("units_available")
    seeded = d.get("seeded_memory_count")
    reqs = (d.get("errors") or {}).get("request_count")
    if not isinstance(units, int) or units <= 0:
        return False, f"units_available={units}"
    if not isinstance(seeded, int) or seeded <= 0:
        return False, f"seeded_memory_count={seeded}"
    if not isinstance(reqs, int) or reqs <= 0:
        return False, f"request_count={reqs}"
    if name in PRESSURE:
        amp = ((d.get("eviction_retention") or {}).get("amp") or {}).get("gold_retention")
        if not isinstance(amp, (int, float)):
            return False, "eviction_retention.amp.gold_retention missing"
    return True, "ok"


def run_one(seed: str, name: str, cond_env: dict) -> dict:
    out_dir = f"{RESULTS}/{seed}/{name}"
    jp = f"{out_dir}/longitudinal_quality.json"
    # Resume guard on REAL data — a stub JSON is NOT "done".
    if os.path.exists(jp) and validate_result(jp, name)[0]:
        log(f"  skip seed={seed} {name} (already complete, real data)")
        return {"seed": seed, "condition": name, "status": "ok (resumed)", "results_json": jp}
    reason = "not attempted"
    for attempt in (1, 2):  # run + one retry on empty/outage
        log(f"=== seed={seed} {name} (attempt {attempt}) === toggles={cond_env}")
        if not ensure_kernel(cond_env):
            reason = "kernel unreachable"; log(f"  !! kernel unreachable, attempt {attempt}")
            continue
        os.makedirs(out_dir, exist_ok=True)
        env = {**os.environ, **HARNESS_ENV,
               "AMP_ENABLED": cond_env["AMP_ENABLED"], "RMK_ENABLED": cond_env["RMK_ENABLED"]}
        cmd = [sys.executable, HARNESS, "--condition", name, "--dataset", "longmemeval",
               "--dataset-file", DATASET, "--sample-size", SAMPLE, "--seed", seed,
               "--results-dir", out_dir]
        t0 = time.time()
        try:
            r = subprocess.run(cmd, cwd=SCRIPTS, env=env, capture_output=True, text=True,
                               timeout=COND_TIMEOUT_S)
            rc, tail = r.returncode, "\n".join((r.stdout or "").splitlines()[-6:])
        except subprocess.TimeoutExpired:
            rc, tail = -1, f"TIMEOUT after {COND_TIMEOUT_S}s (killed)"
        dt = (time.time() - t0) / 60
        ok, reason = validate_result(jp, name)
        if ok:
            log(f"  -> seed={seed} {name}: OK ({dt:.1f} min)")
            return {"seed": seed, "condition": name, "status": "ok",
                    "minutes": round(dt, 1), "results_json": jp}
        # FIX 3: fail loud — do NOT record ok on an empty/invalid cell.
        log(f"  !!! EMPTY/INVALID seed={seed} {name}: {reason} "
            f"(rc={rc}, {dt:.1f} min, attempt {attempt})\n{tail}")
    log(f"  ##### FAILED-EMPTY seed={seed} {name}: no real data after 2 attempts ({reason})")
    return {"seed": seed, "condition": name, "status": f"FAILED-EMPTY:{reason}",
            "minutes": None, "results_json": jp}


def main() -> None:
    os.makedirs(RESULTS, exist_ok=True)
    log(f"A12 sweep (v2 hardened) start: seeds={SEEDS}, N={SAMPLE}, "
        f"{len(CONDITIONS)} conditions, sequential")
    if not build_kernel_once():
        log("ABORT: kernel build failed"); return
    summary, grand_t0 = [], time.time()
    # condition-OUTER so same-env cells run back-to-back -> ensure_kernel reuses the
    # running kernel and recreates only on a genuine env change (minimizes port-churn).
    for name, cond_env in CONDITIONS:
        for seed in SEEDS:
            try:
                res = run_one(seed, name, cond_env)
            except Exception as e:  # noqa: BLE001
                res = {"seed": seed, "condition": name, "status": f"exception:{e!r}"}
            summary.append(res)
            with open(f"{RESULTS}/_summary.json", "w", encoding="utf-8") as f:
                json.dump(summary, f, indent=2)
    fails = [r for r in summary if not str(r["status"]).startswith("ok")]
    mins = (time.time() - grand_t0) / 60
    if fails:  # FIX 3: loud, unmissable failure summary
        log("#" * 64)
        log(f"### A12 FINISHED WITH {len(fails)}/{len(summary)} FAILED CELL(S) — NOT CLEAN")
        for f in fails:
            log(f"###   {f['seed']}/{f['condition']}: {f['status']}")
        log("### Re-run (resume-guard fills only the failed cells). Do NOT analyze yet.")
        log("#" * 64)
    else:
        log(f"A12 ALL {len(summary)} CELLS OK (real data verified) in {mins:.1f} min")
    log("SUMMARY: " + json.dumps(summary, indent=2))


if __name__ == "__main__":
    main()
