use crate::{log_utils::truncate_for_log, AppState};
use anyhow::{anyhow, Result};

fn api_key(state: &AppState) -> &str {
    state.config.openai_api_key.as_deref().unwrap_or("no-key")
}

/// True for sensitivity labels whose content must never be sent to the
/// configured (remote) provider.
pub fn is_private_sensitivity(sensitivity: &str) -> bool {
    matches!(sensitivity, "private" | "secret")
}

/// Embed multiple texts in a single API call.
///
/// The OpenAI embeddings endpoint accepts an array `input`; the response
/// includes an `index` field per object so results can be returned in the
/// same order as the input even if the server reorders them.
///
/// Returns an empty `Vec` when `texts` is empty.
pub async fn embed_texts(state: &AppState, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
    embed_texts_at(state, texts, &state.config.embedding_base_url).await
}

/// Embed against an explicit embedding base URL.  Used by the local-lane
/// path for sensitivity-labeled content; everything else goes through
/// `embed_texts`.
async fn embed_texts_at(state: &AppState, texts: &[&str], base_url: &str) -> Result<Vec<Vec<f32>>> {
    if texts.is_empty() {
        return Ok(Vec::new());
    }

    let payload = serde_json::json!({
        "input": texts,
        "model": state.config.embedding_model,
    });

    let response = state
        .http_client
        .post(format!("{}/v1/embeddings", base_url.trim_end_matches('/')))
        .header("Authorization", format!("Bearer {}", api_key(state)))
        .header("Content-Type", "application/json")
        .json(&payload)
        .send()
        .await?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(anyhow!(
            "Embedding API returned {} ({} bytes): {}",
            status,
            body.len(),
            truncate_for_log(&body)
        ));
    }

    let body: serde_json::Value = response.json().await?;
    let data = body["data"]
        .as_array()
        .ok_or_else(|| anyhow!("Unexpected embedding response shape: {:?}", body))?;

    // Sort by index so the output order matches the input order.
    let mut indexed: Vec<(usize, Vec<f32>)> = data
        .iter()
        .map(|item| {
            let idx = item["index"].as_u64().unwrap_or(0) as usize;
            let vec = item["embedding"]
                .as_array()
                .map(|arr| {
                    arr.iter()
                        .map(|v| v.as_f64().unwrap_or(0.0) as f32)
                        .collect()
                })
                .unwrap_or_default();
            (idx, vec)
        })
        .collect();
    indexed.sort_unstable_by_key(|(i, _)| *i);

    Ok(indexed.into_iter().map(|(_, v)| v).collect())
}

/// Convenience wrapper that embeds a single text.
/// Prefer `embed_texts` when embedding multiple facts in one turn.
pub async fn embed_text(state: &AppState, text: &str) -> Result<Vec<f32>> {
    embed_texts(state, &[text])
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("Embedding API returned empty data array"))
}

/// Sensitivity-aware single-text embedding: content labeled `private` or
/// `secret` never leaves the box — it embeds via `LOCAL_EMBEDDING_BASE_URL`
/// when configured, and is refused otherwise.  All other labels use the
/// normal provider lane.
pub async fn embed_text_for_sensitivity(
    state: &AppState,
    text: &str,
    sensitivity: &str,
) -> Result<Vec<f32>> {
    if !is_private_sensitivity(sensitivity) {
        return embed_text(state, text).await;
    }
    match &state.config.local_embedding_base_url {
        Some(local) => embed_texts_at(state, &[text], local)
            .await?
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("Local embedding lane returned empty data array")),
        None => Err(anyhow!(
            "memory is labeled '{sensitivity}' and no LOCAL_EMBEDDING_BASE_URL is \
             configured — refusing to send its content to the remote embedding \
             provider. Configure a local embedding lane or relabel the memory."
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_labels_are_recognized() {
        assert!(is_private_sensitivity("private"));
        assert!(is_private_sensitivity("secret"));
        for label in ["unknown", "normal", "sensitive"] {
            assert!(
                !is_private_sensitivity(label),
                "{label} must use the normal lane"
            );
        }
    }
}
