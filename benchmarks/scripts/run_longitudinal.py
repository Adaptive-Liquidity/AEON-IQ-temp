#!/usr/bin/env python3
"""Longitudinal memory benchmark — history-dependent recall for AEON-IQ.

Authority: benchmarks/LONGITUDINAL_DESIGN.md is the frozen pre-registration.
This script implements that design faithfully and adds NO parameter that could
tune the timeline to win.  Where the design fixes a value, this script uses it as
the default; where the design says "committed rule," this script encodes exactly
that rule.  If a discrepancy is found, LONGITUDINAL_DESIGN.md wins.

WHY THIS EXISTS (§0 of the design): run_semantic_quality.py seeds every memory at
age ~0 with uniform importance, no co-access edges, no feedback, no pressure — a
state where retrieval collapses to cosine and aeon-full == cosine-baseline.  This
benchmark gives memories *history* (age, access, feedback, pressure) so the
differentiators (time-decay, importance, AMP co-access + eviction, RMK feedback
learning) are actually exercised, and measures whether they produce a
mechanism-attributable lift.

FAIRNESS INVARIANTS (do not weaken):
  * Aging is uniform-at-random, seeded, and GOLD-BLIND (benchmarks/sql/
    longitudinal_age.sql).  Gold and distractor memories draw from the same
    distribution.  This code never special-cases gold in aging or eviction.
  * The ONLY gold-awareness is (a) in scoring (recall/MRR/nDCG use gold ids) and
    (b) in the feedback rule (1.0 on the query's gold-evidence memory, 0.0 on
    retrieved distractors) — both pre-registered (§2).
  * LRU / random comparators evict the SAME count K that the AMP PI controller
    settled on, over FRESH COPIES of the same aged corpus (§5).

────────────────────────────────────────────────────────────────────────────────
CONDITION MATRIX — the `--condition` label is recorded in results; the ACTUAL
condition is set by the KERNEL's environment (this script does not reconfigure the
kernel).  Run each condition against a kernel booted with the matching env (§3):

  cosine-baseline        MEMORY_DECAY_RATE=0  IMPORTANCE_BOOST_FACTOR=0
                         AMP_ENABLED=false    RMK_ENABLED=false
                         GRAPH_RETRIEVAL_ENABLED=false
  decay-only             MEMORY_DECAY_RATE=<default>  IMPORTANCE_BOOST_FACTOR=<default>
                         AMP_ENABLED=false    RMK_ENABLED=false
  amp-only               MEMORY_DECAY_RATE=0  IMPORTANCE_BOOST_FACTOR=0
                         AMP_ENABLED=true     RMK_ENABLED=false
  aeon-full              MEMORY_DECAY_RATE=<default>  IMPORTANCE_BOOST_FACTOR=<default>
                         AMP_ENABLED=true     RMK_ENABLED=true
                         GRAPH_RETRIEVAL_ENABLED=true
  aeon-full-importance   same env as aeon-full; PLUS this harness seeds gold
                         memories at high importance (--gold-importance) so the
                         importance×decay synergy is measured separately and
                         transparently (§3 "Secondary"), reported as its own line.

For the pressure phase and comparators the harness also needs, in ITS OWN env:
  MANAGEMENT_API_KEY          — management auth for the force-sweep endpoint.
  AMP_TARGET_ACTIVE_COUNT     — the kernel's target (default 1000); the harness
                                also trusts the `target` field the sweep endpoint
                                returns, so this is only a fallback for logging.
  AMP_DEADBAND                — convergence deadband for the sweep rule (default 5).
  AEON_BASE_URL               — kernel base URL (default http://localhost:8080).
  DATABASE_URL / PG*          — Postgres connection (see connect_pg()).

DATABASE: host localhost:5432, db memoryos, user memoryos, password
memoryos_secret (overridable via DATABASE_URL / PG* env).  We prefer psycopg
(v3) then psycopg2; if neither imports we fall back to `docker exec <container>
psql`.  The SQL files in benchmarks/sql/ are executed verbatim with bound params.
"""

from __future__ import annotations

import argparse
import json
import os
import subprocess
import time
from collections import defaultdict
from concurrent.futures import ThreadPoolExecutor
from contextlib import contextmanager
from pathlib import Path
from typing import Any

from common import (
    AEON_BASE_URL,
    MANAGEMENT_API_KEY,
    default_results_dir,
    latency_summary,
    not_run,
    utc_now,
    write_csv,
    write_json,
)

# Reuse the vetted dataset loaders, sampling, metrics, and kernel-IO helpers from
# the semantic benchmark rather than re-implementing (and risking divergence).
from run_semantic_quality import (  # noqa: E402
    DatasetFormatError,
    DatasetUnavailable,
    RECALL_KS,
    SEARCH_LIMIT,
    SEMANTIC_THRESHOLD,
    aggregate,
    delete_agent,
    load_raw_dataset,
    normalize_locomo,
    normalize_longmemeval,
    query_metrics,
    request_with_retry,
    sanitize_agent_id,
    stratified_sample,
)

AREA = "longitudinal_quality"
GATE_ENV = "AEON_SEMANTIC_BENCHMARK"  # same cost gate as the semantic benchmark

SQL_DIR = Path(__file__).resolve().parents[1] / "sql"
AGE_SQL = SQL_DIR / "longitudinal_age.sql"
LRU_SQL = SQL_DIR / "lru_evict.sql"
RANDOM_SQL = SQL_DIR / "random_evict.sql"
DISTRACTOR_SQL = SQL_DIR / "seed_distractors.sql"

# Pre-registered defaults (§2/§4).  Overridable ONLY for cheaper smoke runs; the
# committed run uses these.
DEFAULT_ROUNDS = 5
DEFAULT_DELTA_DAYS = 6  # 5 rounds × ~6 days ≈ the 30-day aging window
DEFAULT_WINDOW_DAYS = 30
DEFAULT_PRESSURE_MULTIPLIER = 2.5  # PP corpus ≈ 2.5× AMP_TARGET_ACTIVE_COUNT (§4)
DEFAULT_SWEEP_CAP = 20  # hard cap on forced sweeps (§2 committed rule)

# Per-agent advisory-lock classifier serializing our out-of-band row mutations
# (aging, distractor seed, LRU/random eviction, clock advance) against the
# kernel's AMP pressure sweep — the background 5-min timer AND the management
# force-sweep endpoint both take
# pg_advisory_xact_lock(<classifier>, hashtext(agent_id)) for their read→write.
# MUST equal AMP_SWEEP_LOCK_CLASSIFIER in src/rmk_worker.rs (0x00414D50 =
# 4279632); if the kernel constant changes, change this too or the run
# deadlocks again (the failure this whole mechanism exists to prevent).
AMP_SWEEP_LOCK_CLASSIFIER = 0x00414D50  # 'A','M','P' = 4279632

# Amendment A6: parallelize P0 seeding across this many threads.  Seeding is the
# dominant wall-clock cost (synchronous embedding round-trips at ~3/s serial);
# concurrency is PERF-ONLY (order-independent creates; aging is applied post-seed
# by memory id, gold-blind, so seed order cannot affect stored state).  Set to 1
# to restore the original serial path (used by the A6 integrity check).
SEED_CONCURRENCY = int(os.environ.get("LONGITUDINAL_SEED_CONCURRENCY", "16"))

