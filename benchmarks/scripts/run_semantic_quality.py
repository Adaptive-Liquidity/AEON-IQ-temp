#!/usr/bin/env python3
"""Semantic recall benchmark against public long-memory datasets.

Unlike run_recall_quality.py (deterministic hash embeddings, mechanical
retrieval checks), this benchmark measures SEMANTIC recall quality using
whatever real embedding model the running kernel is configured with.  All
embeddings are computed server-side: memories are seeded through
`POST /api/v1/agents/:id/memories` and queried through
`POST /api/v1/memories/search`, so this client never computes or touches a
vector, and never talks to the database directly.

Supported datasets:
  - LongMemEval (`longmemeval_s` / `longmemeval_oracle` JSON): one isolated
    agent per question (`sem-lme-{question_id}`); every haystack turn becomes
    one memory; gold memories are the evidence turns.
  - LoCoMo (`locomo10.json`-style): one isolated agent per conversation
    (`sem-loco-{conv_idx}`); every dialog turn becomes one memory; each QA
    pair is a query whose gold memories are its `evidence` dialog ids.

The benchmark is optional and cost-gated: it only runs when
`AEON_SEMANTIC_BENCHMARK=true`.  Otherwise it writes a `not_run` artifact and
exits 0.  It must NEVER become a required CI gate.

The `--condition` flag is a free-text label recorded in the results (e.g.
"aeon-full" or "cosine-baseline").  The actual experimental condition is
determined by the KERNEL's environment (MEMORY_DECAY_RATE,
IMPORTANCE_BOOST_FACTOR, AMP_ENABLED, RMK_ENABLED, GRAPH_RETRIEVAL_ENABLED) —
see docs/BENCHMARKS.md.
"""

from __future__ import annotations

import argparse
import json
import math
import os
import random
import re
import time
import urllib.error
import urllib.request
from collections import defaultdict
from pathlib import Path
from typing import Any

from common import (
    AEON_BASE_URL,
    default_results_dir,
    latency_summary,
    not_run,
    request_json,
    utc_now,
    write_csv,
    write_json,
)

AREA = "semantic_quality"
GATE_ENV = "AEON_SEMANTIC_BENCHMARK"
SEMANTIC_THRESHOLD = float(os.environ.get("AEON_SEMANTIC_THRESHOLD", "0.95"))
SEARCH_LIMIT = 10
RECALL_KS = (1, 3, 5, 10)
MAX_ERROR_RATE = 0.20
MIN_REQUESTS_BEFORE_BAIL = 20
AGENT_SAFE_RE = re.compile(r"[^a-zA-Z0-9_-]+")

LONGMEMEVAL_HF_REPO = "xiaowu0162/longmemeval"
LONGMEMEVAL_HF_FILE = "longmemeval_oracle.json"
LOCOMO_RAW_URL = (
    "https://raw.githubusercontent.com/snap-research/locomo/main/data/locomo10.json"
)

