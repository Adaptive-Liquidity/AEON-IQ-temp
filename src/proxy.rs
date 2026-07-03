use axum::{
    body::Body,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::Response,
};
use bytes::Bytes;
use futures::StreamExt;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tracing::{error, info, warn};

use uuid::Uuid;

use crate::{
    memory::{
        extraction::{enqueue_extraction_job, extract_and_store, ExtractionPayload},
        retrieval::{build_injection, retrieve_relevant},
        rmk::{
            policy::PolicyParams,
            reward::{EpisodeMetrics, RewardModel},
            store as rmk_store,
        },
        store,
    },
    models::{ChatRequest, Message},
    providers::Provider,
    AppState,
};

// ── Public handlers ───────────────────────────────────────────────────────────

pub async fn handle_chat_completions(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response<Body>, (StatusCode, String)> {
    // ── 1. Tenant context ─────────────────────────────────────────────────────
    let agent_id = headers
        .get("x-agent-id")
        .and_then(|v| v.to_str().ok())
        .ok_or((
            StatusCode::BAD_REQUEST,
            "Missing required header: x-agent-id".to_string(),
        ))?
        .to_string();

    let session_id = headers
        .get("x-session-id")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
        .unwrap_or_else(|| format!("sess-{}", uuid::Uuid::new_v4()));

    let importance_override: Option<f32> = headers
        .get("x-memory-importance")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<f32>().ok())
        .map(|v| v.clamp(0.0, 1.0));

    // ── 2. Rate limiting ──────────────────────────────────────────────────────
    if !state.rate_limiter.check_and_consume(&agent_id) {
        state.metrics.rate_limited_total.inc();
        return Err((
            StatusCode::TOO_MANY_REQUESTS,
            format!(
                "{{\"error\":{{\"message\":\"Rate limit exceeded for agent {}\",\"type\":\"rate_limit_error\"}}}}",
                agent_id
            ),
        ));
    }

    // ── 3. Parse request ──────────────────────────────────────────────────────
    let mut chat_req: ChatRequest =
        serde_json::from_slice(&body).map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;

    if let Err(e) = store::upsert_agent(&state, &agent_id).await {
        warn!(agent_id = %agent_id, "upsert_agent: {}", e);
    }

    // ── 4. Memory retrieval + injection ───────────────────────────────────────
    let user_message = chat_req
        .messages
        .iter()
        .rev()
        .find(|m| m.role == "user")
        .map(|m| m.content_text())
        .unwrap_or_default();

    let original_messages = chat_req.messages.clone();
    let turn_number = original_messages.len() as i32;

    // When RMK is enabled, override the static retrieval threshold with the
    // agent's current learned policy and capture the policy ID for episode logging.
    // The full PolicyParams are kept so all 6 θ dimensions are applied during retrieval.
    let (effective_threshold, rmk_policy_id, rmk_policy_params): (
        f64,
        Option<Uuid>,
        Option<PolicyParams>,
    ) = if state.config.rmk_config.enabled {
        match rmk_store::get_latest_policy(&state.db, &agent_id).await {
            Ok(Some((pid, policy))) => {
                let threshold = policy.retrieval_threshold;
                (threshold, Some(pid), Some(policy))
            }
            _ => (state.config.retrieval_threshold, None, None),
        }
    } else {
        (state.config.retrieval_threshold, None, None)
    };

    let mut memories_retrieved: usize = 0;
    let mut injected_chars: usize = 0;
    let mut injected_memory_ids: Vec<Uuid> = Vec::new();
    let total_prompt_chars: usize = chat_req
        .messages
        .iter()
        .map(|m| m.content_text().len())
        .sum();
    if !user_message.is_empty() {
        match retrieve_relevant(
            &state,
            &agent_id,
            &session_id,
            &user_message,
            5,
            effective_threshold,
            rmk_policy_params.as_ref(),
        )
        .await
        {
            Ok(memories) => {
                memories_retrieved = memories.len();
                if memories_retrieved > 0 {
                    let wm = store::get_working_memory(&state, &agent_id, &session_id)
                        .await
                        .ok()
                        .flatten();
                    let injection = build_injection(&memories, wm.as_ref());

                    if !injection.is_empty() {
                        injected_chars = injection.len();
                        injected_memory_ids = memories.iter().map(|m| m.id).collect();
                        info!(agent_id = %agent_id, count = memories_retrieved, "Injecting memories");
                        chat_req.messages.insert(
                            0,
                            Message {
                                role: "system".to_string(),
                                content: injection.into(),
                                name: None,
                            },
                        );
                        state
                            .metrics
                            .injection_total
                            .with_label_values(&["hit"])
                            .inc();
                        state
                            .metrics
                            .injected_per_req
                            .observe(memories_retrieved as f64);
                    } else {
                        state
                            .metrics
                            .injection_total
                            .with_label_values(&["miss"])
                            .inc();
                    }
                } else {
                    state
                        .metrics
                        .injection_total
                        .with_label_values(&["miss"])
                        .inc();
                }
            }
            Err(e) => {
                warn!(agent_id = %agent_id, "Memory retrieval skipped: {}", e);
                state
                    .metrics
                    .injection_total
                    .with_label_values(&["miss"])
                    .inc();
            }
        }
    }

    // ── 5. Build + send upstream request ─────────────────────────────────────
    let upstream_url = state
        .provider
        .completions_url(&state.config.upstream_base_url);
    let request_body = state.provider.build_request(&chat_req);

    let auth_header = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    let mut req_builder = state
        .http_client
        .post(&upstream_url)
        .header("Authorization", &auth_header)
        .header("Content-Type", "application/json");

    for (name, value) in state.provider.extra_headers() {
        req_builder = req_builder.header(*name, *value);
    }

    let upstream_resp = req_builder.json(&request_body).send().await.map_err(|e| {
        (
            StatusCode::BAD_GATEWAY,
            format!("Upstream unreachable: {}", e),
        )
    })?;

    let up_status = upstream_resp.status();
    let up_headers = upstream_resp.headers().clone();
    let is_streaming = chat_req.stream.unwrap_or(false);
    let model = chat_req.model.clone();

    // Capture state fields needed for episode logging before state is moved
    // into a proxy sub-function.
    let rmk_log = if state.config.rmk_config.enabled {
        Some((
            state.db.clone(),
            state.config.clone(),
            agent_id.clone(),
            session_id.clone(),
            rmk_policy_id,
            memories_retrieved,
            injected_memory_ids.clone(),
            injected_chars,
            total_prompt_chars,
        ))
    } else {
        None
    };

    // ── 6. Dispatch by provider ───────────────────────────────────────────────
    let result = match state.provider {
        Provider::Anthropic => {
            proxy_anthropic(
                state,
                agent_id,
                session_id,
                original_messages,
                turn_number,
                upstream_resp,
                up_status,
                is_streaming,
                model,
                importance_override,
            )
            .await
        }
        _ => {
            if is_streaming {
                proxy_streaming(
                    state,
                    agent_id,
                    session_id,
                    original_messages,
                    turn_number,
                    upstream_resp,
                    up_status,
                    up_headers,
                    importance_override,
                )
                .await
            } else {
                proxy_buffered(
                    state,
                    agent_id,
                    session_id,
                    original_messages,
                    turn_number,
                    upstream_resp,
                    up_status,
                    up_headers,
                    importance_override,
                )
                .await
            }
        }
    };

    // ── 7. Log RMK episode (background, fire-and-forget) ─────────────────────
    if let Some((db, cfg, aid, sid, policy_id, mem_count, inj_ids, inj_chars, prompt_chars)) =
        rmk_log
    {
        tokio::spawn(async move {
            const RETRIEVAL_LIMIT: f64 = 5.0;
            let precision = (mem_count as f64 / RETRIEVAL_LIMIT).min(1.0);

            // Estimate token savings as the fraction of the total prompt that came
            // from injected memories (chars/4 ≈ tokens).
            let token_savings = if prompt_chars > 0 && inj_chars > 0 {
                (inj_chars as f64 / (prompt_chars + inj_chars) as f64).min(1.0)
            } else {
                0.0
            };

            // Query the real eviction cost: fraction of active memories that are
            // currently soft-evicted for this agent.
            let eviction_cost: f64 = sqlx::query_scalar(
                "SELECT COALESCE(
                    SUM(CASE WHEN soft_evicted THEN 1 ELSE 0 END)::double precision /
                    NULLIF(COUNT(*), 0),
                    0.0
                 )
                 FROM memories
                 WHERE agent_id = $1 AND archived_at IS NULL",
            )
            .bind(&aid)
            .fetch_one(&db)
            .await
            .unwrap_or(0.0);

            let metrics = EpisodeMetrics {
                // task_success starts at the documented "assumed" default of
                // 1.0; the background aggregation job backfills a real value
                // (and recomputes the reward) for episodes whose injected
                // memories later receive /api/v1/feedback.
                task_success: 1.0,
                token_savings,
                retrieval_precision: precision,
                eviction_cost,
            };
            let reward =
                RewardModel::new(cfg.rmk_config.reward_weights.clone()).compute_reward(&metrics);
            if let Err(e) =
                rmk_store::insert_episode(&db, &aid, policy_id, &sid, &inj_ids, &metrics, reward)
                    .await
            {
                warn!(agent_id = %aid, "RMK episode logging failed: {}", e);
            }
        });
    }

    result
}

