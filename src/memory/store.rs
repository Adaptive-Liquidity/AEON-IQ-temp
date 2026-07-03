use anyhow::Result;
use chrono::{DateTime, Utc};
use pgvector::Vector;
use sqlx::QueryBuilder;
use uuid::Uuid;

use crate::{
    models::{ArchivalBatch, Memory, MemorySearchRow, WorkingMemory},
    AppState,
};

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct TemporalMemoryVersion {
    pub memory_id: Uuid,
    pub version_number: i32,
    pub content: String,
    pub memory_type: String,
    pub confidence: f32,
    pub provenance: String,
    pub importance_score: f32,
    pub importance_source: String,
    pub status: String,
    pub sensitivity: String,
    pub valid_from: Option<DateTime<Utc>>,
    pub valid_to: Option<DateTime<Utc>>,
    pub source_turn: Option<i32>,
    pub memory_created_at: DateTime<Utc>,
    pub version_created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ArchivedBetweenRow {
    pub id: Uuid,
    pub content: String,
    pub memory_type: String,
    pub archived_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct SnapshotResolution {
    pub snapshot_id: Uuid,
    pub occurred_at: DateTime<Utc>,
    pub event_type: String,
    pub branch_id: Option<String>,
    pub prev_event_digest: Option<String>,
}

type MemoryVersionMeta = (
    String,
    String,
    f32,
    String,
    f32,
    String,
    String,
    String,
    Option<i32>,
);

// ── Agent ─────────────────────────────────────────────────────────────────────

pub async fn upsert_agent(state: &AppState, agent_id: &str) -> Result<()> {
    sqlx::query("INSERT INTO agents (agent_id) VALUES ($1) ON CONFLICT (agent_id) DO NOTHING")
        .bind(agent_id)
        .execute(&state.db)
        .await?;
    Ok(())
}

pub async fn count_agents(state: &AppState) -> Result<i64> {
    let row: (i64,) = sqlx::query_as("SELECT COUNT(*)::bigint FROM agents")
        .fetch_one(&state.db)
        .await?;
    Ok(row.0)
}

/// Delete an agent and all associated data in a single transaction.
///
/// Returns `true` if the agent existed and was deleted, `false` if not found.
pub async fn delete_agent(state: &AppState, agent_id: &str) -> Result<bool> {
    let mut tx = state.db.begin().await?;

    // memories must be deleted before archival_batches (FK: memories.archival_batch_id)
    sqlx::query("DELETE FROM memories WHERE agent_id = $1")
        .bind(agent_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM working_memory WHERE agent_id = $1")
        .bind(agent_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM entities WHERE agent_id = $1")
        .bind(agent_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM memory_graph WHERE agent_id = $1")
        .bind(agent_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM audit_logs WHERE agent_id = $1")
        .bind(agent_id)
        .execute(&mut *tx)
        .await?;
    // sessions and archival_batches cascade from agents; delete agent row last
    let result = sqlx::query("DELETE FROM agents WHERE agent_id = $1")
        .bind(agent_id)
        .execute(&mut *tx)
        .await?;

    tx.commit().await?;
    Ok(result.rows_affected() > 0)
}

// ── Memories ──────────────────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
pub async fn store_memory(
    state: &AppState,
    agent_id: &str,
    session_id: Option<&str>,
    content: &str,
    memory_type: &str,
    confidence: f32,
    embedding: Vec<f32>,
    source_turn: Option<i32>,
    provenance: &str,
    importance_score: f32,
    importance_source: &str,
) -> Result<Uuid> {
    // ── Dedup check ───────────────────────────────────────────────────────────
    // Bind the vector once in a CTE to avoid double-scan.  If the nearest
    // existing live memory is closer than the threshold, skip the insert and
    // return the existing ID (bumping its confidence if the new value is higher).
    if state.config.dedup_threshold > 0.0 {
        let vec = Vector::from(embedding.clone());
        let mut qb: QueryBuilder<sqlx::Postgres> = QueryBuilder::new("WITH q AS (SELECT ");
        qb.push_bind(vec);
        qb.push(
            " AS vec) \
             SELECT id, (embedding <=> vec)::double precision AS distance \
             FROM memories CROSS JOIN q \
             WHERE agent_id = ",
        );
        qb.push_bind(agent_id);
        qb.push(" AND archived_at IS NULL ORDER BY embedding <=> vec LIMIT 1");

        if let Some((existing_id, distance)) = qb
            .build_query_as::<(Uuid, f64)>()
            .fetch_optional(&state.db)
            .await?
        {
            if distance < state.config.dedup_threshold {
                tracing::debug!(
                    agent_id, %existing_id, distance,
                    "dedup: skipping near-duplicate memory"
                );
                state.metrics.dedup_skipped_total.inc();
                // Raise confidence if the repeated fact is stated with higher certainty.
                sqlx::query(
                    "UPDATE memories \
                     SET access_count = access_count + 1, \
                         last_accessed_at = NOW(), \
                         confidence = GREATEST(confidence, $2) \
                     WHERE id = $1",
                )
                .bind(existing_id)
                .bind(confidence)
                .execute(&state.db)
                .await?;
                return Ok(existing_id);
            }
        }
    }

    let id = store_memory_with_tier(
        state,
        agent_id,
        session_id,
        content,
        memory_type,
        confidence,
        embedding.clone(),
        source_turn,
        "L2",
        provenance,
        importance_score,
        importance_source,
        None,
    )
    .await?;

    // Spawn async conflict detection (only when enabled; never blocks the caller).
    if state.config.conflict_detection_enabled {
        let s = state.clone();
        let aid = agent_id.to_string();
        let c = content.to_string();
        let emb = embedding;
        tokio::spawn(async move {
            crate::memory::conflicts::detect_and_store(&s, &aid, id, &c, &emb).await;
        });
    }

    Ok(id)
}

/// Insert a version snapshot for a memory.
///
/// `change_type` should be one of: "initial", "patch", "status_change".
/// Called inside transactions in store_memory_with_tier and update_memory_content.
struct MemoryVersionInput<'a> {
    memory_id: Uuid,
    agent_id: &'a str,
    content: &'a str,
    memory_type: &'a str,
    confidence: f32,
    provenance: &'a str,
    importance_score: f32,
    importance_source: &'a str,
    status: &'a str,
    sensitivity: &'a str,
    source_turn: Option<i32>,
    change_type: &'a str,
    change_reason: Option<&'a str>,
}

async fn create_memory_version(pool: &sqlx::PgPool, input: MemoryVersionInput<'_>) -> Result<()> {
    // Compute next version number atomically.
    let next_ver: i32 = sqlx::query_scalar(
        "SELECT COALESCE(MAX(version_number), 0) + 1
         FROM memory_versions WHERE memory_id = $1",
    )
    .bind(input.memory_id)
    .fetch_one(pool)
    .await?;

    sqlx::query(
        r#"
        INSERT INTO memory_versions
            (memory_id, agent_id, version_number, content, memory_type, confidence,
             provenance, importance_score, importance_source, status, sensitivity,
             source_turn, change_type, change_reason, changed_by)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, 'system')
        "#,
    )
    .bind(input.memory_id)
    .bind(input.agent_id)
    .bind(next_ver)
    .bind(input.content)
    .bind(input.memory_type)
    .bind(input.confidence)
    .bind(input.provenance)
    .bind(input.importance_score)
    .bind(input.importance_source)
    .bind(input.status)
    .bind(input.sensitivity)
    .bind(input.source_turn)
    .bind(input.change_type)
    .bind(input.change_reason)
    .execute(pool)
    .await?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub async fn store_memory_with_tier(
    state: &AppState,
    agent_id: &str,
    session_id: Option<&str>,
    content: &str,
    memory_type: &str,
    confidence: f32,
    embedding: Vec<f32>,
    source_turn: Option<i32>,
    tier: &str,
    provenance: &str,
    importance_score: f32,
    importance_source: &str,
    archival_batch_id: Option<Uuid>,
) -> Result<Uuid> {
    let vec = Vector::from(embedding);

    // Wrap insert + version creation in a transaction so version 1 is always
    // consistent with the memory row.
    let mut tx = state.db.begin().await?;

    let row: (Uuid,) = sqlx::query_as(
        r#"
        INSERT INTO memories
            (agent_id, session_id, content, memory_type, confidence, embedding,
             source_turn, tier, provenance, importance_score, importance_source,
             archival_batch_id)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)
        RETURNING id
        "#,
    )
    .bind(agent_id)
    .bind(session_id)
    .bind(content)
    .bind(memory_type)
    .bind(confidence)
    .bind(vec)
    .bind(source_turn)
    .bind(tier)
    .bind(provenance)
    .bind(importance_score)
    .bind(importance_source)
    .bind(archival_batch_id)
    .fetch_one(&mut *tx)
    .await?;

    let memory_id = row.0;

    // Version 1: initial snapshot.
    sqlx::query(
        r#"
        INSERT INTO memory_versions
            (memory_id, agent_id, version_number, content, memory_type, confidence,
             provenance, importance_score, importance_source, status, sensitivity,
             source_turn, change_type, change_reason, changed_by)
        VALUES ($1, $2, 1, $3, $4, $5, $6, $7, $8, 'active', 'unknown',
                $9, 'initial', NULL, 'system')
        "#,
    )
    .bind(memory_id)
    .bind(agent_id)
    .bind(content)
    .bind(memory_type)
    .bind(confidence)
    .bind(provenance)
    .bind(importance_score)
    .bind(importance_source)
    .bind(source_turn)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(memory_id)
}

/// Basic cosine-similarity search — used internally by the proxy.
/// Uses a CTE so the embedding vector is only bound once.
#[allow(dead_code)]
pub async fn search_memories(
    state: &AppState,
    agent_id: &str,
    embedding: &[f32],
    limit: i64,
) -> Result<Vec<MemorySearchRow>> {
    search_memories_filtered(state, agent_id, embedding, limit, 1.0, None, None).await
}

/// Proxy-facing search: excludes private/secret memories so they are never
/// injected into external LLM calls.  All proxy and internal callers use this.
pub async fn search_memories_filtered(
    state: &AppState,
    agent_id: &str,
    embedding: &[f32],
    limit: i64,
    threshold: f64,
    memory_type: Option<&str>,
    session_id: Option<&str>,
) -> Result<Vec<MemorySearchRow>> {
    search_memories_impl(
        state,
        agent_id,
        embedding,
        limit,
        threshold,
        memory_type,
        session_id,
        true,
    )
    .await
}

/// Management-API-facing search: returns all sensitivities including
/// private/secret so authenticated admins can inspect every stored memory.
pub async fn search_memories_all_sensitivities(
    state: &AppState,
    agent_id: &str,
    embedding: &[f32],
    limit: i64,
    threshold: f64,
    memory_type: Option<&str>,
    session_id: Option<&str>,
) -> Result<Vec<MemorySearchRow>> {
    search_memories_impl(
        state,
        agent_id,
        embedding,
        limit,
        threshold,
        memory_type,
        session_id,
        false,
    )
    .await
}

/// Shared query implementation.  When `filter_sensitive` is true, rows with
/// sensitivity = 'private' or 'secret' are excluded (LLM-injection path).
/// When false, all sensitivities are returned (management API path).
#[allow(clippy::too_many_arguments)]
async fn search_memories_impl(
    state: &AppState,
    agent_id: &str,
    embedding: &[f32],
    limit: i64,
    threshold: f64,
    memory_type: Option<&str>,
    session_id: Option<&str>,
    filter_sensitive: bool,
) -> Result<Vec<MemorySearchRow>> {
    let vec = Vector::from(embedding.to_vec());
    let decay_rate = state.config.memory_decay_rate;
    let importance_boost = state.config.importance_boost_factor;

    // Stage-1 candidate count: at least the caller's limit, normally the
    // configured ANN candidate budget.
    let ann_limit = state.config.ann_candidate_limit.max(limit);
    // ef_search must cover the candidate budget or pgvector falls into its
    // slow post-ef "overflow" scan; also never go below pgvector's default.
    let ef_search = (state.config.hnsw_ef_search as i64).max(ann_limit).max(40) as u32;

    let qb = build_search_query(
        "",
        vec,
        agent_id,
        limit,
        threshold,
        ann_limit,
        decay_rate,
        importance_boost,
        memory_type,
        session_id,
        filter_sensitive,
    );

    // set_config(..., true) has SET LOCAL semantics (scoped to this
    // transaction) while accepting a bind parameter, unlike bare SET.
    let mut tx = state.db.begin().await?;
    sqlx::query("SELECT set_config('hnsw.ef_search', $1, true)")
        .bind(ef_search.to_string())
        .execute(&mut *tx)
        .await?;
    let mut qb = qb;
    let rows = qb
        .build_query_as::<MemorySearchRow>()
        .fetch_all(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(rows)
}

/// Builds the two-stage retrieval query.  Split out from
/// `search_memories_impl` so the EXPLAIN regression test can verify the plan
/// of the exact SQL the hot path executes (`prefix` is empty in production,
/// `"EXPLAIN (ANALYZE, FORMAT JSON) "` in the test).
///
/// Three-CTE, two-stage pattern:
///   q      — binds the query vector exactly once
///   ann    — index-served nearest-neighbour probe: the ORDER BY uses the
///            bare `embedding <=> vec` operator with a LIMIT so pgvector
///            can serve it from the HNSW index (wrapping the operator in
///            the decay arithmetic forces a sequential scan — the ranking
///            formula must therefore only ever touch the ann CTE's output)
///   ranked — decay + importance re-rank over the ≤ ann_limit candidates
/// With decay_rate = importance_boost = 0 (defaults) the re-rank is the
/// identity and the result equals pure cosine ranking, so the two-stage
/// split is exact; with non-zero factors it is exact whenever the true
/// top-`limit` re-ranked rows lie within the top-`ann_limit` cosine rows.
#[allow(clippy::too_many_arguments)]
fn build_search_query<'a>(
    prefix: &str,
    vec: Vector,
    agent_id: &'a str,
    limit: i64,
    threshold: f64,
    ann_limit: i64,
    decay_rate: f64,
    importance_boost: f64,
    memory_type: Option<&'a str>,
    session_id: Option<&'a str>,
    filter_sensitive: bool,
) -> QueryBuilder<sqlx::Postgres> {
    let mut qb: QueryBuilder<sqlx::Postgres> =
        QueryBuilder::new(format!("{prefix}WITH q AS (SELECT "));
    qb.push_bind(vec);
    qb.push(
        r#"::vector AS vec),
ann AS (
    SELECT id, agent_id, session_id, content, memory_type, confidence, provenance,
           created_at, source_turn, importance_score, importance_source,
           (embedding <=> (SELECT vec FROM q))::double precision AS cosine_dist,
           EXTRACT(EPOCH FROM (NOW() - COALESCE(last_accessed_at, created_at))) / 86400.0 AS days_stale
    FROM memories
    WHERE agent_id = "#,
    );
    qb.push_bind(agent_id);
    qb.push(" AND archived_at IS NULL AND soft_evicted = FALSE AND status = 'active'");
    if filter_sensitive {
        qb.push(" AND sensitivity NOT IN ('private', 'secret')");
    }

    if let Some(mt) = memory_type {
        qb.push(" AND memory_type = ");
        qb.push_bind(mt);
    }
    if let Some(sid) = session_id {
        qb.push(" AND session_id = ");
        qb.push_bind(sid);
    }

    qb.push("\n    ORDER BY embedding <=> (SELECT vec FROM q)\n    LIMIT ");
    qb.push_bind(ann_limit);

    // Exponential decay: exp(decay_rate × days_stale).
    // When decay_rate = 0.0, exp(0) = 1.0 → collapses to pure cosine similarity.
    // This gives a smoother, bounded penalty compared to the linear (1 + k·d) form.
    qb.push("),\nranked AS (\n    SELECT *,\n           cosine_dist\n           * exp(");
    qb.push_bind(decay_rate);
    qb.push(" * days_stale)\n           * (1.0 + ");
    qb.push_bind(importance_boost);
    qb.push(
        r#" * (1.0 - importance_score::double precision))
           AS distance
    FROM ann
)
SELECT id, agent_id, session_id, content, memory_type, confidence, provenance,
       created_at, source_turn, importance_score, importance_source, distance
FROM ranked WHERE distance < "#,
    );
    qb.push_bind(threshold);
    qb.push(" ORDER BY distance LIMIT ");
    qb.push_bind(limit);

    qb
}

/// Bump the access counter, record the access timestamp, and apply a small
/// importance refresh boost for a list of IDs.
/// Called via `tokio::spawn` from the hot path — failures are silent.
pub async fn bump_access_counts(state: AppState, ids: Vec<Uuid>) {
    let refresh_boost = state.config.importance_refresh_boost;
    for id in ids {
        let _ = sqlx::query(
            "UPDATE memories \
             SET access_count = access_count + 1, \
                 last_accessed_at = NOW(), \
                 importance_score = LEAST(1.0, importance_score + $2) \
             WHERE id = $1",
        )
        .bind(id)
        .bind(refresh_boost)
        .execute(&state.db)
        .await;
    }
}

/// Batch-update `utility_ema` for a set of retrieved memory IDs.
///
/// A feedback value of 1.0 means "this memory was useful" — it was retrieved
/// and injected into context.  Only called when AMP or RMK is active.
/// Failures are silent; this is fire-and-forget on the hot path.
pub async fn update_utility_emas(pool: &sqlx::PgPool, ids: &[Uuid], alpha: f64) {
    let _ = sqlx::query(
        "UPDATE memories
         SET utility_ema = $1 * 1.0 + (1.0 - $1) * utility_ema
         WHERE id = ANY($2)",
    )
    .bind(alpha)
    .bind(ids)
    .execute(pool)
    .await;
}

pub async fn list_memories_for_agent(
    state: &AppState,
    agent_id: &str,
    limit: i64,
    offset: i64,
) -> Result<Vec<Memory>> {
    let rows = sqlx::query_as::<_, Memory>(
        r#"
        SELECT id, agent_id, session_id, content, memory_type, confidence, provenance,
               created_at, updated_at, source_turn, importance_score, importance_source,
               status, sensitivity, valid_from, valid_to, suppression_reason, status_updated_at
        FROM memories
        WHERE agent_id = $1
          AND archived_at IS NULL
        ORDER BY created_at DESC
        LIMIT $2 OFFSET $3
        "#,
    )
    .bind(agent_id)
    .bind(limit)
    .bind(offset)
    .fetch_all(&state.db)
    .await?;
    Ok(rows)
}

/// Time-travel: latest version of each memory visible at a timestamp.
pub async fn list_memories_at_timestamp(
    state: &AppState,
    agent_id: &str,
    timestamp: DateTime<Utc>,
    limit: i64,
    offset: i64,
) -> Result<Vec<TemporalMemoryVersion>> {
    let rows = sqlx::query_as::<_, TemporalMemoryVersion>(
        r#"
        WITH latest_versions AS (
            SELECT DISTINCT ON (mv.memory_id)
                mv.memory_id,
                mv.version_number,
                mv.content,
                mv.memory_type,
                mv.confidence,
                mv.provenance,
                mv.importance_score,
                mv.importance_source,
                mv.status,
                mv.sensitivity,
                mv.valid_from,
                mv.valid_to,
                mv.source_turn,
                m.created_at AS memory_created_at,
                mv.created_at AS version_created_at
            FROM memory_versions mv
            JOIN memories m ON m.id = mv.memory_id
            WHERE mv.agent_id = $1
              AND mv.created_at <= $2
              AND m.created_at <= $2
              AND (m.archived_at IS NULL OR m.archived_at > $2)
            ORDER BY mv.memory_id, mv.version_number DESC
        )
        SELECT *
        FROM latest_versions
        ORDER BY memory_created_at DESC
        LIMIT $3 OFFSET $4
        "#,
    )
    .bind(agent_id)
    .bind(timestamp)
    .bind(limit)
    .bind(offset)
    .fetch_all(&state.db)
    .await?;

    Ok(rows)
}

pub async fn count_memories_at_timestamp(
    state: &AppState,
    agent_id: &str,
    timestamp: DateTime<Utc>,
) -> Result<i64> {
    let row: (i64,) = sqlx::query_as(
        r#"
        SELECT COUNT(*)::bigint
        FROM memories
        WHERE agent_id = $1
          AND created_at <= $2
          AND (archived_at IS NULL OR archived_at > $2)
        "#,
    )
    .bind(agent_id)
    .bind(timestamp)
    .fetch_one(&state.db)
    .await?;
    Ok(row.0)
}

pub async fn list_latest_versions_as_of(
    state: &AppState,
    agent_id: &str,
    timestamp: DateTime<Utc>,
) -> Result<Vec<TemporalMemoryVersion>> {
    let rows = sqlx::query_as::<_, TemporalMemoryVersion>(
        r#"
        WITH latest_versions AS (
            SELECT DISTINCT ON (mv.memory_id)
                mv.memory_id,
                mv.version_number,
                mv.content,
                mv.memory_type,
                mv.confidence,
                mv.provenance,
                mv.importance_score,
                mv.importance_source,
                mv.status,
                mv.sensitivity,
                mv.valid_from,
                mv.valid_to,
                mv.source_turn,
                m.created_at AS memory_created_at,
                mv.created_at AS version_created_at
            FROM memory_versions mv
            JOIN memories m ON m.id = mv.memory_id
            WHERE mv.agent_id = $1
              AND mv.created_at <= $2
              AND m.created_at <= $2
              AND (m.archived_at IS NULL OR m.archived_at > $2)
            ORDER BY mv.memory_id, mv.version_number DESC
        )
        SELECT *
        FROM latest_versions
        ORDER BY memory_created_at DESC
        "#,
    )
    .bind(agent_id)
    .bind(timestamp)
    .fetch_all(&state.db)
    .await?;

    Ok(rows)
}

pub async fn list_added_memories_between(
    state: &AppState,
    agent_id: &str,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> Result<Vec<TemporalMemoryVersion>> {
    let rows = sqlx::query_as::<_, TemporalMemoryVersion>(
        r#"
        WITH candidates AS (
            SELECT id
            FROM memories
            WHERE agent_id = $1
              AND created_at > $2
              AND created_at <= $3
        ),
        latest_versions AS (
            SELECT DISTINCT ON (mv.memory_id)
                mv.memory_id,
                mv.version_number,
                mv.content,
                mv.memory_type,
                mv.confidence,
                mv.provenance,
                mv.importance_score,
                mv.importance_source,
                mv.status,
                mv.sensitivity,
                mv.valid_from,
                mv.valid_to,
                mv.source_turn,
                m.created_at AS memory_created_at,
                mv.created_at AS version_created_at
            FROM memory_versions mv
            JOIN memories m ON m.id = mv.memory_id
            JOIN candidates c ON c.id = mv.memory_id
            WHERE mv.agent_id = $1
              AND mv.created_at <= $3
            ORDER BY mv.memory_id, mv.version_number DESC
        )
        SELECT *
        FROM latest_versions
        ORDER BY memory_created_at DESC
        "#,
    )
    .bind(agent_id)
    .bind(from)
    .bind(to)
    .fetch_all(&state.db)
    .await?;

    Ok(rows)
}

pub async fn list_archived_memories_between(
    state: &AppState,
    agent_id: &str,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> Result<Vec<ArchivedBetweenRow>> {
    let rows = sqlx::query_as::<_, ArchivedBetweenRow>(
        r#"
        SELECT id, content, memory_type, archived_at
        FROM memories
        WHERE agent_id = $1
          AND archived_at IS NOT NULL
          AND archived_at > $2
          AND archived_at <= $3
        ORDER BY archived_at DESC
        "#,
    )
    .bind(agent_id)
    .bind(from)
    .bind(to)
    .fetch_all(&state.db)
    .await?;

    Ok(rows)
}

pub async fn retrieval_activity_between(
    state: &AppState,
    agent_id: &str,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> Result<(i64, i64)> {
    let total_row: (i64,) = sqlx::query_as(
        r#"
        SELECT COUNT(*)::bigint
        FROM memory_retrieval_logs
        WHERE agent_id = $1
          AND created_at >= $2
          AND created_at <= $3
        "#,
    )
    .bind(agent_id)
    .bind(from)
    .bind(to)
    .fetch_one(&state.db)
    .await?;

    let unique_row: (i64,) = sqlx::query_as(
        r#"
        SELECT COUNT(DISTINCT mem_id)::bigint
        FROM (
            SELECT UNNEST(injected_memory_ids) AS mem_id
            FROM memory_retrieval_logs
            WHERE agent_id = $1
              AND created_at >= $2
              AND created_at <= $3
        ) t
        "#,
    )
    .bind(agent_id)
    .bind(from)
    .bind(to)
    .fetch_one(&state.db)
    .await?;

    Ok((total_row.0, unique_row.0))
}

// ── Cognitive Hypervisor timeline ────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
pub async fn record_hypervisor_event(
    state: &AppState,
    agent_id: &str,
    session_id: Option<&str>,
    nexus_snapshot_id: Option<Uuid>,
    capsule_digest: Option<&str>,
    prev_event_digest: Option<&str>,
    branch_id: Option<&str>,
    event_type: &str,
) -> Result<Uuid> {
    let id = sqlx::query_scalar::<_, Uuid>(
        r#"
        INSERT INTO cognitive_hypervisor_timeline
            (agent_id, session_id, nexus_snapshot_id, capsule_digest,
             prev_event_digest, branch_id, event_type)
        VALUES ($1, $2, $3, $4, $5, $6, $7)
        RETURNING id
        "#,
    )
    .bind(agent_id)
    .bind(session_id)
    .bind(nexus_snapshot_id)
    .bind(capsule_digest)
    .bind(prev_event_digest)
    .bind(branch_id)
    .bind(event_type)
    .fetch_one(&state.db)
    .await?;

    Ok(id)
}

pub async fn get_latest_event_id(
    state: &AppState,
    agent_id: &str,
    session_id: Option<&str>,
) -> Result<Option<String>> {
    if let Some(sid) = session_id {
        let row = sqlx::query_scalar::<_, String>(
            r#"
            SELECT id::text
            FROM cognitive_hypervisor_timeline
            WHERE agent_id = $1 AND session_id = $2
            ORDER BY created_at DESC, id DESC
            LIMIT 1
            "#,
        )
        .bind(agent_id)
        .bind(sid)
        .fetch_optional(&state.db)
        .await?;
        Ok(row)
    } else {
        let row = sqlx::query_scalar::<_, String>(
            r#"
            SELECT id::text
            FROM cognitive_hypervisor_timeline
            WHERE agent_id = $1
            ORDER BY created_at DESC, id DESC
            LIMIT 1
            "#,
        )
        .bind(agent_id)
        .fetch_optional(&state.db)
        .await?;
        Ok(row)
    }
}

pub async fn resolve_snapshot_at(
    state: &AppState,
    agent_id: &str,
    at: DateTime<Utc>,
) -> Result<Option<SnapshotResolution>> {
    let row = sqlx::query_as::<_, (Uuid, DateTime<Utc>, String, Option<String>, Option<String>)>(
        r#"
        SELECT nexus_snapshot_id, occurred_at, event_type, branch_id, prev_event_digest
        FROM cognitive_hypervisor_timeline
        WHERE agent_id = $1
          AND occurred_at <= $2
          AND nexus_snapshot_id IS NOT NULL
          AND branch_id IS NULL
        ORDER BY occurred_at DESC, created_at DESC, id DESC
        LIMIT 1
        "#,
    )
    .bind(agent_id)
    .bind(at)
    .fetch_optional(&state.db)
    .await?;

    Ok(row.map(
        |(snapshot_id, occurred_at, event_type, branch_id, prev_event_digest)| SnapshotResolution {
            snapshot_id,
            occurred_at,
            event_type,
            branch_id,
            prev_event_digest,
        },
    ))
}

pub async fn resolve_at_branch(
    state: &AppState,
    agent_id: &str,
    branch_id: &str,
    at: DateTime<Utc>,
) -> Result<Option<SnapshotResolution>> {
    let row = sqlx::query_as::<_, (Uuid, DateTime<Utc>, String, Option<String>, Option<String>)>(
        r#"
        SELECT nexus_snapshot_id, occurred_at, event_type, branch_id, prev_event_digest
        FROM cognitive_hypervisor_timeline
        WHERE agent_id = $1
          AND branch_id = $2
          AND occurred_at <= $3
          AND nexus_snapshot_id IS NOT NULL
        ORDER BY occurred_at DESC, created_at DESC, id DESC
        LIMIT 1
        "#,
    )
    .bind(agent_id)
    .bind(branch_id)
    .bind(at)
    .fetch_optional(&state.db)
    .await?;

    Ok(row.map(
        |(snapshot_id, occurred_at, event_type, branch_id, prev_event_digest)| SnapshotResolution {
            snapshot_id,
            occurred_at,
            event_type,
            branch_id,
            prev_event_digest,
        },
    ))
}

/// List tombstoned (archived) memories for an agent, newest-archived first.
pub async fn list_archived_memories(
    state: &AppState,
    agent_id: &str,
    limit: i64,
    offset: i64,
) -> Result<Vec<crate::models::ArchivedMemory>> {
    let rows = sqlx::query_as(
        r#"
        SELECT id, agent_id, session_id, content, memory_type, confidence, provenance,
               created_at, source_turn, importance_score, importance_source, archived_at
        FROM memories
        WHERE agent_id = $1
          AND archived_at IS NOT NULL
        ORDER BY archived_at DESC
        LIMIT $2 OFFSET $3
        "#,
    )
    .bind(agent_id)
    .bind(limit)
    .bind(offset)
    .fetch_all(&state.db)
    .await?;
    Ok(rows)
}

/// Restore a tombstoned memory by clearing `archived_at`.
/// Returns `true` if the memory existed and was archived, `false` otherwise.
pub async fn restore_memory(state: &AppState, id: Uuid) -> Result<bool> {
    let r = sqlx::query(
        "UPDATE memories SET archived_at = NULL WHERE id = $1 AND archived_at IS NOT NULL",
    )
    .bind(id)
    .execute(&state.db)
    .await?;
    Ok(r.rows_affected() > 0)
}

pub async fn delete_memory(state: &AppState, id: Uuid) -> Result<bool> {
    let r = sqlx::query("DELETE FROM memories WHERE id = $1")
        .bind(id)
        .execute(&state.db)
        .await?;
    Ok(r.rows_affected() > 0)
}

/// Update a memory's content and re-embed it.  Returns false if the memory
/// does not exist or is tombstoned.  Creates a new version snapshot.
pub async fn update_memory_content(
    state: &AppState,
    id: Uuid,
    content: &str,
    embedding: Vec<f32>,
) -> Result<bool> {
    let vec = Vector::from(embedding);
    let mut tx = state.db.begin().await?;

    let r = sqlx::query(
        "UPDATE memories SET content = $1, embedding = $2, updated_at = NOW()
         WHERE id = $3 AND archived_at IS NULL",
    )
    .bind(content)
    .bind(vec)
    .bind(id)
    .execute(&mut *tx)
    .await?;

    if r.rows_affected() == 0 {
        return Ok(false);
    }

    // Fetch current meta to snapshot in the version row.
    let meta: Option<MemoryVersionMeta> = sqlx::query_as(
        "SELECT agent_id, memory_type, confidence, provenance,
                    importance_score, importance_source, status, sensitivity, source_turn
             FROM memories WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?;

    if let Some((
        agent_id,
        memory_type,
        confidence,
        provenance,
        importance_score,
        importance_source,
        status,
        sensitivity,
        source_turn,
    )) = meta
    {
        create_memory_version(
            &state.db,
            MemoryVersionInput {
                memory_id: id,
                agent_id: &agent_id,
                content,
                memory_type: &memory_type,
                confidence,
                provenance: &provenance,
                importance_score,
                importance_source: &importance_source,
                status: &status,
                sensitivity: &sensitivity,
                source_turn,
                change_type: "patch",
                change_reason: None,
            },
        )
        .await?;
    }

    tx.commit().await?;
    Ok(true)
}

pub async fn count_memories(state: &AppState) -> Result<i64> {
    let row: (i64,) =
        sqlx::query_as("SELECT COUNT(*)::bigint FROM memories WHERE archived_at IS NULL")
            .fetch_one(&state.db)
            .await?;
    Ok(row.0)
}

// ── Working memory (L1 per-session) ──────────────────────────────────────────

pub async fn get_working_memory(
    state: &AppState,
    agent_id: &str,
    session_id: &str,
) -> Result<Option<WorkingMemory>> {
    let wm = sqlx::query_as::<_, WorkingMemory>(
        "SELECT id, agent_id, session_id, summary, turn_count, updated_at, state
         FROM working_memory
         WHERE agent_id = $1 AND session_id = $2",
    )
    .bind(agent_id)
    .bind(session_id)
    .fetch_optional(&state.db)
    .await?;
    Ok(wm)
}

pub async fn upsert_working_memory(
    state: &AppState,
    agent_id: &str,
    session_id: &str,
    summary: &str,
    structured_state: Option<&crate::models::WorkingMemoryState>,
) -> Result<()> {
    let state_json = structured_state.map(serde_json::to_value).transpose()?;

    sqlx::query(
        r#"
        INSERT INTO working_memory (agent_id, session_id, summary, turn_count, state)
        VALUES ($1, $2, $3, 1, $4)
        ON CONFLICT (agent_id, session_id) DO UPDATE
            SET summary    = EXCLUDED.summary,
                turn_count = working_memory.turn_count + 1,
                updated_at = NOW(),
                state      = EXCLUDED.state
        "#,
    )
    .bind(agent_id)
    .bind(session_id)
    .bind(summary)
    .bind(state_json)
    .execute(&state.db)
    .await?;

    // Mirror turn_count into the sessions table so it stays queryable there.
    sqlx::query(
        r#"
        INSERT INTO sessions (session_id, agent_id, turn_count)
        VALUES ($1, $2, 1)
        ON CONFLICT (agent_id, session_id) DO UPDATE
            SET turn_count = EXCLUDED.turn_count + sessions.turn_count,
                ended_at   = NULL
        "#,
    )
    .bind(session_id)
    .bind(agent_id)
    .execute(&state.db)
    .await?;

    Ok(())
}

/// List all sessions (working_memory rows) for an agent, newest first.
/// Returns session_id, turn_count, updated_at, and a 150-char summary preview.
pub async fn list_sessions_for_agent(
    state: &AppState,
    agent_id: &str,
) -> Result<Vec<crate::models::SessionInfo>> {
    let rows = sqlx::query_as::<_, crate::models::SessionInfo>(
        r#"
        SELECT session_id, agent_id, turn_count, updated_at,
               LEFT(summary, 150) AS summary_preview
        FROM working_memory
        WHERE agent_id = $1
        ORDER BY updated_at DESC
        "#,
    )
    .bind(agent_id)
    .fetch_all(&state.db)
    .await?;
    Ok(rows)
}

/// Fetch the full L1 summary for a single session.
pub async fn get_session_detail(
    state: &AppState,
    agent_id: &str,
    session_id: &str,
) -> Result<Option<WorkingMemory>> {
    get_working_memory(state, agent_id, session_id).await
}

/// Clear working memory for a session (hard-deletes the L1 summary row).
/// Returns false if the session had no working memory entry.
pub async fn delete_session_working_memory(
    state: &AppState,
    agent_id: &str,
    session_id: &str,
) -> Result<bool> {
    let r = sqlx::query("DELETE FROM working_memory WHERE agent_id = $1 AND session_id = $2")
        .bind(agent_id)
        .bind(session_id)
        .execute(&state.db)
        .await?;
    Ok(r.rows_affected() > 0)
}

// ── Entities ──────────────────────────────────────────────────────────────────

/// Upsert an entity, returning its UUID.
///
/// Disambiguation (4.2): before inserting, check whether any existing entity
/// for this agent has a name within Levenshtein distance ≤ 2.  If so, merge
/// into the existing entity by using its canonical name — preventing "Alex"
/// and "Alexander" from being stored as two separate nodes.
pub async fn upsert_entity(
    state: &AppState,
    agent_id: &str,
    name: &str,
    entity_type: &str,
    confidence: f64,
) -> Result<Uuid> {
    // Check for a near-duplicate entity name (case-insensitive Levenshtein).
    let similar: Option<(Uuid, String)> = sqlx::query_as(
        r#"SELECT id, name FROM entities
           WHERE agent_id = $1
             AND name != $2
             AND levenshtein(LOWER(name), LOWER($2)) <= 2
           ORDER BY levenshtein(LOWER(name), LOWER($2)) ASC
           LIMIT 1"#,
    )
    .bind(agent_id)
    .bind(name)
    .fetch_optional(&state.db)
    .await?;

    let canonical = if let Some((_, existing_name)) = similar {
        tracing::info!(
            agent_id = %agent_id,
            "Entity disambiguation: '{}' merged into '{}'",
            name, existing_name
        );
        existing_name
    } else {
        name.to_string()
    };

    let row: (Uuid,) = sqlx::query_as(
        r#"INSERT INTO entities (agent_id, name, entity_type, confidence)
           VALUES ($1, $2, $3, $4)
           ON CONFLICT (agent_id, name) DO UPDATE
               SET entity_type = EXCLUDED.entity_type,
                   confidence  = EXCLUDED.confidence,
                   updated_at  = NOW()
           RETURNING id"#,
    )
    .bind(agent_id)
    .bind(&canonical)
    .bind(entity_type)
    .bind(confidence as f32)
    .fetch_one(&state.db)
    .await?;

    Ok(row.0)
}

/// Link a memory to an entity extracted in the same turn.
pub async fn link_memory_entity(
    state: &AppState,
    memory_id: Uuid,
    entity_id: Uuid,
    agent_id: &str,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO memory_entity_links (memory_id, entity_id, agent_id)
         VALUES ($1, $2, $3)
         ON CONFLICT DO NOTHING",
    )
    .bind(memory_id)
    .bind(entity_id)
    .bind(agent_id)
    .execute(&state.db)
    .await?;
    Ok(())
}

/// Return all entity names known for this agent (used for query-entity matching).
pub async fn get_entity_names(state: &AppState, agent_id: &str) -> Result<Vec<String>> {
    let rows: Vec<(String,)> =
        sqlx::query_as("SELECT name FROM entities WHERE agent_id = $1 ORDER BY name")
            .bind(agent_id)
            .fetch_all(&state.db)
            .await?;
    Ok(rows.into_iter().map(|(n,)| n).collect())
}

/// Walk the knowledge graph one hop: given a set of matched entity names,
/// return all entity names that appear on the other side of any relation.
pub async fn walk_graph_for_entities(
    state: &AppState,
    agent_id: &str,
    entity_names: &[String],
) -> Result<Vec<String>> {
    if entity_names.is_empty() {
        return Ok(Vec::new());
    }
    let lower_names: Vec<String> = entity_names.iter().map(|n| n.to_lowercase()).collect();

    // Two-direction UNION: object side of matching subjects + subject side of matching objects
    let mut qb: QueryBuilder<sqlx::Postgres> =
        QueryBuilder::new("SELECT DISTINCT subject AS name FROM memory_graph WHERE agent_id = ");
    qb.push_bind(agent_id);
    qb.push(" AND LOWER(object) IN (");
    let mut sep = qb.separated(", ");
    for n in &lower_names {
        sep.push_bind(n);
    }
    qb.push(") UNION SELECT DISTINCT object AS name FROM memory_graph WHERE agent_id = ");
    qb.push_bind(agent_id);
    qb.push(" AND LOWER(subject) IN (");
    let mut sep2 = qb.separated(", ");
    for n in &lower_names {
        sep2.push_bind(n);
    }
    qb.push(")");

    let rows: Vec<(String,)> = qb.build_query_as().fetch_all(&state.db).await?;
    let matched: std::collections::HashSet<String> = lower_names.into_iter().collect();
    Ok(rows
        .into_iter()
        .map(|(n,)| n)
        .filter(|n| !matched.contains(&n.to_lowercase()))
        .collect())
}

/// Fetch live memories linked to any of the given entity names via the
/// `memory_entity_links` join table, excluding already-retrieved IDs.
pub async fn get_memories_for_entities(
    state: &AppState,
    agent_id: &str,
    entity_names: &[String],
    exclude_ids: &[Uuid],
    limit: i64,
) -> Result<Vec<Memory>> {
    if entity_names.is_empty() {
        return Ok(Vec::new());
    }
    let lower_names: Vec<String> = entity_names.iter().map(|n| n.to_lowercase()).collect();

    let mut qb: QueryBuilder<sqlx::Postgres> = QueryBuilder::new(
        r#"SELECT DISTINCT m.id, m.agent_id, m.session_id, m.content, m.memory_type,
                  m.confidence, m.provenance, m.created_at, m.source_turn,
                  m.importance_score, m.importance_source
           FROM memories m
           JOIN memory_entity_links mel ON m.id = mel.memory_id
           JOIN entities e ON mel.entity_id = e.id
           WHERE m.agent_id = "#,
    );
    qb.push_bind(agent_id);
    qb.push(" AND m.archived_at IS NULL AND m.soft_evicted = FALSE AND m.status = 'active' AND m.sensitivity NOT IN ('private', 'secret') AND LOWER(e.name) IN (");
    let mut sep = qb.separated(", ");
    for n in &lower_names {
        sep.push_bind(n);
    }
    qb.push(")");

    if !exclude_ids.is_empty() {
        qb.push(" AND m.id NOT IN (");
        let mut sep2 = qb.separated(", ");
        for id in exclude_ids {
            sep2.push_bind(*id);
        }
        qb.push(")");
    }

    qb.push(" ORDER BY m.importance_score DESC, m.created_at DESC LIMIT ");
    qb.push_bind(limit);

    qb.build_query_as::<Memory>()
        .fetch_all(&state.db)
        .await
        .map_err(Into::into)
}

// ── Relations / Knowledge Graph ───────────────────────────────────────────────

pub async fn insert_relation(
    state: &AppState,
    agent_id: &str,
    subject: &str,
    predicate: &str,
    object: &str,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO memory_graph (agent_id, subject, predicate, object)
         VALUES ($1, $2, $3, $4)",
    )
    .bind(agent_id)
    .bind(subject)
    .bind(predicate)
    .bind(object)
    .execute(&state.db)
    .await?;
    Ok(())
}

/// Fetch all relations for an agent (used by the graph view).
pub async fn get_agent_relations(
    state: &AppState,
    agent_id: &str,
    limit: i64,
) -> Result<Vec<crate::models::RelationRow>> {
    let rows = sqlx::query_as::<_, crate::models::RelationRow>(
        r#"
        SELECT id, agent_id, subject, predicate, object, confidence, created_at
        FROM memory_graph
        WHERE agent_id = $1
        ORDER BY created_at DESC
        LIMIT $2
        "#,
    )
    .bind(agent_id)
    .bind(limit)
    .fetch_all(&state.db)
    .await?;
    Ok(rows)
}

// ── Archival helpers ──────────────────────────────────────────────────────────

/// Returns agent IDs that have at least one archivable L2 memory.
/// High-importance memories (score >= 0.9) are protected from archival.
pub async fn agents_with_archivable_memories(
    state: &AppState,
    min_age_days: i64,
) -> Result<Vec<String>> {
    let rows: Vec<(String,)> = sqlx::query_as(
        r#"
        SELECT DISTINCT agent_id
        FROM memories
        WHERE tier = 'L2'
          AND access_count = 0
          AND archived_at IS NULL
          AND importance_score < 0.9
          AND sensitivity NOT IN ('private', 'secret')
          AND created_at < NOW() - INTERVAL '1 day' * $1
        "#,
    )
    .bind(min_age_days)
    .fetch_all(&state.db)
    .await?;
    Ok(rows.into_iter().map(|(id,)| id).collect())
}

/// Fetch up to `limit` archivable L2 memories for one agent.
/// High-importance memories (score >= 0.9) are protected from archival.
pub async fn fetch_archivable_memories(
    state: &AppState,
    agent_id: &str,
    min_age_days: i64,
    limit: i64,
) -> Result<Vec<(Uuid, String)>> {
    let rows: Vec<(Uuid, String)> = sqlx::query_as(
        r#"
        SELECT id, content
        FROM memories
        WHERE agent_id = $1
          AND tier = 'L2'
          AND access_count = 0
          AND archived_at IS NULL
          AND importance_score < 0.9
          AND sensitivity NOT IN ('private', 'secret')
          AND created_at < NOW() - INTERVAL '1 day' * $2
        ORDER BY created_at ASC
        LIMIT $3
        "#,
    )
    .bind(agent_id)
    .bind(min_age_days)
    .bind(limit)
    .fetch_all(&state.db)
    .await?;
    Ok(rows)
}

/// Tombstone a list of memories by setting archived_at = NOW().
/// Originals remain in the DB for audit/lineage; they are excluded from all
/// retrieval queries via `AND archived_at IS NULL`.
#[allow(dead_code)]
pub async fn tombstone_memories(state: &AppState, ids: &[Uuid]) -> Result<u64> {
    if ids.is_empty() {
        return Ok(0);
    }
    let mut qb: QueryBuilder<sqlx::Postgres> =
        QueryBuilder::new("UPDATE memories SET archived_at = NOW() WHERE id IN (");
    let mut sep = qb.separated(", ");
    for id in ids {
        sep.push_bind(id);
    }
    qb.push(")");
    let r = qb.build().execute(&state.db).await?;
    Ok(r.rows_affected())
}

// ── Archival batch versioning ─────────────────────────────────────────────────

/// Create an archival batch record before compacting. Returns the new batch id.
pub async fn create_archival_batch(
    state: &AppState,
    agent_id: &str,
    source_count: i32,
    l3_count: i32,
) -> Result<Uuid> {
    let row: (Uuid,) = sqlx::query_as(
        "INSERT INTO archival_batches (agent_id, source_count, l3_count)
         VALUES ($1, $2, $3) RETURNING id",
    )
    .bind(agent_id)
    .bind(source_count)
    .bind(l3_count)
    .fetch_one(&state.db)
    .await?;
    Ok(row.0)
}

/// Mark an archival batch as failed so it is distinguishable from a
/// successful batch that happened to produce zero L3 facts.
pub async fn fail_archival_batch(state: &AppState, batch_id: Uuid) -> Result<()> {
    sqlx::query(
        "UPDATE archival_batches SET status = 'failed', completed_at = NOW() WHERE id = $1",
    )
    .bind(batch_id)
    .execute(&state.db)
    .await?;
    Ok(())
}

/// Tombstone L2 memories and tag them with the batch that archived them.
pub async fn tombstone_memories_with_batch(
    state: &AppState,
    ids: &[Uuid],
    batch_id: Uuid,
) -> Result<u64> {
    if ids.is_empty() {
        return Ok(0);
    }
    let mut qb: QueryBuilder<sqlx::Postgres> =
        QueryBuilder::new("UPDATE memories SET archived_at = NOW(), archival_batch_id = ");
    qb.push_bind(batch_id);
    qb.push(" WHERE id IN (");
    let mut sep = qb.separated(", ");
    for id in ids {
        sep.push_bind(id);
    }
    qb.push(")");
    let r = qb.build().execute(&state.db).await?;
    Ok(r.rows_affected())
}

/// List archival batches for an agent, newest first.
pub async fn list_archival_batches(
    state: &AppState,
    agent_id: &str,
    limit: i64,
    offset: i64,
) -> Result<Vec<ArchivalBatch>> {
    sqlx::query_as(
        "SELECT id, agent_id, created_at, source_count, l3_count, status
         FROM archival_batches
         WHERE agent_id = $1
         ORDER BY created_at DESC
         LIMIT $2 OFFSET $3",
    )
    .bind(agent_id)
    .bind(limit)
    .bind(offset)
    .fetch_all(&state.db)
    .await
    .map_err(Into::into)
}

/// Count archival batches for an agent.
pub async fn count_archival_batches(state: &AppState, agent_id: &str) -> Result<i64> {
    let row: (i64,) =
        sqlx::query_as("SELECT COUNT(*)::bigint FROM archival_batches WHERE agent_id = $1")
            .bind(agent_id)
            .fetch_one(&state.db)
            .await?;
    Ok(row.0)
}

pub struct RestoreResult {
    pub l2_restored: u64,
    pub l3_tombstoned: u64,
}

/// Restore an archival batch:
///   - un-tombstones the L2 source memories (clears archived_at)
///   - tombstones the L3 compressed facts that replaced them
///   - marks the batch status = 'restored'
///
/// Returns `None` if the batch does not exist or is already restored.
pub async fn restore_archival_batch(
    state: &AppState,
    batch_id: Uuid,
) -> Result<Option<RestoreResult>> {
    // Guard: only restore a completed batch
    let updated = sqlx::query(
        "UPDATE archival_batches SET status = 'restored'
         WHERE id = $1 AND status = 'completed'",
    )
    .bind(batch_id)
    .execute(&state.db)
    .await?;

    if updated.rows_affected() == 0 {
        return Ok(None);
    }

    let l2 = sqlx::query(
        "UPDATE memories SET archived_at = NULL
         WHERE archival_batch_id = $1 AND tier = 'L2' AND archived_at IS NOT NULL",
    )
    .bind(batch_id)
    .execute(&state.db)
    .await?;

    let l3 = sqlx::query(
        "UPDATE memories SET archived_at = NOW()
         WHERE archival_batch_id = $1 AND tier = 'L3' AND archived_at IS NULL",
    )
    .bind(batch_id)
    .execute(&state.db)
    .await?;

    Ok(Some(RestoreResult {
        l2_restored: l2.rows_affected(),
        l3_tombstoned: l3.rows_affected(),
    }))
}

// ── Export ────────────────────────────────────────────────────────────────────

/// Fetch all live (non-tombstoned) memories for an agent suitable for NDJSON export.
/// Embeddings are excluded — they must be re-computed on import.
pub async fn export_memories_for_agent(
    state: &AppState,
    agent_id: &str,
) -> Result<Vec<crate::models::MemoryExportRow>> {
    let rows = sqlx::query_as(
        "SELECT id, session_id, content, memory_type, confidence, provenance, \
                tier, importance_score, importance_source, created_at \
         FROM memories \
         WHERE agent_id = $1 AND archived_at IS NULL \
         ORDER BY created_at ASC",
    )
    .bind(agent_id)
    .fetch_all(&state.db)
    .await?;
    Ok(rows)
}

// ── Bulk operations ───────────────────────────────────────────────────────────

/// Archive or delete memories matching an optional set of filters.
/// Returns the number of rows affected.
///
/// `action` must be either `"archive"` (tombstone) or `"delete"` (hard-delete).
pub async fn bulk_operation_memories(
    state: &AppState,
    agent_id: &str,
    action: &str,
    session_id: Option<&str>,
    memory_type: Option<&str>,
    older_than: Option<chrono::DateTime<chrono::Utc>>,
    importance_below: Option<f32>,
) -> Result<u64> {
    let mut qb: QueryBuilder<sqlx::Postgres> = if action == "archive" {
        QueryBuilder::new(
            "UPDATE memories SET archived_at = NOW() WHERE archived_at IS NULL AND agent_id = ",
        )
    } else {
        QueryBuilder::new("DELETE FROM memories WHERE archived_at IS NULL AND agent_id = ")
    };
    qb.push_bind(agent_id);

    if let Some(sid) = session_id {
        qb.push(" AND session_id = ");
        qb.push_bind(sid);
    }
    if let Some(mt) = memory_type {
        qb.push(" AND memory_type = ");
        qb.push_bind(mt);
    }
    if let Some(dt) = older_than {
        qb.push(" AND created_at < ");
        qb.push_bind(dt);
    }
    if let Some(imp) = importance_below {
        qb.push(" AND importance_score < ");
        qb.push_bind(imp);
    }

    let r = qb.build().execute(&state.db).await?;
    Ok(r.rows_affected())
}

// ── Conflict store ────────────────────────────────────────────────────────────

pub async fn store_conflict(
    state: &AppState,
    agent_id: &str,
    memory_a: Uuid,
    memory_b: Uuid,
    reason: &str,
) -> Result<Uuid> {
    let row: (Uuid,) = sqlx::query_as(
        "INSERT INTO memory_conflicts (agent_id, memory_a, memory_b, reason) \
         VALUES ($1, $2, $3, $4) RETURNING id",
    )
    .bind(agent_id)
    .bind(memory_a)
    .bind(memory_b)
    .bind(reason)
    .fetch_one(&state.db)
    .await?;
    Ok(row.0)
}

pub async fn list_conflicts(
    state: &AppState,
    agent_id: &str,
    include_resolved: bool,
) -> Result<Vec<crate::models::MemoryConflict>> {
    let rows = if include_resolved {
        sqlx::query_as(
            "SELECT id, agent_id, memory_a, memory_b, reason, resolved_at, resolution, created_at \
             FROM memory_conflicts WHERE agent_id = $1 ORDER BY created_at DESC LIMIT 200",
        )
        .bind(agent_id)
        .fetch_all(&state.db)
        .await?
    } else {
        sqlx::query_as(
            "SELECT id, agent_id, memory_a, memory_b, reason, resolved_at, resolution, created_at \
             FROM memory_conflicts WHERE agent_id = $1 AND resolved_at IS NULL \
             ORDER BY created_at DESC LIMIT 200",
        )
        .bind(agent_id)
        .fetch_all(&state.db)
        .await?
    };
    Ok(rows)
}

/// Mark a conflict as resolved.  Returns `true` if found and unresolved, `false` otherwise.
pub async fn resolve_conflict(
    state: &AppState,
    conflict_id: Uuid,
    resolution: &str,
) -> Result<bool> {
    let r = sqlx::query(
        "UPDATE memory_conflicts \
         SET resolved_at = NOW(), resolution = $2 \
         WHERE id = $1 AND resolved_at IS NULL",
    )
    .bind(conflict_id)
    .bind(resolution)
    .execute(&state.db)
    .await?;
    Ok(r.rows_affected() > 0)
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────
//
// Integration tests use `#[sqlx::test]` which creates an isolated Postgres
// database per test, runs all migrations, then drops the database when done.
//
// Requirements:
//   - DATABASE_URL must point to a pgvector-enabled Postgres instance where
//     the user has CREATE DATABASE privilege (or set PGCREATEDB=true).
//   - Locally: `docker compose up postgres` then run `cargo test`.
//   - In CI: use the `pgvector/pgvector:pg16` image.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::Config, metrics::Metrics, providers::Provider, rate_limit::RateLimiter};
    use std::sync::Arc;

    // ── Helpers ───────────────────────────────────────────────────────────────

    /// Unit vector with a 1.0 at `hot` and 0.0 everywhere else.
    fn unit_vec(hot: usize) -> Vec<f32> {
        let mut v = vec![0.0f32; 1536];
        v[hot] = 1.0;
        v
    }

    /// Two-component unit vector (for decay tests where cosine dist != 0).
    /// Caller is responsible for ensuring a² + b² ≈ 1.
    fn two_hot(dim_a: usize, a: f32, dim_b: usize, b: f32) -> Vec<f32> {
        let mut v = vec![0.0f32; 1536];
        v[dim_a] = a;
        v[dim_b] = b;
        v
    }

    fn build_state(pool: sqlx::PgPool, decay_rate: f64) -> crate::AppState {
        build_state_with_importance(pool, decay_rate, 0.0)
    }

    fn build_state_with_importance(
        pool: sqlx::PgPool,
        decay_rate: f64,
        importance_boost_factor: f64,
    ) -> crate::AppState {
        crate::AppState {
            config: Arc::new(Config {
                database_url: String::new(),
                upstream_base_url: "http://localhost".into(),
                port: 8080,
                db_max_connections: 5,
                db_acquire_timeout_secs: 5,
                db_idle_timeout_secs: 300,
                openai_api_key: None,
                embedding_model: "test".into(),
                embedding_dimension: 1536,
                embedding_base_url: "http://localhost".into(),
                extractor_model: "test".into(),
                extractor_base_url: "http://localhost".into(),
                retrieval_threshold: 0.80,
                memory_decay_rate: decay_rate,
                importance_boost_factor,
                importance_refresh_boost: 0.05,
                ann_candidate_limit: 100,
                hnsw_ef_search: 100,
                management_api_key: None,
                allow_unauth_management: true,
                max_body_bytes: 10 * 1024 * 1024,
                archival_interval_hours: 0,
                archival_min_age_days: 7,
                archival_min_memories: 10,
                upstream_provider: "openai".into(),
                rate_limit_rpm: 0,
                rate_limit_burst: 20,
                graph_retrieval_enabled: false,
                dedup_threshold: 0.0,
                conflict_detection_enabled: false,
                retrieval_log_query_text: false,
                hnsw_maintenance_enabled: false,
                hnsw_maintenance_interval_hours: 0,
                hnsw_maintenance_reindex_enabled: false,
                hnsw_maintenance_vacuum_enabled: false,
                hnsw_maintenance_vacuum_analyze: false,
                hnsw_maintenance_advisory_lock_id: 173456781234,
                hnsw_maintenance_work_mem: None,
                amp_config: crate::memory::amp::config::AmpConfig::default(),
                rmk_config: crate::memory::rmk::config::RmkConfig::default(),
            }),
            db: pool,
            evidence_signer: Arc::new(crate::attestation::EvidenceSigner::from_env().unwrap()),
            http_client: reqwest::Client::new(),
            metrics: Arc::new(Metrics::new().unwrap()),
            provider: Provider::OpenAI,
            rate_limiter: Arc::new(RateLimiter::new(0, 0)),
        }
    }

    // ── Provenance ────────────────────────────────────────────────────────────

    /// Provenance label and confidence value survive a round-trip through the DB.
    #[sqlx::test(migrations = "./migrations")]
    async fn provenance_round_trips(pool: sqlx::PgPool) {
        let state = build_state(pool, 0.0);
        const AGENT: &str = "prov-agent";

        let id = store_memory(
            &state,
            AGENT,
            None,
            "user stated a fact",
            "episodic",
            0.85,
            unit_vec(0),
            None,
            "user_stated",
            0.5_f32,
            "extractor",
        )
        .await
        .unwrap();

        let rows = list_memories_for_agent(&state, AGENT, 10, 0).await.unwrap();
        let m = rows.iter().find(|r| r.id == id).unwrap();
        assert_eq!(m.provenance, "user_stated");
        assert!(
            (m.confidence - 0.85).abs() < 1e-5,
            "confidence mismatch: {}",
            m.confidence
        );
    }

    // ── Tombstone ─────────────────────────────────────────────────────────────

    /// Tombstoned memories are excluded from both vector search and list queries.
    #[sqlx::test(migrations = "./migrations")]
    async fn tombstoned_memories_excluded(pool: sqlx::PgPool) {
        let state = build_state(pool, 0.0);
        const AGENT: &str = "tomb-agent";

        let id = store_memory(
            &state,
            AGENT,
            None,
            "soon to be tombstoned",
            "episodic",
            0.9,
            unit_vec(0),
            None,
            "user_stated",
            0.5_f32,
            "extractor",
        )
        .await
        .unwrap();

        // Reachable before tombstone
        let before = search_memories_filtered(&state, AGENT, &unit_vec(0), 5, 0.01, None, None)
            .await
            .unwrap();
        assert_eq!(before.len(), 1, "should find the memory before tombstone");

        tombstone_memories(&state, &[id]).await.unwrap();

        // Excluded after tombstone
        let after = search_memories_filtered(&state, AGENT, &unit_vec(0), 5, 0.01, None, None)
            .await
            .unwrap();
        assert!(
            after.is_empty(),
            "tombstoned memory must be excluded from search"
        );

        let listed = list_memories_for_agent(&state, AGENT, 10, 0).await.unwrap();
        assert!(
            listed.is_empty(),
            "tombstoned memory must be excluded from list"
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn update_memory_content_creates_incrementing_patch_version(pool: sqlx::PgPool) {
        let state = build_state(pool.clone(), 0.0);
        const AGENT: &str = "version-agent";

        let id = store_memory(
            &state,
            AGENT,
            None,
            "versioned fact v1",
            "semantic",
            0.9,
            unit_vec(0),
            None,
            "user_stated",
            0.5_f32,
            "extractor",
        )
        .await
        .unwrap();

        let updated = update_memory_content(&state, id, "versioned fact v2", unit_vec(1))
            .await
            .unwrap();
        assert!(updated, "existing live memory should be patchable");

        let rows: Vec<(i32, String, String)> = sqlx::query_as(
            "SELECT version_number, content, change_type
             FROM memory_versions
             WHERE memory_id = $1
             ORDER BY version_number ASC",
        )
        .bind(id)
        .fetch_all(&pool)
        .await
        .unwrap();

        assert_eq!(rows.len(), 2, "initial plus patch snapshots expected");
        assert_eq!(rows[0].0, 1);
        assert_eq!(rows[0].1, "versioned fact v1");
        assert_eq!(rows[0].2, "initial");
        assert_eq!(rows[1].0, 2);
        assert_eq!(rows[1].1, "versioned fact v2");
        assert_eq!(rows[1].2, "patch");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn timeline_records_and_resolves_snapshot(pool: sqlx::PgPool) {
        let state = build_state(pool.clone(), 0.0);
        const AGENT: &str = "timeline-agent";

        let snapshot_a = Uuid::new_v4();
        let snapshot_b = Uuid::new_v4();

        let event_a = record_hypervisor_event(
            &state,
            AGENT,
            Some("session-a"),
            Some(snapshot_a),
            None,
            None,
            None,
            "snapshot_created",
        )
        .await
        .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let event_b = record_hypervisor_event(
            &state,
            AGENT,
            Some("session-b"),
            Some(snapshot_b),
            None,
            None,
            None,
            "snapshot_created",
        )
        .await
        .unwrap();

        let first_at: DateTime<Utc> = sqlx::query_scalar(
            "SELECT occurred_at FROM cognitive_hypervisor_timeline WHERE id = $1",
        )
        .bind(event_a)
        .fetch_one(&pool)
        .await
        .unwrap();
        let second_at: DateTime<Utc> = sqlx::query_scalar(
            "SELECT occurred_at FROM cognitive_hypervisor_timeline WHERE id = $1",
        )
        .bind(event_b)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(
            first_at < second_at,
            "snapshot events should have distinct occurred_at values"
        );

        let midpoint_micros = (second_at - first_at).num_microseconds().unwrap() / 2;
        let between_at = first_at + chrono::Duration::microseconds(midpoint_micros);

        let between = resolve_snapshot_at(&state, AGENT, between_at)
            .await
            .unwrap()
            .expect("earlier snapshot should resolve between events");
        assert_eq!(between.snapshot_id, snapshot_a);
        assert_eq!(between.occurred_at, first_at);
        assert_eq!(between.event_type, "snapshot_created");

        let after = resolve_snapshot_at(&state, AGENT, second_at + chrono::Duration::seconds(1))
            .await
            .unwrap()
            .expect("later snapshot should resolve after both events");
        assert_eq!(after.snapshot_id, snapshot_b);
        assert_eq!(after.occurred_at, second_at);
        assert_eq!(after.event_type, "snapshot_created");

        let before = resolve_snapshot_at(&state, AGENT, first_at - chrono::Duration::seconds(1))
            .await
            .unwrap();
        assert!(
            before.is_none(),
            "no snapshot should resolve before any event"
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn timeline_is_append_only(pool: sqlx::PgPool) {
        let state = build_state(pool.clone(), 0.0);
        const AGENT: &str = "append-only-timeline-agent";

        for _ in 0..3 {
            record_hypervisor_event(
                &state,
                AGENT,
                None,
                Some(Uuid::new_v4()),
                None,
                None,
                None,
                "snapshot_created",
            )
            .await
            .unwrap();
        }

        let first_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*)::bigint FROM cognitive_hypervisor_timeline WHERE agent_id = $1",
        )
        .bind(AGENT)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(first_count, 3);

        record_hypervisor_event(
            &state,
            AGENT,
            Some("session-c"),
            None,
            Some("sha256:proof-capsule"),
            None,
            None,
            "proof_capsule_emitted",
        )
        .await
        .unwrap();

        let second_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*)::bigint FROM cognitive_hypervisor_timeline WHERE agent_id = $1",
        )
        .bind(AGENT)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(second_count, 4);
        assert!(second_count > first_count);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn timeline_branches_are_append_only_and_resolve_independently(pool: sqlx::PgPool) {
        let state = build_state(pool.clone(), 0.0);
        const AGENT: &str = "branch-timeline-agent";
        const BRANCH: &str = "branch/replay-001";

        let main_snapshot = Uuid::new_v4();
        let branch_snapshot = Uuid::new_v4();

        record_hypervisor_event(
            &state,
            AGENT,
            Some("session-main"),
            Some(main_snapshot),
            Some("sha256:main-event"),
            None,
            None,
            "snapshot_created",
        )
        .await
        .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let branch_event = record_hypervisor_event(
            &state,
            AGENT,
            Some("session-branch"),
            Some(branch_snapshot),
            Some("sha256:branch-event"),
            Some("sha256:main-event"),
            Some(BRANCH),
            "time_travel_branch_created",
        )
        .await
        .unwrap();

        let branch_row: (Option<String>, Option<String>) = sqlx::query_as(
            "SELECT prev_event_digest, branch_id
             FROM cognitive_hypervisor_timeline
             WHERE id = $1",
        )
        .bind(branch_event)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(branch_row.0.as_deref(), Some("sha256:main-event"));
        assert_eq!(branch_row.1.as_deref(), Some(BRANCH));

        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*)::bigint FROM cognitive_hypervisor_timeline WHERE agent_id = $1",
        )
        .bind(AGENT)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(count, 2, "branch creation must append instead of rewriting");

        let branch_at: DateTime<Utc> = sqlx::query_scalar(
            "SELECT occurred_at FROM cognitive_hypervisor_timeline WHERE id = $1",
        )
        .bind(branch_event)
        .fetch_one(&pool)
        .await
        .unwrap();

        let mainline = resolve_snapshot_at(&state, AGENT, branch_at + chrono::Duration::seconds(1))
            .await
            .unwrap()
            .expect("mainline snapshot should remain resolvable");
        assert_eq!(mainline.snapshot_id, main_snapshot);
        assert!(
            mainline.branch_id.is_none(),
            "mainline resolution must ignore branch rows"
        );

        let branch = resolve_at_branch(
            &state,
            AGENT,
            BRANCH,
            branch_at + chrono::Duration::seconds(1),
        )
        .await
        .unwrap()
        .expect("branch snapshot should resolve on its branch");
        assert_eq!(branch.snapshot_id, branch_snapshot);
        assert_eq!(branch.branch_id.as_deref(), Some(BRANCH));
        assert_eq!(
            branch.prev_event_digest.as_deref(),
            Some("sha256:main-event")
        );

        let missing = resolve_at_branch(
            &state,
            AGENT,
            "branch/unknown",
            branch_at + chrono::Duration::seconds(1),
        )
        .await
        .unwrap();
        assert!(missing.is_none(), "unknown branches should not resolve");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn time_travel_branches_does_not_prune_memory_versions(pool: sqlx::PgPool) {
        let state = build_state(pool.clone(), 0.0);
        const AGENT: &str = "time-travel-branch-agent";

        let id = store_memory(
            &state,
            AGENT,
            None,
            "branchable fact v1",
            "semantic",
            0.9,
            unit_vec(0),
            None,
            "user_stated",
            0.5_f32,
            "extractor",
        )
        .await
        .unwrap();
        update_memory_content(&state, id, "branchable fact v2", unit_vec(1))
            .await
            .unwrap();

        let before_restore: i64 =
            sqlx::query_scalar("SELECT COUNT(*)::bigint FROM memory_versions WHERE memory_id = $1")
                .bind(id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(
            before_restore >= 2,
            "initial insert plus update should create at least two versions"
        );

        tombstone_memories(&state, &[id]).await.unwrap();
        let restored = restore_memory(&state, id).await.unwrap();
        assert!(restored, "tombstoned memory should restore successfully");

        let after_restore: i64 =
            sqlx::query_scalar("SELECT COUNT(*)::bigint FROM memory_versions WHERE memory_id = $1")
                .bind(id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(
            after_restore >= before_restore,
            "restore must not prune memory_versions history"
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn resolve_snapshot_at_ignores_rows_without_snapshot_id(pool: sqlx::PgPool) {
        let state = build_state(pool, 0.0);
        const AGENT: &str = "null-snapshot-timeline-agent";

        record_hypervisor_event(
            &state,
            AGENT,
            Some("session-a"),
            None,
            None,
            None,
            None,
            "capability_denied",
        )
        .await
        .unwrap();

        let resolution = resolve_snapshot_at(&state, AGENT, Utc::now())
            .await
            .unwrap();
        assert!(
            resolution.is_none(),
            "capability_denied rows without snapshots must not resolve"
        );
    }

    // ── 20-turn retention ─────────────────────────────────────────────────────

    /// A fact stored at turn 3 is still retrievable by vector similarity at turn
    /// 20, even after 17 unrelated facts have been stored in between.
    #[sqlx::test(migrations = "./migrations")]
    async fn retention_across_20_turns(pool: sqlx::PgPool) {
        let state = build_state(pool, 0.0);
        const AGENT: &str = "retention-agent";

        // Turn 3: key fact — embedding lives at dim 0
        store_memory(
            &state,
            AGENT,
            Some("s1"),
            "Alex is building NovaPay",
            "episodic",
            0.95,
            unit_vec(0),
            Some(3),
            "user_stated",
            0.5_f32,
            "extractor",
        )
        .await
        .unwrap();

        // Turn 5: second key fact — embedding at dim 1
        store_memory(
            &state,
            AGENT,
            Some("s1"),
            "NovaPay processes cross-border payments",
            "semantic",
            0.95,
            unit_vec(1),
            Some(5),
            "user_stated",
            0.5_f32,
            "extractor",
        )
        .await
        .unwrap();

        // Turns 1-2, 4, 6-20: noise in unrelated dimensions
        for turn in (1i32..=20).filter(|&t| t != 3 && t != 5) {
            store_memory(
                &state,
                AGENT,
                Some("s1"),
                &format!("Noise chatter at turn {}", turn),
                "episodic",
                0.80,
                unit_vec(((turn as usize) % 1500) + 20),
                Some(turn),
                "assistant_derived",
                0.5_f32,
                "extractor",
            )
            .await
            .unwrap();
        }

        // At turn 20: query using the NovaPay vector (dim 0, exact match)
        let results = search_memories_filtered(&state, AGENT, &unit_vec(0), 5, 0.10, None, None)
            .await
            .unwrap();

        assert_eq!(results.len(), 1, "should find exactly the NovaPay fact");
        assert!(
            results[0].content.contains("NovaPay"),
            "top result should be the turn-3 NovaPay fact, got: {:?}",
            results[0].content
        );
        assert_eq!(results[0].source_turn, Some(3));
    }

    // ── Decay scoring ─────────────────────────────────────────────────────────
    //
    // Setup:
    //   query  = [1.0, 0.0, ...]           (unit vector at dim 0)
    //   fresh  = [0.8, 0.6,  0.0, ...]     cosine dist ≈ 0.20 from query
    //   stale  = [0.9, 0.0,  0.4359, ...]  cosine dist ≈ 0.10 from query
    //
    // Without decay: stale ranks first (lower cosine dist 0.10 < 0.20).
    // With exponential decay_rate=0.05 and stale aged 30 days:
    //   adjusted_dist(stale) = 0.10 * exp(0.05 * 30) ≈ 0.45
    //   adjusted_dist(fresh) = 0.20 * exp(0)         = 0.20  ← wins

    #[sqlx::test(migrations = "./migrations")]
    async fn decay_reorders_stale_memories(pool: sqlx::PgPool) {
        const AGENT: &str = "decay-agent";
        // sqrt(1 - 0.81) = sqrt(0.19) ≈ 0.4359
        let fresh_emb = two_hot(0, 0.8, 1, 0.6);
        let stale_emb = two_hot(0, 0.9, 2, 0.4359);
        let query = unit_vec(0);

        let stale_id = {
            let state = build_state(pool.clone(), 0.0);
            store_memory(
                &state,
                AGENT,
                None,
                "fresh fact",
                "episodic",
                0.9,
                fresh_emb,
                None,
                "user_stated",
                0.5_f32,
                "extractor",
            )
            .await
            .unwrap();
            store_memory(
                &state,
                AGENT,
                None,
                "stale fact",
                "episodic",
                0.9,
                stale_emb,
                None,
                "user_stated",
                0.5_f32,
                "extractor",
            )
            .await
            .unwrap()
        };

        // Artificially age the stale memory by 30 days
        sqlx::query(
            "UPDATE memories SET last_accessed_at = NOW() - INTERVAL '30 days' WHERE id = $1",
        )
        .bind(stale_id)
        .execute(&pool)
        .await
        .unwrap();

        // Without decay: stale (cosine dist 0.10) ranks ahead of fresh (0.20)
        let no_decay = build_state(pool.clone(), 0.0);
        let results = search_memories_filtered(&no_decay, AGENT, &query, 2, 1.0, None, None)
            .await
            .unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(
            results[0].content, "stale fact",
            "without decay: lower cosine dist should rank first"
        );

        // With exponential decay (rate 0.05): stale adjusted dist ≈ 0.45, fresh = 0.20
        let with_decay = build_state(pool, 0.05);
        let results = search_memories_filtered(&with_decay, AGENT, &query, 2, 5.0, None, None)
            .await
            .unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(
            results[0].content, "fresh fact",
            "with decay: fresh should rank first despite higher base cosine distance"
        );
    }

    // ── Importance-weighted retrieval ─────────────────────────────────────────
    //
    // Same embedding, different importance scores:
    //   high-importance (score=0.9) vs low-importance (score=0.1)
    //   With boost_factor=2.0:
    //     high: dist_adj = D * (1 + 2*0.1) = D*1.2  ← ranks first
    //     low:  dist_adj = D * (1 + 2*0.9) = D*2.8  ← ranks second
    // Both have the same base cosine distance so the importance factor decides.

    #[sqlx::test(migrations = "./migrations")]
    async fn importance_weighted_retrieval(pool: sqlx::PgPool) {
        const AGENT: &str = "test-importance";
        let emb_hi = two_hot(0, 0.8, 1, 0.6);
        let emb_lo = two_hot(0, 0.8, 1, 0.6); // same embedding — only importance differs
        let query = unit_vec(0);

        {
            let state = build_state(pool.clone(), 0.0);
            store_memory(
                &state,
                AGENT,
                None,
                "high importance fact",
                "episodic",
                0.9,
                emb_hi,
                None,
                "user_stated",
                0.9_f32,
                "user_stated",
            )
            .await
            .unwrap();
            store_memory(
                &state,
                AGENT,
                None,
                "low importance fact",
                "episodic",
                0.9,
                emb_lo,
                None,
                "user_stated",
                0.1_f32,
                "extractor",
            )
            .await
            .unwrap();
        }

        // Without importance boost: both have same adjusted dist — both present
        {
            let state = build_state(pool.clone(), 0.0);
            let results = search_memories_filtered(&state, AGENT, &query, 2, 1.0, None, None)
                .await
                .unwrap();
            assert_eq!(
                results.len(),
                2,
                "both facts should be returned without boost"
            );
        }

        // With importance boost=2.0: high-importance should rank first
        {
            let state = build_state_with_importance(pool.clone(), 0.0, 2.0);
            let results = search_memories_filtered(&state, AGENT, &query, 2, 5.0, None, None)
                .await
                .unwrap();
            assert_eq!(results.len(), 2, "both facts should be returned with boost");
            assert_eq!(
                results[0].content, "high importance fact",
                "high-importance should rank first with boost enabled"
            );
        }
    }

    // ── Archival batch round-trip ─────────────────────────────────────────────
    //
    // Simulates the full L2→L3 compaction + restore cycle:
    //   1. Store two L2 memories.
    //   2. Create an archival batch and tombstone the L2 memories.
    //   3. Store two L3 facts tagged with the batch_id.
    //   4. Verify the L2 memories are excluded from live search/list.
    //   5. Call restore_archival_batch.
    //   6. Verify L2 memories are live again and L3 facts are tombstoned.

    #[sqlx::test(migrations = "./migrations")]
    async fn archival_batch_roundtrip(pool: sqlx::PgPool) {
        let state = build_state(pool.clone(), 0.0);
        const AGENT: &str = "batch-roundtrip-agent";

        upsert_agent(&state, AGENT).await.unwrap();

        let id_a = store_memory(
            &state,
            AGENT,
            None,
            "L2 fact A",
            "episodic",
            0.9,
            unit_vec(0),
            None,
            "user_stated",
            0.5,
            "extractor",
        )
        .await
        .unwrap();
        let id_b = store_memory(
            &state,
            AGENT,
            None,
            "L2 fact B",
            "episodic",
            0.9,
            unit_vec(1),
            None,
            "user_stated",
            0.5,
            "extractor",
        )
        .await
        .unwrap();

        // Both L2 memories are live
        let before = list_memories_for_agent(&state, AGENT, 10, 0).await.unwrap();
        assert_eq!(
            before.len(),
            2,
            "both L2 facts should be live before archival"
        );

        let batch_id = create_archival_batch(&state, AGENT, 2, 2).await.unwrap();

        // Tombstone L2 memories with batch
        tombstone_memories_with_batch(&state, &[id_a, id_b], batch_id)
            .await
            .unwrap();

        // Store L3 compressed facts tagged with the same batch
        let l3_a = store_memory_with_tier(
            &state,
            AGENT,
            None,
            "L3 compressed fact A",
            "semantic",
            0.7,
            unit_vec(10),
            None,
            "L3",
            "inferred",
            0.5,
            "extractor",
            Some(batch_id),
        )
        .await
        .unwrap();
        let l3_b = store_memory_with_tier(
            &state,
            AGENT,
            None,
            "L3 compressed fact B",
            "semantic",
            0.7,
            unit_vec(11),
            None,
            "L3",
            "inferred",
            0.5,
            "extractor",
            Some(batch_id),
        )
        .await
        .unwrap();

        // L2 memories excluded after tombstone
        let after_archival = list_memories_for_agent(&state, AGENT, 10, 0).await.unwrap();
        assert!(
            after_archival
                .iter()
                .all(|m| !["L2 fact A", "L2 fact B"].contains(&m.content.as_str())),
            "tombstoned L2 facts must not appear in live list"
        );

        // Restore the batch
        let result = restore_archival_batch(&state, batch_id).await.unwrap();
        let result = result.expect("restore should succeed for a completed batch");
        assert_eq!(result.l2_restored, 2, "should restore 2 L2 memories");
        assert_eq!(result.l3_tombstoned, 2, "should tombstone 2 L3 facts");

        // L2 memories are live again
        let after_restore = list_memories_for_agent(&state, AGENT, 10, 0).await.unwrap();
        let contents: Vec<&str> = after_restore.iter().map(|m| m.content.as_str()).collect();
        assert!(
            contents.contains(&"L2 fact A"),
            "L2 fact A must be live after restore"
        );
        assert!(
            contents.contains(&"L2 fact B"),
            "L2 fact B must be live after restore"
        );

        // L3 facts are tombstoned (not in live list)
        assert!(
            after_restore.iter().all(|m| m.id != l3_a && m.id != l3_b),
            "L3 facts must be tombstoned after restore"
        );

        // Batch status is 'restored'
        let batch: Vec<ArchivalBatch> = list_archival_batches(&state, AGENT, 10, 0).await.unwrap();
        assert_eq!(batch.len(), 1);
        assert_eq!(batch[0].status, "restored");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn restore_already_restored_batch_returns_none(pool: sqlx::PgPool) {
        let state = build_state(pool, 0.0);
        const AGENT: &str = "double-restore-agent";

        upsert_agent(&state, AGENT).await.unwrap();

        let id = store_memory(
            &state,
            AGENT,
            None,
            "L2 memory",
            "episodic",
            0.9,
            unit_vec(0),
            None,
            "user_stated",
            0.5,
            "extractor",
        )
        .await
        .unwrap();

        let batch_id = create_archival_batch(&state, AGENT, 1, 1).await.unwrap();
        tombstone_memories_with_batch(&state, &[id], batch_id)
            .await
            .unwrap();
        store_memory_with_tier(
            &state,
            AGENT,
            None,
            "L3 fact",
            "semantic",
            0.7,
            unit_vec(5),
            None,
            "L3",
            "inferred",
            0.5,
            "extractor",
            Some(batch_id),
        )
        .await
        .unwrap();

        // First restore succeeds
        let first = restore_archival_batch(&state, batch_id).await.unwrap();
        assert!(first.is_some(), "first restore must succeed");

        // Second restore returns None (already restored)
        let second = restore_archival_batch(&state, batch_id).await.unwrap();
        assert!(
            second.is_none(),
            "restoring an already-restored batch must return None"
        );
    }

    // ── Retrieval plan regression ────────────────────────────────────────────

    /// The two-stage retrieval query must be served by the HNSW index.  The
    /// ANN stage's ORDER BY uses the bare `embedding <=> vec` operator with a
    /// LIMIT precisely so pgvector can use `idx_memories_hnsw`; wrapping the
    /// operator in decay/importance arithmetic silently falls back to a
    /// sequential scan whose latency is linear in corpus size (observed at
    /// 95 ms p50 for 10k memories vs ~3 ms index-served).  This test EXPLAINs
    /// the exact SQL the hot path builds and fails if the plan ever loses the
    /// index scan.
    #[sqlx::test(migrations = "./migrations")]
    async fn retrieval_plan_uses_hnsw_index(pool: sqlx::PgPool) {
        use sqlx::Row;

        let state = build_state(pool, 0.0);
        const AGENT: &str = "explain-agent";

        sqlx::query("INSERT INTO agents (agent_id) VALUES ($1) ON CONFLICT DO NOTHING")
            .bind(AGENT)
            .execute(&state.db)
            .await
            .unwrap();

        // Seed 1,500 rows with per-row random embeddings in one statement
        // (the LATERAL's `WHERE g > 0` forces per-row evaluation).
        sqlx::query(
            r#"INSERT INTO memories
                 (agent_id, session_id, content, memory_type, confidence, embedding,
                  tier, provenance, importance_score, importance_source, status, sensitivity)
               SELECT $1, 'seed', 'memory ' || g, 'semantic', 1.0, v.vec,
                      'L2', 'user_stated', 0.5, 'user_stated', 'active', 'unknown'
               FROM generate_series(1, 1500) g
               CROSS JOIN LATERAL (
                   SELECT ('[' || string_agg(random()::text, ',') || ']')::vector AS vec
                   FROM generate_series(1, 1536) WHERE g > 0
               ) v"#,
        )
        .bind(AGENT)
        .execute(&state.db)
        .await
        .unwrap();
        sqlx::query("ANALYZE memories")
            .execute(&state.db)
            .await
            .unwrap();

        let query_vec = unit_vec(3);

        // Behavioral check through the real entry point: results come back
        // ordered with distances present.
        let rows = search_memories_filtered(&state, AGENT, &query_vec, 5, 2.0, None, None)
            .await
            .unwrap();
        assert_eq!(rows.len(), 5, "expected 5 results from seeded corpus");

        // Plan check on the exact SQL the hot path builds.  The property under
        // test is that the ANN stage is *index-servable*: pgvector can only
        // use the HNSW index when the ORDER BY is the bare `<=>` operator.
        // On a small test corpus the cost model may still prefer a seq scan,
        // so disable it for the EXPLAIN — a shape regression (e.g. wrapping
        // the operator in arithmetic again) produces a seq scan even with
        // seqscan disabled, which is exactly what the assertion catches.
        let mut qb = build_search_query(
            "EXPLAIN ",
            Vector::from(query_vec),
            AGENT,
            5,
            2.0,
            100,
            0.0,
            0.0,
            None,
            None,
            true,
        );
        let mut tx = state.db.begin().await.unwrap();
        // Ban every non-index path for the ANN stage: if the query shape is
        // index-servable the planner must now pick the HNSW scan (the outer
        // re-rank Sort survives on disable-cost, which is fine); if the shape
        // regresses, a scan+sort still appears and the assertions fail.
        for knob in [
            "SET LOCAL enable_seqscan = off",
            "SET LOCAL enable_bitmapscan = off",
            "SET LOCAL enable_sort = off",
        ] {
            sqlx::query(knob).execute(&mut *tx).await.unwrap();
        }
        let plan_rows = qb.build().fetch_all(&mut *tx).await.unwrap();
        tx.rollback().await.unwrap();
        let plan: String = plan_rows
            .iter()
            .map(|r| r.get::<String, _>(0))
            .collect::<Vec<_>>()
            .join("\n");

        assert!(
            plan.contains("Index Scan using idx_memories_hnsw"),
            "retrieval plan no longer uses the HNSW index:\n{plan}"
        );
        assert!(
            !plan.contains("Seq Scan on memories"),
            "retrieval plan regressed to a sequential scan:\n{plan}"
        );
    }
}
