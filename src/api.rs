use axum::{
    body::Body,
    extract::{Path, Query, State},
    http::{header, StatusCode},
    response::{Json, Response},
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use uuid::Uuid;

use crate::{
    archival,
    embeddings::{embed_text, embed_texts},
    memory::store,
    models::{
        ArchivalBatch, ArchivedMemory, Memory, MemoryConflict, MemoryExportRow, RelationRow,
        SessionInfo, WorkingMemory,
    },
    AppState,
};

// ── Shared response shapes ────────────────────────────────────────────────────

#[derive(Serialize)]
pub struct AgentInfo {
    pub agent_id: String,
    pub memory_count: i64,
}

#[derive(Serialize)]
pub struct AgentListResponse {
    pub agents: Vec<AgentInfo>,
    pub total: i64,
}

#[derive(Serialize)]
pub struct MemoryDto {
    pub id: String,
    pub content: String,
    pub memory_type: String,
    pub confidence: f32,
    pub provenance: String,
    pub created_at: String,
    pub updated_at: String,
    pub session_id: Option<String>,
    pub source_turn: Option<i32>,
    pub importance_score: f32,
    pub importance_source: String,
    pub status: String,
    pub sensitivity: String,
    pub valid_from: Option<String>,
    pub valid_to: Option<String>,
    pub suppression_reason: Option<String>,
}

impl From<Memory> for MemoryDto {
    fn from(m: Memory) -> Self {
        Self {
            id: m.id.to_string(),
            content: m.content,
            memory_type: m.memory_type,
            confidence: m.confidence,
            provenance: m.provenance,
            created_at: m.created_at.to_rfc3339(),
            updated_at: m.updated_at.to_rfc3339(),
            session_id: m.session_id,
            source_turn: m.source_turn,
            importance_score: m.importance_score,
            importance_source: m.importance_source,
            status: if m.status.is_empty() {
                "active".to_string()
            } else {
                m.status
            },
            sensitivity: if m.sensitivity.is_empty() {
                "unknown".to_string()
            } else {
                m.sensitivity
            },
            valid_from: m.valid_from.map(|t| t.to_rfc3339()),
            valid_to: m.valid_to.map(|t| t.to_rfc3339()),
            suppression_reason: m.suppression_reason,
        }
    }
}

#[derive(Serialize)]
pub struct MemoryListResponse {
    pub memories: Vec<MemoryDto>,
    pub total: i64,
    pub offset: i64,
    pub limit: i64,
}

#[derive(Serialize)]
pub struct TimeTravelMemoryDto {
    pub id: String,
    pub version_number: i32,
    pub content: String,
    pub memory_type: String,
    pub confidence: f32,
    pub provenance: String,
    pub importance_score: f32,
    pub importance_source: String,
    pub status: String,
    pub sensitivity: String,
    pub valid_from: Option<String>,
    pub valid_to: Option<String>,
    pub source_turn: Option<i32>,
    pub created_at: String,
    pub version_created_at: String,
}

#[derive(Serialize)]
pub struct TimeTravelResponse {
    pub timestamp: String,
    pub memories: Vec<TimeTravelMemoryDto>,
    pub total: i64,
    pub offset: i64,
    pub limit: i64,
}

#[derive(Serialize)]
pub struct ArchivedDiffDto {
    pub memory_id: String,
    pub content: String,
    pub memory_type: String,
    pub archived_at: String,
}

#[derive(Serialize)]
pub struct ModifiedMemoryDto {
    pub memory_id: String,
    pub before: TimeTravelMemoryDto,
    pub after: TimeTravelMemoryDto,
}

#[derive(Serialize)]
pub struct StatusChangedDto {
    pub memory_id: String,
    pub old_status: String,
    pub new_status: String,
}

#[derive(Serialize)]
pub struct RetrievalActivityDto {
    pub total_retrievals: i64,
    pub unique_memories_recalled: i64,
}

#[derive(Serialize)]
pub struct MemoryDiffSummaryDto {
    pub added: usize,
    pub modified: usize,
    pub archived: usize,
    pub status_changed: usize,
    pub total_retrievals: i64,
    pub unique_memories_recalled: i64,
}

#[derive(Serialize)]
pub struct MemoryDiffResponse {
    pub from: String,
    pub to: String,
    pub summary: MemoryDiffSummaryDto,
    pub added: Vec<TimeTravelMemoryDto>,
    pub modified: Vec<ModifiedMemoryDto>,
    pub archived: Vec<ArchivedDiffDto>,
    pub status_changed: Vec<StatusChangedDto>,
    pub retrieval_activity: RetrievalActivityDto,
}

#[derive(Deserialize)]
pub struct HypervisorTimelineEventBody {
    pub session_id: Option<String>,
    pub nexus_snapshot_id: Option<Uuid>,
    pub capsule_digest: Option<String>,
    pub branch_id: Option<String>,
    pub event_type: String,
}

#[derive(Deserialize)]
pub struct TimelineAtQuery {
    pub timestamp: DateTime<Utc>,
    pub branch_id: Option<String>,
}

#[derive(Serialize)]
pub struct HypervisorTimelineRecordResponse {
    pub id: String,
    pub recorded: bool,
}

#[derive(Serialize)]
pub struct SnapshotResolutionResponse {
    pub snapshot_id: String,
    pub occurred_at: String,
    pub event_type: String,
    pub branch_id: Option<String>,
    pub prev_event_digest: Option<String>,
}

#[derive(Serialize)]
pub struct ArchivedMemoryDto {
    pub id: String,
    pub content: String,
    pub memory_type: String,
    pub confidence: f32,
    pub provenance: String,
    pub created_at: String,
    pub session_id: Option<String>,
    pub source_turn: Option<i32>,
    pub importance_score: f32,
    pub importance_source: String,
    pub archived_at: String,
}

impl From<ArchivedMemory> for ArchivedMemoryDto {
    fn from(m: ArchivedMemory) -> Self {
        Self {
            id: m.id.to_string(),
            content: m.content,
            memory_type: m.memory_type,
            confidence: m.confidence,
            provenance: m.provenance,
            created_at: m.created_at.to_rfc3339(),
            session_id: m.session_id,
            source_turn: m.source_turn,
            importance_score: m.importance_score,
            importance_source: m.importance_source,
            archived_at: m.archived_at.to_rfc3339(),
        }
    }
}

#[derive(Serialize)]
pub struct ArchivedMemoryListResponse {
    pub memories: Vec<ArchivedMemoryDto>,
    pub total: i64,
}

// ── Archival batch types ──────────────────────────────────────────────────────

#[derive(Serialize)]
pub struct ArchivalBatchDto {
    pub id: String,
    pub agent_id: String,
    pub created_at: String,
    pub source_count: i32,
    pub l3_count: i32,
    pub status: String,
}

impl From<ArchivalBatch> for ArchivalBatchDto {
    fn from(b: ArchivalBatch) -> Self {
        Self {
            id: b.id.to_string(),
            agent_id: b.agent_id,
            created_at: b.created_at.to_rfc3339(),
            source_count: b.source_count,
            l3_count: b.l3_count,
            status: b.status,
        }
    }
}

#[derive(Serialize)]
pub struct ArchivalBatchListResponse {
    pub batches: Vec<ArchivalBatchDto>,
    pub total: i64,
}

#[derive(Serialize)]
pub struct StatsResponse {
    pub agent_count: i64,
    pub memory_count: i64,
    pub tokens_saved_estimate: i64,
}

// ── Session types ─────────────────────────────────────────────────────────────

#[derive(Serialize)]
pub struct SessionDto {
    pub session_id: String,
    pub turn_count: i32,
    pub updated_at: String,
    pub summary_preview: Option<String>,
}

impl From<SessionInfo> for SessionDto {
    fn from(s: SessionInfo) -> Self {
        Self {
            session_id: s.session_id,
            turn_count: s.turn_count,
            updated_at: s.updated_at.to_rfc3339(),
            summary_preview: s.summary_preview,
        }
    }
}

#[derive(Serialize)]
pub struct SessionDetailDto {
    pub session_id: String,
    pub turn_count: i32,
    pub updated_at: String,
    pub summary: Option<String>,
}

impl From<WorkingMemory> for SessionDetailDto {
    fn from(w: WorkingMemory) -> Self {
        Self {
            session_id: w.session_id,
            turn_count: w.turn_count,
            updated_at: w.updated_at.to_rfc3339(),
            summary: w.summary,
        }
    }
}

#[derive(Serialize)]
pub struct SessionListResponse {
    pub sessions: Vec<SessionDto>,
    pub total: usize,
}

// ── Query params / bodies ─────────────────────────────────────────────────────

#[derive(Deserialize, Default)]
pub struct Pagination {
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

#[derive(Deserialize)]
pub struct TimeTravelQuery {
    pub timestamp: DateTime<Utc>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

#[derive(Deserialize)]
pub struct MemoryDiffQuery {
    pub from: DateTime<Utc>,
    pub to: DateTime<Utc>,
}

fn to_time_travel_dto(row: &store::TemporalMemoryVersion) -> TimeTravelMemoryDto {
    TimeTravelMemoryDto {
        id: row.memory_id.to_string(),
        version_number: row.version_number,
        content: row.content.clone(),
        memory_type: row.memory_type.clone(),
        confidence: row.confidence,
        provenance: row.provenance.clone(),
        importance_score: row.importance_score,
        importance_source: row.importance_source.clone(),
        status: row.status.clone(),
        sensitivity: row.sensitivity.clone(),
        valid_from: row.valid_from.map(|t| t.to_rfc3339()),
        valid_to: row.valid_to.map(|t| t.to_rfc3339()),
        source_turn: row.source_turn,
        created_at: row.memory_created_at.to_rfc3339(),
        version_created_at: row.version_created_at.to_rfc3339(),
    }
}

fn is_known_hypervisor_event(event_type: &str) -> bool {
    matches!(
        event_type,
        "snapshot_created"
            | "capability_denied"
            | "proof_capsule_emitted"
            | "time_travel_branch_created"
    )
}

#[derive(Deserialize)]
pub struct CreateMemoryBody {
    pub content: String,
    pub memory_type: Option<String>,
}

#[derive(Deserialize)]
pub struct PatchMemoryBody {
    pub content: String,
}

// ── Semantic search types ─────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct MemorySearchRequest {
    pub agent_id: String,
    pub query: String,
    /// Maximum results (default 20, max 100).
    pub limit: Option<i64>,
    /// Cosine distance upper bound — lower = more similar (default 0.80).
    pub threshold: Option<f64>,
    /// Filter by memory_type (episodic | semantic | procedural).
    pub memory_type: Option<String>,
    /// Filter to a specific session.
    pub session_id: Option<String>,
    /// When true, include subject–predicate–object relations in the response.
    pub include_relations: Option<bool>,
}

#[derive(Serialize)]
pub struct SearchResult {
    pub id: String,
    pub content: String,
    pub memory_type: String,
    pub confidence: f32,
    /// 1.0 = identical, 0.0 = maximally dissimilar.
    pub similarity: f32,
    pub created_at: String,
    pub source_turn: Option<i32>,
    pub session_id: Option<String>,
    pub importance_score: f32,
}

#[derive(Serialize)]
pub struct RelationDto {
    pub id: String,
    pub subject: String,
    pub predicate: String,
    pub object: String,
    pub confidence: f32,
    pub created_at: String,
}

impl From<RelationRow> for RelationDto {
    fn from(r: RelationRow) -> Self {
        Self {
            id: r.id.to_string(),
            subject: r.subject,
            predicate: r.predicate,
            object: r.object,
            confidence: r.confidence,
            created_at: r.created_at.to_rfc3339(),
        }
    }
}

#[derive(Serialize)]
pub struct MemorySearchResponse {
    pub results: Vec<SearchResult>,
    pub relations: Vec<RelationDto>,
    pub total: usize,
    /// Ed25519 counter-signature over the served hit set (agent, session
    /// filter, hit IDs, hit content digests).  Lets callers — notably the
    /// Nexus hypervisor — verify the hits were not altered in transit before
    /// reporting Attested* memory-evidence modes.
    pub evidence: crate::attestation::EvidenceEnvelope,
}

// ── Handlers ──────────────────────────────────────────────────────────────────

pub async fn list_agents(
    State(state): State<AppState>,
) -> Result<Json<AgentListResponse>, (StatusCode, String)> {
    let rows: Vec<(String,)> =
        sqlx::query_as("SELECT agent_id FROM agents ORDER BY created_at DESC LIMIT 200")
            .fetch_all(&state.db)
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let mut agents = Vec::with_capacity(rows.len());
    for (agent_id,) in &rows {
        let count: (i64,) =
            sqlx::query_as("SELECT COUNT(*)::bigint FROM memories WHERE agent_id = $1")
                .bind(agent_id)
                .fetch_one(&state.db)
                .await
                .unwrap_or((0,));
        agents.push(AgentInfo {
            agent_id: agent_id.clone(),
            memory_count: count.0,
        });
    }

    Ok(Json(AgentListResponse {
        total: agents.len() as i64,
        agents,
    }))
}

pub async fn delete_agent(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
) -> Result<StatusCode, (StatusCode, String)> {
    let deleted = store::delete_agent(&state, &agent_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    if deleted {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err((
            StatusCode::NOT_FOUND,
            format!("agent '{}' not found", agent_id),
        ))
    }
}

pub async fn list_memories(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
    Query(pagination): Query<Pagination>,
) -> Result<Json<MemoryListResponse>, (StatusCode, String)> {
    let limit = pagination.limit.unwrap_or(50).min(200);
    let offset = pagination.offset.unwrap_or(0);

    let memories = store::list_memories_for_agent(&state, &agent_id, limit, offset)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let total: (i64,) = sqlx::query_as("SELECT COUNT(*)::bigint FROM memories WHERE agent_id = $1")
        .bind(&agent_id)
        .fetch_one(&state.db)
        .await
        .unwrap_or((0,));

    Ok(Json(MemoryListResponse {
        total: total.0,
        offset,
        limit,
        memories: memories.into_iter().map(MemoryDto::from).collect(),
    }))
}

/// GET /api/v1/agents/:id/memories/at
///
/// Returns memory state as of a specific timestamp.
pub async fn memories_at_timestamp(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
    Query(query): Query<TimeTravelQuery>,
) -> Result<Json<TimeTravelResponse>, (StatusCode, String)> {
    let limit = query.limit.unwrap_or(50).min(200);
    let offset = query.offset.unwrap_or(0);

    let memories =
        store::list_memories_at_timestamp(&state, &agent_id, query.timestamp, limit, offset)
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let total = store::count_memories_at_timestamp(&state, &agent_id, query.timestamp)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(TimeTravelResponse {
        timestamp: query.timestamp.to_rfc3339(),
        memories: memories.iter().map(to_time_travel_dto).collect(),
        total,
        offset,
        limit,
    }))
}

/// GET /api/v1/agents/:id/memories/diff
///
/// Returns memory changes between two timestamps.
pub async fn memories_diff(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
    Query(query): Query<MemoryDiffQuery>,
) -> Result<Json<MemoryDiffResponse>, (StatusCode, String)> {
    if query.from >= query.to {
        return Err((
            StatusCode::BAD_REQUEST,
            "from must be earlier than to".to_string(),
        ));
    }

    let before_rows = store::list_latest_versions_as_of(&state, &agent_id, query.from)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let after_rows = store::list_latest_versions_as_of(&state, &agent_id, query.to)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let added_rows = store::list_added_memories_between(&state, &agent_id, query.from, query.to)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let archived_rows =
        store::list_archived_memories_between(&state, &agent_id, query.from, query.to)
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let (total_retrievals, unique_memories_recalled) =
        store::retrieval_activity_between(&state, &agent_id, query.from, query.to)
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let before_map: HashMap<Uuid, store::TemporalMemoryVersion> =
        before_rows.into_iter().map(|r| (r.memory_id, r)).collect();
    let after_map: HashMap<Uuid, store::TemporalMemoryVersion> =
        after_rows.into_iter().map(|r| (r.memory_id, r)).collect();

    let mut modified = Vec::new();
    let mut status_changed = Vec::new();

    for (memory_id, after) in &after_map {
        if let Some(before) = before_map.get(memory_id) {
            if before.content != after.content {
                modified.push(ModifiedMemoryDto {
                    memory_id: memory_id.to_string(),
                    before: to_time_travel_dto(before),
                    after: to_time_travel_dto(after),
                });
            }
            if before.status != after.status {
                status_changed.push(StatusChangedDto {
                    memory_id: memory_id.to_string(),
                    old_status: before.status.clone(),
                    new_status: after.status.clone(),
                });
            }
        }
    }

    let added: Vec<TimeTravelMemoryDto> = added_rows.iter().map(to_time_travel_dto).collect();
    let archived: Vec<ArchivedDiffDto> = archived_rows
        .into_iter()
        .map(|r| ArchivedDiffDto {
            memory_id: r.id.to_string(),
            content: r.content,
            memory_type: r.memory_type,
            archived_at: r.archived_at.to_rfc3339(),
        })
        .collect();

    Ok(Json(MemoryDiffResponse {
        from: query.from.to_rfc3339(),
        to: query.to.to_rfc3339(),
        summary: MemoryDiffSummaryDto {
            added: added.len(),
            modified: modified.len(),
            archived: archived.len(),
            status_changed: status_changed.len(),
            total_retrievals,
            unique_memories_recalled,
        },
        added,
        modified,
        archived,
        status_changed,
        retrieval_activity: RetrievalActivityDto {
            total_retrievals,
            unique_memories_recalled,
        },
    }))
}

/// POST /api/v1/agents/:id/timeline
///
/// Records a Nexus Cognitive Hypervisor event in AEON-IQ's append-only ledger.
pub async fn record_hypervisor_timeline_event(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
    Json(body): Json<HypervisorTimelineEventBody>,
) -> Result<Json<HypervisorTimelineRecordResponse>, (StatusCode, String)> {
    if !is_known_hypervisor_event(&body.event_type) {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("unknown hypervisor event_type '{}'", body.event_type),
        ));
    }

    // H4: compute prev_event_digest server-side — do not trust client-supplied value
    let prev_event_digest =
        store::get_latest_event_id(&state, &agent_id, body.session_id.as_deref())
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let id = store::record_hypervisor_event(
        &state,
        &agent_id,
        body.session_id.as_deref(),
        body.nexus_snapshot_id,
        body.capsule_digest.as_deref(),
        prev_event_digest.as_deref(),
        body.branch_id.as_deref(),
        &body.event_type,
    )
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(HypervisorTimelineRecordResponse {
        id: id.to_string(),
        recorded: true,
    }))
}

