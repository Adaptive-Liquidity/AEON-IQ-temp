"""Hourly watchdog for the A12 5-seed sweep (run_a12.py, PID in run_a12.pid).

Every hour, independently of any chat session, it records a health snapshot and, if
the sweep PROCESS HAS DIED before finishing, restarts it (the sweep is resume-guarded,
so it picks up at the next incomplete cell). Restarts are capped so a persistent failure
can't loop. A stalled-but-alive run is flagged, not force-killed (run_a12.py already has a
2h/condition timeout). Exits when all 30 cells (5 seeds x 6 conditions) are done.

Human-readable history -> watchdog_a12.log; latest structured snapshot -> watchdog_status.json.
"""
import json
import os
import subprocess
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
SWEEP = os.path.join(HERE, "run_a12.py")
PIDFILE = os.path.join(HERE, "run_a12.pid")
SWEEP_LOG = os.path.join(HERE, "run_a12.log")
SWEEP_ERR = os.path.join(HERE, "run_a12.err.log")
WLOG = os.path.join(HERE, "watchdog_a12.log")
WSTATUS = os.path.join(HERE, "watchdog_status.json")

CLONE = ("C:/Users/Benna/AppData/Local/Temp/claude/"
         "C--Users-Benna-Documents-nexus2-Nexus-main--claude-worktrees-hungry-noyce-a51d27/"
         "26896b37-6307-41d1-8d32-46519222e924/scratchpad/AEON-IQ")
SUMMARY = f"{CLONE}/benchmarks/results/a12_5seed/_summary.json"

INTERVAL_S = 3600           # hourly
TOTAL_CELLS = 30            # 5 seeds x 6 conditions
MAX_RESTARTS = 3
MAX_RUNTIME_H = 30          # safety cap on the watchdog itself
STALL_HOURS = 3            # flag if completed count hasn't moved in this many checks


def wlog(msg: str) -> None:
    line = f"[{time.strftime('%Y-%m-%d %H:%M:%S')}] {msg}"
    print(line, flush=True)
    with open(WLOG, "a", encoding="utf-8") as f:
        f.write(line + "\n")


def sh(args: list[str], timeout: int = 30) -> str:
    try:
        r = subprocess.run(args, capture_output=True, text=True, timeout=timeout)
        return (r.stdout or "").strip()
    except Exception as e:  # noqa: BLE001
        return f"__err__:{e!r}"


def pid_alive(pid: int) -> bool:
    out = sh(["tasklist", "/FI", f"PID eq {pid}", "/NH"])
    return str(pid) in out


def read_pid() -> int | None:
    try:
        with open(PIDFILE, encoding="utf-8") as f:
            return int(f.read().strip())
    except Exception:  # noqa: BLE001
        return None


def _has_real_data(jp: str) -> bool:
    """A cell only counts if its JSON has real data — not just a status/exit code."""
    try:
        with open(jp, encoding="utf-8") as f:
            d = json.load(f)
        return (isinstance(d.get("units_available"), int) and d["units_available"] > 0
                and isinstance(d.get("seeded_memory_count"), int)
                and d["seeded_memory_count"] > 0)
    except Exception:  # noqa: BLE001
        return False


def summary_state() -> dict:
    try:
        with open(SUMMARY, encoding="utf-8") as f:
            rows = json.load(f)
    except Exception:  # noqa: BLE001
        return {"completed": 0, "ok": 0, "bad": [], "rows": 0}
    ok, bad = 0, []
    for r in rows:
        st = str(r.get("status", ""))
        real = _has_real_data(r.get("results_json", ""))
        if st.startswith("ok") and real:   # key on REAL DATA, not exit code
            ok += 1
        else:
            bad.append(f"{r.get('seed')}/{r.get('condition')}={st}{'' if real else '/EMPTY'}")
    return {"completed": len(rows), "ok": ok, "bad": bad, "rows": len(rows)}


def current_cell() -> str:
    try:
        with open(SWEEP_LOG, encoding="utf-8") as f:
            for line in reversed(f.readlines()):
                if "=== seed=" in line:
                    return line.split("===")[1].strip()
    except Exception:  # noqa: BLE001
        pass
    return "?"


def docker_health(name: str) -> str:
    return sh(["docker", "inspect", "-f", "{{.State.Health.Status}}", name]) or "?"


def kernel_error_count() -> int:
    out = sh(["docker", "logs", "aeonbench-memoryos"], timeout=40)
    if out.startswith("__err__"):
        return -1
    return sum(1 for ln in out.splitlines() if "ERROR" in ln or "500 Internal" in ln)


