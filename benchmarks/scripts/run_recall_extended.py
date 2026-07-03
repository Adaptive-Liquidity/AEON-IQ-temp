#!/usr/bin/env python3
"""Extended ranked-retrieval benchmark: larger corpus, distractors, MRR/nDCG.

Addresses the main methodological gaps of run_recall_quality.py (7 memories,
14 queries, recall@k only):

- 120 target facts across 12 topical clusters, seeded among 1,000
  lexically-confusable distractor memories in the same agent (1,120 total).
- 3 query variants per fact (360 queries): high-overlap paraphrase,
  reduced-overlap paraphrase, keyword-style.
- Ranked metrics at limit=10: recall@{1,3,5,10}, MRR@10, nDCG@10,
  precision@5, plus per-cluster and per-query-type breakdowns.
- Search latency percentiles over all 360 calls at the 1,120-memory corpus.

Determinism: the corpus and queries are generated from fixed templates with a
fixed PRNG seed; embeddings use the same hash embedding as the mock upstream
(MOCK_EMBEDDING_MODE=hash), so results are exactly reproducible. As with the
base suite, this measures the retrieval pipeline (SQL ranking, thresholding,
ANN behaviour) under lexical-hash similarity - not semantic embedding quality.
"""

from __future__ import annotations

import argparse
import math
import random
import time
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

from common import (
    AEON_BASE_URL,
    DATABASE_URL,
    default_results_dir,
    hash_embedding,
    latency_summary,
    not_run,
    request_json,
    utc_now,
    vector_literal,
    write_csv,
    write_json,
)

AGENT_ID = "bench-recall-ext"
DATASET_VERSION = "0.2.0"
RNG_SEED = 42
TARGETS_PER_CLUSTER = 10
DISTRACTORS_TOTAL = 1000
SEARCH_LIMIT = 10
SEARCH_THRESHOLD = 0.95