/// GET /api/v1/agents/:id/timeline/at
///
/// Resolves the latest Nexus snapshot at or before a timestamp.
pub async fn resolve_hypervisor_snapshot_at(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
    Query(query): Query<TimelineAtQuery>,
) -> Result<Json<SnapshotResolutionResponse>, (StatusCode, String)> {
    let resolution = if let Some(branch_id) = query.branch_id.as_deref() {
        store::resolve_at_branch(&state, &agent_id, branch_id, query.timestamp)
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    } else {
        store::resolve_snapshot_at(&state, &agent_id, query.timestamp)
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    };

    if let Some(resolution) = resolution {
        Ok(Json(SnapshotResolutionResponse {
            snapshot_id: resolution.snapshot_id.to_string(),
            occurred_at: resolution.occurred_at.to_rfc3339(),
            event_type: resolution.event_type,
            branch_id: resolution.branch_id,
            prev_event_digest: resolution.prev_event_digest,
        }))
    } else {
        let branch_context = query
            .branch_id
            .as_deref()
            .map(|branch_id| format!(" on branch '{}'", branch_id))
            .unwrap_or_default();
        Err((
            StatusCode::NOT_FOUND,
            format!(
                "no hypervisor snapshot found for agent '{}'{} at or before {}",
                agent_id,
                branch_context,
                query.timestamp.to_rfc3339()
            ),
        ))
    }
}

