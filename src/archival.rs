//! LTM Archival Job — Phase B
//!
//! Runs on a configurable interval (default 24 h).  For each agent that has
//! at least `archival_min_memories` L2 memories older than `archival_min_age_days`
//! with zero retrieval hits, it:
//!
//! 1. Fetches up to 50 such memories.
//! 2. Asks the extractor LLM to compress them into 3-5 essential facts AND
//!    a 2-3 sentence cohesive narrative summary.
//! 3. Stores the compressed facts as L3 semantic memories AND the narrative as
//!    an L3 memory with `memory_type = 'narrative'` (`tier = 'L3'`, lower
//!    confidence).  Both are versioned (initial snapshot in `memory_versions`).
//! 4. Tombstones the originals (sets archived_at = NOW()) — they are retained
//!    for audit/lineage but excluded from all retrieval queries.
//!
//! All LLM work happens on this background path only — the retrieval hot path
//! is never blocked by narrative or fact synthesis.
//!
//! The job is started via `tokio::spawn(archival::run_job(state))` in `main.rs`
//! and only runs when `ARCHIVAL_INTERVAL_HOURS > 0`.

use anyhow::Result;
use tracing::{error, info, warn};
use uuid::Uuid;

use crate::{embeddings::embed_texts, memory::store, AppState};

/// Returned by `archive_agent` so callers (API + cron job) can report results.
pub struct ArchivalResult {
    pub batch_id: Uuid,
    pub source_count: usize,
    pub l3_count: usize,
    /// Number of narrative L3 memories created (0 or 1).
    pub narrative_count: usize,
    pub status: &'static str,
}

/// Output of the LLM compaction call.  Both fields are best-effort: a successful
/// compaction must produce at least one fact, but a missing or empty narrative
/// is tolerated and reported as `narrative = None`.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct CompactionOutput {
    facts: Vec<String>,
    narrative: Option<String>,
}

// ── Entry point ───────────────────────────────────────────────────────────────

pub async fn run_job(state: AppState) {
    let interval_secs = state.config.archival_interval_hours * 3600;
    let mut ticker = tokio::time::interval(std::time::Duration::from_secs(interval_secs));

    // Skip the first immediate tick so the job waits a full interval before
    // its first run (prevents stampede on startup).
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    ticker.tick().await; // consume the first immediate tick

    loop {
        ticker.tick().await;
        info!("LTM archival cycle starting");
        if let Err(e) = run_cycle(&state).await {
            error!("LTM archival cycle error: {:#}", e);
            state
                .metrics
                .archival_total
                .with_label_values(&["error"])
                .inc();
        } else {
            state
                .metrics
                .archival_total
                .with_label_values(&["ok"])
                .inc();
        }
    }
}

// ── One full cycle ────────────────────────────────────────────────────────────

async fn run_cycle(state: &AppState) -> Result<()> {
    let min_age = state.config.archival_min_age_days as i64;
    let min_count = state.config.archival_min_memories;

    let agent_ids = store::agents_with_archivable_memories(state, min_age).await?;
    info!("Archival candidates: {} agents", agent_ids.len());

    for agent_id in agent_ids {
        match archive_agent(state, &agent_id, min_age, min_count).await {
            Ok(Some(r)) => info!(agent_id = %agent_id, batch = %r.batch_id,
                source = r.source_count, l3 = r.l3_count,
                narrative = r.narrative_count, "Archival complete"),
            Ok(None) => {}
            Err(e) => warn!(agent_id = %agent_id, "Per-agent archival failed: {}", e),
        }
    }
    Ok(())
}

// ── Per-agent compaction ──────────────────────────────────────────────────────