# Amendment A7 (pre-registered): the create API silently drops importance
# (CreateMemoryBody = {content, memory_type}) and hardcodes importance_score=1.0,
# so the importance-weighting term is inert until scores are set out-of-band.
# Exactly as the timeline is SQL-injected to test decay (the API has no timestamp
# field either), the importance-variant condition SQL-sets importance_score
# post-seed with these FIXED, pre-registered values: gold rows high, all others
# low.  Non-gold MUST be < gold: the retrieval penalty is (1 - importance_score),
# so leaving non-gold at the API default 1.0 would rank gold BELOW non-gold.
A7_GOLD_IMPORTANCE = 0.95
A7_NONGOLD_IMPORTANCE = 0.5
DEFAULT_DEADBAND = int(os.environ.get("AMP_DEADBAND", "5"))
DEFAULT_TARGET_ACTIVE = int(os.environ.get("AMP_TARGET_ACTIVE_COUNT", "1000"))

CONDITIONS = (
    "cosine-baseline",
    "decay-only",
    "amp-only",
    # amp-rmk (A11): amp-only kernel toggles + RMK ON at compressed cadence.  A pure
    # RMK-isolation arm — same env as amp-only except RMK_ENABLED=true — so any delta
    # vs amp-only is RMK's learning loop alone.  Runs the pressure phase (below); does
    # NOT trigger importance injection (that is keyed on aeon-full-importance only).
    "amp-rmk",
    "aeon-full",
    "aeon-full-importance",
)

# Simulated "now" for the seed clock (P0).  MUST anchor to the real wall-clock at
# run time, because the kernel computes decay staleness as (real NOW() -
# last_accessed_at) (src/memory/store.rs: days_stale = EXTRACT(EPOCH FROM (NOW() -
# ...))).  A FIXED PAST epoch (the old "2026-01-01") silently added the gap between
# that date and the real run date to every memory's age — e.g. run on 2026-07-03 it
# injected ~183 extra days of staleness, so exp(0.03·183) ≈ 242× inflated every
# distance past the retrieval threshold and collapsed recall to 0 (bench smoke,
# 2026-07-03).  Anchoring to now keeps the intended 0..window_days seed ages.
#
# Determinism is preserved where it matters: the RELATIVE aging pattern (which
# memory is older) is a pure function of (memory id, seed) in longitudinal_age.sql,
# and the per-round offsets are fixed.  Only the absolute anchor moves to "now",
# which is required for the kernel's real-clock decay to see the designed window.
# The exact epoch used is recorded in the results payload ("sim_epoch") for audit.
SIM_EPOCH = utc_now()

CSV_FIELDS = [
    "phase",
    "round",
    "sim_day",
    "dataset",
    "question_id",
    "category",
    "agent_id",
    "condition",
    "gold_count",
    "result_count",
    "first_gold_rank",
    "recall_at_1",
    "recall_at_3",
    "recall_at_5",
    "recall_at_10",
    "mrr_at_10",
    "ndcg_at_10",
    "precision_at_5",
    "evaluated",
    "search_status_code",
    "search_latency_ms",
    "note",
]


# ── Postgres access (psycopg → psycopg2 → docker exec psql) ────────────────────


class PgError(RuntimeError):
    pass


def _pg_dsn() -> str:
    return os.environ.get(
        "DATABASE_URL",
        "postgresql://memoryos:memoryos_secret@localhost:5432/memoryos",
    )


def _docker_pg_container() -> str:
    return os.environ.get("AEON_PG_CONTAINER", "").strip()