CSV_FIELDS = [
    "dataset",
    "question_id",
    "category",
    "agent_id",
    "condition",
    "gold_count",
    "seeded_count",
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


class DatasetUnavailable(Exception):
    """Dataset file could not be obtained (missing file, restricted network)."""


class DatasetFormatError(Exception):
    """Dataset file was obtained but its structure was not recognised."""


# ── Generic helpers ───────────────────────────────────────────────────────────


def first_present(d: dict[str, Any], names: tuple[str, ...]) -> Any:
    for name in names:
        value = d.get(name)
        if value is not None:
            return value
    return None


def sanitize_agent_id(raw: str) -> str:
    return AGENT_SAFE_RE.sub("-", raw).strip("-")[:64] or "sem-agent"


def request_with_retry(
    method: str,
    url: str,
    body: dict[str, Any] | None = None,
    timeout: int = 60,
    attempts: int = 3,
) -> tuple[int, Any, float]:
    """request_json with retry on 5xx and connection errors.

    Returns (status, payload, elapsed_ms); status 0 means connection failure.
    """
    delay = 0.5
    last: tuple[int, Any, float] = (0, "no attempt made", 0.0)
    for attempt in range(attempts):
        try:
            status, payload, elapsed_ms = request_json(method, url, body, timeout=timeout)
            last = (status, payload, elapsed_ms)
            if status < 500:
                return last
        except (urllib.error.URLError, TimeoutError, OSError) as exc:
            last = (0, f"connection error: {exc}", 0.0)
        if attempt < attempts - 1:
            time.sleep(delay)
            delay *= 2
    return last


# ── Dataset acquisition ───────────────────────────────────────────────────────


def load_raw_dataset(dataset: str, dataset_file: Path | None) -> Any:
    if dataset_file is not None:
        if not dataset_file.exists():
            raise DatasetUnavailable(
                f"--dataset-file {dataset_file} does not exist; supply a local "
                f"{'LongMemEval (longmemeval_s/oracle)' if dataset == 'longmemeval' else 'LoCoMo (locomo10)'}"
                " JSON file"
            )
        try:
            return json.loads(dataset_file.read_text(encoding="utf-8"))
        except json.JSONDecodeError as exc:
            raise DatasetFormatError(f"{dataset_file} is not valid JSON: {exc}") from exc
    return download_dataset(dataset)


def download_dataset(dataset: str) -> Any:
    """Best-effort download; network may be restricted in CI/dev environments."""
    failures: list[str] = []
    if dataset == "longmemeval":
        try:
            from huggingface_hub import hf_hub_download  # type: ignore

            path = hf_hub_download(
                repo_id=LONGMEMEVAL_HF_REPO,
                filename=LONGMEMEVAL_HF_FILE,
                repo_type="dataset",
            )
            return json.loads(Path(path).read_text(encoding="utf-8"))
        except ImportError:
            failures.append(
                "huggingface_hub not installed (pip install huggingface_hub or datasets)"
            )
        except Exception as exc:  # network / auth / repo layout
            failures.append(f"huggingface_hub download failed: {exc}")
    else:  # locomo
        try:
            with urllib.request.urlopen(LOCOMO_RAW_URL, timeout=60) as resp:
                return json.loads(resp.read().decode("utf-8"))
        except Exception as exc:
            failures.append(f"download of {LOCOMO_RAW_URL} failed: {exc}")
    raise DatasetUnavailable(
        f"no --dataset-file given and automatic download of '{dataset}' failed "
        f"({'; '.join(failures)}). Download the dataset manually and pass --dataset-file."
    )


# ── Normalisation ─────────────────────────────────────────────────────────────
# Both loaders produce:
#   corpora: {agent_id: [{"key": str, "content": str}, ...]}
#   units:   [{"qid", "question", "category", "agent_id", "gold_keys": set[str]}]


def unwrap_list(raw: Any) -> list[Any]:
    if isinstance(raw, dict):
        for key in ("data", "instances", "examples", "samples", "conversations"):
            if isinstance(raw.get(key), list):
                return raw[key]
    if isinstance(raw, list):
        return raw
    raise DatasetFormatError(
        f"expected a JSON list of instances (or a dict wrapping one under "
        f"'data'/'instances'/...); got {type(raw).__name__}"
        + (f" with keys {sorted(raw.keys())}" if isinstance(raw, dict) else "")
    )


def normalize_longmemeval(raw: Any) -> tuple[dict[str, list[dict]], list[dict], dict]:
    instances = unwrap_list(raw)
    if not instances:
        raise DatasetFormatError("LongMemEval file contains an empty list")

    corpora: dict[str, list[dict]] = {}
    units: list[dict] = []
    skipped_no_gold = 0
    seen_agents: set[str] = set()

    for idx, inst in enumerate(instances):
        if not isinstance(inst, dict):
            raise DatasetFormatError(
                f"LongMemEval instance {idx} is {type(inst).__name__}, expected object"
            )
        qid = first_present(inst, ("question_id", "qid", "id")) or f"q{idx}"
        question = first_present(inst, ("question", "query"))
        sessions = first_present(inst, ("haystack_sessions", "sessions"))
        if question is None or sessions is None:
            raise DatasetFormatError(
                f"LongMemEval instance {idx} missing 'question'/'haystack_sessions' "
                f"(variants tried: question|query, haystack_sessions|sessions); "
                f"found keys: {sorted(inst.keys())}"
            )
        session_ids = first_present(inst, ("haystack_session_ids", "session_ids")) or []
        answer_sids = first_present(inst, ("answer_session_ids", "answer_session_id")) or []
        if isinstance(answer_sids, (str, int)):
            answer_sids = [answer_sids]
        answer_sid_set = {str(s) for s in answer_sids}
        category = str(
            first_present(inst, ("question_type", "type", "category")) or "unknown"
        )

        agent_id = sanitize_agent_id(f"sem-lme-{qid}")
        if agent_id in seen_agents:
            agent_id = sanitize_agent_id(f"sem-lme-{qid}-{idx}")
        seen_agents.add(agent_id)

        turns: list[dict] = []
        gold_keys: set[str] = set()
        for si, session in enumerate(sessions):
            if isinstance(session, dict):
                session_turns = first_present(session, ("turns", "messages")) or []
            else:
                session_turns = session
            if not isinstance(session_turns, list):
                raise DatasetFormatError(
                    f"LongMemEval instance {idx} session {si}: expected a list of "
                    f"turns, got {type(session_turns).__name__}"
                )
            sid = str(session_ids[si]) if si < len(session_ids) else f"session_{si}"
            session_is_answer = sid in answer_sid_set
            for ti, turn in enumerate(session_turns):
                if not isinstance(turn, dict):
                    raise DatasetFormatError(
                        f"LongMemEval instance {idx} session {si} turn {ti}: "
                        f"expected object with role/content, got {type(turn).__name__}"
                    )
                content = first_present(turn, ("content", "text", "message"))
                if not content or not str(content).strip():
                    continue
                key = f"s{si}t{ti}"
                turns.append({"key": key, "content": str(content).strip()})
                if bool(turn.get("has_answer")) or session_is_answer:
                    gold_keys.add(key)

        if not turns or not gold_keys:
            skipped_no_gold += 1
            continue
        corpora[agent_id] = turns
        units.append(
            {
                "qid": str(qid),
                "question": str(question),
                "category": category,
                "agent_id": agent_id,
                "gold_keys": gold_keys,
            }
        )

    if not units:
        sample_keys = sorted(instances[0].keys()) if isinstance(instances[0], dict) else []
        raise DatasetFormatError(
            "no usable LongMemEval instances: every instance lacked evidence "
            "markers (per-turn 'has_answer' or 'answer_session_ids' matching "
            f"'haystack_session_ids'). First instance keys: {sample_keys}"
        )
    return corpora, units, {"skipped_no_gold": skipped_no_gold}


def normalize_locomo(raw: Any) -> tuple[dict[str, list[dict]], list[dict], dict]:
    items = unwrap_list(raw)
    if not items:
        raise DatasetFormatError("LoCoMo file contains an empty list")

    corpora: dict[str, list[dict]] = {}
    units: list[dict] = []
    skipped_no_gold = 0
    qa_total = 0

    for cidx, item in enumerate(items):
        if not isinstance(item, dict):
            raise DatasetFormatError(
                f"LoCoMo item {cidx} is {type(item).__name__}, expected object"
            )
        conv = first_present(item, ("conversation", "conversations", "dialog"))
        qa_list = first_present(item, ("qa", "qas", "questions"))
        if conv is None or qa_list is None:
            raise DatasetFormatError(
                f"LoCoMo item {cidx} missing 'conversation'/'qa' (variants tried: "
                f"conversation|conversations|dialog, qa|qas|questions); "
                f"found keys: {sorted(item.keys())}"
            )

        sessions: list[list[Any]] = []
        if isinstance(conv, dict):
            session_keys = sorted(
                (k for k in conv if re.fullmatch(r"session_\d+", k) and isinstance(conv[k], list)),
                key=lambda k: int(k.split("_")[1]),
            )
            sessions = [conv[k] for k in session_keys]
            if not sessions:
                nested = first_present(conv, ("sessions", "turns"))
                if isinstance(nested, list):
                    sessions = nested if nested and isinstance(nested[0], list) else [nested]
        elif isinstance(conv, list):
            sessions = conv if conv and isinstance(conv[0], list) else [conv]
        if not sessions:
            raise DatasetFormatError(
                f"LoCoMo item {cidx}: could not find sessions inside 'conversation' "
                f"(expected 'session_N' list keys, or a list of turn lists); found: "
                f"{sorted(conv.keys()) if isinstance(conv, dict) else type(conv).__name__}"
            )

        agent_id = sanitize_agent_id(f"sem-loco-{cidx}")
        turns: list[dict] = []
        for si, sess in enumerate(sessions):
            if isinstance(sess, dict):
                sess = first_present(sess, ("turns", "messages")) or []
            for ti, turn in enumerate(sess):
                if not isinstance(turn, dict):
                    continue
                text = first_present(turn, ("text", "content", "clean_text"))
                if not text or not str(text).strip():
                    continue
                speaker = first_present(turn, ("speaker", "role", "name"))
                key = str(first_present(turn, ("dia_id", "id", "turn_id")) or f"s{si}t{ti}")
                content = f"{speaker}: {text}".strip() if speaker else str(text).strip()
                turns.append({"key": key, "content": content})
        if not turns:
            continue
        key_set = {t["key"] for t in turns}

        for qidx, qa in enumerate(qa_list):
            if not isinstance(qa, dict):
                continue
            qa_total += 1
            question = first_present(qa, ("question", "query"))
            if not question:
                continue
            evidence = qa.get("evidence") or []
            if isinstance(evidence, (str, int)):
                evidence = [evidence]
            flat: list[str] = []
            for ev in evidence:
                if isinstance(ev, (list, tuple)):
                    flat.extend(str(e) for e in ev)
                else:
                    flat.append(str(ev))
            gold_keys = {e for e in flat if e in key_set}
            if not gold_keys:
                # e.g. adversarial QAs (category 5) have no in-corpus evidence
                skipped_no_gold += 1
                continue
            units.append(
                {
                    "qid": f"conv{cidx}-qa{qidx}",
                    "question": str(question),
                    "category": str(qa.get("category", "unknown")),
                    "agent_id": agent_id,
                    "gold_keys": gold_keys,
                }
            )
        corpora[agent_id] = turns

    if not units:
        raise DatasetFormatError(
            f"no usable LoCoMo QA pairs out of {qa_total}: no 'evidence' entry "
            "matched any turn 'dia_id'. Check that evidence ids (e.g. 'D1:3') "
            "use the same format as the turns' dia_id fields."
        )
    # Drop corpora that no sampled-able unit references
    used_agents = {u["agent_id"] for u in units}
    corpora = {a: t for a, t in corpora.items() if a in used_agents}
    return corpora, units, {"skipped_no_gold": skipped_no_gold}


# ── Sampling ──────────────────────────────────────────────────────────────────


def stratified_sample(units: list[dict], n: int, seed: int) -> list[dict]:
    """Sample n units, stratified by 'category' when more than one is present."""
    rng = random.Random(seed)
    if n >= len(units):
        return list(units)
    groups: dict[str, list[dict]] = defaultdict(list)
    for unit in units:
        groups[unit.get("category", "unknown")].append(unit)
    if len(groups) <= 1:
        return rng.sample(units, n)

    total = len(units)
    keys = sorted(groups)
    alloc = {k: (n * len(groups[k])) // total for k in keys}
    remainders = sorted(
        keys,
        key=lambda k: ((n * len(groups[k])) % total, k),
        reverse=True,
    )
    i = 0
    while sum(alloc.values()) < n:
        k = remainders[i % len(remainders)]
        if alloc[k] < len(groups[k]):
            alloc[k] += 1
        i += 1
        if i > 10 * len(keys):  # every group saturated
            break
    sampled: list[dict] = []
    for k in keys:
        take = min(alloc[k], len(groups[k]))
        if take:
            sampled.extend(rng.sample(groups[k], take))
    # top up if saturation left us short
    if len(sampled) < n:
        chosen = {id(u) for u in sampled}
        rest = [u for u in units if id(u) not in chosen]
        sampled.extend(rng.sample(rest, min(n - len(sampled), len(rest))))
    return sampled


# ── Metrics ───────────────────────────────────────────────────────────────────


def dcg(relevances: list[int], k: int) -> float:
    return sum(rel / math.log2(i + 2) for i, rel in enumerate(relevances[:k]))


def query_metrics(result_ids: list[str], gold_ids: set[str]) -> dict[str, Any]:
    rels = [1 if rid in gold_ids else 0 for rid in result_ids[:SEARCH_LIMIT]]
    first_rank = next((i + 1 for i, r in enumerate(rels) if r), None)
    ideal = dcg([1] * min(len(gold_ids), SEARCH_LIMIT), SEARCH_LIMIT)
    out: dict[str, Any] = {
        "first_gold_rank": first_rank,
        "mrr_at_10": round(1.0 / first_rank, 4) if first_rank else 0.0,
        "ndcg_at_10": round(dcg(rels, SEARCH_LIMIT) / ideal, 4) if ideal > 0 else 0.0,
        "precision_at_5": round(sum(rels[:5]) / 5.0, 4),
    }
    for k in RECALL_KS:
        out[f"recall_at_{k}"] = 1 if any(rels[:k]) else 0
    return out


def aggregate(rows: list[dict]) -> dict[str, Any]:
    ok = [r for r in rows if r.get("evaluated")]
    if not ok:
        return {"queries": 0}

    def mean(key: str) -> float:
        return round(sum(float(r[key]) for r in ok) / len(ok), 4)

    out: dict[str, Any] = {"queries": len(ok)}
    for k in RECALL_KS:
        out[f"recall_at_{k}"] = mean(f"recall_at_{k}")
    out["mrr_at_10"] = mean("mrr_at_10")
    out["ndcg_at_10"] = mean("ndcg_at_10")
    out["precision_at_5"] = mean("precision_at_5")
    ranks = [r["first_gold_rank"] for r in ok if r.get("first_gold_rank")]
    out["mean_first_gold_rank"] = round(sum(ranks) / len(ranks), 4) if ranks else None
    return out


# ── Kernel interaction ────────────────────────────────────────────────────────


def delete_agent(agent_id: str) -> None:
    request_with_retry("DELETE", f"{AEON_BASE_URL}/api/v1/agents/{agent_id}", timeout=60)


def seed_agent(
    agent_id: str, turns: list[dict], counters: dict[str, int]
) -> dict[str, str]:
    """Seed every turn as one memory; return {turn_key: memory_id}.

    The kernel embeds server-side.  Note: with DEDUP_THRESHOLD > 0 the kernel
    may return an EXISTING memory id for a near-duplicate turn; gold matching
    uses id sets, so deduped gold turns still resolve correctly.
    """
    # Pre-clean any residue from a crashed/kept previous run so corpora stay isolated.
    delete_agent(agent_id)
    mapping: dict[str, str] = {}
    for turn in turns:
        status, payload, _ = request_with_retry(
            "POST",
            f"{AEON_BASE_URL}/api/v1/agents/{agent_id}/memories",
            {"content": turn["content"], "memory_type": "episodic", "importance": 0.5},
        )
        counters["requests"] += 1
        # create_memory responds {"id": "<uuid>", "success": true} (src/api.rs)
        if 200 <= status < 300 and isinstance(payload, dict) and payload.get("id"):
            mapping[turn["key"]] = str(payload["id"])
        else:
            counters["errors"] += 1
    return mapping


def evaluate_unit(
    dataset: str,
    unit: dict,
    mapping: dict[str, str],
    condition: str,
    latencies: list[float],
    counters: dict[str, int],
) -> dict[str, Any]:
    gold_ids = {mapping[k] for k in unit["gold_keys"] if k in mapping}
    base = {
        "dataset": dataset,
        "question_id": unit["qid"],
        "category": unit["category"],
        "agent_id": unit["agent_id"],
        "condition": condition,
        "gold_count": len(gold_ids),
        "seeded_count": len(set(mapping.values())),
        "evaluated": False,
        "note": "",
    }
    if not gold_ids:
        base["note"] = "all gold turns failed to seed; excluded from aggregates"
        return base

    status, payload, elapsed_ms = request_with_retry(
        "POST",
        f"{AEON_BASE_URL}/api/v1/memories/search",
        {
            "agent_id": unit["agent_id"],
            "query": unit["question"],
            "limit": SEARCH_LIMIT,
            "threshold": SEMANTIC_THRESHOLD,
        },
    )
    counters["requests"] += 1
    base["search_status_code"] = status
    if not (200 <= status < 300):
        counters["errors"] += 1
        base["note"] = f"search failed: {payload}"[:200]
        return base

    latencies.append(elapsed_ms)
    results = payload.get("results", []) if isinstance(payload, dict) else []
    result_ids = [str(r.get("id")) for r in results[:SEARCH_LIMIT]]
    base.update(query_metrics(result_ids, gold_ids))
    base["result_count"] = len(results)
    base["search_latency_ms"] = round(elapsed_ms, 3)
    base["evaluated"] = True
    return base


# ── Main ──────────────────────────────────────────────────────────────────────


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--dataset", required=True, choices=["longmemeval", "locomo"])
    parser.add_argument(
        "--dataset-file",
        type=Path,
        default=None,
        help="Local dataset JSON; if absent an HF/GitHub download is attempted",
    )
    parser.add_argument("--sample-size", type=int, default=100)
    parser.add_argument(
        "--condition",
        default="unspecified",
        help="Free-text label recorded in results (e.g. aeon-full, cosine-baseline). "
        "The kernel's env config determines the actual condition.",
    )
    parser.add_argument("--results-dir", type=Path, default=default_results_dir())
    parser.add_argument("--seed", type=int, default=42, help="PRNG seed for sampling")
    parser.add_argument(
        "--keep-agents",
        action="store_true",
        help="Do not delete the sem-* benchmark agents after evaluation",
    )
    return parser.parse_args()


def emit_not_run(results_dir: Path, filename: str, reason: str) -> int:
    not_run(AREA, reason, results_dir, filename)
    print(f"[semantic] not_run: {reason}")
    return 0


def main() -> int:
    args = parse_args()
    args.results_dir.mkdir(parents=True, exist_ok=True)
    artifact = f"semantic_quality_{args.dataset}.json"

    if os.environ.get(GATE_ENV, "").strip().lower() != "true":
        return emit_not_run(
            args.results_dir,
            "semantic_quality.json",
            f"{GATE_ENV} != true; semantic benchmark is optional/cost-gated and "
            "never a required CI gate",
        )

    try:
        raw = load_raw_dataset(args.dataset, args.dataset_file)
    except DatasetUnavailable as exc:
        return emit_not_run(args.results_dir, artifact, f"dataset unavailable: {exc}")
    except DatasetFormatError as exc:
        return emit_not_run(args.results_dir, artifact, f"dataset format error: {exc}")

    try:
        if args.dataset == "longmemeval":
            corpora, units, load_stats = normalize_longmemeval(raw)
        else:
            corpora, units, load_stats = normalize_locomo(raw)
    except DatasetFormatError as exc:
        return emit_not_run(args.results_dir, artifact, f"dataset format error: {exc}")

    status, _, _ = request_with_retry("GET", f"{AEON_BASE_URL}/api/v1/stats", timeout=15)
    if not (200 <= status < 300):
        return emit_not_run(
            args.results_dir,
            artifact,
            f"kernel unreachable or unauthorized at {AEON_BASE_URL} "
            f"(GET /api/v1/stats -> {status}); check AEON_BASE_URL/MANAGEMENT_API_KEY",
        )

    sampled = stratified_sample(units, args.sample_size, args.seed)
    agent_units: dict[str, list[dict]] = defaultdict(list)
    agent_order: list[str] = []
    for unit in sampled:
        if unit["agent_id"] not in agent_units:
            agent_order.append(unit["agent_id"])
        agent_units[unit["agent_id"]].append(unit)

    print(
        f"[semantic] dataset={args.dataset} units={len(sampled)}/{len(units)} "
        f"agents={len(agent_order)} condition={args.condition} "
        f"threshold={SEMANTIC_THRESHOLD} base={AEON_BASE_URL}"
    )

    counters = {"requests": 0, "errors": 0}
    latencies: list[float] = []
    rows: list[dict] = []
    created_agents: list[str] = []
    seeded_memory_count = 0
    run_status = "ok"
    bail_reason = None
    processed = 0

    try:
        for agent_id in agent_order:
            mapping = seed_agent(agent_id, corpora[agent_id], counters)
            created_agents.append(agent_id)
            seeded_memory_count += len(mapping)

            for unit in agent_units[agent_id]:
                rows.append(
                    evaluate_unit(
                        args.dataset, unit, mapping, args.condition, latencies, counters
                    )
                )
                processed += 1
                if processed % 10 == 0:
                    print(f"[semantic] {processed}/{len(sampled)} instances evaluated")

            if not args.keep_agents:
                delete_agent(agent_id)
                created_agents.remove(agent_id)

            error_rate = counters["errors"] / max(counters["requests"], 1)
            if counters["requests"] >= MIN_REQUESTS_BEFORE_BAIL and error_rate > MAX_ERROR_RATE:
                run_status = "fail"
                bail_reason = (
                    f"error rate {error_rate:.2%} exceeded {MAX_ERROR_RATE:.0%} after "
                    f"{counters['requests']} requests; writing partial artifact"
                )
                print(f"[semantic] BAIL: {bail_reason}")
                break
    finally:
        if not args.keep_agents:
            for agent_id in list(created_agents):
                delete_agent(agent_id)

    per_category: dict[str, Any] = {}
    for category in sorted({r["category"] for r in rows}):
        per_category[category] = aggregate([r for r in rows if r["category"] == category])

    payload = {
        "area": AREA,
        "status": run_status,
        "reason": bail_reason,
        "dataset": args.dataset,
        "dataset_file": str(args.dataset_file) if args.dataset_file else None,
        "condition": args.condition,
        "threshold": SEMANTIC_THRESHOLD,
        "aeon_base_url": AEON_BASE_URL,
        "generated_at": utc_now(),
        "seed": args.seed,
        "sample_size_requested": args.sample_size,
        "sample_size_evaluated": sum(1 for r in rows if r.get("evaluated")),
        "units_available": len(units),
        "seeded_memory_count": seeded_memory_count,
        "load_stats": load_stats,
        "errors": {
            "request_count": counters["requests"],
            "error_count": counters["errors"],
            "error_rate": round(counters["errors"] / max(counters["requests"], 1), 4),
        },
        "agents_kept": created_agents if args.keep_agents else [],
        "summary": aggregate(rows),
        "per_category": per_category,
        "search_latency": latency_summary(latencies),
        "rows": rows,
        "method": (
            "Memories seeded via POST /api/v1/agents/:id/memories (kernel embeds "
            "server-side with its configured embedding model); queries via "
            "POST /api/v1/memories/search; gold = memory ids of evidence turns."
        ),
    }
    write_json(args.results_dir / artifact, payload)
    write_csv(args.results_dir / f"semantic_quality_{args.dataset}.csv", rows, CSV_FIELDS)
    print(
        f"[semantic] status={run_status} evaluated={payload['sample_size_evaluated']} "
        f"summary={json.dumps(payload['summary'])}"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