/// Compact stale L2 memories for one agent into L3 facts.
///
/// Returns `Ok(None)` when there are not enough candidates.
/// Returns `Ok(Some(result))` on success.
pub async fn archive_agent(
    state: &AppState,
    agent_id: &str,
    min_age_days: i64,
    min_memories: usize,
) -> Result<Option<ArchivalResult>> {
    let candidates = store::fetch_archivable_memories(state, agent_id, min_age_days, 50).await?;

    if candidates.len() < min_memories {
        return Ok(None);
    }

    let count = candidates.len();
    info!(agent_id = %agent_id, count, "Compacting L2 → L3");

    // ── Ask the LLM to compress facts AND build a narrative ──────────────────
    let numbered: Vec<String> = candidates
        .iter()
        .enumerate()
        .map(|(i, (_, c))| format!("{}. {}", i + 1, c))
        .collect();

    let compaction = call_compaction_llm(state, &numbered).await?;

    if compaction.facts.is_empty() {
        warn!(agent_id = %agent_id, "No compressed facts returned — skipping deletion");
        return Ok(None);
    }

    // ── Create versioned batch record before mutating any rows ───────────────
    let narrative_text = narrative_text(&compaction);
    let narrative_count = if narrative_text.is_some() { 1 } else { 0 };
    let stored_l3_count = compaction.facts.len() + narrative_count;

    let batch_id =
        store::create_archival_batch(state, agent_id, count as i32, stored_l3_count as i32).await?;

    // ── Batch-embed all compressed facts (and narrative, if present) ─────────
    let mut texts_to_embed: Vec<&str> = compaction.facts.iter().map(|s| s.as_str()).collect();
    if let Some(n) = narrative_text.as_deref() {
        texts_to_embed.push(n);
    }
    let embeddings = match embed_texts(state, &texts_to_embed).await {
        Ok(embs) => embs,
        Err(e) => {
            warn!(agent_id = %agent_id, "Batch embedding failed for L3 archival output: {}", e);
            if let Err(fe) = store::fail_archival_batch(state, batch_id).await {
                warn!(agent_id = %agent_id, "Could not mark batch as failed: {}", fe);
            }
            return Ok(None);
        }
    };

    persist_archival(
        state,
        agent_id,
        &candidates,
        &compaction,
        batch_id,
        embeddings,
    )
    .await
}