def live_rows() -> str:
    return sh(["docker", "exec", "-e", "PGPASSWORD=memoryos_secret", "aeonbench-postgres",
               "psql", "-U", "memoryos", "-d", "memoryos", "-t", "-A", "-c",
               "SELECT count(*) FROM memories WHERE archived_at IS NULL;"]) or "?"


def restart_sweep() -> int | None:
    try:
        DETACHED = 0x00000008 | 0x00000200  # DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP
        out = open(SWEEP_LOG, "a", encoding="utf-8")
        err = open(SWEEP_ERR, "a", encoding="utf-8")
        p = subprocess.Popen([sys.executable, SWEEP], stdout=out, stderr=err,
                             creationflags=DETACHED, close_fds=True)
        with open(PIDFILE, "w", encoding="utf-8") as f:
            f.write(str(p.pid))
        return p.pid
    except Exception as e:  # noqa: BLE001
        wlog(f"  RESTART FAILED: {e!r}")
        return None


def main() -> None:
    wlog(f"watchdog start: hourly checks, {TOTAL_CELLS} cells expected, "
         f"max_restarts={MAX_RESTARTS}")
    restarts = 0
    last_completed = -1
    stall_count = 0
    t_start = time.time()
    while True:
        if (time.time() - t_start) / 3600 > MAX_RUNTIME_H:
            wlog("watchdog runtime cap reached — exiting"); return
        pid = read_pid()
        alive = pid_alive(pid) if pid else False
        s = summary_state()
        cell = current_cell()
        mem_h = docker_health("aeonbench-memoryos")
        pg_h = docker_health("aeonbench-postgres")
        errs = kernel_error_count()
        rows = live_rows()

        # progress / stall tracking
        if s["completed"] > last_completed:
            stall_count = 0
        else:
            stall_count += 1
        last_completed = s["completed"]

        health = "OK"
        action = ""
        if s["completed"] >= TOTAL_CELLS:
            if s["ok"] >= TOTAL_CELLS:
                wlog(f"DONE (CLEAN): {s['ok']}/{TOTAL_CELLS} cells verified REAL. Watchdog exiting.")
                _snapshot(pid, alive, s, cell, mem_h, pg_h, errs, rows, restarts, "DONE")
            else:
                wlog(f"DONE-WITH-FAILURES: only {s['ok']}/{TOTAL_CELLS} real; bad={s['bad']}. "
                     f"NEEDS ATTENTION — DO NOT ANALYZE. Watchdog exiting.")
                _snapshot(pid, alive, s, cell, mem_h, pg_h, errs, rows, restarts,
                          "DONE-WITH-FAILURES")
            return
        if not alive:
            if restarts < MAX_RESTARTS:
                newpid = restart_sweep()
                restarts += 1
                action = f"RESTARTED sweep (pid={newpid}, restart {restarts}/{MAX_RESTARTS})"
                health = "RECOVERED"
            else:
                action = "sweep DEAD and restart cap reached — needs a human"
                health = "CRITICAL"
        elif stall_count >= STALL_HOURS:
            action = f"no new cell completed in {stall_count}h — possible stall (not killing)"
            health = "STALLED"

        bad = f" bad={s['bad']}" if s["bad"] else ""
        wlog(f"{health}: {s['ok']}/{TOTAL_CELLS} done | cur[{cell}] | pid={pid} alive={alive} "
             f"| mem={mem_h} pg={pg_h} | kernErr={errs} liveRows={rows}{bad}"
             + (f" | {action}" if action else ""))
        _snapshot(pid, alive, s, cell, mem_h, pg_h, errs, rows, restarts, health)
        time.sleep(INTERVAL_S)


def _snapshot(pid, alive, s, cell, mem_h, pg_h, errs, rows, restarts, health) -> None:
    try:
        with open(WSTATUS, "w", encoding="utf-8") as f:
            json.dump({"ts": time.strftime("%Y-%m-%d %H:%M:%S"), "health": health,
                       "completed": s["completed"], "ok": s["ok"], "bad": s["bad"],
                       "current_cell": cell, "sweep_pid": pid, "sweep_alive": alive,
                       "memoryos": mem_h, "postgres": pg_h, "kernel_errors": errs,
                       "live_rows": rows, "restarts_used": restarts}, f, indent=2)
    except Exception:  # noqa: BLE001
        pass


if __name__ == "__main__":
    main()