class PgExecutor:
    """Executes committed SQL files (and small inline statements) with bound params.

    Backends, in order of preference:
      1. psycopg (v3)   — named params via %(name)s
      2. psycopg2       — named params via %(name)s
      3. docker exec psql — fallback; params are safely quoted and substituted for
         the :name placeholders, since psql -v does not do value quoting for SQL.
    """

    def __init__(self) -> None:
        self.backend: str
        self._conn = None
        self._connect()

    def _connect(self) -> None:
        dsn = _pg_dsn()
        try:
            import psycopg  # type: ignore

            self._conn = psycopg.connect(dsn, autocommit=True)
            self.backend = "psycopg"
            return
        except ImportError:
            pass
        except Exception as exc:  # connection refused, auth, etc.
            raise PgError(f"psycopg connect failed: {exc}") from exc
        try:
            import psycopg2  # type: ignore

            self._conn = psycopg2.connect(dsn)
            self._conn.autocommit = True
            self.backend = "psycopg2"
            return
        except ImportError:
            pass
        except Exception as exc:
            raise PgError(f"psycopg2 connect failed: {exc}") from exc
        container = _docker_pg_container()
        if not container:
            raise PgError(
                "no psycopg/psycopg2 importable and AEON_PG_CONTAINER not set for the "
                "docker-exec fallback; `pip install psycopg[binary]` or export "
                "AEON_PG_CONTAINER=<postgres container name>"
            )
        self.backend = "docker"

    # -- parameter binding ------------------------------------------------------

    @staticmethod
    def _to_named(sql: str) -> str:
        """Convert :name placeholders to %(name)s for the DBAPI backends.

        The committed .sql files use :name (psql-style) so a reviewer can run them
        with psql -v directly; we translate for psycopg/psycopg2.  We avoid
        touching ::type casts by only converting ':' followed by an identifier
        that is NOT preceded by another ':'.
        """
        import re

        return re.sub(r"(?<!:):([a-zA-Z_][a-zA-Z0-9_]*)", r"%(\1)s", sql)

    @staticmethod
    def _psql_quote(value: Any) -> str:
        if value is None:
            return "NULL"
        if isinstance(value, bool):
            return "TRUE" if value else "FALSE"
        if isinstance(value, (int, float)):
            return repr(value)
        # string / timestamp — single-quote and escape embedded quotes
        return "'" + str(value).replace("'", "''") + "'"

    def _docker_substitute(self, sql: str, params: dict[str, Any]) -> str:
        import re

        def repl(m: "re.Match[str]") -> str:
            name = m.group(1)
            if name in params:
                return self._psql_quote(params[name])
            return m.group(0)  # leave ::type casts and unknown tokens intact

        return re.sub(r"(?<!:):([a-zA-Z_][a-zA-Z0-9_]*)", repl, sql)

    # -- execution --------------------------------------------------------------

    def execute(self, sql: str, params: dict[str, Any]) -> None:
        if self.backend in ("psycopg", "psycopg2"):
            with self._conn.cursor() as cur:  # type: ignore[union-attr]
                cur.execute(self._to_named(sql), params)
            return
        # docker exec psql
        rendered = self._docker_substitute(sql, params)
        self._run_psql(rendered)

    def scalar(self, sql: str, params: dict[str, Any]) -> Any:
        if self.backend in ("psycopg", "psycopg2"):
            with self._conn.cursor() as cur:  # type: ignore[union-attr]
                cur.execute(self._to_named(sql), params)
                row = cur.fetchone()
                return row[0] if row else None
        rendered = self._docker_substitute(sql, params)
        out = self._run_psql(rendered, tuples_only=True)
        out = out.strip()
        return out if out else None

    def rows(self, sql: str, params: dict[str, Any]) -> list[tuple]:
        """Return all result rows as tuples.

        For the docker backend, columns are emitted pipe-separated (psql -A -F'|')
        and split back into string tuples; callers must not embed '|' in values.
        """
        if self.backend in ("psycopg", "psycopg2"):
            with self._conn.cursor() as cur:  # type: ignore[union-attr]
                cur.execute(self._to_named(sql), params)
                return list(cur.fetchall())
        rendered = self._docker_substitute(sql, params)
        out = self._run_psql(rendered, tuples_only=True)
        result: list[tuple] = []
        for line in out.splitlines():
            line = line.rstrip("\n")
            if line.strip() == "":
                continue
            result.append(tuple(line.split("|")))
        return result

    def _run_psql(self, rendered_sql: str, tuples_only: bool = False) -> str:
        container = _docker_pg_container()
        db = os.environ.get("PGDATABASE", "memoryos")
        user = os.environ.get("PGUSER", "memoryos")
        args = ["docker", "exec", "-i", container, "psql", "-U", user, "-d", db, "-v",
                "ON_ERROR_STOP=1"]
        if tuples_only:
            args += ["-t", "-A"]
        args += ["-c", rendered_sql]
        proc = subprocess.run(args, capture_output=True, text=True)
        if proc.returncode != 0:
            raise PgError(
                f"docker exec psql failed (rc={proc.returncode}): "
                f"{proc.stderr.strip() or proc.stdout.strip()}"
            )
        return proc.stdout

    @staticmethod
    def _split_statements(sql: str) -> list[str]:
        """Split a multi-statement SQL string on top-level ';'.

        String/comment-aware so a ';' inside a single-quoted literal (e.g. the
        distractor content) or a '--' line comment does NOT split a statement.
        Handles the '' escape for embedded quotes.  Our .sql files use neither
        dollar-quoting nor /* */ blocks, so this is sufficient and correct.
        """
        stmts: list[str] = []
        buf: list[str] = []
        i, n = 0, len(sql)
        in_str = False
        while i < n:
            ch = sql[i]
            if in_str:
                buf.append(ch)
                if ch == "'":
                    if i + 1 < n and sql[i + 1] == "'":  # escaped ''
                        buf.append(sql[i + 1])
                        i += 2
                        continue
                    in_str = False
                i += 1
                continue
            if ch == "'":
                in_str = True
                buf.append(ch)
            elif ch == "-" and i + 1 < n and sql[i + 1] == "-":
                j = sql.find("\n", i)
                i = n if j == -1 else j
                continue
            elif ch == ";":
                stmt = "".join(buf).strip()
                if stmt:
                    stmts.append(stmt)
                buf = []
            else:
                buf.append(ch)
            i += 1
        tail = "".join(buf).strip()
        if tail:
            stmts.append(tail)
        return stmts

    def execute_file(self, path: Path, params: dict[str, Any]) -> None:
        sql = path.read_text(encoding="utf-8")
        if self.backend in ("psycopg", "psycopg2"):
            # psycopg cannot run multiple statements in one parameterized
            # execute(); split (string/comment-aware) and run each with the same
            # bound params (extra keys are ignored by the driver).
            for stmt in self._split_statements(sql):
                self.execute(stmt, params)
        else:
            # psql -c handles multi-statement files directly.
            self.execute(sql, params)

    # -- advisory-locked mutation (serialize vs the kernel AMP sweep) -----------

    @contextmanager
    def advisory_lock(self, agent_id: str):
        """Hold the per-agent AMP-sweep advisory xact lock across the block.

        The kernel's AMP pressure sweep (background 5-min timer AND the
        management force-sweep endpoint) takes
        ``pg_advisory_xact_lock(AMP_SWEEP_LOCK_CLASSIFIER, hashtext(agent_id))``
        for the duration of its read->decide->write.  Any out-of-band mutation of
        the SAME agent's ``memories`` rows must hold the same lock, or the two
        ``UPDATE memories`` statements deadlock on opposing row-lock orders
        (observed 2026-07-04).  The lock spans multiple statements, so it needs a
        real transaction; the connection is autocommit=True, so we open an
        explicit block.  Transaction-scoped: released on commit/rollback.
        """
        if self.backend == "psycopg":
            with self._conn.transaction():  # type: ignore[union-attr]
                with self._conn.cursor() as cur:  # type: ignore[union-attr]
                    cur.execute(
                        "SELECT pg_advisory_xact_lock(%s::int4, hashtext(%s))",
                        (AMP_SWEEP_LOCK_CLASSIFIER, agent_id),
                    )
                yield
        elif self.backend == "psycopg2":
            prev = self._conn.autocommit  # type: ignore[union-attr]
            self._conn.autocommit = False  # type: ignore[union-attr]
            try:
                with self._conn.cursor() as cur:  # type: ignore[union-attr]
                    cur.execute(
                        "SELECT pg_advisory_xact_lock(%s::int4, hashtext(%s))",
                        (AMP_SWEEP_LOCK_CLASSIFIER, agent_id),
                    )
                yield
                self._conn.commit()  # type: ignore[union-attr]
            except Exception:
                self._conn.rollback()  # type: ignore[union-attr]
                raise
            finally:
                self._conn.autocommit = prev  # type: ignore[union-attr]
        else:
            # docker-exec backend: each `psql -c` is a fresh session, so a
            # cross-statement session lock cannot be held here.  execute_locked /
            # execute_file_locked inline BEGIN;lock;...;COMMIT in a single -c for
            # that backend instead; a bare advisory_lock() block is best-effort.
            yield

    def execute_locked(self, sql: str, params: dict[str, Any]) -> None:
        """execute() one statement while holding the agent's AMP-sweep lock."""
        agent_id = params["agent_id"]
        if self.backend == "docker":
            wrapped = (
                f"BEGIN; SELECT pg_advisory_xact_lock({AMP_SWEEP_LOCK_CLASSIFIER}, "
                f"hashtext({self._psql_quote(agent_id)})); "
                f"{self._docker_substitute(sql, params)}; COMMIT;"
            )
            self._run_psql(wrapped)
            return
        with self.advisory_lock(agent_id):
            self.execute(sql, params)

    def execute_file_locked(self, path: Path, params: dict[str, Any]) -> None:
        """execute_file() while holding the agent's AMP-sweep lock."""
        agent_id = params["agent_id"]
        if self.backend == "docker":
            sql = path.read_text(encoding="utf-8")
            wrapped = (
                f"BEGIN; SELECT pg_advisory_xact_lock({AMP_SWEEP_LOCK_CLASSIFIER}, "
                f"hashtext({self._psql_quote(agent_id)})); "
                f"{self._docker_substitute(sql, params)}; COMMIT;"
            )
            self._run_psql(wrapped)
            return
        with self.advisory_lock(agent_id):
            self.execute_file(path, params)

    def close(self) -> None:
        if self._conn is not None:
            try:
                self._conn.close()
            except Exception:
                pass


