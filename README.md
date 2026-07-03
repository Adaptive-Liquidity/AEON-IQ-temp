# AEON-IQ / MemoryOS Kernel

> **Give any AI agent persistent memory with zero code changes.**

[![CI](https://github.com/adaptiveliquidity/AEON-IQ/actions/workflows/ci.yml/badge.svg)](https://github.com/adaptiveliquidity/AEON-IQ/actions/workflows/ci.yml)
[![Benchmark Proof](https://github.com/adaptiveliquidity/AEON-IQ/actions/workflows/benchmarks.yml/badge.svg)](https://github.com/adaptiveliquidity/AEON-IQ/actions/workflows/benchmarks.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)
[![Docker](https://img.shields.io/badge/docker-ghcr.io%2Fadaptiveliquidity%2Faeon--iq-blue)](https://github.com/adaptiveliquidity/AEON-IQ/pkgs/container/aeon-iq)

MemoryOS Kernel is a **transparent OpenAI-compatible proxy** that sits between your agent and any LLM. It automatically retrieves relevant past facts on every request and extracts new facts from every response — all in the background, with no changes to your existing code.

```
Your App  ──→  POST /v1/chat/completions  ──→  MemoryOS Kernel  ──→  OpenAI / Anthropic / Ollama
                                                      │                        │
                                               inject memories          stream response
                                               measured in benchmarks         │
                                                      └──── [background] extract + store facts
```

## Validation status

- `proof_status`: `pass`
- Workflow: `Benchmark Proof`
- Artifact pattern: `aeon-iq-benchmark-proof-<commit>`
- Evidence: [v0.1.0 validation](docs/VALIDATION_v0.1.0.md) and [benchmark methodology](docs/BENCHMARKS.md)

## 30-second quickstart

```bash
git clone https://github.com/adaptiveliquidity/AEON-IQ.git
cd AEON-IQ
cp .env.example .env          # add your OPENAI_API_KEY
docker compose up --build     # ~60 s first build, instant after
```

Then point any OpenAI SDK client at the kernel:

```python
from openai import OpenAI

client = OpenAI(
    api_key="sk-...",                          # your real key — passed through unchanged
    base_url="http://localhost:8080/v1",       # ← one env var change
    default_headers={"x-agent-id": "my-bot"}  # ← one new header
)

# Everything else is exactly the same OpenAI API you already know.
response = client.chat.completions.create(
    model="gpt-4o-mini",
    messages=[{"role": "user", "content": "My name is Alex. I work at NovaPay."}]
)
# Later, in a new session...
response = client.chat.completions.create(
    model="gpt-4o-mini",
    messages=[{"role": "user", "content": "What's my name?"}]
)
# → "Your name is Alex, and you work at NovaPay."
```

| Service   | URL                    |
|-----------|------------------------|
| Kernel    | http://localhost:8080  |
| Dashboard | http://localhost:3000  |
| Postgres  | localhost:5432         |

Security note: `.env.example` enables `ALLOW_UNAUTH_MANAGEMENT=true` only for local development. For production, set `MANAGEMENT_API_KEY` and `ALLOW_UNAUTH_MANAGEMENT=false`.

---

## Benchmarks

AEON-IQ includes a reproducible benchmark/proof suite for proxy latency,
retrieval latency, estimated token reduction, recall quality, temporal memory
correctness, and narrative archival correctness. See
[docs/BENCHMARKS.md](docs/BENCHMARKS.md) for methodology, current claim
support, known limitations, and rerun instructions.

---

## How it works

Every `POST /v1/chat/completions` passes through five stages:

```
┌────────────────────────────────────────────────────────────────┐
│                       MemoryOS Kernel                          │
│                                                                │
│  [1] Auth & rate-limit ── extract x-agent-id / x-session-id   │
│             │                                                  │
│             ▼                                                  │
│  [2] Retrieve ── embed user message → pgvector similarity      │
│        search → co-access bonus re-ranking → inject top        │
│        memories as <retrieved_memories> system message         │
│             │                                                  │
│             ▼                                                  │
│  [3] Translate & forward ── adapt to upstream provider format  │
│             │                                                  │
│             ▼                                                  │
│  [4] Tee response ── stream chunks to client while capturing   │
│             │                                                  │
│             ▼ (background, non-blocking)                       │
│  [5] Extract & store ── LLM extracts structured facts,         │
│        batch-embeds them, stores to Postgres + updates L1      │
│        session summary + records co-access edges               │
└────────────────────────────────────────────────────────────────┘
```

### Memory tiers

| Tier | Storage | Description |
|------|---------|-------------|
| **L1** | `working_memory` | Per-session rolling summary; updated every turn |
| **L2** | `memories` (tier='L2') | Individual extracted facts; default tier |
| **L3** | `memories` (tier='L3') | Compressed archival facts and narrative summaries; created by the LTM job |

The **LTM archival job** automatically compacts stale L2 facts into concise L3 summaries on a configurable schedule. Each compaction also produces a 2-3 sentence cohesive **narrative memory** (`memory_type = 'narrative'`) that captures the throughline of the archived material. Every compaction run is versioned and reversible — you can restore any batch with a single API call, which automatically un-tombstones the source L2 memories and re-tombstones both the compressed facts and their narrative.

### Retrieval formula

```
adjusted_dist = cosine_dist
    × exp(MEMORY_DECAY_RATE × days_stale)
    × (1 + IMPORTANCE_BOOST_FACTOR × (1 − importance_score))
```

When AMP/RMK is enabled, a post-query re-rank additionally subtracts a capped
co-access bonus: `adjusted_dist − min(graph_bonus_weight × co_access_neighbour_weight, max_bonus)`,
floored at 0.

When all factors are 0 or disabled (defaults), this collapses to pure cosine similarity. The **co-access bonus** promotes memories that frequently appear alongside other retrieved memories, building a pheromone-style graph of related facts over time.

### Importance scoring

Memories get an importance score from the highest-priority available signal:

1. **Caller header** — `x-memory-importance: 0.95` (operator override)
2. **Agent XML tags** — `<important>…</important>` in assistant responses (floored at 0.9)
3. **LLM extractor** — four-tier rubric: 1.0 = critical, 0.8–0.99 = high, 0.5–0.79 = standard, 0–0.49 = trivial

Memories with `importance_score ≥ 0.9` are **never automatically archived** regardless of age or access count.

---

## Adaptive Memory & Self-tuning

### Adaptive Memory Pressure (AMP)

Enable with `AMP_ENABLED=true`. AMP builds a **co-access pheromone graph** — every time two memories are retrieved together, an edge between them grows stronger. This graph drives the retrieval bonus described above, naturally surfacing related facts without any manual curation.

Additional AMP capabilities (configurable, off by default):
- **Pressure scoring** — `pressure = a·days_stale + b·(1 − utility_ema)` identifies stale, rarely-accessed memories
- **Soft eviction** — a PI controller gradually raises the eviction threshold when memory count exceeds a target
- **Co-access decay** — edge weights decay daily so recent co-occurrences matter more than old ones

### Reflexive Memory Kernel (RMK)

Enable with `RMK_ENABLED=true`. RMK replaces static retrieval thresholds with a **learned policy vector** θ = [pressure_a, pressure_b, kp, ki, graph_bonus_weight, retrieval_threshold] that self-tunes from episode rewards.

How it works:
1. At the start of each request, the agent's latest policy is read from the database and applied (retrieval threshold, AMP coefficients)
2. After each response, episode metrics are logged (retrieval precision, token savings proxy, reward)
3. A background worker runs hourly; for agents with enough episodes it applies **ε-greedy exploration** (±10% perturbation with probability ε=0.1) and persists a new policy

Reward function: `R = task_success + 0.5·token_savings + precision@5 − 0.1·eviction_cost`

The first policy for each agent is seeded from the static AMP defaults — enabling RMK without prior training is equivalent to running with static AMP.

---

## Integration examples

### Drop-in OpenAI SDK (Python)

```python
from openai import OpenAI

client = OpenAI(
    api_key="sk-...",
    base_url="http://localhost:8080/v1",
    default_headers={"x-agent-id": "my-bot"}
)
```

### LangChain

```python
from langchain_openai import ChatOpenAI

llm = ChatOpenAI(
    model="gpt-4o-mini",
    base_url="http://localhost:8080/v1",
    default_headers={"x-agent-id": "langchain-agent"},
)
```

### CrewAI / AutoGen

Both frameworks use the OpenAI SDK under the hood. Set one environment variable:

```bash
export OPENAI_BASE_URL=http://localhost:8080/v1
# Add x-agent-id to the default_headers on the underlying OpenAI client
```

### Anthropic (via kernel translation)

Use the OpenAI SDK — the kernel translates the wire format automatically:

```bash
UPSTREAM_PROVIDER=anthropic
UPSTREAM_BASE_URL=https://api.anthropic.com
OPENAI_API_KEY=sk-ant-...   # your Anthropic key
```

### Local models (Ollama / LM Studio)

```bash
UPSTREAM_BASE_URL=http://host.docker.internal:11434
UPSTREAM_PROVIDER=openai
EMBEDDING_BASE_URL=https://api.openai.com   # keep OpenAI for embeddings
```

### Signal important facts

```python
# Override importance for a turn (0.0–1.0):
client.chat.completions.create(
    model="gpt-4o-mini",
    messages=[...],
    extra_headers={"x-memory-importance": "0.95"}
)
```

Or wrap critical content in assistant responses:

```
<important>User's compliance requirement: must use AES-256.</important>
```

---

## Request headers

| Header | Required | Description |
|--------|----------|-------------|
| `x-agent-id` | **Yes** | Unique identifier for the agent / user |
| `x-session-id` | No | Groups turns into one session (UUID auto-generated if absent) |
| `x-memory-importance` | No | Override importance score 0.0–1.0 for this turn's memories |

---

## Configuration reference

### Core

| Variable | Default | Description |
|----------|---------|-------------|
| `DATABASE_URL` | *(required)* | PostgreSQL connection string |
| `OPENAI_API_KEY` | — | API key forwarded to upstream LLM; used for embeddings and extraction |
| `PORT` | `8080` | Kernel listen port |
| `UPSTREAM_PROVIDER` | `openai` | Provider: `openai` \| `anthropic` \| `gemini` |
| `UPSTREAM_BASE_URL` | `https://api.openai.com` | LLM provider base URL |
| `EMBEDDING_BASE_URL` | `UPSTREAM_BASE_URL` | Override for embedding calls only |
| `EXTRACTOR_BASE_URL` | `https://api.openai.com` | Override for fact-extraction LLM |
| `EXTRACTOR_MODEL` | `gpt-4o-mini` | Model used for background fact extraction |
| `EMBEDDING_MODEL` | `text-embedding-3-small` | Embedding model |
| `EMBEDDING_DIMENSION` | `1536` | Must match `vector(N)` in schema |

### Retrieval

| Variable | Default | Description |
|----------|---------|-------------|
| `RETRIEVAL_THRESHOLD` | `0.80` | Cosine distance upper bound (lower = stricter) |
| `MEMORY_DECAY_RATE` | `0.0` | Per-day staleness penalty; 0 = disabled |
| `IMPORTANCE_BOOST_FACTOR` | `0.0` | Importance weight in retrieval; 0 = disabled |
| `IMPORTANCE_REFRESH_BOOST` | `0.05` | Per-retrieval importance bump; 0 = disabled |
| `GRAPH_RETRIEVAL_ENABLED` | `false` | Entity graph-walk augmentation during retrieval |
| `DEDUP_THRESHOLD` | `0.05` | Cosine distance below which a new insert is skipped as duplicate; 0 = disabled |
| `CONFLICT_DETECTION_ENABLED` | `false` | Async LLM-based contradiction detection on each L2 insert |

### Security & limits

| Variable | Default | Description |
|----------|---------|-------------|
| `MANAGEMENT_API_KEY` | *(unset)* | Protects `/api/v1/*`; must be set or `ALLOW_UNAUTH_MANAGEMENT=true` required |
| `ALLOW_UNAUTH_MANAGEMENT` | `false` | Set `true` to allow unauthenticated management access (dev only; logs warning) |
| `MAX_BODY_BYTES` | `10485760` | Max request body size in bytes (10 MiB); returns HTTP 413 if exceeded |
| `RATE_LIMIT_RPM` | `0` | Per-agent requests per minute cap; 0 = disabled |
| `RATE_LIMIT_BURST` | `20` | Token bucket burst size |

### Database pool

| Variable | Default | Description |
|----------|---------|-------------|
| `DB_MAX_CONNECTIONS` | `20` | PgPool max connections |
| `DB_ACQUIRE_TIMEOUT_SECS` | `5` | Seconds to wait for a connection before error |
| `DB_IDLE_TIMEOUT_SECS` | `300` | Seconds before idle connections are reclaimed |

### LTM archival

| Variable | Default | Description |
|----------|---------|-------------|
| `ARCHIVAL_INTERVAL_HOURS` | `24` | LTM compaction job frequency; 0 = disabled |
| `ARCHIVAL_MIN_AGE_DAYS` | `7` | Minimum age before an L2 memory is a compaction candidate |
| `ARCHIVAL_MIN_MEMORIES` | `10` | Minimum candidate count before triggering compaction |

### Adaptive Memory Pressure (AMP)

| Variable | Default | Description |
|----------|---------|-------------|
| `AMP_ENABLED` | `false` | Enable AMP: co-access graph bonuses, pressure scoring, soft eviction |

### Reflexive Memory Kernel (RMK)

| Variable | Default | Description |
|----------|---------|-------------|
| `RMK_ENABLED` | `false` | Enable RMK: learned policy θ for adaptive thresholds and AMP coefficients |

---

## Management API

Authenticate with `X-Management-Key: <key>` or `Authorization: Bearer <key>` (set `MANAGEMENT_API_KEY`).

| Method | Path | Description |
|--------|------|-------------|
| GET | `/api/v1/agents` | List all agents |
| DELETE | `/api/v1/agents/:id` | Delete agent and all its data (cascade) |
| GET | `/api/v1/agents/:id/memories` | Paginated live memories |
| GET | `/api/v1/agents/:id/memories/at` | Time-travel snapshot at `timestamp` using latest memory versions as-of that time |
| GET | `/api/v1/agents/:id/memories/diff` | Temporal diff between `from` and `to`: added/modified/archived/status_changed/retrieval_activity |
| POST | `/api/v1/agents/:id/memories` | Create memory manually |
| POST | `/api/v1/agents/:id/memories/bulk` | Bulk archive or delete memories by filter |
| GET | `/api/v1/agents/:id/memories/archived` | Tombstoned memories |
| GET | `/api/v1/agents/:id/archival/batches` | Archival batch history |
| POST | `/api/v1/agents/:id/archival/trigger` | Manually trigger L2→L3 compaction |
| GET | `/api/v1/agents/:id/sessions` | List sessions with turn counts |
| GET | `/api/v1/agents/:id/sessions/:sid` | Session detail + working memory |
| DELETE | `/api/v1/agents/:id/sessions/:sid` | Delete session working memory |
| GET | `/api/v1/agents/:id/conflicts` | List unresolved (or all) memory conflicts |
| GET | `/api/v1/agents/:id/export` | Export all live memories as NDJSON |
| POST | `/api/v1/agents/:id/import` | Import NDJSON; re-embeds and deduplicates |
| POST | `/api/v1/memories/search` | Semantic search across all memories |
| PATCH | `/api/v1/memories/:id` | Update memory content (re-embeds) |
| DELETE | `/api/v1/memories/:id` | Hard-delete a memory |
| POST | `/api/v1/memories/:id/restore` | Restore a tombstoned memory |
| POST | `/api/v1/archival/batches/:id/restore` | Restore entire archival batch (L2 back, L3 tombstoned) |
| POST | `/api/v1/conflicts/:id/resolve` | Resolve conflict: keep_a \| keep_b \| keep_both \| dismissed |
| GET | `/api/v1/stats` | Agent + memory counts |

### Example: inspect memories

```bash
curl -H "X-Management-Key: $MANAGEMENT_API_KEY" \
  http://localhost:8080/api/v1/agents/my-bot/memories
```

### Example: semantic search

```bash
curl -X POST \
  -H "X-Management-Key: $MANAGEMENT_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{"query": "user preferences", "agent_id": "my-bot", "limit": 5}' \
  http://localhost:8080/api/v1/memories/search
```

### Example: time-travel snapshot

```bash
curl -H "X-Management-Key: $MANAGEMENT_API_KEY" \
  "http://localhost:8080/api/v1/agents/my-bot/memories/at?timestamp=2026-06-01T00:00:00Z&limit=20&offset=0"
```

### Example: memory diff

```bash
curl -H "X-Management-Key: $MANAGEMENT_API_KEY" \
  "http://localhost:8080/api/v1/agents/my-bot/memories/diff?from=2026-05-01T00:00:00Z&to=2026-06-02T00:00:00Z"
```

---

## Dashboard

Open **http://localhost:3000** after `docker compose up`.

- **Overview** — agent count, total memories, injection hit rate, and heuristic token counters
- **Memory Explorer** — browse, search, manually add, and delete memories per agent
  - **Archived** tab — view tombstoned memories; restore individuals with one click
  - **Batch History** — every L2→L3 compaction run with full restore support
- **Knowledge Graph** — entity/relation graph extracted from conversations
- **Cognition** — retrieval timeline, temporal snapshots, memory diffs, and recall debugging; a full memory lifecycle timeline remains future work
- **Metrics** — Prometheus-backed usage charts (requests, extraction rate, retrieval hits)

Default credentials: `admin@memoryos.dev` / `changeme` (set `DASHBOARD_ADMIN_EMAIL` and `DASHBOARD_ADMIN_PASSWORD`).

---

## Observability

Prometheus metrics at `GET /metrics` (text format 0.0.4):

| Metric | Type | Description |
|--------|------|-------------|
| `memoryos_requests_total` | Counter | Total proxy requests |
| `memoryos_extraction_total{status}` | Counter | Extraction outcomes: ok \| error \| low_confidence |
| `memoryos_injection_total{result}` | Counter | Retrieval: hit \| miss |
| `memoryos_rate_limited_total` | Counter | Requests blocked by rate limiter |

Grafana dashboard provisioned automatically at **http://localhost:3001**.

---

## Production notes

1. Replace `memoryos_secret` with a strong Postgres password
2. Set `MANAGEMENT_API_KEY` to a secret value — **the server will not start without it** unless `ALLOW_UNAUTH_MANAGEMENT=true` is also set
3. Put Nginx/Caddy in front of port 8080 (TLS termination)
4. Set `RUST_LOG=memoryos_kernel=warn` to reduce log volume
5. Add Postgres replicas/backups for the `postgres_data` volume
6. The kernel is stateless — scale horizontally behind a load balancer
7. Tune `MAX_BODY_BYTES` if your agents send very large context windows (default 10 MiB)

---

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for contributor workflow and [CLAUDE.md](CLAUDE.md) for architecture overview and design decisions.

```bash
# Fast type-check (no DB needed)
cargo check

# Unit tests (no DB needed)
cargo test -- --skip memory::store::tests

# Full tests (requires pgvector Postgres)
docker compose up -d postgres
cargo test

# Dashboard
cd dashboard && npm install && npm run dev
```

Rust edition 2021, sqlx 0.9 (non-macro form — no compile-time DB dependency). All PRs run the full CI matrix before merge.

---

## License

MIT — see [LICENSE](LICENSE).