pub async fn create_memory(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
    Json(body): Json<CreateMemoryBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    store::upsert_agent(&state, &agent_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let embedding = embed_text(&state, &body.content).await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Embedding: {}", e),
        )
    })?;

    let id = store::store_memory(
        &state,
        &agent_id,
        None,
        &body.content,
        body.memory_type.as_deref().unwrap_or("semantic"),
        1.0,
        embedding,
        None,
        "user_stated",
        1.0_f32,
        "user_stated",
    )
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(
        serde_json::json!({ "id": id.to_string(), "success": true }),
    ))
}

pub async fn patch_memory(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<PatchMemoryBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let uuid = Uuid::parse_str(&id).map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;

    // A PATCH re-embeds content for a row that may already carry a
    // sensitivity label — the one path where labeled content could reach the
    // remote provider.  Route private/secret through the local lane (or
    // refuse with 409 when none is configured).
    let sensitivity: Option<String> =
        sqlx::query_scalar("SELECT sensitivity FROM memories WHERE id = $1")
            .bind(uuid)
            .fetch_optional(&state.db)
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let Some(sensitivity) = sensitivity else {
        return Err((StatusCode::NOT_FOUND, format!("memory {} not found", id)));
    };

    let embedding =
        crate::embeddings::embed_text_for_sensitivity(&state, &body.content, &sensitivity)
            .await
            .map_err(|e| {
                if crate::embeddings::is_private_sensitivity(&sensitivity)
                    && state.config.local_embedding_base_url.is_none()
                {
                    (StatusCode::CONFLICT, format!("{}", e))
                } else {
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        format!("Embedding: {}", e),
                    )
                }
            })?;

    let updated = store::update_memory_content(&state, uuid, &body.content, embedding)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    if updated {
        Ok(Json(serde_json::json!({ "updated": true })))
    } else {
        Err((
            StatusCode::NOT_FOUND,
            format!("memory {} not found or archived", id),
        ))
    }
}