# ── Kernel interaction beyond what run_semantic_quality provides ───────────────


def kernel_reachable() -> bool:
    status, _, _ = request_with_retry("GET", f"{AEON_BASE_URL}/api/v1/stats", timeout=15)
    return 200 <= status < 300


def seed_memory(
    agent_id: str, content: str, importance: float, counters: dict[str, int]
) -> str | None:
    """Seed a single memory; return its id or None on failure.

    Mirrors run_semantic_quality.seed_agent but per-memory so we can vary
    importance for the aeon-full-importance condition (gold seeded high).
    """
    status, payload, _ = request_with_retry(
        "POST",
        f"{AEON_BASE_URL}/api/v1/agents/{agent_id}/memories",
        {"content": content, "memory_type": "episodic", "importance": importance},
    )
    counters["requests"] += 1
    if 200 <= status < 300 and isinstance(payload, dict) and payload.get("id"):
        return str(payload["id"])
    counters["errors"] += 1
    return None


def seed_corpus(
    agent_id: str,
    turns: list[dict],
    gold_keys: set[str],
    gold_importance: float | None,
    counters: dict[str, int],
    max_workers: int | None = None,
) -> dict[str, str]:
    """Seed every turn; return {turn_key: memory_id}.

    When gold_importance is set (aeon-full-importance condition), gold turns are
    seeded at that importance and everything else at 0.5.  Otherwise ALL turns get
    the same 0.5 (the primary, conservative "uniform importance" setup, §3).

    Amendment A6: seeding is parallelized across ``max_workers`` threads (default
    SEED_CONCURRENCY / env LONGITUDINAL_SEED_CONCURRENCY).  PERF-ONLY: each turn
    is an independent create, the {turn_key: memory_id} mapping is
    order-independent, and P0 aging (applied AFTER seeding, keyed on memory id and
    gold-blind) makes seed order irrelevant to stored state.  Pass max_workers=1
    for the original serial path (the A6 integrity check compares the two).  Each
    worker uses a private counter; tallies are summed single-threaded after join,
    so request/error counts (and the error-rate BAIL) stay exact.
    """
    if max_workers is None:
        max_workers = SEED_CONCURRENCY
    delete_agent(agent_id)  # clean any residue for corpus isolation

    def _work(turn: dict) -> tuple[str, str | None, dict[str, int]]:
        if gold_importance is not None and turn["key"] in gold_keys:
            importance = gold_importance
        else:
            importance = 0.5
        local = {"requests": 0, "errors": 0}
        mem_id = seed_memory(agent_id, turn["content"], importance, local)
        return turn["key"], mem_id, local

    if max_workers and max_workers > 1:
        with ThreadPoolExecutor(max_workers=max_workers) as ex:
            outcomes = list(ex.map(_work, turns))
    else:
        outcomes = [_work(t) for t in turns]

    mapping: dict[str, str] = {}
    for key, mem_id, local in outcomes:
        counters["requests"] += local["requests"]
        counters["errors"] += local["errors"]
        if mem_id is not None:
            mapping[key] = mem_id
    return mapping


def search(agent_id: str, query: str, counters: dict[str, int]) -> tuple[int, list[dict], float]:
    status, payload, elapsed_ms = request_with_retry(
        "POST",
        f"{AEON_BASE_URL}/api/v1/memories/search",
        {
            "agent_id": agent_id,
            "query": query,
            "limit": SEARCH_LIMIT,
            "threshold": SEMANTIC_THRESHOLD,
        },
    )
    counters["requests"] += 1
    if not (200 <= status < 300):
        counters["errors"] += 1
        return status, [], elapsed_ms
    results = payload.get("results", []) if isinstance(payload, dict) else []
    return status, results, elapsed_ms


def post_feedback(agent_id: str, memory_id: str, value: float, counters: dict[str, int]) -> None:
    """POST /api/v1/feedback with the FIXED rule value (§2): 1.0 gold, 0.0 distractor."""
    status, _, _ = request_with_retry(
        "POST",
        f"{AEON_BASE_URL}/api/v1/feedback",
        {"agent_id": agent_id, "memory_id": memory_id, "feedback": value},
    )
    counters["requests"] += 1
    if not (200 <= status < 300):
        counters["errors"] += 1


def force_sweep(agent_id: str, counters: dict[str, int]) -> dict[str, Any] | None:
    """Drive POST /api/v1/agents/:id/amp/sweep (management-gated).

    Contract (from the design's force-sweep endpoint contract):
      returns {swept, active_count, target, aggressiveness, soft_evicted_total,
               newly_evicted}.
    """
    status, payload, _ = request_with_retry(
        "POST",
        f"{AEON_BASE_URL}/api/v1/agents/{agent_id}/amp/sweep",
        {},
        # request_with_retry → request_json already injects X-Management-Key from
        # common.headers(); pass nothing extra.
    )
    counters["requests"] += 1
    if not (200 <= status < 300) or not isinstance(payload, dict):
        counters["errors"] += 1
        return None
    return payload


# ── Metric row helpers ─────────────────────────────────────────────────────────


def score_unit(
    phase: str,
    round_idx: int,
    sim_day: float,
    dataset: str,
    unit: dict,
    mapping: dict[str, str],
    agent_id: str,
    condition: str,
    counters: dict[str, int],
    latencies: list[float],
) -> tuple[dict, list[str]]:
    """Search + score one QA unit against a (possibly aged/evicted) corpus.

    Returns (row, result_ids).  result_ids is returned so callers can drive
    feedback / reinforcement without re-querying.
    """
    gold_ids = {mapping[k] for k in unit["gold_keys"] if k in mapping}
    row: dict[str, Any] = {
        "phase": phase,
        "round": round_idx,
        "sim_day": round(sim_day, 4),
        "dataset": dataset,
        "question_id": unit["qid"],
        "category": unit["category"],
        "agent_id": agent_id,
        "condition": condition,
        "gold_count": len(gold_ids),
        "evaluated": False,
        "note": "",
    }
    if not gold_ids:
        row["note"] = "no gold ids resolved (gold turns failed to seed)"
        return row, []

    status, results, elapsed_ms = search(unit["agent_id"], unit["question"], counters)
    row["search_status_code"] = status
    if not (200 <= status < 300):
        row["note"] = "search failed"
        return row, []

    latencies.append(elapsed_ms)
    result_ids = [str(r.get("id")) for r in results[:SEARCH_LIMIT]]
    row.update(query_metrics(result_ids, gold_ids))
    row["result_count"] = len(results)
    row["search_latency_ms"] = round(elapsed_ms, 3)
    row["evaluated"] = True
    return row, result_ids