# 12 topical clusters. Each defines shared vocabulary (which makes distractors
# lexically confusable with targets) and unique entity names per fact (which
# make each target retrievable at all under bag-of-words hashing).
CLUSTERS: list[dict[str, Any]] = [
    {
        "name": "deployment",
        "shared": "service deploys to the region cluster every window",
        "entities": ["Zephyr", "Kestrel", "Auriga", "Brontes", "Caldera", "Dynamo", "Erebus", "Fennec", "Gyrfalcon", "Halcyon"],
        "template": "The {e} service deploys to the {e}-primary region cluster every {n} hour maintenance window.",
        "q_full": "Which region cluster does the {e} service deploy to and how often?",
        "q_partial": "Recall the rollout cadence for {e}.",
        "q_keyword": "{e} deploy window",
    },
    {
        "name": "database",
        "shared": "table stores rows partitioned by key retention days",
        "entities": ["ledgerbook", "eventlog", "sessionmap", "auditrail", "vectordex", "graphedge", "policyreg", "capstore", "timequeue", "snapindex"],
        "template": "The {e} table stores rows partitioned by {e}_key with a retention of {n} days.",
        "q_full": "What is the partition key and retention for the {e} table?",
        "q_partial": "Recall how {e} rows are kept.",
        "q_keyword": "{e} partition retention",
    },
    {
        "name": "security",
        "shared": "requires approval before access tokens rotate policy",
        "entities": ["Aegis", "Bastion", "Citadel", "Drawbridge", "Enclave", "Fortress", "Garrison", "Keystone", "Palisade", "Rampart"],
        "template": "The {e} policy requires two-person approval before {e} access tokens rotate every {n} days.",
        "q_full": "What approval does the {e} policy require before token rotation?",
        "q_partial": "Recall the {e} rotation rule.",
        "q_keyword": "{e} token approval",
    },
    {
        "name": "budget",
        "shared": "monthly spend capped at dollars alerts finance owner",
        "entities": ["Atlas", "Borealis", "Cascade", "Denali", "Everest", "Fuji", "Grampian", "Himal", "Iguazu", "Jotun"],
        "template": "Project {e} monthly spend is capped at {n} thousand dollars with alerts to the {e} finance owner.",
        "q_full": "What is the monthly spend cap for project {e}?",
        "q_partial": "Recall who is alerted about {e} costs.",
        "q_keyword": "{e} spend cap",
    },
    {
        "name": "oncall",
        "shared": "rotation pages the team channel escalation after minutes",
        "entities": ["puma", "lynx", "ocelot", "serval", "caracal", "margay", "bobcat", "jaguarundi", "kodkod", "oncilla"],
        "template": "The {e} rotation pages the {e}-team channel with escalation after {n} minutes of no acknowledgement.",
        "q_full": "How long before the {e} rotation escalates a page?",
        "q_partial": "Recall the paging path for {e}.",
        "q_keyword": "{e} escalation minutes",
    },
    {
        "name": "compliance",
        "shared": "audit evidence must be exported quarterly reviewer signs",
        "entities": ["SOX-A", "GDPR-B", "HIPAA-C", "PCI-D", "FEDRAMP-E", "ISO-F", "SOC2-G", "CCPA-H", "NIST-I", "DORA-J"],
        "template": "For control {e}, audit evidence must be exported quarterly and the {e} reviewer signs the packet within {n} days.",
        "q_full": "Who signs the audit packet for control {e} and when?",
        "q_partial": "Recall the evidence export rule for {e}.",
        "q_keyword": "{e} audit evidence",
    },
    {
        "name": "model",
        "shared": "endpoint serves the model with context tokens temperature",
        "entities": ["quasar", "pulsar", "magnetar", "blazar", "cepheid", "wolfrayet", "protostar", "reddwarf", "neutron", "supergiant"],
        "template": "The {e} endpoint serves the {e}-v2 model with a {n} thousand token context and temperature locked at zero.",
        "q_full": "What context size does the {e} endpoint serve?",
        "q_partial": "Recall the {e} model settings.",
        "q_keyword": "{e} context tokens",
    },
    {
        "name": "storage",
        "shared": "bucket replicates objects across zones lifecycle moves cold",
        "entities": ["ambervault", "bluecrate", "cindershelf", "dunecache", "embercask", "frostbin", "gladehold", "hazelloft", "ironcellar", "junipershed"],
        "template": "The {e} bucket replicates objects across three zones and lifecycle rules move cold data after {n} days.",
        "q_full": "When does the {e} bucket move data to cold storage?",
        "q_partial": "Recall replication for {e}.",
        "q_keyword": "{e} lifecycle cold",
    },
    {
        "name": "network",
        "shared": "gateway routes traffic through the mesh timeout retries",
        "entities": ["viaduct", "causeway", "trestle", "aqueduct", "skybridge", "underpass", "culvert", "flyover", "pontoon", "bascule"],
        "template": "The {e} gateway routes traffic through the {e}-mesh with a {n} second timeout and two retries.",
        "q_full": "What timeout does the {e} gateway apply?",
        "q_partial": "Recall how {e} traffic is routed.",
        "q_keyword": "{e} gateway timeout",
    },
    {
        "name": "release",
        "shared": "train ships versions gated by the checklist freeze",
        "entities": ["harvest", "solstice", "equinox", "perihelion", "aphelion", "zenith", "nadir", "meridian", "twilight", "auroral"],
        "template": "The {e} release train ships versions gated by the {e} checklist with a freeze {n} days before launch.",
        "q_full": "How long is the freeze before the {e} release train launches?",
        "q_partial": "Recall the gating for the {e} train.",
        "q_keyword": "{e} release freeze",
    },
    {
        "name": "team",
        "shared": "squad owns the roadmap meets weekly demo staffed engineers",
        "entities": ["Foxtrot", "Gavotte", "Habanera", "Iberia", "Jitterbug", "Kalinka", "Lambada", "Mazurka", "Nocturne", "Oberek"],
        "template": "The {e} squad owns the {e} roadmap, meets weekly for demo review, and is staffed with {n} engineers.",
        "q_full": "How many engineers staff the {e} squad?",
        "q_partial": "Recall what the {e} squad owns.",
        "q_keyword": "{e} squad engineers",
    },
    {
        "name": "incident",
        "shared": "postmortem filed within hours severity review action items",
        "entities": ["INC-Umber", "INC-Vermel", "INC-Willow", "INC-Xenon", "INC-Yarrow", "INC-Zircon", "INC-Amber", "INC-Basalt", "INC-Coral", "INC-Dolomite"],
        "template": "Incident {e} requires a postmortem filed within {n} hours and a severity review with tracked action items.",
        "q_full": "How quickly must the {e} postmortem be filed?",
        "q_partial": "Recall the review required for {e}.",
        "q_keyword": "{e} postmortem hours",
    },
]