pub async fn delete_memory(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let uuid = Uuid::parse_str(&id).map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;

    let deleted = store::delete_memory(&state, uuid)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(serde_json::json!({ "deleted": deleted })))
}

/// POST /api/v1/memories/search
/// Embed the query text, run pgvector HNSW search, return ranked results + optional graph data.
pub async fn search_memories_semantic(
    State(state): State<AppState>,
    Json(req): Json<MemorySearchRequest>,
) -> Result<Json<MemorySearchResponse>, (StatusCode, String)> {
    let limit = req.limit.unwrap_or(20).min(100);
    let threshold = req.threshold.unwrap_or(state.config.retrieval_threshold);

    let embedding = embed_text(&state, &req.query).await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Embedding: {}", e),
        )
    })?;

    let search_start = std::time::Instant::now();
    let rows = store::search_memories_all_sensitivities(
        &state,
        &req.agent_id,
        &embedding,
        limit,
        threshold,
        req.memory_type.as_deref(),
        req.session_id.as_deref(),
    )
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    state
        .metrics
        .vector_search_secs
        .observe(search_start.elapsed().as_secs_f64());

    let relations: Vec<RelationDto> = if req.include_relations.unwrap_or(false) {
        store::get_agent_relations(&state, &req.agent_id, 200)
            .await
            .unwrap_or_default()
            .into_iter()
            .map(RelationDto::from)
            .collect()
    } else {
        vec![]
    };

    let total = rows.len();
    let results: Vec<SearchResult> = rows
        .into_iter()
        .map(|r| SearchResult {
            id: r.id.to_string(),
            content: r.content,
            memory_type: r.memory_type,
            confidence: r.confidence,
            similarity: (1.0 - r.distance.unwrap_or(1.0)) as f32,
            created_at: r.created_at.to_rfc3339(),
            source_turn: r.source_turn,
            session_id: r.session_id,
            importance_score: r.importance_score,
        })
        .collect();

    // Counter-sign exactly what is being served (see attestation.rs).
    let payload = crate::attestation::EvidencePayload::new(
        &req.agent_id,
        req.session_id.as_deref(),
        results.iter().map(|r| (r.id.clone(), r.content.clone())),
    );
    let evidence = state.evidence_signer.sign(&payload).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Evidence signing: {}", e),
        )
    })?;

    Ok(Json(MemorySearchResponse {
        results,
        relations,
        total,
        evidence,
    }))
}

#[derive(Serialize)]
pub struct VerifyingKeyResponse {
    pub version: String,
    pub key_id: String,
    /// False when the key is ephemeral (AEON_EVIDENCE_SIGNING_KEY unset) —
    /// verifiers must not pin an ephemeral key.
    pub persistent: bool,
}

/// Expose the evidence-signature verifying key so operators can provision
/// Nexus (`NEXUS_AEON_VERIFYING_KEY`) and doctor scripts can cross-check it.
pub async fn get_evidence_verifying_key(
    State(state): State<AppState>,
) -> Json<VerifyingKeyResponse> {
    Json(VerifyingKeyResponse {
        version: crate::attestation::EVIDENCE_SIG_VERSION.to_string(),
        key_id: state.evidence_signer.key_id().to_string(),
        persistent: state.evidence_signer.persistent,
    })
}

pub async fn get_stats(
    State(state): State<AppState>,
) -> Result<Json<StatsResponse>, (StatusCode, String)> {
    let agent_count = store::count_agents(&state)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let memory_count = store::count_memories(&state)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(StatsResponse {
        agent_count,
        memory_count,
        tokens_saved_estimate: memory_count * 200,
    }))
}