def apply_feedback_rule(
    agent_id: str,
    result_ids: list[str],
    gold_ids: set[str],
    counters: dict[str, int],
) -> None:
    """The pre-registered feedback rule (§2), applied every round:

    1.0 on the gold-evidence memory the query points to; 0.0 on retrieved
    distractors.  We only send feedback for memories that were actually retrieved
    (distractors) plus all gold ids (positive reinforcement), matching the design.
    """
    retrieved = set(result_ids)
    for gid in gold_ids:
        post_feedback(agent_id, gid, 1.0, counters)
    for rid in retrieved:
        if rid not in gold_ids:
            post_feedback(agent_id, rid, 0.0, counters)


# ── Phases ─────────────────────────────────────────────────────────────────────


def inject_importance(
    pg: PgExecutor,
    agent_id: str,
    gold_ids: set[str],
    gold_importance: float,
    base_importance: float = A7_NONGOLD_IMPORTANCE,
) -> None:
    """A7: SQL-set importance_score post-seed (the create API ignores importance
    and hardcodes 1.0 — see LONGITUDINAL_DESIGN.md amendment A7 + API-gap finding).

    Two locked UPDATEs, order matters: (1) ALL live rows → base_importance, then
    (2) gold rows → gold_importance.  Base must run first and be < gold, else gold
    left at the API default 1.0 would out-rank... be out-ranked (penalty is
    1 - importance_score).  Advisory-locked (execute_locked) to serialize vs the
    AMP sweep, same as aging.
    """
    pg.execute_locked(
        "UPDATE memories SET importance_score = :imp "
        "WHERE agent_id = :agent_id AND archived_at IS NULL",
        {"agent_id": agent_id, "imp": float(base_importance)},
    )
    if gold_ids:
        pg.execute_locked(
            "UPDATE memories SET importance_score = :imp "
            "WHERE agent_id = :agent_id AND id = ANY(:ids::uuid[])",
            {"agent_id": agent_id, "imp": float(gold_importance),
             "ids": list(gold_ids)},
        )


def phase_seed_with_history(
    pg: PgExecutor,
    agent_id: str,
    turns: list[dict],
    gold_keys: set[str],
    gold_importance: float | None,
    seed: int,
    window_days: int,
    counters: dict[str, int],
) -> dict[str, str]:
    """P0: seed all turns via API (real embeddings), SQL-age gold-blind, and (for
    the importance variant) SQL-inject pre-registered importance_score (A7)."""
    mapping = seed_corpus(agent_id, turns, gold_keys, gold_importance, counters)
    pg.execute_file_locked(
        AGE_SQL,
        {
            "agent_id": agent_id,
            "seed": seed,
            "window_days": window_days,
            "as_of": SIM_EPOCH,
        },
    )
    # A7: importance is only non-uniform in the importance variant (gold_importance
    # set).  The primary conditions keep uniform importance (the create default),
    # which is inert by design — a uniform score is a constant factor on distance
    # and cannot change ranking.
    if gold_importance is not None:
        gold_ids = {mapping[k] for k in gold_keys if k in mapping}
        inject_importance(pg, agent_id, gold_ids, gold_importance)
    return mapping


def advance_clock(pg: PgExecutor, agent_id: str, delta_days: float) -> None:
    """Advance the simulated clock by shifting the corpus back in time by
    delta_days.  Moving created_at/last_accessed_at earlier is equivalent to
    moving "now" forward — decay is a function of (now - last_accessed_at) — and
    keeps SIM_EPOCH fixed.  Gold-blind: applies uniformly to all live memories.
    """
    pg.execute_locked(
        """
        UPDATE memories
        SET created_at       = created_at - make_interval(secs => :secs),
            last_accessed_at = last_accessed_at - make_interval(secs => :secs)
        WHERE agent_id = :agent_id
          AND archived_at IS NULL
        """,
        {"agent_id": agent_id, "secs": float(delta_days) * 86400.0},
    )


def run_rounds(
    pg: PgExecutor,
    dataset: str,
    condition: str,
    agent_id: str,
    mapping: dict[str, str],
    units: list[dict],
    rounds: int,
    delta_days: float,
    counters: dict[str, int],
    latencies: list[float],
) -> list[dict]:
    """P1..PR: per-round QA scoring + reinforcement + feedback + clock advance."""
    rows: list[dict] = []
    sim_day = 0.0
    # A10 (repeated-measurement design): re-score EVERY unit each round, so the
    # SAME question is re-asked on a corpus that ages underneath it — round r
    # measures recall at seed-age + (r-1)·delta_days, i.e. a genuine recall-vs-age
    # curve.  The prior design partitioned an agent's units across rounds; with
    # ~1 question per LongMemEval record that left rounds 2..R empty and produced
    # NO aging curve at any N.  NB: these are NOT independent samples across rounds
    # — the same unit recurs each round (a repeated measure on an aging corpus).
    for r in range(1, rounds + 1):
        for unit in units:
            gold_ids = {mapping[k] for k in unit["gold_keys"] if k in mapping}
            row, result_ids = score_unit(
                "round", r, sim_day, dataset, unit, mapping, agent_id, condition,
                counters, latencies,
            )
            rows.append(row)
            # (b) reinforcement: re-issue the same query so correlated gold sets
            # are co-retrieved, growing AMP co-access edges; (c) feedback rule.
            if row.get("evaluated"):
                apply_feedback_rule(agent_id, result_ids, gold_ids, counters)
                # a second co-retrieval pass strengthens the co-access edge for
                # gold pairs that appear together (design §4 P1..PR step b).
                # search() returns raw result dicts; extract ids like score_unit.
                _, reinforce_results, _ = search(
                    unit["agent_id"], unit["question"], counters
                )
                reinforce_ids = [
                    str(rr.get("id")) for rr in reinforce_results[:SEARCH_LIMIT]
                ]
                apply_feedback_rule(agent_id, reinforce_ids, gold_ids, counters)
        # (d) advance the simulated clock via SQL.
        advance_clock(pg, agent_id, delta_days)
        sim_day += delta_days
    return rows


def converge_sweeps(
    agent_id: str,
    deadband: int,
    sweep_cap: int,
    counters: dict[str, int],
) -> dict[str, Any]:
    """PP: drive forced sweeps per the committed convergence rule (§2).

    Repeat until active_count is within deadband of target for 2 CONSECUTIVE
    sweeps, OR sweep_cap sweeps, whichever first.  Return the settled state incl.
    K (soft_evicted_total) and the per-sweep trace.
    """
    trace: list[dict[str, Any]] = []
    consecutive_in_band = 0
    last: dict[str, Any] | None = None
    for i in range(1, sweep_cap + 1):
        result = force_sweep(agent_id, counters)
        if result is None:
            trace.append({"sweep": i, "error": "sweep endpoint failed"})
            break
        last = result
        active = int(result.get("active_count", 0))
        target = int(result.get("target", DEFAULT_TARGET_ACTIVE))
        trace.append(
            {
                "sweep": i,
                "active_count": active,
                "target": target,
                "aggressiveness": result.get("aggressiveness"),
                "soft_evicted_total": result.get("soft_evicted_total"),
                "newly_evicted": result.get("newly_evicted"),
            }
        )
        if abs(active - target) <= deadband:
            consecutive_in_band += 1
            if consecutive_in_band >= 2:
                break
        else:
            consecutive_in_band = 0
    k = int(last.get("soft_evicted_total", 0)) if last else 0
    return {
        "settled_k": k,
        "sweeps_run": len(trace),
        "converged": consecutive_in_band >= 2,
        "final": last,
        "trace": trace,
    }