/// Transparent pass-through for GET /v1/models.
pub async fn handle_models(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Response<Body>, (StatusCode, String)> {
    let auth = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    let models_url = format!("{}/v1/models", state.config.upstream_base_url);
    let resp = state
        .http_client
        .get(&models_url)
        .header("Authorization", &auth)
        .send()
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, e.to_string()))?;

    let status = resp.status();
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, e.to_string()))?;

    Ok(Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Body::from(bytes))
        .expect("static headers; infallible"))
}

async fn enqueue_or_extract(
    state: AppState,
    agent_id: String,
    session_id: String,
    messages: Vec<Message>,
    assistant_content: String,
    turn_number: i32,
    importance_override: Option<f32>,
) {
    if !state.config.extraction_outbox_enabled() {
        extract_and_store(
            state,
            agent_id,
            session_id,
            messages,
            assistant_content,
            turn_number,
            importance_override,
        )
        .await;
        return;
    }

    let payload = ExtractionPayload::new(
        messages,
        assistant_content,
        turn_number,
        importance_override,
    );
    match enqueue_extraction_job(&state.db, &agent_id, Some(&session_id), &payload).await {
        Ok(job_id) => info!(agent_id = %agent_id, job_id = %job_id, "Queued extraction job"),
        Err(e) => {
            error!(agent_id = %agent_id, "Extraction enqueue failed: {:#}", e);
            state
                .metrics
                .extraction_total
                .with_label_values(&["error"])
                .inc();
        }
    }
}