def build_corpus() -> tuple[list[dict[str, Any]], list[str]]:
    rng = random.Random(RNG_SEED)
    targets: list[dict[str, Any]] = []
    for cluster in CLUSTERS:
        for i, entity in enumerate(cluster["entities"][:TARGETS_PER_CLUSTER]):
            n = rng.randint(2, 90)
            content = cluster["template"].format(e=entity, n=n) + " " + cluster["shared"] + "."
            label = f"{cluster['name']}_{i}"
            targets.append(
                {
                    "label": label,
                    "cluster": cluster["name"],
                    "content": content,
                    "queries": {
                        "full_paraphrase": cluster["q_full"].format(e=entity),
                        "partial_paraphrase": cluster["q_partial"].format(e=entity),
                        "keyword": cluster["q_keyword"].format(e=entity),
                    },
                }
            )
    distractors: list[str] = []
    per_cluster = DISTRACTORS_TOTAL // len(CLUSTERS)
    extra = DISTRACTORS_TOTAL - per_cluster * len(CLUSTERS)
    for ci, cluster in enumerate(CLUSTERS):
        count = per_cluster + (1 if ci < extra else 0)
        for j in range(count):
            filler = rng.choice(["review", "meeting", "note", "draft", "update", "summary", "ticket", "thread"])
            distractors.append(
                f"Distractor {cluster['name']} {j}: general {filler} about {cluster['shared']} without a specific owner."
            )
    return targets, distractors


def seed_corpus(targets: list[dict[str, Any]], distractors: list[str]) -> dict[str, str]:
    import psycopg

    now = datetime.now(timezone.utc)
    label_to_id: dict[str, str] = {}
    with psycopg.connect(DATABASE_URL) as conn:
        with conn.cursor() as cur:
            cur.execute("DELETE FROM memories WHERE agent_id = %s", (AGENT_ID,))
            cur.execute("DELETE FROM memory_retrieval_logs WHERE agent_id = %s", (AGENT_ID,))
            cur.execute("INSERT INTO agents (agent_id) VALUES (%s) ON CONFLICT DO NOTHING", (AGENT_ID,))
            for item in targets:
                cur.execute(
                    """
                    INSERT INTO memories
                        (agent_id, session_id, content, memory_type, confidence, embedding,
                         created_at, updated_at, tier, provenance, importance_score,
                         importance_source, status, sensitivity)
                    VALUES (%s, %s, %s, 'semantic', 1.0, %s::vector, %s, %s, 'L2',
                            'user_stated', 0.7, 'user_stated', 'active', 'unknown')
                    RETURNING id
                    """,
                    (AGENT_ID, f"{AGENT_ID}-seed", item["content"], vector_literal(hash_embedding(item["content"])), now, now),
                )
                label_to_id[item["label"]] = str(cur.fetchone()[0])
            for content in distractors:
                cur.execute(
                    """
                    INSERT INTO memories
                        (agent_id, session_id, content, memory_type, confidence, embedding,
                         created_at, updated_at, tier, provenance, importance_score,
                         importance_source, status, sensitivity)
                    VALUES (%s, %s, %s, 'semantic', 1.0, %s::vector, %s, %s, 'L2',
                            'user_stated', 0.5, 'user_stated', 'active', 'unknown')
                    """,
                    (AGENT_ID, f"{AGENT_ID}-seed", content, vector_literal(hash_embedding(content)), now, now),
                )
        conn.commit()
    return label_to_id