def copy_corpus_agent(
    pg: PgExecutor, src_agent: str, dst_agent: str
) -> dict[str, str]:
    """Create a fresh copy of src's live memories under dst_agent, preserving
    content, embedding, importance, and timeline, returning
    {src_memory_id: dst_memory_id}.

    Used so LRU / random comparators evict over a corpus IDENTICAL to the one AMP
    evicted, without re-embedding (the stored embedding is copied).  Fully
    gold-blind: it copies every live row unconditionally.

    The `memories` table has no free provenance column to stash the source id, so
    we copy row-by-row with INSERT ... RETURNING id and pair each new id with its
    source id to build the remap deterministically (in the source's id order).
    """
    # Ensure the destination agent exists (FK on memories.agent_id → agents) and
    # is clean of any prior copy.
    delete_agent(dst_agent)
    pg.execute(
        "INSERT INTO agents (agent_id) VALUES (:aid) ON CONFLICT (agent_id) DO NOTHING",
        {"aid": dst_agent},
    )
    src_ids = [
        str(r[0])
        for r in pg.rows(
            "SELECT id FROM memories WHERE agent_id = :src AND archived_at IS NULL "
            "ORDER BY id ASC",
            {"src": src_agent},
        )
    ]
    remap: dict[str, str] = {}
    for src_id in src_ids:
        # Insert a copy sourced from this specific row; column list matches the
        # verified schema (memory_type, confidence, embedding, importance_score,
        # created_at, last_accessed_at, utility_ema).
        new_id = pg.scalar(
            """
            INSERT INTO memories (agent_id, session_id, content, memory_type,
                                  confidence, embedding, importance_score,
                                  created_at, last_accessed_at, utility_ema)
            SELECT :dst, session_id, content, memory_type, confidence, embedding,
                   importance_score, created_at, last_accessed_at, utility_ema
            FROM memories
            WHERE id = :src_id
            RETURNING id
            """,
            {"dst": dst_agent, "src_id": src_id},
        )
        if new_id is not None:
            remap[src_id] = str(new_id)
    return remap


def remap_mapping(mapping: dict[str, str], remap: dict[str, str]) -> dict[str, str]:
    """Translate a {turn_key: src_memory_id} mapping onto the copied corpus."""
    return {k: remap[v] for k, v in mapping.items() if v in remap}


def score_after_eviction(
    policy: str,
    dataset: str,
    condition: str,
    agent_id: str,
    mapping: dict[str, str],
    units: list[dict],
    counters: dict[str, int],
    latencies: list[float],
) -> tuple[list[dict], dict[str, Any]]:
    """Re-query a post-eviction corpus and compute recall@k + gold retention."""
    rows: list[dict] = []
    gold_total = 0
    gold_surviving = 0
    for unit in units:
        gold_ids = {mapping[k] for k in unit["gold_keys"] if k in mapping}
        # per-unit mapping already points at the surviving (non-archived) copy;
        # we approximate gold retention at the corpus level below.
        u = dict(unit)
        u["agent_id"] = agent_id
        row, _ = score_unit(
            f"evict:{policy}", 0, 0.0, dataset, u, mapping, agent_id, condition,
            counters, latencies,
        )
        rows.append(row)
        gold_total += len(gold_ids)
    return rows, {"gold_total": gold_total}


def embedding_dim(pg: PgExecutor, agent_id: str, default: int = 1536) -> int:
    """Detect the embedding dimension from a live real memory of this agent.

    The distractor far-vector must match the column's declared width (vector(1536)
    for OpenAI small, vector(384) for bge-small).  Rather than hard-code it we read
    it off a real embedding the API already produced during P0, so the harness
    auto-adapts to whatever EMBEDDING_DIMENSION the kernel was booted with.
    """
    val = pg.scalar(
        "SELECT vector_dims(embedding) FROM memories "
        "WHERE agent_id = :aid AND embedding IS NOT NULL LIMIT 1",
        {"aid": agent_id},
    )
    try:
        d = int(val)
        return d if d > 0 else default
    except (TypeError, ValueError):
        return default


def gold_retention(
    pg: PgExecutor, agent_id: str, gold_ids: set[str]
) -> dict[str, Any]:
    """Fraction of the given gold memory ids still RETRIEVABLE after eviction.

    A gold memory counts as retained only if it satisfies the kernel's own
    retrievability filter (src/memory/store.rs search_memories_impl):
    ``archived_at IS NULL AND soft_evicted = FALSE``.  This is critical for a fair
    AMP-vs-LRU/random comparison: AMP evicts by setting ``soft_evicted = TRUE``
    (never touching archived_at), while the LRU/random comparators set
    ``archived_at``.  Checking archived_at alone would make AMP's evictions
    invisible and pin its retention at ~1.0 by construction — a metric artifact,
    not a result (found 2026-07-04 when AMP scored a perfect 1.0 on every agent
    despite having soft-evicted real memories).
    """
    if not gold_ids:
        return {"gold_total": 0, "gold_kept": 0, "retention": None}
    id_list = ",".join(f"'{g}'" for g in gold_ids)
    kept = pg.scalar(
        f"SELECT COUNT(*)::int FROM memories "
        f"WHERE agent_id = :aid AND archived_at IS NULL AND soft_evicted = FALSE "
        f"AND id IN ({id_list})",
        {"aid": agent_id},
    )
    kept_n = int(kept) if kept is not None else 0
    return {
        "gold_total": len(gold_ids),
        "gold_kept": kept_n,
        "retention": round(kept_n / len(gold_ids), 4),
    }


# ── Main ───────────────────────────────────────────────────────────────────────


def parse_args() -> argparse.Namespace:
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--dataset", default="longmemeval", choices=["longmemeval", "locomo"])
    p.add_argument("--dataset-file", type=Path, default=None)
    p.add_argument("--condition", default="aeon-full", choices=list(CONDITIONS))
    p.add_argument("--sample-size", type=int, default=100)
    p.add_argument("--seed", type=int, default=42)
    p.add_argument("--rounds", type=int, default=DEFAULT_ROUNDS)
    p.add_argument("--delta-days", type=float, default=DEFAULT_DELTA_DAYS)
    p.add_argument("--window-days", type=int, default=DEFAULT_WINDOW_DAYS)
    p.add_argument("--pressure-multiplier", type=float, default=DEFAULT_PRESSURE_MULTIPLIER)
    p.add_argument("--sweep-cap", type=int, default=DEFAULT_SWEEP_CAP)
    p.add_argument("--deadband", type=int, default=DEFAULT_DEADBAND)
    p.add_argument("--target-active", type=int, default=DEFAULT_TARGET_ACTIVE)
    p.add_argument(
        "--gold-importance",
        type=float,
        default=None,
        help="Only for aeon-full-importance: importance to seed gold memories at "
        "(non-gold stay 0.5).  Ignored for other conditions.",
    )
    p.add_argument("--results-dir", type=Path, default=None)
    p.add_argument("--keep-agents", action="store_true")
    p.add_argument(
        "--skip-pressure",
        action="store_true",
        help="Skip the PP pressure/eviction phase (e.g. when AMP is disabled).",
    )
    return p.parse_args()