pub async fn list_archived_memories(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
    Query(pagination): Query<Pagination>,
) -> Result<Json<ArchivedMemoryListResponse>, (StatusCode, String)> {
    let limit = pagination.limit.unwrap_or(50).min(200);
    let offset = pagination.offset.unwrap_or(0);

    let memories = store::list_archived_memories(&state, &agent_id, limit, offset)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let total: (i64,) = sqlx::query_as(
        "SELECT COUNT(*)::bigint FROM memories WHERE agent_id = $1 AND archived_at IS NOT NULL",
    )
    .bind(&agent_id)
    .fetch_one(&state.db)
    .await
    .unwrap_or((0,));

    Ok(Json(ArchivedMemoryListResponse {
        total: total.0,
        memories: memories.into_iter().map(ArchivedMemoryDto::from).collect(),
    }))
}

pub async fn restore_memory(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let uuid = Uuid::parse_str(&id).map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;

    let restored = store::restore_memory(&state, uuid)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    if restored {
        Ok(Json(serde_json::json!({ "restored": true })))
    } else {
        Err((
            StatusCode::NOT_FOUND,
            format!("memory {} not found or not archived", id),
        ))
    }
}

// ── Archival batch handlers ───────────────────────────────────────────────────

pub async fn list_archival_batches(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
    Query(pagination): Query<Pagination>,
) -> Result<Json<ArchivalBatchListResponse>, (StatusCode, String)> {
    let limit = pagination.limit.unwrap_or(50).min(200);
    let offset = pagination.offset.unwrap_or(0);

    let batches = store::list_archival_batches(&state, &agent_id, limit, offset)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let total = store::count_archival_batches(&state, &agent_id)
        .await
        .unwrap_or(batches.len() as i64);

    Ok(Json(ArchivalBatchListResponse {
        total,
        batches: batches.into_iter().map(ArchivalBatchDto::from).collect(),
    }))
}

pub async fn restore_archival_batch(
    State(state): State<AppState>,
    Path(batch_id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let uuid = Uuid::parse_str(&batch_id).map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;

    let result = store::restore_archival_batch(&state, uuid)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    match result {
        Some(r) => Ok(Json(serde_json::json!({
            "restored": true,
            "batch_id": batch_id,
            "l2_restored": r.l2_restored,
            "l3_tombstoned": r.l3_tombstoned,
        }))),
        None => Err((
            StatusCode::NOT_FOUND,
            format!("batch {} not found or already restored", batch_id),
        )),
    }
}

// ── Archival trigger ──────────────────────────────────────────────────────────

/// POST /api/v1/agents/:id/archival/trigger
///
/// Runs one compaction cycle for this agent synchronously.  Returns the batch
/// info if compaction ran, or a message explaining why it was skipped.
pub async fn trigger_archival(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let min_age = state.config.archival_min_age_days as i64;
    let min_mems = state.config.archival_min_memories;

    match archival::archive_agent(&state, &agent_id, min_age, min_mems).await {
        Ok(Some(r)) => Ok(Json(serde_json::json!({
            "batch_id":         r.batch_id.to_string(),
            "source_count":     r.source_count,
            "l3_count":         r.l3_count,
            "narrative_count":  r.narrative_count,
            "status":           r.status,
        }))),
        Ok(None) => Ok(Json(serde_json::json!({
            "status":  "skipped",
            "reason":  format!(
                "fewer than {} archivable memories older than {} day(s)",
                min_mems, min_age
            ),
        }))),
        Err(e) => Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
    }
}

// ── AMP pressure-sweep trigger ───────────────────────────────────────────────

/// POST /api/v1/agents/:id/amp/sweep
///
/// Runs one AMP pressure-sweep cycle for this agent synchronously and returns
/// the resulting controller/eviction state. This is the same routine the
/// background sweep worker runs on its 5-minute cadence; exposing it lets
/// operators and the longitudinal benchmark drive eviction to convergence
/// deterministically instead of waiting on the timer.
pub async fn trigger_amp_sweep(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let report = crate::rmk_worker::run_pressure_sweep_for_agent(&state, &agent_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(serde_json::json!({
        "swept":              true,
        "active_count":       report.active_count,
        "target":             report.target,
        "aggressiveness":     report.aggressiveness,
        "soft_evicted_total": report.soft_evicted_total,
        "newly_evicted":      report.newly_evicted,
        "newly_restored":     report.newly_restored,
    })))
}

// ── Session (working memory) handlers ────────────────────────────────────────

/// GET /api/v1/agents/:id/sessions
pub async fn list_sessions(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
) -> Result<Json<SessionListResponse>, (StatusCode, String)> {
    let sessions = store::list_sessions_for_agent(&state, &agent_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let total = sessions.len();
    Ok(Json(SessionListResponse {
        total,
        sessions: sessions.into_iter().map(SessionDto::from).collect(),
    }))
}

/// GET /api/v1/agents/:id/sessions/:session_id
pub async fn get_session(
    State(state): State<AppState>,
    Path((agent_id, session_id)): Path<(String, String)>,
) -> Result<Json<SessionDetailDto>, (StatusCode, String)> {
    let wm = store::get_session_detail(&state, &agent_id, &session_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    match wm {
        Some(w) => Ok(Json(SessionDetailDto::from(w))),
        None => Err((
            StatusCode::NOT_FOUND,
            format!("no session {} for agent {}", session_id, agent_id),
        )),
    }
}

/// DELETE /api/v1/agents/:id/sessions/:session_id
pub async fn delete_session(
    State(state): State<AppState>,
    Path((agent_id, session_id)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let deleted = store::delete_session_working_memory(&state, &agent_id, &session_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    if deleted {
        Ok(Json(serde_json::json!({ "deleted": true })))
    } else {
        Err((
            StatusCode::NOT_FOUND,
            format!(
                "no working memory for session {} / agent {}",
                session_id, agent_id
            ),
        ))
    }
}

// ── Conflict types & handlers ─────────────────────────────────────────────────

#[derive(Serialize)]
pub struct ConflictDto {
    pub id: String,
    pub memory_a: Option<String>,
    pub memory_b: Option<String>,
    pub reason: String,
    pub resolved_at: Option<String>,
    pub resolution: Option<String>,
    pub created_at: String,
}

impl From<MemoryConflict> for ConflictDto {
    fn from(c: MemoryConflict) -> Self {
        Self {
            id: c.id.to_string(),
            memory_a: c.memory_a.map(|u| u.to_string()),
            memory_b: c.memory_b.map(|u| u.to_string()),
            reason: c.reason,
            resolved_at: c.resolved_at.map(|t| t.to_rfc3339()),
            resolution: c.resolution,
            created_at: c.created_at.to_rfc3339(),
        }
    }
}

#[derive(Serialize)]
pub struct ConflictListResponse {
    pub conflicts: Vec<ConflictDto>,
    pub total: usize,
}

#[derive(Deserialize)]
pub struct ResolveConflictBody {
    /// One of: keep_a | keep_b | keep_both | dismissed
    pub resolution: String,
}

#[derive(Deserialize, Default)]
pub struct ConflictListParams {
    /// When "true", include already-resolved conflicts.
    pub include_resolved: Option<String>,
}

/// GET /api/v1/agents/:id/conflicts
pub async fn list_conflicts(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
    Query(params): Query<ConflictListParams>,
) -> Result<Json<ConflictListResponse>, (StatusCode, String)> {
    let include_resolved = params
        .include_resolved
        .as_deref()
        .map(|s| s.eq_ignore_ascii_case("true"))
        .unwrap_or(false);

    let conflicts = store::list_conflicts(&state, &agent_id, include_resolved)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let total = conflicts.len();
    Ok(Json(ConflictListResponse {
        total,
        conflicts: conflicts.into_iter().map(ConflictDto::from).collect(),
    }))
}

/// POST /api/v1/conflicts/:id/resolve
pub async fn resolve_conflict(
    State(state): State<AppState>,
    Path(conflict_id): Path<Uuid>,
    Json(body): Json<ResolveConflictBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let valid = ["keep_a", "keep_b", "keep_both", "dismissed"];
    if !valid.contains(&body.resolution.as_str()) {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("resolution must be one of: {}", valid.join(", ")),
        ));
    }

    let resolved = store::resolve_conflict(&state, conflict_id, &body.resolution)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    if resolved {
        Ok(Json(
            serde_json::json!({ "resolved": true, "resolution": body.resolution }),
        ))
    } else {
        Err((
            StatusCode::NOT_FOUND,
            format!("conflict {} not found or already resolved", conflict_id),
        ))
    }
}

// ── Bulk operation types & handler ────────────────────────────────────────────

#[derive(Deserialize)]
pub struct BulkOperationRequest {
    /// "archive" (tombstone) or "delete" (hard-delete)
    pub action: String,
    pub filter: BulkFilter,
}

#[derive(Deserialize, Default)]
pub struct BulkFilter {
    pub session_id: Option<String>,
    pub memory_type: Option<String>,
    /// ISO 8601 timestamp — memories created before this are included.
    pub older_than: Option<String>,
    pub importance_below: Option<f32>,
}

/// POST /api/v1/agents/:id/memories/bulk
pub async fn bulk_operation(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
    Json(body): Json<BulkOperationRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    if body.action != "archive" && body.action != "delete" {
        return Err((
            StatusCode::BAD_REQUEST,
            "action must be \"archive\" or \"delete\"".to_string(),
        ));
    }

    let older_than: Option<DateTime<Utc>> = body
        .filter
        .older_than
        .as_deref()
        .map(|s| s.parse::<DateTime<Utc>>())
        .transpose()
        .map_err(|e| {
            (
                StatusCode::BAD_REQUEST,
                format!("invalid older_than: {}", e),
            )
        })?;

    let affected = store::bulk_operation_memories(
        &state,
        &agent_id,
        &body.action,
        body.filter.session_id.as_deref(),
        body.filter.memory_type.as_deref(),
        older_than,
        body.filter.importance_below,
    )
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(serde_json::json!({ "affected": affected })))
}

// ── Export / Import handlers ──────────────────────────────────────────────────

/// GET /api/v1/agents/:id/export
///
/// Returns all live memories as NDJSON (one JSON object per line).
/// Embeddings are excluded; re-compute them on import.
pub async fn export_memories(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
) -> Result<Response<Body>, (StatusCode, String)> {
    let rows: Vec<MemoryExportRow> = store::export_memories_for_agent(&state, &agent_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let mut lines = String::new();
    for row in rows {
        match serde_json::to_string(&row) {
            Ok(line) => {
                lines.push_str(&line);
                lines.push('\n');
            }
            Err(e) => {
                return Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string()));
            }
        }
    }

    let response = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/x-ndjson")
        .header(
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{}.ndjson\"", agent_id),
        )
        .body(Body::from(lines))
        .expect("static headers; infallible");

    Ok(response)
}

