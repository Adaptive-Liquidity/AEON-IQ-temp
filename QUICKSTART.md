# MemoryOS Kernel — Quickstart

Give any AI agent persistent memory with **zero code changes**. Token impact is
workload-dependent; see `docs/BENCHMARKS.md` for reproducible benchmark
methodology and local proof scripts.

---

## How it works

```
Your App  →  POST /v1/chat/completions  →  MemoryOS Kernel  →  OpenAI / Any LLM
                                               ↑                      ↓
                                         inject relevant         stream back
                                           memories               response
                                               ↓
                                         [background] extract + store new facts
```

The only change to your existing code: point `OPENAI_BASE_URL` at the kernel and
add one header.

---

## Prerequisites

- Docker + Docker Compose v2
- An OpenAI API key (or any OpenAI-compatible provider)

---

## Step 1 — Clone & configure

```bash
git clone https://github.com/adaptiveliquidity/AEON-IQ.git
cd AEON-IQ
cp .env.example .env
# Edit .env and set OPENAI_API_KEY=sk-...
```

---

## Step 2 — Start everything

```bash
docker compose up --build
```

Services started:
| Service    | URL                          |
|------------|------------------------------|
| Kernel     | http://localhost:8080        |
| Dashboard  | http://localhost:3000        |
| Postgres   | localhost:5432               |

Wait ~60 seconds on first run for the Rust binary to compile inside Docker.
Subsequent starts are instant (layer-cached).

---

## Step 3 — Use it

### Option A — Drop-in OpenAI SDK replacement (Python)

```python
from openai import OpenAI

client = OpenAI(
    api_key="sk-...",                          # your real OpenAI key
    base_url="http://localhost:8080/v1",       # ← point at the kernel
    default_headers={"x-agent-id": "my-bot"}  # ← one new header
)

# Exactly the same API you already know:
response = client.chat.completions.create(
    model="gpt-4o-mini",
    messages=[{"role": "user", "content": "My name is Alex, remember that."}]
)
print(response.choices[0].message.content)
```

### Option B — LangChain

```python
from langchain_openai import ChatOpenAI

llm = ChatOpenAI(
    model="gpt-4o-mini",
    base_url="http://localhost:8080/v1",
    default_headers={"x-agent-id": "langchain-agent"},
)
```

### Option C — curl

```bash
curl http://localhost:8080/v1/chat/completions \
  -H "Authorization: Bearer $OPENAI_API_KEY" \
  -H "Content-Type: application/json"         \
  -H "x-agent-id: test-agent"                 \
  -d '{
    "model": "gpt-4o-mini",
    "messages": [{"role":"user","content":"I work at Acme Corp. Remember this."}]
  }'
```

---

## Headers

| Header                | Required | Description                                                   |
|-----------------------|----------|---------------------------------------------------------------|
| `x-agent-id`          | **Yes**  | Unique identifier for the agent / user                        |
| `x-session-id`        | No       | Groups turns into one session (auto-generated)                |
| `x-memory-importance` | No       | Override importance score 0.0–1.0 for this turn's memories   |

---

## Archival history and restore

MemoryOS automatically compacts old L2 memories into concise L3 facts. Every
compaction run is recorded as an **archival batch** so you can audit or undo it.

### List batches for an agent

```bash
curl -H "X-Management-Key: $MANAGEMENT_API_KEY" \
  http://localhost:8080/api/v1/agents/my-agent/archival/batches
```

```json
{
  "batches": [
    {
      "id": "018f1a2b-...",
      "created_at": "2026-05-17T03:00:00Z",
      "source_count": 24,
      "l3_count": 4,
      "status": "completed"
    }
  ],
  "total": 1
}
```

### Restore a batch

```bash
curl -X POST -H "X-Management-Key: $MANAGEMENT_API_KEY" \
  http://localhost:8080/api/v1/archival/batches/018f1a2b-.../restore
```

This atomically:
1. Un-tombstones the original L2 memories (makes them live again)
2. Tombstones the L3 compressed facts that replaced them
3. Sets `batch.status = "restored"` (idempotent — cannot be restored twice)

---

## Dashboard

Open **http://localhost:3000** after starting.