// ── OpenAI / Gemini streaming path ───────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
async fn proxy_streaming(
    state: AppState,
    agent_id: String,
    session_id: String,
    original_messages: Vec<Message>,
    turn_number: i32,
    upstream_resp: reqwest::Response,
    up_status: reqwest::StatusCode,
    up_headers: reqwest::header::HeaderMap,
    importance_override: Option<f32>,
) -> Result<Response<Body>, (StatusCode, String)> {
    let mut byte_stream = upstream_resp.bytes_stream();
    let (forward_tx, forward_rx) = mpsc::channel::<Bytes>(128);
    let (capture_tx, capture_rx) = tokio::sync::oneshot::channel::<Vec<u8>>();

    tokio::spawn(async move {
        let mut captured: Vec<u8> = Vec::new();
        while let Some(chunk) = byte_stream.next().await {
            match chunk {
                Ok(bytes) => {
                    captured.extend_from_slice(&bytes);
                    if forward_tx.send(bytes).await.is_err() {
                        break;
                    }
                }
                Err(e) => {
                    error!("Upstream stream error: {}", e);
                    break;
                }
            }
        }
        let _ = capture_tx.send(captured);
    });

    tokio::spawn(async move {
        if let Ok(captured) = capture_rx.await {
            let content = state.provider.parse_streaming(&captured);
            if !content.is_empty() {
                enqueue_or_extract(
                    state,
                    agent_id,
                    session_id,
                    original_messages,
                    content,
                    turn_number,
                    importance_override,
                )
                .await;
            }
        }
    });

    let client_stream = ReceiverStream::new(forward_rx).map(Ok::<_, std::io::Error>);
    let mut builder = Response::builder().status(up_status.as_u16());
    for (k, v) in &up_headers {
        if k.as_str().eq_ignore_ascii_case("content-length") {
            continue;
        }
        builder = builder.header(k.as_str(), v);
    }
    Ok(builder
        .body(Body::from_stream(client_stream))
        .expect("response builder; infallible"))
}