/// POST /api/v1/agents/:id/import
///
/// Accepts an NDJSON body (one memory JSON per line, same shape as export).
/// Each line is embedded and stored; dedup check runs per the configured threshold.
/// Returns {"imported": N, "skipped_dedup": N, "errors": N}.
pub async fn import_memories(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
    body: String,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    // Ensure the agent row exists before importing.
    store::upsert_agent(&state, &agent_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let lines: Vec<&str> = body
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .collect();

    if lines.is_empty() {
        return Ok(Json(serde_json::json!({
            "imported": 0, "skipped_dedup": 0, "errors": 0
        })));
    }

    // Parse all lines first so we can batch-embed contents.
    let mut parsed: Vec<serde_json::Value> = Vec::with_capacity(lines.len());
    let mut errors: u64 = 0;
    for line in &lines {
        match serde_json::from_str::<serde_json::Value>(line) {
            Ok(v) => parsed.push(v),
            Err(_) => errors += 1,
        }
    }

    // Batch-embed all contents in one API call.
    let contents: Vec<&str> = parsed
        .iter()
        .filter_map(|v| v["content"].as_str())
        .collect();

    let embeddings = embed_texts(&state, &contents)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let mut imported: u64 = 0;
    let mut skipped_dedup: u64 = 0;

    let mut emb_iter = embeddings.into_iter();
    for row in &parsed {
        let content = match row["content"].as_str() {
            Some(c) => c,
            None => {
                errors += 1;
                continue;
            }
        };
        let embedding = match emb_iter.next() {
            Some(e) => e,
            None => {
                errors += 1;
                break;
            }
        };

        let memory_type = row["memory_type"].as_str().unwrap_or("semantic");
        let confidence = row["confidence"].as_f64().unwrap_or(0.8) as f32;
        let provenance = row["provenance"].as_str().unwrap_or("user_stated");
        let importance_score = row["importance_score"].as_f64().unwrap_or(0.5) as f32;
        let importance_source = row["importance_source"].as_str().unwrap_or("extractor");

        // Track dedup by sampling the counter before and after each insert.
        // Import is a sequential operation so per-call deltas are meaningful.
        let dedup_before = state.metrics.dedup_skipped_total.get();
        match store::store_memory(
            &state,
            &agent_id,
            None,
            content,
            memory_type,
            confidence,
            embedding,
            None,
            provenance,
            importance_score,
            importance_source,
        )
        .await
        {
            Ok(_) => {
                if state.metrics.dedup_skipped_total.get() > dedup_before {
                    skipped_dedup += 1;
                } else {
                    imported += 1;
                }
            }
            Err(_) => errors += 1,
        }
    }

    Ok(Json(serde_json::json!({
        "imported": imported,
        "skipped_dedup": skipped_dedup,
        "errors": errors,
    })))
}

// ── Memory status and sensitivity management ──────────────────────────────────

#[derive(Deserialize)]
pub struct PatchStatusBody {
    pub status: String,
    pub reason: Option<String>,
}

#[derive(Deserialize)]
pub struct PatchSensitivityBody {
    pub sensitivity: String,
}

const VALID_STATUSES: &[&str] = &["active", "candidate", "quarantined", "suppressed"];
const VALID_SENSITIVITIES: &[&str] = &["unknown", "normal", "private", "sensitive", "secret"];
type StatusVersionMeta = (
    String,
    String,
    String,
    f32,
    String,
    f32,
    String,
    String,
    Option<i32>,
);

/// PATCH /api/v1/memories/:id/status
///
/// Changes lifecycle status (active|candidate|quarantined|suppressed) and
/// records a version snapshot with change_type='status_change'.
pub async fn patch_memory_status(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<PatchStatusBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    if !VALID_STATUSES.contains(&body.status.as_str()) {
        return Err((
            StatusCode::BAD_REQUEST,
            format!(
                "invalid status '{}'; must be one of: {}",
                body.status,
                VALID_STATUSES.join(", ")
            ),
        ));
    }

    let uuid = Uuid::parse_str(&id).map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;

    let mut tx = state
        .db
        .begin()
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let suppression_reason: Option<String> = if body.status == "suppressed" {
        body.reason.clone()
    } else {
        None
    };

    let r = sqlx::query(
        "UPDATE memories
         SET status = $1,
             suppression_reason = $2,
             status_updated_at = NOW(),
             updated_at = NOW()
         WHERE id = $3 AND archived_at IS NULL",
    )
    .bind(&body.status)
    .bind(&suppression_reason)
    .bind(uuid)
    .execute(&mut *tx)
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    if r.rows_affected() == 0 {
        return Err((StatusCode::NOT_FOUND, format!("memory {} not found", id)));
    }

    // Snapshot the change as a new version.
    let meta: Option<StatusVersionMeta> = sqlx::query_as(
        "SELECT agent_id, content, memory_type, confidence, provenance,
                    importance_score, importance_source, sensitivity, source_turn
             FROM memories WHERE id = $1",
    )
    .bind(uuid)
    .fetch_optional(&mut *tx)
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    if let Some((
        agent_id,
        content,
        memory_type,
        confidence,
        provenance,
        importance_score,
        importance_source,
        sensitivity,
        source_turn,
    )) = meta
    {
        // Use the pool directly for the version insert (can't use tx due to ownership).
        // The outer tx will either commit or rollback the status update.
        let _ = sqlx::query(
            r#"
            INSERT INTO memory_versions
                (memory_id, agent_id, version_number, content, memory_type, confidence,
                 provenance, importance_score, importance_source, status, sensitivity,
                 source_turn, change_type, change_reason, changed_by)
            SELECT $1, $2,
                   COALESCE(MAX(version_number), 0) + 1,
                   $3, $4, $5, $6, $7, $8, $9, $10, $11, 'status_change', $12, 'system'
            FROM memory_versions
            WHERE memory_id = $1
            "#,
        )
        .bind(uuid)
        .bind(&agent_id)
        .bind(&content)
        .bind(&memory_type)
        .bind(confidence)
        .bind(&provenance)
        .bind(importance_score)
        .bind(&importance_source)
        .bind(&body.status)
        .bind(&sensitivity)
        .bind(source_turn)
        .bind(&body.reason)
        .execute(&state.db)
        .await;
    }

    tx.commit()
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(
        serde_json::json!({ "updated": true, "status": body.status }),
    ))
}