def dcg_at(rank: int | None, k: int) -> float:
    if rank is None or rank > k:
        return 0.0
    return 1.0 / math.log2(rank + 1)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--results-dir", type=Path, default=default_results_dir())
    args = parser.parse_args()
    args.results_dir.mkdir(parents=True, exist_ok=True)

    try:
        import psycopg  # noqa: F401
    except Exception as exc:
        not_run("recall_extended", f"psycopg required: {exc}", args.results_dir, "recall_extended.json")
        return 0

    targets, distractors = build_corpus()
    label_to_id = seed_corpus(targets, distractors)

    rows: list[dict[str, Any]] = []
    latencies: list[float] = []
    errors = 0
    for item in targets:
        expected_id = label_to_id[item["label"]]
        for qtype, query in item["queries"].items():
            start = time.perf_counter()
            status, payload, _ = request_json(
                "POST",
                f"{AEON_BASE_URL}/api/v1/memories/search",
                {"agent_id": AGENT_ID, "query": query, "limit": SEARCH_LIMIT, "threshold": SEARCH_THRESHOLD},
                timeout=30,
            )
            elapsed_ms = (time.perf_counter() - start) * 1000.0
            if 200 <= status < 300:
                latencies.append(elapsed_ms)
                results = payload.get("results", [])
            else:
                errors += 1
                results = []
            rank = None
            for pos, r in enumerate(results, start=1):
                if r.get("id") == expected_id:
                    rank = pos
                    break
            rows.append(
                {
                    "label": item["label"],
                    "cluster": item["cluster"],
                    "query_type": qtype,
                    "query": query,
                    "rank": rank if rank is not None else "",
                    "recall_at_1": rank == 1,
                    "recall_at_3": rank is not None and rank <= 3,
                    "recall_at_5": rank is not None and rank <= 5,
                    "recall_at_10": rank is not None and rank <= 10,
                    "reciprocal_rank": round(1.0 / rank, 4) if rank else 0.0,
                    "ndcg_at_10": round(dcg_at(rank, 10), 4),
                    "precision_at_5": round((1 if rank is not None and rank <= 5 else 0) / 5.0, 3),
                    "result_count": len(results),
                    "latency_ms": round(elapsed_ms, 3),
                    "status_code": status,
                }
            )

    def aggregate(subset: list[dict[str, Any]]) -> dict[str, Any]:
        if not subset:
            return {}
        n = len(subset)
        return {
            "queries": n,
            "recall_at_1": round(sum(1 for r in subset if r["recall_at_1"]) / n, 4),
            "recall_at_3": round(sum(1 for r in subset if r["recall_at_3"]) / n, 4),
            "recall_at_5": round(sum(1 for r in subset if r["recall_at_5"]) / n, 4),
            "recall_at_10": round(sum(1 for r in subset if r["recall_at_10"]) / n, 4),
            "mrr_at_10": round(sum(r["reciprocal_rank"] for r in subset) / n, 4),
            "ndcg_at_10": round(sum(r["ndcg_at_10"] for r in subset) / n, 4),
            "precision_at_5": round(sum(r["precision_at_5"] for r in subset) / n, 4),
        }

    by_type = {
        qtype: aggregate([r for r in rows if r["query_type"] == qtype])
        for qtype in ("full_paraphrase", "partial_paraphrase", "keyword")
    }
    by_cluster = {
        c["name"]: aggregate([r for r in rows if r["cluster"] == c["name"]]) for c in CLUSTERS
    }

    payload = {
        "status": "ok",
        "generated_at": utc_now(),
        "dataset_version": DATASET_VERSION,
        "method": (
            "120 target facts across 12 clusters seeded among 1000 lexically-confusable "
            "distractors (1120 memories, one agent). 3 query variants per fact (360 queries), "
            "search limit=10 threshold=0.95. Rank of the expected memory id yields recall@k, "
            "MRR@10, nDCG@10 (single relevant document per query). Hash embeddings: this "
            "exercises the retrieval pipeline deterministically, not semantic embedding quality."
        ),
        "corpus": {
            "targets": len(targets),
            "distractors": len(distractors),
            "clusters": len(CLUSTERS),
            "queries": len(rows),
            "rng_seed": RNG_SEED,
        },
        "summary": aggregate(rows),
        "search_latency": latency_summary(latencies, errors),
        "by_query_type": by_type,
        "by_cluster": by_cluster,
    }
    write_json(args.results_dir / "recall_extended.json", payload)
    write_csv(
        args.results_dir / "recall_extended.csv",
        rows,
        [
            "label", "cluster", "query_type", "query", "rank",
            "recall_at_1", "recall_at_3", "recall_at_5", "recall_at_10",
            "reciprocal_rank", "ndcg_at_10", "precision_at_5",
            "result_count", "latency_ms", "status_code",
        ],
    )
    print(f"recall_extended: {payload['summary']}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
