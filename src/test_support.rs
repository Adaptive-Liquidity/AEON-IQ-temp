//! Shared test-only helpers (compiled under `#[cfg(test)]` only).

use std::sync::Arc;

use crate::{
    config::Config, metrics::Metrics, providers::Provider, rate_limit::RateLimiter, AppState,
};

/// Minimal AppState over a test pool with neutral config defaults.
pub fn test_state(pool: sqlx::PgPool) -> AppState {
    AppState {
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
            memory_decay_rate: 0.0,
            importance_boost_factor: 0.0,
            importance_refresh_boost: 0.05,
            ann_candidate_limit: 100,
            hnsw_ef_search: 100,
            local_embedding_base_url: None,
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
        // The V1 default: credential authentication inactive, so every existing
        // test exercises exactly the path it did before.
        credential_auth: None,
    }
}