/// PATCH /api/v1/memories/:id/sensitivity
///
/// Sets the sensitivity classification for a memory.
pub async fn patch_memory_sensitivity(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<PatchSensitivityBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    if !VALID_SENSITIVITIES.contains(&body.sensitivity.as_str()) {
        return Err((
            StatusCode::BAD_REQUEST,
            format!(
                "invalid sensitivity '{}'; must be one of: {}",
                body.sensitivity,
                VALID_SENSITIVITIES.join(", ")
            ),
        ));
    }

    let uuid = Uuid::parse_str(&id).map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;

    let r = sqlx::query(
        "UPDATE memories
         SET sensitivity = $1, status_updated_at = NOW(), updated_at = NOW()
         WHERE id = $2 AND archived_at IS NULL",
    )
    .bind(&body.sensitivity)
    .bind(uuid)
    .execute(&state.db)
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    if r.rows_affected() == 0 {
        return Err((StatusCode::NOT_FOUND, format!("memory {} not found", id)));
    }

    Ok(Json(
        serde_json::json!({ "updated": true, "sensitivity": body.sensitivity }),
    ))
}

// ── Retrieval logs ────────────────────────────────────────────────────────────

#[derive(Deserialize, Default)]
pub struct RetrievalLogQuery {
    pub limit: Option<i64>,
    pub offset: Option<i64>,
    pub session_id: Option<String>,
}

/// GET /api/v1/agents/:id/retrievals
///
/// Returns paginated retrieval log entries for an agent.
pub async fn list_retrievals(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
    Query(q): Query<RetrievalLogQuery>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let limit = q.limit.unwrap_or(50).min(200);
    let offset = q.offset.unwrap_or(0);

    let mut qb: sqlx::QueryBuilder<sqlx::Postgres> = sqlx::QueryBuilder::new(
        "SELECT id, agent_id, session_id, query_hash, query_text,
                candidate_memory_ids, injected_memory_ids, suppressed_memory_ids,
                scores, latency_ms, created_at
         FROM memory_retrieval_logs
         WHERE agent_id = ",
    );
    qb.push_bind(&agent_id);

    if let Some(ref sid) = q.session_id {
        qb.push(" AND session_id = ");
        qb.push_bind(sid);
    }

    qb.push(" ORDER BY created_at DESC LIMIT ");
    qb.push_bind(limit);
    qb.push(" OFFSET ");
    qb.push_bind(offset);

    let rows = qb
        .build()
        .fetch_all(&state.db)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    use sqlx::Row;
    let entries: Vec<serde_json::Value> = rows
        .into_iter()
        .map(|r| {
            serde_json::json!({
                "id": r.get::<uuid::Uuid, _>("id").to_string(),
                "agent_id": r.get::<String, _>("agent_id"),
                "session_id": r.get::<Option<String>, _>("session_id"),
                "query_hash": r.get::<String, _>("query_hash"),
                "query_text": r.get::<Option<String>, _>("query_text"),
                "candidate_memory_ids": r.get::<Vec<uuid::Uuid>, _>("candidate_memory_ids")
                    .iter().map(|u| u.to_string()).collect::<Vec<_>>(),
                "injected_memory_ids": r.get::<Vec<uuid::Uuid>, _>("injected_memory_ids")
                    .iter().map(|u| u.to_string()).collect::<Vec<_>>(),
                "suppressed_memory_ids": r.get::<Vec<uuid::Uuid>, _>("suppressed_memory_ids")
                    .iter().map(|u| u.to_string()).collect::<Vec<_>>(),
                "scores": r.get::<serde_json::Value, _>("scores"),
                "latency_ms": r.get::<Option<i32>, _>("latency_ms"),
                "created_at": r.get::<chrono::DateTime<chrono::Utc>, _>("created_at").to_rfc3339(),
            })
        })
        .collect();

    let total = entries.len();
    Ok(Json(serde_json::json!({
        "agent_id": agent_id,
        "retrievals": entries,
        "total": total,
        "limit": limit,
        "offset": offset,
    })))
}

