//! Async conflict detection — Phase 2.2
//!
//! After each new L2 memory is inserted, `detect_and_store` is spawned in the
//! background (via `tokio::spawn`).  It fetches the top-5 most similar existing
//! memories, sends them to the extractor LLM, and stores any contradictions it
//! finds in the `memory_conflicts` table.
//!
//! The detection is opt-in: it only runs when `CONFLICT_DETECTION_ENABLED=true`.
//! The extractor endpoint and model are the same as the archival compaction job.

use pgvector::Vector;
use tracing::{debug, warn};
use uuid::Uuid;

use crate::{memory::store, AppState};

/// Entry point — call from `store_memory` after a successful insert.
pub async fn detect_and_store(
    state: &AppState,
    agent_id: &str,
    new_memory_id: Uuid,
    new_content: &str,
    embedding: &[f32],
) {
    if !state.config.conflict_detection_enabled {
        return;
    }

    if let Err(e) = run_detection(state, agent_id, new_memory_id, new_content, embedding).await {
        warn!(agent_id, %new_memory_id, "Conflict detection error: {:#}", e);
    }
}

async fn run_detection(
    state: &AppState,
    agent_id: &str,
    new_memory_id: Uuid,
    new_content: &str,
    embedding: &[f32],
) -> anyhow::Result<()> {
    // Fetch top-5 most similar memories, excluding the newly inserted one.
    let vec = Vector::from(embedding.to_vec());
    let candidates: Vec<(Uuid, String)> = sqlx::query_as(
        "SELECT id, content \
         FROM memories \
         WHERE agent_id = $1 AND id <> $2 AND archived_at IS NULL \
           AND sensitivity NOT IN ('private', 'secret') \
         ORDER BY embedding <=> $3 \
         LIMIT 5",
    )
    .bind(agent_id)
    .bind(new_memory_id)
    .bind(vec)
    .fetch_all(&state.db)
    .await?;

    if candidates.is_empty() {
        debug!(agent_id, "conflict check: no candidates to compare");
        return Ok(());
    }

    // Privacy note: `new_content` is sent to the extractor provider.  This is
    // safe because sensitivity is assigned post-hoc only (PATCH
    // /memories/:id/sensitivity) — a memory is always 'unknown' at insert
    // time, when conflict detection runs, so labeled private/secret content
    // can never reach this prompt.  The candidate SELECT above additionally
    // excludes private/secret rows from the comparison set.  If sensitivity
    // ever becomes assignable at insert time, this path must gain the same
    // guard as PATCH re-embed (see embeddings::embed_text_for_sensitivity).
    let numbered: String = candidates
        .iter()
        .enumerate()
        .map(|(i, (id, c))| format!("{}. [{}] {}", i + 1, id, c))
        .collect::<Vec<_>>()
        .join("\n");

    let prompt = format!(
        "New fact: \"{}\"\n\nExisting facts:\n{}\n\n\
         Do any of the existing facts directly contradict the new fact? \
         Return JSON: {{\"conflicts\": [{{\"existing_id\": \"<uuid>\", \"reason\": \"<explanation>\"}}]}} \
         or {{\"conflicts\": []}} if there are no contradictions. \
         Only flag direct factual contradictions, not differences in phrasing or level of detail.",
        new_content, numbered
    );

    let api_key = state.config.openai_api_key.as_deref().unwrap_or("no-key");
    let resp = state
        .http_client
        .post(format!(
            "{}/v1/chat/completions",
            state.config.extractor_base_url
        ))
        .header("Authorization", format!("Bearer {}", api_key))
        .header("Content-Type", "application/json")
        .json(&serde_json::json!({
            "model": state.config.extractor_model,
            "messages": [{"role": "user", "content": prompt}],
            "temperature": 0.0,
            "response_format": {"type": "json_object"}
        }))
        .send()
        .await?;

    if !resp.status().is_success() {
        return Err(anyhow::anyhow!("Conflict LLM returned {}", resp.status()));
    }

    let body: serde_json::Value = resp.json().await?;
    let raw = body["choices"][0]["message"]["content"]
        .as_str()
        .unwrap_or("{}");

    let parsed: serde_json::Value = serde_json::from_str(raw).unwrap_or(serde_json::json!({}));
    let conflicts = parsed["conflicts"].as_array();

    if let Some(arr) = conflicts {
        for item in arr {
            let id_str = item["existing_id"].as_str().unwrap_or("");
            let reason = item["reason"].as_str().unwrap_or("contradicts new fact");
            if let Ok(existing_id) = Uuid::parse_str(id_str) {
                if candidates.iter().any(|(id, _)| *id == existing_id) {
                    match store::store_conflict(state, agent_id, existing_id, new_memory_id, reason)
                        .await
                    {
                        Ok(cid) => debug!(agent_id, %cid, "stored conflict"),
                        Err(e) => warn!(agent_id, "failed to store conflict: {}", e),
                    }
                }
            }
        }
    }

    Ok(())
}