// ── OpenAI / Gemini buffered path ─────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
async fn proxy_buffered(
    state: AppState,
    agent_id: String,
    session_id: String,
    original_messages: Vec<Message>,
    turn_number: i32,
    upstream_resp: reqwest::Response,
    up_status: reqwest::StatusCode,
    up_headers: reqwest::header::HeaderMap,
    importance_override: Option<f32>,
) -> Result<Response<Body>, (StatusCode, String)> {
    let bytes = upstream_resp.bytes().await.map_err(|e| {
        (
            StatusCode::BAD_GATEWAY,
            format!("Failed to read upstream: {}", e),
        )
    })?;

    let bytes_clone = bytes.clone();
    tokio::spawn(async move {
        let content = state.provider.parse_buffered(&bytes_clone);
        if !content.is_empty() {
            enqueue_or_extract(
                state,
                agent_id,
                session_id,
                original_messages,
                content,
                turn_number,
                importance_override,
            )
            .await;
        }
    });

    let mut builder = Response::builder().status(up_status.as_u16());
    for (k, v) in &up_headers {
        if k.as_str().eq_ignore_ascii_case("transfer-encoding") {
            continue;
        }
        builder = builder.header(k.as_str(), v);
    }
    Ok(builder
        .body(Body::from(bytes))
        .expect("response builder; infallible"))
}

// ── Anthropic path (buffer + synthesize OpenAI response) ──────────────────────
//
// Anthropic's wire format (request AND response) differs from OpenAI's.
// To keep callers using the OpenAI SDK with zero changes, we:
//   1. Buffer the entire Anthropic response (streaming or not).
//   2. Extract the assistant text.
//   3. Spawn memory extraction as usual.
//   4. Re-emit a synthetic OpenAI-format response so the caller's SDK is happy.
//
// Trade-off: first-byte latency equals full upstream latency rather than true
// token-by-token streaming.  Acceptable for agent use cases (batch-style calls).

#[allow(clippy::too_many_arguments)]
async fn proxy_anthropic(
    state: AppState,
    agent_id: String,
    session_id: String,
    original_messages: Vec<Message>,
    turn_number: i32,
    upstream_resp: reqwest::Response,
    up_status: reqwest::StatusCode,
    is_streaming: bool,
    model: String,
    importance_override: Option<f32>,
) -> Result<Response<Body>, (StatusCode, String)> {
    // Forward non-2xx errors as-is (Anthropic error JSON is informative).
    if !up_status.is_success() {
        let status = StatusCode::from_u16(up_status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
        let body = upstream_resp.bytes().await.unwrap_or_default();
        return Ok(Response::builder()
            .status(status)
            .header("content-type", "application/json")
            .body(Body::from(body))
            .expect("static headers; infallible"));
    }

    let bytes = upstream_resp.bytes().await.map_err(|e| {
        (
            StatusCode::BAD_GATEWAY,
            format!("Failed to read Anthropic response: {}", e),
        )
    })?;

    let content = if is_streaming {
        state.provider.parse_streaming(&bytes)
    } else {
        state.provider.parse_buffered(&bytes)
    };

    if !content.is_empty() {
        let state_c = state.clone();
        let agent_id_c = agent_id.clone();
        let session_id_c = session_id.clone();
        let messages_c = original_messages.clone();
        let content_c = content.clone();
        tokio::spawn(async move {
            enqueue_or_extract(
                state_c,
                agent_id_c,
                session_id_c,
                messages_c,
                content_c,
                turn_number,
                importance_override,
            )
            .await;
        });
    }

    if is_streaming {
        let sse = state.provider.synthesize_sse(&content, &model);
        Ok(Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "text/event-stream")
            .header("cache-control", "no-cache")
            .header("x-accel-buffering", "no")
            .body(Body::from(sse))
            .expect("static headers; infallible"))
    } else {
        let json_body = state.provider.synthesize_json(&content, &model);
        let json_bytes = serde_json::to_vec(&json_body)
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        Ok(Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "application/json")
            .body(Body::from(json_bytes))
            .expect("static headers; infallible"))
    }
}