/// Trim-and-filter the narrative to a non-empty string (or `None`).
fn narrative_text(compaction: &CompactionOutput) -> Option<String> {
    compaction
        .narrative
        .as_ref()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Persist a compaction result: store the compressed L3 facts and (when present)
/// the narrative L3 row — all tagged with `batch_id` — then tombstone the L2
/// sources.  Split out of `archive_agent` so it can be exercised deterministically
/// with a canned `CompactionOutput` + pre-computed embeddings, without the LLM or
/// the embedding HTTP call.  `embeddings` must be ordered facts-first, narrative
/// last (matching `archive_agent`'s `texts_to_embed`).
async fn persist_archival(
    state: &AppState,
    agent_id: &str,
    candidates: &[(Uuid, String)],
    compaction: &CompactionOutput,
    batch_id: Uuid,
    embeddings: Vec<Vec<f32>>,
) -> Result<Option<ArchivalResult>> {
    let count = candidates.len();
    let narrative_text = narrative_text(compaction);

    let mut emb_iter = embeddings.into_iter();
    let mut facts_stored = 0usize;
    for fact in &compaction.facts {
        let emb = match emb_iter.next() {
            Some(e) => e,
            None => break,
        };
        if let Err(e) = store::store_memory_with_tier(
            state,
            agent_id,
            None,
            fact,
            "semantic",
            0.7,
            emb,
            None,
            "L3",
            "inferred",
            0.5_f32,
            "extractor",
            Some(batch_id),
        )
        .await
        {
            warn!(agent_id = %agent_id, "Failed to store L3 fact: {}", e);
        } else {
            facts_stored += 1;
        }
    }

    let mut narrative_stored = 0usize;
    if let Some(narrative) = narrative_text.as_deref() {
        if let Some(emb) = emb_iter.next() {
            match store::store_memory_with_tier(
                state,
                agent_id,
                None,
                narrative,
                "narrative",
                0.7,
                emb,
                None,
                "L3",
                "inferred",
                0.5_f32,
                "extractor",
                Some(batch_id),
            )
            .await
            {
                Ok(_) => narrative_stored = 1,
                Err(e) => warn!(
                    agent_id = %agent_id,
                    "Failed to store L3 narrative: {}", e
                ),
            }
        } else {
            warn!(agent_id = %agent_id, "Missing narrative embedding — skipping narrative store");
        }
    }

    // ── Tombstone originals and tag them with the batch ───────────────────────
    let ids: Vec<uuid::Uuid> = candidates.iter().map(|(id, _)| *id).collect();
    let tombstoned = store::tombstone_memories_with_batch(state, &ids, batch_id).await?;

    state.metrics.archival_compacted.observe(tombstoned as f64);
    if narrative_stored > 0 {
        state.metrics.narrative_total.inc();
    }

    info!(
        agent_id = %agent_id,
        batch = %batch_id,
        original = count,
        facts = facts_stored,
        narrative = narrative_stored,
        tombstoned,
        "L2→L3 compaction complete"
    );

    Ok(Some(ArchivalResult {
        batch_id,
        source_count: count,
        l3_count: facts_stored,
        narrative_count: narrative_stored,
        status: "completed",
    }))
}

// ── LLM compaction call ───────────────────────────────────────────────────────

async fn call_compaction_llm(state: &AppState, numbered: &[String]) -> Result<CompactionOutput> {
    let prompt = format!(
        "You are compressing {} stale memory facts for long-term storage.\n\
         Return ONLY a JSON object with two keys:\n\
         - \"facts\": an array of 3-5 concise, high-signal strings preserving the \
         most important concrete information.\n\
         - \"narrative\": a 2-3 sentence cohesive prose summary of the same \
         material. Plain English, no JSON, no bullet points.\n\
         Example: {{\
         \"facts\": [\"Alex is building NovaPay\", \"NovaPay does cross-border payments\"], \
         \"narrative\": \"Alex is building NovaPay, a cross-border payments product. \
         The discussion covered both architecture and go-to-market plans.\"\
         }}\n\n{}",
        numbered.len(),
        numbered.join("\n")
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
            "temperature": 0.1,
            "response_format": {"type": "json_object"}
        }))
        .send()
        .await?;

    if !resp.status().is_success() {
        return Err(anyhow::anyhow!("Compaction LLM returned {}", resp.status()));
    }

    let body: serde_json::Value = resp.json().await?;
    let raw = body["choices"][0]["message"]["content"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("No compaction content"))?;

    Ok(parse_compaction_output(raw))
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn strip_code_fences(s: &str) -> &str {
    // Some models wrap JSON in ```json ... ``` even with response_format set.
    let s = s.trim();
    let s = s
        .strip_prefix("```json")
        .or_else(|| s.strip_prefix("```"))
        .unwrap_or(s);
    s.trim_end_matches("```").trim()
}

/// Backwards-compatible facts parser; delegates to `parse_compaction_output`.
#[cfg(test)]
fn parse_compressed_facts(raw: &str) -> Vec<String> {
    parse_compaction_output(raw).facts
}