def emit_not_run(results_dir: Path, reason: str) -> int:
    not_run(AREA, reason, results_dir, "longitudinal_quality.json")
    print(f"[longitudinal] not_run: {reason}")
    return 0


def main() -> int:
    args = parse_args()
    results_dir = args.results_dir or default_results_dir()
    results_dir.mkdir(parents=True, exist_ok=True)

    if os.environ.get(GATE_ENV, "").strip().lower() != "true":
        return emit_not_run(
            results_dir,
            f"{GATE_ENV} != true; longitudinal benchmark is cost-gated (real "
            "embeddings) and never a required CI gate",
        )

    # For aeon-full-importance, default gold importance high if not given (§3).
    gold_importance = args.gold_importance
    if args.condition == "aeon-full-importance" and gold_importance is None:
        gold_importance = A7_GOLD_IMPORTANCE

    try:
        raw = load_raw_dataset(args.dataset, args.dataset_file)
    except DatasetUnavailable as exc:
        return emit_not_run(results_dir, f"dataset unavailable: {exc}")
    except DatasetFormatError as exc:
        return emit_not_run(results_dir, f"dataset format error: {exc}")

    try:
        if args.dataset == "longmemeval":
            corpora, units, load_stats = normalize_longmemeval(raw)
        else:
            corpora, units, load_stats = normalize_locomo(raw)
    except DatasetFormatError as exc:
        return emit_not_run(results_dir, f"dataset format error: {exc}")

    if not kernel_reachable():
        return emit_not_run(
            results_dir,
            f"kernel unreachable at {AEON_BASE_URL} (GET /api/v1/stats); check "
            "AEON_BASE_URL / MANAGEMENT_API_KEY",
        )

    try:
        pg = PgExecutor()
    except PgError as exc:
        return emit_not_run(results_dir, f"postgres unavailable: {exc}")

    sampled = stratified_sample(units, args.sample_size, args.seed)
    agent_units: dict[str, list[dict]] = defaultdict(list)
    agent_order: list[str] = []
    for unit in sampled:
        if unit["agent_id"] not in agent_units:
            agent_order.append(unit["agent_id"])
        agent_units[unit["agent_id"]].append(unit)

    print(
        f"[longitudinal] condition={args.condition} dataset={args.dataset} "
        f"units={len(sampled)}/{len(units)} agents={len(agent_order)} "
        f"rounds={args.rounds} Δdays={args.delta_days} pg_backend={pg.backend} "
        f"base={AEON_BASE_URL}"
    )

    counters = {"requests": 0, "errors": 0}
    latencies: list[float] = []
    rows: list[dict] = []
    eviction_blocks: list[dict[str, Any]] = []
    created_agents: list[str] = []
    seeded_memory_count = 0

    try:
        for agent_id in agent_order:
            turns = corpora[agent_id]
            # union of gold keys across this agent's sampled units
            gold_keys: set[str] = set()
            for u in agent_units[agent_id]:
                gold_keys |= u["gold_keys"]

            # ── P0: seed-with-history ──────────────────────────────────────
            mapping = phase_seed_with_history(
                pg, agent_id, turns, gold_keys, gold_importance, args.seed,
                args.window_days, counters,
            )
            created_agents.append(agent_id)
            seeded_memory_count += len(mapping)

            # ── P1..PR: rounds ────────────────────────────────────────────
            rows.extend(
                run_rounds(
                    pg, args.dataset, args.condition, agent_id, mapping,
                    agent_units[agent_id], args.rounds, args.delta_days,
                    counters, latencies,
                )
            )

            # ── PP: pressure + eviction comparison (AMP vs LRU vs random) ──
            run_pressure = (not args.skip_pressure) and args.condition in (
                "amp-only",
                "amp-rmk",
                "aeon-full",
                "aeon-full-importance",
            )
            if run_pressure:
                block = run_pressure_phase(
                    pg, args, agent_id, mapping, gold_keys,
                    agent_units[agent_id], counters, latencies, rows,
                )
                eviction_blocks.append(block)
                for extra in block.pop("_agents_created", []):
                    if not args.keep_agents:
                        delete_agent(extra)

            if not args.keep_agents:
                delete_agent(agent_id)
                created_agents.remove(agent_id)

            error_rate = counters["errors"] / max(counters["requests"], 1)
            if counters["requests"] >= 20 and error_rate > 0.20:
                print(f"[longitudinal] BAIL: error rate {error_rate:.2%}")
                break
    finally:
        if not args.keep_agents:
            for agent_id in list(created_agents):
                delete_agent(agent_id)
        pg.close()

    payload = build_payload(
        args, rows, eviction_blocks, counters, latencies, load_stats,
        len(units), seeded_memory_count, gold_importance,
    )
    write_json(results_dir / "longitudinal_quality.json", payload)
    write_csv(results_dir / "longitudinal_quality.csv", rows, CSV_FIELDS)
    print(
        f"[longitudinal] status={payload['status']} "
        f"rounds_summary={json.dumps(payload['round_trend'])}"
    )
    return 0


