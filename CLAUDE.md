# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Commands

### Rust kernel
```bash
cargo check                        # fast type-check (no DB needed)
cargo build                        # debug build
cargo build --release              # production build

# Unit tests only (no database required)
cargo test -- --skip memory::store::tests --skip rmk_worker::tests --skip archival::tests::persist_archival

# All tests including DB integration tests (requires DATABASE_URL → pgvector Postgres)
# Run `docker compose up postgres` first, then:
cargo test
```

The `#[sqlx::test]` integration tests (in `src/memory/store.rs` and `src/rmk_worker.rs`) each create an isolated test database, run all migrations, execute the test, then drop the database. They need a Postgres instance with pgvector installed and a user with `CREATEDB` privilege — the `docker compose up postgres` service satisfies this. They will panic with "DATABASE_URL must be set" if no database is available, which is expected in environments without Postgres.

### Full stack (Docker)
```bash
docker compose up --build          # build + start all services
docker compose up --build memoryos # rebuild kernel only
docker compose logs -f memoryos    # tail kernel logs
```

### Dashboard (Next.js)
```bash
cd dashboard
npm install
npm run dev        # dev server on :3000
npm run build      # production build
npm run lint       # ESLint
```

### Smoke test
```bash
OPENAI_API_KEY=sk-... python test_memory.py   # requires running kernel on :8080
```

## Architecture

### Request lifecycle

Every `POST /v1/chat/completions` goes through `src/proxy.rs`:

1. **Auth & rate limit** — extract `x-agent-id` (required), `x-session-id` (auto-generated if absent); check per-agent token bucket (`src/rate_limit.rs`)
2. **Memory retrieval** — embed the last user message; `search_memories_filtered` runs a two-CTE pgvector query; when `AMP_ENABLED` or `RMK_ENABLED`, a `RetrievalAugmenter` re-ranks results by subtracting co-access graph bonuses from the cosine distance; when `GRAPH_RETRIEVAL_ENABLED=true`, a one-hop entity graph walk further augments the set; inject results as a `<retrieved_memories>` system message at position 0; background-record pairwise co-access edges for all retrieved memories
3. **Forward upstream** — translate request format via `Provider::build_request`, add any provider-specific headers
4. **Tee response** — for OpenAI/Gemini: stream chunks to client while capturing the full body; for Anthropic: buffer entirely (Anthropic's wire format differs), then re-emit as synthetic OpenAI SSE/JSON so callers using the OpenAI SDK need zero changes
5. **Background extraction** — `tokio::spawn(extract_and_store(...))`: calls the extractor LLM to pull structured facts, batch-embeds them in one API call, stores to Postgres

### Memory tiers

| Tier | Table | Description |
|------|-------|-------------|
| L1 | `working_memory` | Per-session rolling summary + structured state (JSONB); updated every turn |
| L2 | `memories` (tier='L2') | Individual extracted facts; default tier |
| L3 | `memories` (tier='L3') | Compressed archival facts; confidence capped at 0.7 |

L1 structured state (`working_memory.state` JSONB, added Phase 4.3):
```json
{ "summary": "...", "active_entities": ["NovaPay"], "current_goal": "...", "open_questions": ["..."] }
```
When present, `build_injection` renders each field as a labelled section (`[ACTIVE_ENTITIES]`, `[CURRENT_GOAL]`, `[OPEN_QUESTIONS]`) instead of a single `[SESSION_SUMMARY]` block. The plain `summary` column is kept for backward compatibility.

Archival job (`src/archival.rs`) runs on `ARCHIVAL_INTERVAL_HOURS` schedule: compacts stale zero-access L2 facts into L3 via LLM compression, then tombstones (sets `archived_at = NOW()`) originals. All queries filter `AND archived_at IS NULL` — nothing is hard-deleted.

Each compaction run creates an **archival batch** (`archival_batches` table) that links the tombstoned L2 sources and the new L3 facts via `memories.archival_batch_id`. This enables atomic batch-level restore: `POST /api/v1/archival/batches/:batch_id/restore` un-tombstones L2 memories and re-tombstones the L3 facts that replaced them, then sets `batch.status = 'restored'`. A restored batch cannot be restored again (idempotency guard). If the embedding step fails after the batch record is created, the batch is marked `status = 'failed'` (migration 0010).

**Narrative consolidation** — alongside the 3-5 compressed semantic facts, the archival LLM call also returns a 2-3 sentence cohesive prose summary. When the narrative is non-empty it is stored as a separate L3 memory with `memory_type = 'narrative'`, tagged with the same `archival_batch_id`, and versioned in `memory_versions` (initial snapshot like any other memory). A missing or blank narrative is tolerated: facts still archive normally and `narrative_count = 0` is reported. Narrative L3 rows participate fully in retrieval, restoration, and version history. `POST /api/v1/agents/:id/archival/trigger` returns an extra `narrative_count` field (0 or 1). The `memoryos_narrative_total` Prometheus counter increments per stored narrative. All LLM work for narrative consolidation happens on the background archival path only — the retrieval hot path is unchanged.

### Decay-weighted retrieval

`search_memories_filtered` uses a three-CTE, two-stage SQL pattern:
- `q` — binds the query vector exactly once
- `ann` — index-served nearest-neighbour probe: `ORDER BY embedding <=> vec LIMIT ANN_CANDIDATE_LIMIT` (bare operator, so pgvector serves it from the HNSW index; `SET LOCAL hnsw.ef_search` is applied per retrieval transaction). Computes cosine distance + `days_stale` (days since `last_accessed_at`, or `created_at` if never accessed)
- `ranked` — three-factor adjusted distance over the ≤ K ANN candidates (exponential decay, migration 0019+):

```
adjusted_dist = cosine_dist
    × exp(MEMORY_DECAY_RATE × days_stale)
    × (1 + IMPORTANCE_BOOST_FACTOR × (1 − importance_score))
```

When `MEMORY_DECAY_RATE=0.0`, `exp(0) = 1.0`, and `IMPORTANCE_BOOST_FACTOR=0.0` (defaults) the formula collapses to pure cosine similarity.  The exponential form gives a smoother, monotonic staleness penalty vs the older linear formula.

The two-stage split keeps retrieval index-served (never wrap the `<=>` operator in arithmetic inside an ORDER BY — that silently forces a sequential scan that scales linearly with corpus size; the `retrieval_plan_uses_hnsw_index` test guards this). It is exact under the neutral defaults and exact whenever the true top-`limit` re-ranked rows lie within the top-`ANN_CANDIDATE_LIMIT` cosine rows.

The retrieval query also filters `AND archived_at IS NULL AND soft_evicted = FALSE AND status = 'active'` so quarantined, suppressed, and soft-evicted memories are invisible to retrieval.

### Importance scoring

Each memory has an `importance_score` (0.0–1.0) and `importance_source` assigned at extraction time. The score comes from one of three signals, in priority order:

1. **`user_stated`** — `x-memory-importance` request header (e.g. `x-memory-importance: 0.95`) — caller override, highest priority
2. **`agent_marked`** — `<important>…</important>` XML tags in assistant responses — score floored at 0.9
3. **`extractor`** — LLM-assigned score using a four-tier rubric:
   - 1.0 = critical/permanent (identity, goals, compliance rules)
   - 0.8–0.99 = high business value (key decisions, product names)
   - 0.5–0.79 = standard episodic detail or preference
   - 0.0–0.49 = trivial / conversational filler

**Archival protection**: memories with `importance_score >= 0.9` are excluded from the L2→L3 compaction job — they are never automatically archived regardless of age or access count.

**Refresh-on-read**: every retrieval increments `importance_score` by `IMPORTANCE_REFRESH_BOOST` (default 0.05, capped at 1.0), implementing a spacing-effect memory reinforcement signal.

### Provider abstraction

`src/providers.rs` contains the `Provider` enum (OpenAI / Anthropic / Gemini). Key methods: `completions_url`, `build_request`, `parse_buffered`, `parse_streaming`, `synthesize_json`, `synthesize_sse`. Set `UPSTREAM_PROVIDER` to select.

Anthropic-specific: `build_anthropic_body` lifts system messages to the top-level `system` field and merges consecutive same-role messages (Anthropic requires alternating roles). The response is always buffered server-side and re-emitted as OpenAI-format SSE.

Embeddings use a separate endpoint (`EMBEDDING_BASE_URL`, defaults to `UPSTREAM_BASE_URL`) so Anthropic users can point embeddings at OpenAI independently.

### Provenance and confidence

Facts extracted from conversation are tagged with provenance:
- `user_stated` — capped at 0.95 confidence
- `assistant_derived` — capped at 0.70 (may be hallucinated)
- `inferred` — capped at 0.50

The extraction prompt (`EXTRACTION_SYSTEM_PROMPT` in `src/memory/extraction.rs`) requires a `cited_line` for every fact and rejects injection-like content.

### Database

Migrations run automatically at startup via `sqlx::migrate!("./migrations")`. The kernel uses `sqlx::query` (non-macro form) throughout to avoid compile-time DB dependency — never use `sqlx::query!` macros.  Current migration range: 0001–0027.

`QueryBuilder` is used for dynamic filter composition (filters on `memory_type`, `session_id`, etc.). The vector embedding is always bound exactly once inside a CTE to avoid double-binding.

### Authentication

- **Kernel management API** (`/api/v1/*`): `X-Management-Key` or `Authorization: Bearer` header.
  - Key comparison uses **constant-time equality** (`subtle::ConstantTimeEq`) to prevent timing side-channel attacks.
  - **Startup guard**: the server refuses to start unless `MANAGEMENT_API_KEY` is set OR `ALLOW_UNAUTH_MANAGEMENT=true` is explicitly provided. This prevents accidental unauthenticated exposure when the env var is forgotten in a deployment.
  - Development shortcut: `ALLOW_UNAUTH_MANAGEMENT=true` (logs a prominent warning at startup).
- **Dashboard**: NextAuth v5 (Auth.js) with Credentials provider; JWT sessions; `session.user.agentId` is derived from email as `email.toLowerCase().replace(/[@.+]/g, "-")`; non-admins are scoped to their own `agentId`

### Key config variables

| Variable | Default | Notes |
|----------|---------|-------|
| `UPSTREAM_PROVIDER` | `openai` | `openai` \| `anthropic` \| `gemini` |
| `UPSTREAM_BASE_URL` | `https://api.openai.com` | LLM base URL |
| `EMBEDDING_BASE_URL` | `UPSTREAM_BASE_URL` | Override for embeddings only |
| `EXTRACTOR_BASE_URL` | `https://api.openai.com` | Override for extraction LLM |
| `RETRIEVAL_THRESHOLD` | `0.80` | Cosine distance upper bound |
| `MEMORY_DECAY_RATE` | `0.0` | Per-day staleness penalty; 0 = disabled |
| `IMPORTANCE_BOOST_FACTOR` | `0.0` | Importance weight in retrieval; 0 = disabled |
| `IMPORTANCE_REFRESH_BOOST` | `0.05` | Per-retrieval importance bump; 0 = disabled |
| `ANN_CANDIDATE_LIMIT` | `100` | Stage-1 ANN candidate count before decay/importance re-rank |
| `HNSW_EF_SEARCH` | `100` | `SET LOCAL hnsw.ef_search` per retrieval transaction (clamped ≥ candidate limit) |
| `LOCAL_EMBEDDING_BASE_URL` | unset | Local lane (loopback/private allowed, scoped validation) for embedding `private`/`secret` content; when unset such operations are refused |
| `RATE_LIMIT_RPM` | `0` | Per-agent request cap; 0 = disabled |
| `MANAGEMENT_API_KEY` | unset | Required unless `ALLOW_UNAUTH_MANAGEMENT=true`; protects `/api/v1/*` |
| `ALLOW_UNAUTH_MANAGEMENT` | `false` | Must be `true` when no management key is set (dev only; logs warning) |
| `MAX_BODY_BYTES` | `10485760` | Request body size limit in bytes (10 MiB default); HTTP 413 if exceeded |
| `EMBEDDING_DIMENSION` | `1536` | Must match `vector(N)` in schema |
| `GRAPH_RETRIEVAL_ENABLED` | `false` | Enable graph-walk augmentation during retrieval |
| `DB_MAX_CONNECTIONS` | `20` | PgPool max connections |
| `DB_ACQUIRE_TIMEOUT_SECS` | `5` | Seconds to wait for a pool connection before error |
| `DB_IDLE_TIMEOUT_SECS` | `300` | Seconds before idle connections are reclaimed |
| `DEDUP_THRESHOLD` | `0.05` | Cosine distance below which an insert is skipped as a near-duplicate; 0 = disabled |
| `CONFLICT_DETECTION_ENABLED` | `false` | Enable async LLM-based contradiction detection on each L2 insert |
| `AMP_ENABLED` | `false` | Enable Adaptive Memory Pressure (co-access graph bonuses, pressure scoring) |
| `RMK_ENABLED` | `false` | Enable Reflexive Memory Kernel (learned policy θ; implies AMP co-access recording) |
| `RETRIEVAL_LOG_QUERY_TEXT` | `false` | When true, store raw query text in `memory_retrieval_logs`; default stores only SHA-256 hash |

To switch embedding model to bge-small (384 dims): change `EMBEDDING_MODEL`, `EMBEDDING_DIMENSION=384`, and update `vector(1536)` → `vector(384)` in `migrations/0001_initial.sql`.

### AppState

`AppState` in `src/main.rs` is `Clone` (backed by `Arc`s) and threaded through every handler via Axum's `State` extractor:

```
AppState {
    config:       Arc<Config>,
    db:           PgPool,
    http_client:  reqwest::Client,
    metrics:      Arc<Metrics>,
    provider:     Provider,          // Copy
    rate_limiter: Arc<RateLimiter>,
}
```

### Management API summary

| Method | Path | Description |
|--------|------|-------------|
| GET | `/api/v1/agents` | List all agents |
| DELETE | `/api/v1/agents/:id` | Delete agent and all its data (cascade) |
| GET | `/api/v1/agents/:id/memories` | Paginated live memories |
| GET | `/api/v1/agents/:id/memories/at` | Time-travel snapshot at `timestamp` using latest version per memory at or before that time |
| GET | `/api/v1/agents/:id/memories/diff` | Temporal diff in `[from,to]`: added, modified, archived, status_changed, retrieval_activity |
| POST | `/api/v1/agents/:id/memories` | Create memory manually |
| PATCH | `/api/v1/memories/:id` | Update memory content (re-embeds; `private`/`secret` rows use the local embedding lane or are refused with 409) |
| POST | `/api/v1/agents/:id/timeline` | Append hash-chained cognitive-hypervisor timeline event (server computes `prev_event_digest`) |
| GET | `/api/v1/agents/:id/timeline/at` | Resolve latest snapshot at a timestamp (branch-aware via `branch_id`) |
| GET | `/api/v1/evidence/verifying-key` | Ed25519 evidence-signature verifying key (key_id + persistence) |
| GET | `/api/v1/agents/:id/memories/archived` | Tombstoned memories |
| GET | `/api/v1/agents/:id/archival/batches` | Archival batch history |
| POST | `/api/v1/agents/:id/archival/trigger` | Manually trigger L2→L3 compaction |
| GET | `/api/v1/agents/:id/sessions` | List sessions with turn counts |
| GET | `/api/v1/agents/:id/sessions/:sid` | Session detail + working memory |
| DELETE | `/api/v1/agents/:id/sessions/:sid` | Delete session working memory |
| POST | `/api/v1/memories/search` | Semantic search |
| DELETE | `/api/v1/memories/:id` | Hard-delete a memory |
| POST | `/api/v1/memories/:id/restore` | Restore individual tombstoned memory |
| POST | `/api/v1/archival/batches/:id/restore` | Restore entire batch (L2 back, L3 tombstoned) |
| POST | `/api/v1/agents/:id/memories/bulk` | Bulk archive or delete memories by filter |
| GET | `/api/v1/agents/:id/conflicts` | List unresolved (or all) memory conflicts |
| POST | `/api/v1/conflicts/:id/resolve` | Resolve a conflict (keep_a/keep_b/keep_both/dismissed) |
| GET | `/api/v1/agents/:id/export` | Export all live memories as NDJSON (no embeddings) |
| POST | `/api/v1/agents/:id/import` | Import NDJSON; re-embeds each memory, runs dedup check |
| GET | `/api/v1/stats` | Agent + memory counts |
| POST | `/api/v1/feedback` | Record retrieval-quality feedback; updates `utility_ema` |
| GET | `/api/v1/agents/:id/retrievals` | Paginated retrieval log (session_id filter available) |
| GET | `/api/v1/memories/:id/versions` | Full version history for a memory |
| PATCH | `/api/v1/memories/:id/status` | Set status (active\|candidate\|quarantined\|suppressed); creates version snapshot |
| PATCH | `/api/v1/memories/:id/sensitivity` | Set sensitivity (unknown\|normal\|private\|sensitive\|secret) |

### Temporal memory endpoints

- `GET /api/v1/agents/:id/memories/at?timestamp=<ISO>&limit=<n>&offset=<n>`
  - Reconstructs memory state at a point in time from `memory_versions`.
  - Includes memories created at/before `timestamp`.
  - Excludes memories archived at/before `timestamp`.
  - Returns paginated rows and total count.
- `GET /api/v1/agents/:id/memories/diff?from=<ISO>&to=<ISO>`
  - Compares latest version as-of `from` vs latest version as-of `to`.
  - Returns `added`, `modified` (before/after), `archived`, `status_changed`.
  - Includes retrieval activity summary from `memory_retrieval_logs` in `[from,to]`.

### Observability

Prometheus metrics at `GET /metrics` (text format 0.0.4). Key counters/histograms: `memoryos_requests_total`, `memoryos_extraction_total{status=ok|error|low_confidence}`, `memoryos_injection_total{result=hit|miss}`, `memoryos_rate_limited_total`. Grafana dashboard provisioned via `docker/grafana/provisioning/`.

### Knowledge graph (Phase 4)

`entities` and `memory_graph` tables store named entities and subject-predicate-object triples extracted by the MMU. Phase 4 additions:

- **`memory_entity_links`** (migration 0007) — join table mapping `memory_id → entity_id`. Populated by `extraction.rs` after each turn: every stored memory is linked to every entity extracted in the same turn.
- **Graph-walk retrieval** (`GRAPH_RETRIEVAL_ENABLED=true`) — `retrieve_relevant` matches entity names from the query against `entities`, walks `memory_graph` one hop, then fetches memories linked to those related entities via `memory_entity_links`. Results are merged after the primary vector set.
- **Entity disambiguation** (migration 0008, `fuzzystrmatch`) — `upsert_entity` checks `levenshtein(LOWER(name), LOWER(new)) <= 2` before inserting. Near-duplicates (e.g. "Alex" / "Alexander") are merged into the canonical (existing) entity name.

### Adaptive Memory Pressure (AMP)

`AMP_ENABLED=true` activates a closed-loop system that keeps active memory count near a configurable target and builds a co-access pheromone graph.

**Components** (`src/memory/amp/`):

| File | Role |
|------|------|
| `pressure.rs` | Stateless formula: `pressure = a·days_stale + b·(1 − utility_ema)`, clamped [0, 1] |
| `pi_controller.rs` | PI controller that drives the soft-eviction threshold |
| `co_access.rs` | `CoAccessGraph` — records pairwise co-occurrence edges in `co_access_edges`; provides `get_neighbor_weight_sum` for retrieval bonus |
| `augmenter.rs` | `RetrievalAugmenter` — subtracts `graph_bonus_weight × neighbour_weight_sum` from cosine distance to re-rank retrieved memories |
| `utility.rs` | EMA tracker for per-memory retrieval frequency |
| `config.rs` | `AmpConfig` with defaults for all sub-params |

**New DB columns** added to `memories` (migration 0014):
- `utility_ema DOUBLE PRECISION` — EMA of retrieval frequency
- `pressure DOUBLE PRECISION` — computed pressure score
- `soft_evicted BOOLEAN` — soft-eviction flag
- `soft_evicted_at TIMESTAMPTZ`

**`co_access_edges` table** (migration 0015): undirected, normalised to `(min_uuid, max_uuid)` order; `weight` accumulates on conflict, capped at `max_edge_weight`. A nightly decay pass (via `rmk_worker::run_co_access_decay_job`) multiplies all weights by `(1 − decay_per_day)` and prunes below `min_edge_weight`.

**Retrieval wiring** (`src/memory/retrieval.rs`): when AMP or RMK is enabled and the result set is non-empty, a `RetrievalAugmenter` is instantiated using per-request config clones (so global state is never mutated). When an RMK policy exists for the agent, `RmkAdapter::apply()` overrides all 6 θ dimensions before constructing the augmenter. `get_neighbor_weight_sums()` (batch, single query via UNION ALL) is called for all candidates simultaneously instead of N individual round-trips.

**Utility EMA wiring**: after every successful retrieval, `update_utility_emas()` runs in the same tokio::spawn as `bump_access_counts`, doing a single SQL UPDATE with `WHERE id = ANY($ids)`.  Feedback=1.0 ("this memory was retrieved and is therefore useful").

### Reflexive Memory Kernel (RMK)

`RMK_ENABLED=true` replaces static config thresholds with a **per-agent learned policy vector** θ = `[pressure_a, pressure_b, kp, ki, graph_bonus_weight, retrieval_threshold]`.

**Components** (`src/memory/rmk/`):

| File | Role |
|------|------|
| `policy.rs` | `PolicyParams` struct — θ dimensions with AMP-matching defaults |
| `reward.rs` | `RewardModel`: `R = task·success + 0.5·token_savings + precision@5 − 0.1·eviction_cost` |
| `meta_learner.rs` | ε-greedy exploration: perturbs each param ±10% of its bounded range with probability `epsilon` (default 0.1); stateless across cycles — hill-climb accept/reject lives in `rmk_worker` against persisted per-policy mean rewards |
| `adapter.rs` | `RmkAdapter::apply` — maps θ back to `PressureParams`, `ControllerParams`, `CoAccessParams` |
| `store.rs` | DB CRUD: `get_latest_policy` (latest *accepted*), `get_previous_accepted_policy`, `get_mean_reward_for_policy`, `mark_policy_rejected`, `insert_policy`, `insert_episode`, `count_episodes`, `list_all_agent_ids_with_episodes` |
| `config.rs` | `RmkConfig` with epsilon, buffer_size, min_episodes_before_update, update_cooldown_secs, reward_weights |

**DB tables** (migrations 0017–0018):
- `rmk_policies` — versioned θ rows per agent with `status` (accepted|rejected, migration 0026); latest **accepted** row wins; hill-climb regressions are marked rejected and stop serving
- `rmk_episodes` — per-turn metrics log (task_success, token_savings, retrieval_precision, eviction_cost, reward, policy_id FK) plus feedback-correlation columns (migration 0025): `session_id`, `injected_memory_ids`, `task_success_source` (assumed|feedback)

**Per-request flow** (`src/proxy.rs`):
1. Fetch latest policy from `rmk_policies` for the agent; use `policy.retrieval_threshold` (falls back to static `RETRIEVAL_THRESHOLD` if no policy exists yet)
2. Capture `memories_retrieved` and `rmk_policy_id` after retrieval
3. After the upstream response is dispatched, background-spawn `rmk_store::insert_episode` with computed metrics and reward

**Background worker** (`src/rmk_worker.rs`, spawned from `main.rs`):
- `run_policy_update_job` — sleeps for `update_cooldown_secs` (default 3600 s); for each agent with ≥ `min_episodes_before_update` (default 20) episodes: compares the serving policy's mean episode reward (per `rmk_episodes.policy_id`) against its predecessor's — a regression (both with ≥ 5 episodes) is marked rejected and the next candidate re-rolls from the last known-good policy; then applies `MetaLearner::suggest_explore()` and persists the candidate
- `run_task_success_aggregation_job` — every 120 s, backfills `task_success` (and recomputes reward) on recent episodes from `retrieval_feedback` rows matching the episode's `injected_memory_ids` (24 h attribution window; episodes without feedback keep the assumed 1.0)
- `run_co_access_decay_job` — runs once per day; calls `CoAccessGraph::decay_all()` to apply weight decay and prune stale edges
- `run_pressure_sweep_job` — runs every 5 minutes; for each agent: counts active memories, drives a `PIController` restored from persisted per-agent state (`amp_controller_state`, migration 0027, so integral action accumulates across sweeps), computes pressure for each memory via `PressureManager::compute_pressure()`, batch-updates the `pressure` column in one UNNEST statement, soft-evicts memories above `threshold_high`, and restores those below `threshold_low`

`run_policy_update_job` and `run_pressure_sweep_job` start when `RMK_ENABLED=true`; `run_co_access_decay_job` and `run_pressure_sweep_job` also start when `AMP_ENABLED=true` without RMK.

**Episode metrics** (proxy.rs): `token_savings` is computed from `injected_chars / (prompt_chars + injected_chars)`; `eviction_cost` queries the real fraction of soft-evicted memories for the agent.  `task_success` starts at 1.0 ("assumed"); the aggregation worker backfills a feedback-derived value for episodes whose injected memories later receive `/api/v1/feedback` (source-tagged via `task_success_source`).

### Sensitivity enforcement ("private never leaves the box")

Sensitivity is assigned post-hoc via `PATCH /api/v1/memories/:id/sensitivity`
(inserts default `'unknown'`; the extractor never assigns it), so enforcement
targets already-labeled rows re-touching the provider:
- Retrieval injection excludes `private`/`secret` (`filter_sensitive` path).
- Archival candidates exclude `private`/`secret` (they never auto-compact).
- Conflict-detection candidates exclude `private`/`secret`; the new-content
  prompt is safe by timing (content is `'unknown'` at insert-time detection).
- PATCH re-embed routes `private`/`secret` content through
  `LOCAL_EMBEDDING_BASE_URL` (scoped validation permits loopback/private for
  this variable only) or refuses with 409 when no local lane is configured
  (`embeddings::embed_text_for_sensitivity`).

### Memory status, sensitivity, and validity (migration 0019)

Every memory row has:
- `status` (TEXT, default `'active'`): `active` | `candidate` | `quarantined` | `suppressed`
- `sensitivity` (TEXT, default `'unknown'`): `unknown` | `normal` | `private` | `sensitive` | `secret`
- `valid_from` / `valid_to` (TIMESTAMPTZ, nullable): time-bounded validity window
- `suppression_reason` (TEXT, nullable): free-text reason when status = 'suppressed'
- `status_updated_at` (TIMESTAMPTZ): last status/sensitivity change time

Retrieval queries filter `AND status = 'active'` alongside the existing `AND archived_at IS NULL AND soft_evicted = FALSE`.  The partial index `idx_memories_active ON memories(agent_id, created_at DESC) WHERE archived_at IS NULL AND status = 'active'` serves the hot path.

### Memory version history (migration 0020)

`memory_versions` stores complete snapshots (not diffs) of every memory change:
- Every new memory insert creates version 1 inside a transaction in `store_memory_with_tier()`.
- Every `update_memory_content()` call creates the next version with `change_type='patch'`.
- `PATCH /api/v1/memories/:id/status` creates a version with `change_type='status_change'`.
- Migration 0020 backfills version 1 for all existing memories.

### Retrieval audit log (migration 0021)

`memory_retrieval_logs` records every `retrieve_relevant()` call:
- `query_hash`: SHA-256 of the query text (always stored, privacy-safe)
- `query_text`: raw query text (only when `RETRIEVAL_LOG_QUERY_TEXT=true`)
- `candidate_memory_ids` / `injected_memory_ids` / `suppressed_memory_ids`: UUID arrays
- `scores`: JSONB with per-memory `{cosine_dist, importance_score, confidence}`
- `latency_ms`: retrieval wall-clock time

The log insert is fire-and-forget (tokio::spawn); failures log a warning and the proxy continues normally.

### CI/CD (Phase 3)

GitHub Actions workflows in `.github/workflows/`:
- **`ci.yml`** — runs on every push/PR to `main`: Rust lint (`clippy`, `rustfmt`), unit tests (no DB), integration tests (Postgres+pgvector service container), dashboard lint (`npm run lint`).
- **`publish.yml`** — Docker publish workflow; builds and pushes the kernel image to the registry on release tags.

### MCP Server (Phase 3)

A TypeScript Model Context Protocol server lives in `mcp-server/`. It exposes AEON-IQ memory operations as MCP tools so any MCP-compatible AI assistant can read/write memories without direct HTTP calls.

```bash
cd mcp-server
npm install
npm run build
```

Key tools exposed: `retrieve_memories`, `store_memory`, `list_agents`, `get_stats`.

### Client SDKs (Phase 3)

`sdks/python/` — Python SDK wrapping the management API and proxy endpoint. Install with `pip install -e sdks/python`.

`sdks/typescript/` — TypeScript/Node SDK. Install with `npm install` inside `sdks/typescript/`.