- **Overview** — active agents, stored memories, and heuristic token/cost counters
- **Memory Explorer** — search + delete memories per agent, manually add memories
  - **Archived** tab — browse tombstoned memories and restore individuals with one click
  - **Batch History** sub-tab — view every L2→L3 compaction run; restore entire batches atomically

---

## Run the memory-retention test

Proves the agent remembers facts from turn 3 when asked at turn 20:

```bash
# Make sure the kernel is running, then:
pip install openai
OPENAI_API_KEY=sk-... python test_memory.py
```

Expected output:
```
✅ PASS: Agent correctly remembered name (Alex) and startup (NovaPay)
```

---

## Environment variables

| Variable                    | Default                        | Description                                                   |
|-----------------------------|--------------------------------|---------------------------------------------------------------|
| `OPENAI_API_KEY`            | —                              | Used for embeddings + extraction                              |
| `UPSTREAM_BASE_URL`         | `https://api.openai.com`       | LLM provider base URL                                         |
| `EXTRACTOR_MODEL`           | `gpt-4o-mini`                  | Model used for background fact extraction                     |
| `EMBEDDING_MODEL`           | `text-embedding-3-small`       | Embedding model                                               |
| `EMBEDDING_DIMENSION`       | `1536`                         | Must match schema; change for bge-small → 384                 |
| `DATABASE_URL`              | *(set by docker-compose)*      | PostgreSQL connection string                                  |
| `PORT`                      | `8080`                         | Kernel listen port                                            |
| `RUST_LOG`                  | `memoryos_kernel=info`         | Log verbosity                                                 |
| `MANAGEMENT_API_KEY`        | *(unset)*                      | Required to protect `/api/v1/*`; server won't start without it unless `ALLOW_UNAUTH_MANAGEMENT=true` |
| `ALLOW_UNAUTH_MANAGEMENT`   | `false`                        | Set `true` for local dev when no management key is configured |
| `MAX_BODY_BYTES`            | `10485760`                     | Max request body in bytes (10 MiB); returns HTTP 413 if exceeded |
| `AMP_ENABLED`               | `false`                        | Enable co-access graph bonuses and pressure scoring           |
| `RMK_ENABLED`               | `false`                        | Enable self-tuning policy θ (implies AMP co-access recording) |

---

## Switching providers

```bash
# Use Anthropic (via a claude-openai-compat adapter like LiteLLM)
UPSTREAM_BASE_URL=http://localhost:4000  # LiteLLM proxy

# Use local Ollama
UPSTREAM_BASE_URL=http://host.docker.internal:11434
```

---

## Architecture overview

```
┌──────────────────────────────────────────────────────────┐
│                    MemoryOS Kernel                        │
│                                                          │
│  POST /v1/chat/completions                               │
│       │                                                  │
│       ▼                                                  │
│  [1] Extract x-agent-id, x-session-id                   │
│       │                                                  │
│       ▼                                                  │
│  [2] Embed user message → pgvector similarity search    │
│      Retrieve top-5 relevant L2 memories                 │
│      Fetch L1 session summary                            │
│       │                                                  │
│       ▼                                                  │
│  [3] Inject memory context as system message            │
│       │                                                  │
│       ▼                                                  │
│  [4] Forward to upstream LLM (preserving stream:true)   │
│       │                                                  │
│       ▼                                                  │
│  [5] Tee response: stream to client + capture           │
│                                      │                   │
│                              [background]                │
│                                      ▼                   │
│                         [6] MMU extraction (gpt-4o-mini)│
│                              → facts → embeddings        │
│                              → entities + relations      │
│                              → update L1 summary         │
└──────────────────────────────────────────────────────────┘
```

---

## Production deployment

1. Replace `memoryos_secret` in docker-compose with a strong password
2. Set `MANAGEMENT_API_KEY=<strong-secret>` — the server **refuses to start** without it (unless `ALLOW_UNAUTH_MANAGEMENT=true` is set)
3. Put Nginx or Caddy in front of port 8080 (TLS termination)
4. Set `RUST_LOG=memoryos_kernel=warn` to reduce log volume
5. Add Postgres replicas / backups for the `postgres_data` volume
6. The kernel is stateless — scale horizontally behind a load balancer