def run_pressure_phase(
    pg: PgExecutor,
    args: argparse.Namespace,
    agent_id: str,
    mapping: dict[str, str],
    gold_keys: set[str],
    units: list[dict],
    counters: dict[str, int],
    latencies: list[float],
    rows: list[dict],
) -> dict[str, Any]:
    """PP: seed FAR-embedding distractors (SQL, non-retrievable — §10d isolation
    fix), converge AMP sweeps to settle K, then run LRU/random comparators over
    fresh copies evicting the SAME K; score all three.

    Primary pressure metric is gold_retention (fraction of gold surviving
    eviction, pure archived_at set-membership); post-eviction recall is secondary
    and, by construction, measured over real memories only (distractors can never
    enter the top-k).
    """
    agents_created: list[str] = []
    gold_ids = {mapping[k] for k in gold_keys if k in mapping}

    # Seed a fixed distractor corpus so active_count ≫ target (§4 PP).
    #
    # Q2 isolation fix (§10d): distractors are inserted DIRECTLY via SQL with a
    # fixed FAR (one-hot) embedding — NOT through the API with real embeddings.
    # They count toward active_count (so AMP still has rows to soft-evict) but are
    # never retrieval candidates (cosine sim ≈ 0 ≪ 0.95 threshold), so they cannot
    # contaminate recall@k / MRR / nDCG.  gold_retention is the primary pressure
    # metric; post-eviction recall is measured over real memories only.
    n_distractors = int(args.target_active * args.pressure_multiplier)
    dim = embedding_dim(pg, agent_id)
    pg.execute_file_locked(
        DISTRACTOR_SQL,
        {"agent_id": agent_id, "n_distractors": n_distractors, "dim": dim,
         "as_of": SIM_EPOCH},
    )
    # Re-age the whole live corpus (real + distractor) gold-blind so pressure
    # reflects a realistic mixed-age corpus (same window, same seed family).  The
    # aging hash is a pure function of (id, seed), so real memories land on their
    # same P0 ages and distractors draw from the identical distribution.
    pg.execute_file_locked(
        AGE_SQL,
        {"agent_id": agent_id, "seed": args.seed, "window_days": args.window_days,
         "as_of": SIM_EPOCH},
    )

    # Drive forced sweeps per the committed convergence rule → settle K.
    conv = converge_sweeps(agent_id, args.deadband, args.sweep_cap, counters)
    k = conv["settled_k"]

    # AMP arm: score the kernel's own surviving corpus.
    amp_rows, _ = score_after_eviction(
        "amp", args.dataset, args.condition, agent_id, mapping, units,
        counters, latencies,
    )
    rows.extend(amp_rows)
    amp_retention = gold_retention(pg, agent_id, gold_ids)

    # LRU + random arms: fresh copies of the same aged corpus, evict the SAME K.
    lru_agent = sanitize_agent_id(f"{agent_id}-lru")
    rnd_agent = sanitize_agent_id(f"{agent_id}-rnd")
    agents_created += [lru_agent, rnd_agent]

    lru_remap = copy_corpus_agent(pg, agent_id, lru_agent)
    rnd_remap = copy_corpus_agent(pg, agent_id, rnd_agent)
    lru_mapping = remap_mapping(mapping, lru_remap)
    rnd_mapping = remap_mapping(mapping, rnd_remap)
    lru_gold = {lru_remap[g] for g in gold_ids if g in lru_remap}
    rnd_gold = {rnd_remap[g] for g in gold_ids if g in rnd_remap}

    if k > 0:
        pg.execute_file_locked(LRU_SQL, {"agent_id": lru_agent, "k": k})
        pg.execute_file_locked(RANDOM_SQL, {"agent_id": rnd_agent, "k": k, "seed": args.seed})

    lru_rows, _ = score_after_eviction(
        "lru", args.dataset, args.condition, lru_agent, lru_mapping, units,
        counters, latencies,
    )
    rnd_rows, _ = score_after_eviction(
        "random", args.dataset, args.condition, rnd_agent, rnd_mapping, units,
        counters, latencies,
    )
    rows.extend(lru_rows)
    rows.extend(rnd_rows)

    block = {
        "agent_id": agent_id,
        "settled_k": k,
        "distractors_seeded": n_distractors,
        "convergence": conv,
        "amp": {
            "post_eviction": aggregate(amp_rows),
            "gold_retention": amp_retention,
        },
        "lru": {
            "post_eviction": aggregate(lru_rows),
            "gold_retention": gold_retention(pg, lru_agent, lru_gold),
        },
        "random": {
            "post_eviction": aggregate(rnd_rows),
            "gold_retention": gold_retention(pg, rnd_agent, rnd_gold),
        },
        "_agents_created": agents_created,
    }
    return block


def build_payload(
    args: argparse.Namespace,
    rows: list[dict],
    eviction_blocks: list[dict],
    counters: dict[str, int],
    latencies: list[float],
    load_stats: dict,
    units_available: int,
    seeded_memory_count: int,
    gold_importance: float | None,
) -> dict[str, Any]:
    round_rows = [r for r in rows if r.get("phase") == "round"]
    # Per-round trend (to show divergence over simulated time, §6).
    round_trend: dict[str, Any] = {}
    for r in sorted({row["round"] for row in round_rows if row.get("round")}):
        round_trend[f"round_{r}"] = aggregate([x for x in round_rows if x["round"] == r])

    # Aggregate eviction retention block (AMP vs LRU vs random), averaged across
    # agents that ran the pressure phase.
    eviction_retention = summarize_eviction(eviction_blocks)

    status = "ok"
    if counters["requests"] >= 20 and counters["errors"] / max(counters["requests"], 1) > 0.20:
        status = "fail"

    return {
        "area": AREA,
        "status": status,
        "authority": "benchmarks/LONGITUDINAL_DESIGN.md",
        "condition": args.condition,
        "dataset": args.dataset,
        "dataset_file": str(args.dataset_file) if args.dataset_file else None,
        "generated_at": utc_now(),
        "seed": args.seed,
        "rounds": args.rounds,
        "delta_days": args.delta_days,
        "window_days": args.window_days,
        "sim_epoch": SIM_EPOCH,
        "pressure_multiplier": args.pressure_multiplier,
        "sweep_cap": args.sweep_cap,
        "deadband": args.deadband,
        "target_active": args.target_active,
        "gold_importance": gold_importance,
        "sample_size_requested": args.sample_size,
        "units_available": units_available,
        "seeded_memory_count": seeded_memory_count,
        "threshold": SEMANTIC_THRESHOLD,
        "aeon_base_url": AEON_BASE_URL,
        "load_stats": load_stats,
        "errors": {
            "request_count": counters["requests"],
            "error_count": counters["errors"],
            "error_rate": round(counters["errors"] / max(counters["requests"], 1), 4),
        },
        "summary": aggregate(round_rows),
        "round_trend": round_trend,
        "eviction_retention": eviction_retention,
        "eviction_detail": eviction_blocks,
        "search_latency": latency_summary(latencies),
        "rows": rows,
        "method": (
            "P0 seed via POST /api/v1/agents/:id/memories (server-side real "
            "embeddings) then gold-blind seeded SQL aging (benchmarks/sql/"
            "longitudinal_age.sql); P1..PR per-round QA scoring + reinforcement "
            "co-retrieval + POST /api/v1/feedback (1.0 gold / 0.0 distractor) + "
            "SQL clock advance; PP force-sweep convergence to settle K, then LRU "
            "and random comparators evicting the SAME K over fresh corpus copies."
        ),
    }


def summarize_eviction(blocks: list[dict]) -> dict[str, Any]:
    if not blocks:
        return {"agents": 0}

    def collect(policy: str, metric_key: str) -> list[float]:
        vals: list[float] = []
        for b in blocks:
            v = b.get(policy, {}).get("post_eviction", {}).get(metric_key)
            if isinstance(v, (int, float)):
                vals.append(float(v))
        return vals

    def retention(policy: str) -> list[float]:
        vals: list[float] = []
        for b in blocks:
            r = b.get(policy, {}).get("gold_retention", {}).get("retention")
            if isinstance(r, (int, float)):
                vals.append(float(r))
        return vals

    def mean(xs: list[float]) -> float | None:
        return round(sum(xs) / len(xs), 4) if xs else None

    out: dict[str, Any] = {
        "agents": len(blocks),
        "settled_k_mean": mean([float(b["settled_k"]) for b in blocks if "settled_k" in b]),
    }
    for policy in ("amp", "lru", "random"):
        out[policy] = {
            f"recall_at_{k}": mean(collect(policy, f"recall_at_{k}")) for k in RECALL_KS
        }
        out[policy]["ndcg_at_10"] = mean(collect(policy, "ndcg_at_10"))
        out[policy]["gold_retention"] = mean(retention(policy))
    return out


if __name__ == "__main__":
    raise SystemExit(main())