// ── Memory lifecycle timeline ─────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct LifecycleQuery {
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

#[derive(Serialize)]
pub struct LifecycleEventDto {
    pub event_type: String,
    pub memory_id: Option<String>,
    pub timestamp: String,
    pub detail: serde_json::Value,
}

/// GET /api/v1/agents/:id/lifecycle
///
/// Unified, chronological memory-lifecycle event feed for an agent: creation,
/// modification, status change, narrative generation, archival consolidation /
/// restoration, and retrieval — assembled from `memory_versions`,
/// `archival_batches`, and `memory_retrieval_logs`. Newest-first, paginated.
pub async fn agent_lifecycle(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
    Query(q): Query<LifecycleQuery>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let limit = q.limit.unwrap_or(50).clamp(1, 200);
    let offset = q.offset.unwrap_or(0).max(0);

    let events = store::fetch_lifecycle_events(&state, &agent_id, limit, offset)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let total = store::count_lifecycle_events(&state, &agent_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let dtos: Vec<LifecycleEventDto> = events
        .into_iter()
        .map(|e| LifecycleEventDto {
            event_type: e.event_type,
            memory_id: e.memory_id.map(|id| id.to_string()),
            timestamp: e.timestamp.to_rfc3339(),
            detail: e.detail,
        })
        .collect();

    Ok(Json(serde_json::json!({
        "agent_id": agent_id,
        "events": dtos,
        "total": total,
        "limit": limit,
        "offset": offset,
    })))
}

// ── Memory versions ───────────────────────────────────────────────────────────

#[derive(Serialize)]
pub struct MemoryVersionDto {
    pub id: String,
    pub memory_id: String,
    pub version_number: i32,
    pub content: String,
    pub memory_type: String,
    pub confidence: f32,
    pub provenance: String,
    pub importance_score: f32,
    pub importance_source: String,
    pub status: String,
    pub sensitivity: String,
    pub source_turn: Option<i32>,
    pub change_type: String,
    pub change_reason: Option<String>,
    pub changed_by: String,
    pub created_at: String,
}

/// GET /api/v1/memories/:id/versions
///
/// Returns the full version history for a memory, newest first.
pub async fn list_memory_versions(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let uuid = Uuid::parse_str(&id).map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;

    let rows = sqlx::query(
        r#"
        SELECT id, memory_id, version_number, content, memory_type, confidence,
               provenance, importance_score, importance_source, status, sensitivity,
               source_turn, change_type, change_reason, changed_by, created_at
        FROM memory_versions
        WHERE memory_id = $1
        ORDER BY version_number DESC
        "#,
    )
    .bind(uuid)
    .fetch_all(&state.db)
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    use sqlx::Row;
    let versions: Vec<MemoryVersionDto> = rows
        .into_iter()
        .map(|r| MemoryVersionDto {
            id: r.get::<uuid::Uuid, _>("id").to_string(),
            memory_id: r.get::<uuid::Uuid, _>("memory_id").to_string(),
            version_number: r.get("version_number"),
            content: r.get("content"),
            memory_type: r.get("memory_type"),
            confidence: r.get("confidence"),
            provenance: r.get("provenance"),
            importance_score: r.get("importance_score"),
            importance_source: r.get("importance_source"),
            status: r.get("status"),
            sensitivity: r.get("sensitivity"),
            source_turn: r.get("source_turn"),
            change_type: r.get("change_type"),
            change_reason: r.get("change_reason"),
            changed_by: r.get("changed_by"),
            created_at: r
                .get::<chrono::DateTime<chrono::Utc>, _>("created_at")
                .to_rfc3339(),
        })
        .collect();

    let total = versions.len();
    Ok(Json(serde_json::json!({
        "memory_id": id,
        "versions": versions,
        "total": total
    })))
}

// ── Retrieval feedback ─────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct FeedbackBody {
    pub agent_id: String,
    pub memory_id: String,
    /// Feedback score in [0, 1]: 1.0 = highly useful, 0.0 = not useful.
    pub feedback: f64,
}

/// POST /api/v1/feedback
///
/// Records explicit retrieval-quality feedback for a memory and updates
/// its utility_ema.  Callers (agents, MCP clients, dashboard users) use
/// this to signal whether a retrieved memory was actually helpful.
pub async fn post_feedback(
    State(state): State<AppState>,
    Json(body): Json<FeedbackBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let memory_uuid =
        Uuid::parse_str(&body.memory_id).map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    let feedback = body.feedback.clamp(0.0, 1.0);

    // Insert feedback record.
    sqlx::query(
        "INSERT INTO retrieval_feedback (agent_id, memory_id, feedback)
         VALUES ($1, $2, $3)",
    )
    .bind(&body.agent_id)
    .bind(memory_uuid)
    .bind(feedback)
    .execute(&state.db)
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    // Update utility_ema using the feedback signal.
    let alpha = state.config.amp_config.feedback_ema_alpha;
    sqlx::query(
        "UPDATE memories
         SET utility_ema = $1 * $2 + (1.0 - $1) * utility_ema
         WHERE id = $3 AND archived_at IS NULL",
    )
    .bind(alpha)
    .bind(feedback)
    .bind(memory_uuid)
    .execute(&state.db)
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(serde_json::json!({ "recorded": true })))
}