fn parse_compaction_output(raw: &str) -> CompactionOutput {
    let raw = strip_code_fences(raw);
    let mut out = CompactionOutput::default();

    if let Ok(v) = serde_json::from_str::<serde_json::Value>(raw) {
        let arr = v
            .get("facts")
            .and_then(|x| x.as_array())
            .or_else(|| v.as_array());
        if let Some(arr) = arr {
            out.facts = arr
                .iter()
                .filter_map(|x| x.as_str().map(|s| s.to_string()))
                .collect();
        }

        if let Some(narrative) = v.get("narrative").and_then(|x| x.as_str()) {
            let trimmed = narrative.trim();
            if !trimmed.is_empty() {
                out.narrative = Some(trimmed.to_string());
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_facts_from_object_key() {
        let raw =
            r#"{"facts": ["Alex is building NovaPay", "NovaPay does cross-border payments"]}"#;
        let facts = parse_compressed_facts(raw);
        assert_eq!(facts.len(), 2);
        assert_eq!(facts[0], "Alex is building NovaPay");
    }

    #[test]
    fn parse_facts_from_bare_array() {
        let raw = r#"["Fact one", "Fact two", "Fact three"]"#;
        let facts = parse_compressed_facts(raw);
        assert_eq!(facts.len(), 3);
    }

    #[test]
    fn parse_facts_strips_markdown_fences() {
        let raw = "```json\n{\"facts\": [\"Stripped fact\"]}\n```";
        let facts = parse_compressed_facts(raw);
        assert_eq!(facts, vec!["Stripped fact"]);
    }

    #[test]
    fn parse_facts_strips_plain_fences() {
        let raw = "```\n[\"Bare array fact\"]\n```";
        let facts = parse_compressed_facts(raw);
        assert_eq!(facts, vec!["Bare array fact"]);
    }

    #[test]
    fn parse_facts_returns_empty_on_garbage() {
        assert!(parse_compressed_facts("not json at all").is_empty());
        assert!(parse_compressed_facts("").is_empty());
        assert!(parse_compressed_facts("{}").is_empty());
    }

    #[test]
    fn parse_compaction_with_facts_and_narrative() {
        let raw = r#"{
            "facts": ["Alex builds NovaPay", "NovaPay handles cross-border payments"],
            "narrative": "Alex is building NovaPay, a cross-border payments product. The conversation covered both technical architecture and go-to-market planning."
        }"#;
        let out = parse_compaction_output(raw);
        assert_eq!(out.facts.len(), 2);
        assert!(out.narrative.is_some());
        let narrative = out.narrative.unwrap();
        assert!(narrative.starts_with("Alex is building NovaPay"));
        assert!(!narrative.contains('\n'));
    }

    #[test]
    fn parse_compaction_missing_narrative_is_none() {
        let raw = r#"{"facts": ["Only facts here"]}"#;
        let out = parse_compaction_output(raw);
        assert_eq!(out.facts, vec!["Only facts here"]);
        assert!(out.narrative.is_none(), "missing narrative must be None");
    }

    /// A 1536-dim embedding vector with a single distinguishing component, so
    /// each stored row gets a distinct (but deterministic) vector.
    #[cfg(test)]
    fn seed_vec(seed: f32) -> Vec<f32> {
        let mut v = vec![0.0f32; 1536];
        v[0] = seed;
        v
    }

    /// #13: deterministic end-to-end archival persistence — seeds L2 sources,
    /// persists a canned compaction (facts + narrative) with pre-computed
    /// embeddings (no LLM, no embedding HTTP), and asserts the full contract:
    /// L3 facts stored, a narrative L3 row created, facts + narrative sharing
    /// one archival_batch_id, initial memory_versions snapshots present, and the
    /// L2 sources tombstoned and tagged with the batch.
    #[sqlx::test(migrations = "./migrations")]
    async fn persist_archival_stores_facts_narrative_versions_and_tombstones(pool: sqlx::PgPool) {
        let state = crate::test_support::test_state(pool);
        const AGENT: &str = "archival-smoke-agent";

        // archival_batches has a FK to agents — register the agent first.
        sqlx::query("INSERT INTO agents (agent_id) VALUES ($1) ON CONFLICT DO NOTHING")
            .bind(AGENT)
            .execute(&state.db)
            .await
            .unwrap();

        // Seed two stale L2 sources to compact.
        let src_a = store::store_memory_with_tier(
            &state,
            AGENT,
            None,
            "L2 source A",
            "semantic",
            0.6,
            seed_vec(0.1),
            None,
            "L2",
            "user_stated",
            0.5,
            "extractor",
            None,
        )
        .await
        .unwrap();
        let src_b = store::store_memory_with_tier(
            &state,
            AGENT,
            None,
            "L2 source B",
            "semantic",
            0.6,
            seed_vec(0.2),
            None,
            "L2",
            "user_stated",
            0.5,
            "extractor",
            None,
        )
        .await
        .unwrap();

        let candidates = vec![
            (src_a, "L2 source A".to_string()),
            (src_b, "L2 source B".to_string()),
        ];

        // Canned compaction with a non-empty narrative + facts-first embeddings.
        let compaction = CompactionOutput {
            facts: vec![
                "Compressed fact one".to_string(),
                "Compressed fact two".to_string(),
            ],
            narrative: Some("A cohesive narrative summary of the archived material.".to_string()),
        };
        let embeddings = vec![seed_vec(0.3), seed_vec(0.4), seed_vec(0.5)];

        let batch_id = store::create_archival_batch(&state, AGENT, candidates.len() as i32, 3)
            .await
            .unwrap();

        let result = persist_archival(
            &state,
            AGENT,
            &candidates,
            &compaction,
            batch_id,
            embeddings,
        )
        .await
        .unwrap()
        .expect("archival must succeed with facts present");
        assert_eq!(result.batch_id, batch_id);
        assert_eq!(result.l3_count, 2, "two compressed facts stored");
        assert_eq!(result.narrative_count, 1, "one narrative stored");

        // L3 compressed facts under the batch.
        let fact_ids: Vec<(Uuid,)> = sqlx::query_as(
            "SELECT id FROM memories
             WHERE agent_id = $1 AND tier = 'L3' AND memory_type = 'semantic'
               AND archival_batch_id = $2",
        )
        .bind(AGENT)
        .bind(batch_id)
        .fetch_all(&state.db)
        .await
        .unwrap();
        assert_eq!(fact_ids.len(), 2, "two L3 semantic facts under the batch");

        // A narrative L3 row under the SAME batch (proves shared archival_batch_id).
        let narrative_ids: Vec<(Uuid,)> = sqlx::query_as(
            "SELECT id FROM memories
             WHERE agent_id = $1 AND tier = 'L3' AND memory_type = 'narrative'
               AND archival_batch_id = $2",
        )
        .bind(AGENT)
        .bind(batch_id)
        .fetch_all(&state.db)
        .await
        .unwrap();
        assert_eq!(narrative_ids.len(), 1, "exactly one narrative L3 row");

        // Every L3 row has an initial memory_versions snapshot.
        let l3_ids = fact_ids.iter().chain(narrative_ids.iter()).map(|(id,)| *id);
        for id in l3_ids {
            let (versions,): (i64,) =
                sqlx::query_as("SELECT COUNT(*) FROM memory_versions WHERE memory_id = $1")
                    .bind(id)
                    .fetch_one(&state.db)
                    .await
                    .unwrap();
            assert!(
                versions >= 1,
                "L3 row {id} must have an initial memory_versions snapshot"
            );
        }

        // L2 sources tombstoned and tagged with the batch.
        for src in [src_a, src_b] {
            let (archived_at, batch): (Option<chrono::DateTime<chrono::Utc>>, Option<Uuid>) =
                sqlx::query_as("SELECT archived_at, archival_batch_id FROM memories WHERE id = $1")
                    .bind(src)
                    .fetch_one(&state.db)
                    .await
                    .unwrap();
            assert!(archived_at.is_some(), "L2 source {src} must be tombstoned");
            assert_eq!(batch, Some(batch_id), "tombstoned L2 tagged with the batch");
        }
    }

    #[test]
    fn parse_compaction_empty_narrative_is_none() {
        let raw = r#"{"facts": ["F"], "narrative": "   "}"#;
        let out = parse_compaction_output(raw);
        assert!(
            out.narrative.is_none(),
            "blank narrative must be normalised to None"
        );
    }

    #[test]
    fn parse_compaction_strips_fences() {
        let raw = "```json\n{\"facts\": [\"f\"], \"narrative\": \"a story\"}\n```";
        let out = parse_compaction_output(raw);
        assert_eq!(out.facts, vec!["f"]);
        assert_eq!(out.narrative.as_deref(), Some("a story"));
    }
}
