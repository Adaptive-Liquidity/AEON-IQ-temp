use crate::memory::amp::config::AmpConfig;
use crate::memory::rmk::config::RmkConfig;
use crate::url_guard::{validate_local_provider_url, validate_provider_url};
use anyhow::{Context, Result};

/// Default retrieval-distance threshold, shared by the static env config and
/// the seeded RMK policy (`PolicyParams::default`) so an RMK-enabled agent's
/// first policy does not silently change retrieval strictness.
pub const DEFAULT_RETRIEVAL_THRESHOLD: f64 = 0.80;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessRole {
    Proxy,
    Worker,
    All,
}

impl ProcessRole {
    pub fn from_env() -> Result<Self> {
        let value = std::env::var("MEMORYOS_ROLE").ok();
        Self::from_env_value(value.as_deref())
    }

    fn from_env_value(value: Option<&str>) -> Result<Self> {
        match value.map(str::trim).filter(|value| !value.is_empty()) {
            None => Ok(Self::All),
            Some(value) if value.eq_ignore_ascii_case("proxy") => Ok(Self::Proxy),
            Some(value) if value.eq_ignore_ascii_case("worker") => Ok(Self::Worker),
            Some(value) if value.eq_ignore_ascii_case("all") => Ok(Self::All),
            Some(value) => {
                anyhow::bail!("MEMORYOS_ROLE must be one of proxy, worker, or all; got {value:?}")
            }
        }
    }

    pub fn serves_proxy(self) -> bool {
        matches!(self, Self::Proxy | Self::All)
    }

    pub fn runs_workers(self) -> bool {
        matches!(self, Self::Worker | Self::All)
    }
}

#[derive(Debug, Clone)]
pub struct ExtractionOutboxConfig {
    pub enabled: bool,
    pub worker_poll_secs: u64,
    pub max_attempts: u32,
    pub backoff_base_secs: u64,
}

#[derive(Debug, Clone)]
pub struct Config {
    // ── Core ─────────────────────────────────────────────────────────────────
    pub database_url: String,
    pub upstream_base_url: String,
    pub port: u16,

    // ── Database pool ─────────────────────────────────────────────────────────
    pub db_max_connections: u32,
    pub db_acquire_timeout_secs: u64,
    pub db_idle_timeout_secs: u64,

    // ── Embeddings ────────────────────────────────────────────────────────────
    /// Server-side key for embedding and extraction calls.
    pub openai_api_key: Option<String>,
    pub embedding_model: String,
    pub embedding_dimension: i32,
    /// Base URL for the embeddings endpoint.  Defaults to `upstream_base_url`.
    /// Override when the LLM provider (e.g. Anthropic) doesn't serve embeddings
    /// and you want to point at OpenAI or a local model instead.
    pub embedding_base_url: String,

    // ── Extraction (MMU) ──────────────────────────────────────────────────────
    pub extractor_model: String,
    pub extractor_base_url: String,

    // ── Retrieval ─────────────────────────────────────────────────────────────
    /// Cosine distance upper bound; memories with distance ≥ this are dropped.
    pub retrieval_threshold: f64,
    /// Decay rate per day applied to cosine distance during retrieval.
    /// `adjusted_dist = cosine_dist * exp(decay_rate * days_since_last_access)`
    /// Set to 0.0 (default) to disable decay and use pure cosine similarity.
    pub memory_decay_rate: f64,
    /// Importance weight in retrieval formula. 0.0 = disabled (default).
    /// adjusted_dist *= (1 + importance_boost_factor * (1 - importance_score))
    pub importance_boost_factor: f64,
    /// Per-retrieval boost added to importance_score (spacing-effect refresh). 0.0 = disabled.
    pub importance_refresh_boost: f32,
    /// Number of ANN candidates fetched by the index-served first stage of
    /// retrieval before the decay/importance re-rank is applied. Clamped to at
    /// least the request's `limit` at query time.
    pub ann_candidate_limit: i64,
    /// Value applied as `SET LOCAL hnsw.ef_search` on each retrieval
    /// transaction (pgvector's default of 40 is too low once tombstones
    /// accumulate; see docs/HNSW_MAINTENANCE.md). Clamped to at least the
    /// effective ANN candidate limit at query time.
    pub hnsw_ef_search: u32,
    /// Local embedding lane for `private`/`secret` memories: an
    /// OpenAI-compatible embeddings endpoint on this host or private network.
    /// Loopback/private targets are allowed for THIS variable only (scoped
    /// validation); when unset, operations that would send private content to
    /// the remote provider are refused instead.
    pub local_embedding_base_url: Option<String>,

    // ── Management API security ───────────────────────────────────────────────
    /// When set, all /api/v1/* routes require this key via
    /// X-Management-Key or Authorization: Bearer headers.
    pub management_api_key: Option<String>,
    /// Must be explicitly set to `true` when `management_api_key` is absent.
    /// Prevents accidental unauthenticated exposure in production.
    /// Default: false.
    pub allow_unauth_management: bool,

    // ── Request limits ────────────────────────────────────────────────────────
    /// Maximum accepted request body size in bytes.  Requests larger than this
    /// are rejected with HTTP 413 before any processing.  Default: 10 MiB.
    pub max_body_bytes: usize,

    // ── LTM Archival ──────────────────────────────────────────────────────────
    /// How often the archival job runs in hours. 0 = disabled.
    pub archival_interval_hours: u64,
    /// L2 memories older than this many days with zero retrieval hits are
    /// candidates for compaction into L3.
    pub archival_min_age_days: u64,
    /// Minimum candidate count per agent before triggering compaction.
    pub archival_min_memories: usize,

    // ── Provider ──────────────────────────────────────────────────────────────
    /// Upstream LLM provider: "openai" (default) | "anthropic" | "gemini".
    /// Controls request translation and response parsing.
    pub upstream_provider: String,

    // ── Rate limiting ─────────────────────────────────────────────────────────
    /// Max proxy requests per minute per agent. 0 = disabled.
    pub rate_limit_rpm: u32,
    /// Token bucket burst size (max instantaneous quota).
    pub rate_limit_burst: u32,

    // ── Graph retrieval ───────────────────────────────────────────────────────
    /// When true, retrieval augments vector search with a graph walk over
    /// `memory_entity_links` and `memory_graph`. Default: false.
    pub graph_retrieval_enabled: bool,

    // ── Data integrity ────────────────────────────────────────────────────────
    /// Cosine distance below which a new L2 memory is considered a near-duplicate
    /// of an existing one and skipped. 0.0 = disabled.
    pub dedup_threshold: f64,
    /// When true, an async LLM call is made after each L2 insert to detect
    /// contradictions against the top-k most similar existing memories.
    pub conflict_detection_enabled: bool,

    // ── Retrieval logging ─────────────────────────────────────────────────────
    /// When false (default), only the SHA-256 hash of the query text is stored
    /// in memory_retrieval_logs (privacy-safe).  Set to true to also store the
    /// raw query_text (useful for debugging retrieval quality).
    pub retrieval_log_query_text: bool,

    // ── Adaptive Memory Pressure ──────────────────────────────────────────────
    /// AMP is disabled by default.  Set `AMP_ENABLED=true` to activate.
    pub amp_config: AmpConfig,

    // ── Reflexive Memory Kernel ───────────────────────────────────────────────
    /// RMK is disabled by default.  Set `RMK_ENABLED=true` to activate.
    pub rmk_config: RmkConfig,

    // ── HNSW maintenance ─────────────────────────────────────────────────────
    /// Enable/disable periodic pgvector HNSW index maintenance.
    pub hnsw_maintenance_enabled: bool,
    /// Maintenance cadence in hours. 0 disables automatic runs.
    pub hnsw_maintenance_interval_hours: u64,
    /// Run `REINDEX INDEX CONCURRENTLY idx_memories_hnsw` as part of each cycle.
    pub hnsw_maintenance_reindex_enabled: bool,
    /// Run `VACUUM` (and `ANALYZE`) on `memories` as part of each cycle.
    pub hnsw_maintenance_vacuum_enabled: bool,
    /// Enable `ANALYZE` inside maintenance `VACUUM`.
    pub hnsw_maintenance_vacuum_analyze: bool,
    /// Shared advisory-lock key for `pg_advisory_lock` synchronization across workers.
    pub hnsw_maintenance_advisory_lock_id: i64,
    /// Optional `maintenance_work_mem` override (`2GB`, `512MB`, ...).
    pub hnsw_maintenance_work_mem: Option<String>,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        Ok(Self {
            database_url: std::env::var("DATABASE_URL").context("DATABASE_URL is required")?,
            upstream_base_url: {
                let raw = std::env::var("UPSTREAM_BASE_URL")
                    .unwrap_or_else(|_| "https://api.openai.com".to_string());
                validate_provider_url("UPSTREAM_BASE_URL", &raw)?
            },
            port: std::env::var("PORT")
                .unwrap_or_else(|_| "8080".to_string())
                .parse()
                .context("PORT must be a valid number")?,

            db_max_connections: std::env::var("DB_MAX_CONNECTIONS")
                .unwrap_or_else(|_| "20".to_string())
                .parse()
                .context("DB_MAX_CONNECTIONS must be a positive integer")?,
            db_acquire_timeout_secs: std::env::var("DB_ACQUIRE_TIMEOUT_SECS")
                .unwrap_or_else(|_| "5".to_string())
                .parse()
                .context("DB_ACQUIRE_TIMEOUT_SECS must be a positive integer")?,
            db_idle_timeout_secs: std::env::var("DB_IDLE_TIMEOUT_SECS")
                .unwrap_or_else(|_| "300".to_string())
                .parse()
                .context("DB_IDLE_TIMEOUT_SECS must be a positive integer")?,

            openai_api_key: std::env::var("OPENAI_API_KEY").ok(),
            embedding_model: std::env::var("EMBEDDING_MODEL")
                .unwrap_or_else(|_| "text-embedding-3-small".to_string()),
            embedding_dimension: std::env::var("EMBEDDING_DIMENSION")
                .unwrap_or_else(|_| "1536".to_string())
                .parse()
                .context("EMBEDDING_DIMENSION must be a number")?,
            embedding_base_url: {
                let raw = std::env::var("EMBEDDING_BASE_URL").unwrap_or_else(|_| {
                    std::env::var("UPSTREAM_BASE_URL")
                        .unwrap_or_else(|_| "https://api.openai.com".to_string())
                });
                validate_provider_url("EMBEDDING_BASE_URL", &raw)?
            },

            extractor_model: std::env::var("EXTRACTOR_MODEL")
                .unwrap_or_else(|_| "gpt-4o-mini".to_string()),
            extractor_base_url: {
                let raw = std::env::var("EXTRACTOR_BASE_URL")
                    .unwrap_or_else(|_| "https://api.openai.com".to_string());
                validate_provider_url("EXTRACTOR_BASE_URL", &raw)?
            },

            retrieval_threshold: std::env::var("RETRIEVAL_THRESHOLD")
                .unwrap_or_else(|_| DEFAULT_RETRIEVAL_THRESHOLD.to_string())
                .parse()
                .context("RETRIEVAL_THRESHOLD must be a float")?,
            memory_decay_rate: std::env::var("MEMORY_DECAY_RATE")
                .unwrap_or_else(|_| "0.0".to_string())
                .parse()
                .context("MEMORY_DECAY_RATE must be a float")?,
            ann_candidate_limit: std::env::var("ANN_CANDIDATE_LIMIT")
                .unwrap_or_else(|_| "100".to_string())
                .parse()
                .context("ANN_CANDIDATE_LIMIT must be an integer")?,
            hnsw_ef_search: std::env::var("HNSW_EF_SEARCH")
                .unwrap_or_else(|_| "100".to_string())
                .parse()
                .context("HNSW_EF_SEARCH must be an integer")?,
            local_embedding_base_url: match std::env::var("LOCAL_EMBEDDING_BASE_URL") {
                Ok(raw) if !raw.trim().is_empty() => Some(validate_local_provider_url(
                    "LOCAL_EMBEDDING_BASE_URL",
                    raw.trim(),
                )?),
                _ => None,
            },
            importance_boost_factor: std::env::var("IMPORTANCE_BOOST_FACTOR")
                .unwrap_or_else(|_| "0.0".to_string())
                .parse()
                .context("IMPORTANCE_BOOST_FACTOR must be a float")?,
            importance_refresh_boost: std::env::var("IMPORTANCE_REFRESH_BOOST")
                .unwrap_or_else(|_| "0.05".to_string())
                .parse()
                .context("IMPORTANCE_REFRESH_BOOST must be a float")?,

            management_api_key: std::env::var("MANAGEMENT_API_KEY")
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty()),
            allow_unauth_management: std::env::var("ALLOW_UNAUTH_MANAGEMENT")
                .unwrap_or_else(|_| "false".to_string())
                .eq_ignore_ascii_case("true"),

            max_body_bytes: std::env::var("MAX_BODY_BYTES")
                .unwrap_or_else(|_| (10 * 1024 * 1024).to_string())
                .parse()
                .context("MAX_BODY_BYTES must be a positive integer")?,

            archival_interval_hours: std::env::var("ARCHIVAL_INTERVAL_HOURS")
                .unwrap_or_else(|_| "24".to_string())
                .parse()
                .context("ARCHIVAL_INTERVAL_HOURS must be a number")?,
            archival_min_age_days: std::env::var("ARCHIVAL_MIN_AGE_DAYS")
                .unwrap_or_else(|_| "7".to_string())
                .parse()
                .context("ARCHIVAL_MIN_AGE_DAYS must be a number")?,
            archival_min_memories: std::env::var("ARCHIVAL_MIN_MEMORIES")
                .unwrap_or_else(|_| "10".to_string())
                .parse()
                .context("ARCHIVAL_MIN_MEMORIES must be a number")?,

            upstream_provider: std::env::var("UPSTREAM_PROVIDER")
                .unwrap_or_else(|_| "openai".to_string()),

            rate_limit_rpm: std::env::var("RATE_LIMIT_RPM")
                .unwrap_or_else(|_| "0".to_string())
                .parse()
                .context("RATE_LIMIT_RPM must be a non-negative integer")?,
            rate_limit_burst: std::env::var("RATE_LIMIT_BURST")
                .unwrap_or_else(|_| "20".to_string())
                .parse()
                .context("RATE_LIMIT_BURST must be a non-negative integer")?,

            graph_retrieval_enabled: std::env::var("GRAPH_RETRIEVAL_ENABLED")
                .unwrap_or_else(|_| "false".to_string())
                .parse()
                .unwrap_or(false),

            dedup_threshold: std::env::var("DEDUP_THRESHOLD")
                .unwrap_or_else(|_| "0.05".to_string())
                .parse()
                .context("DEDUP_THRESHOLD must be a float")?,
            conflict_detection_enabled: std::env::var("CONFLICT_DETECTION_ENABLED")
                .unwrap_or_else(|_| "false".to_string())
                .eq_ignore_ascii_case("true"),

            retrieval_log_query_text: std::env::var("RETRIEVAL_LOG_QUERY_TEXT")
                .unwrap_or_else(|_| "false".to_string())
                .eq_ignore_ascii_case("true"),

            amp_config: AmpConfig {
                enabled: std::env::var("AMP_ENABLED")
                    .unwrap_or_else(|_| "false".to_string())
                    .eq_ignore_ascii_case("true"),
                ..Default::default()
            },

            rmk_config: RmkConfig {
                enabled: std::env::var("RMK_ENABLED")
                    .unwrap_or_else(|_| "false".to_string())
                    .eq_ignore_ascii_case("true"),
                // Rate-limiting knobs for the meta-learning loop.  Previously only
                // `enabled` was env-wired; these fell back to hardcoded defaults
                // (3600 s / 20), making a "compressed cadence" impossible to set.
                update_cooldown_secs: std::env::var("RMK_UPDATE_COOLDOWN_SECS")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or_else(|| RmkConfig::default().update_cooldown_secs),
                min_episodes_before_update: std::env::var("RMK_MIN_EPISODES_BEFORE_UPDATE")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or_else(|| RmkConfig::default().min_episodes_before_update),
                ..Default::default()
            },

            hnsw_maintenance_enabled: std::env::var("HNSW_MAINTENANCE_ENABLED")
                .unwrap_or_else(|_| "true".to_string())
                .eq_ignore_ascii_case("true"),
            hnsw_maintenance_interval_hours: std::env::var("HNSW_MAINTENANCE_INTERVAL_HOURS")
                .unwrap_or_else(|_| "168".to_string())
                .parse()
                .context("HNSW_MAINTENANCE_INTERVAL_HOURS must be a number")?,
            hnsw_maintenance_reindex_enabled: std::env::var("HNSW_MAINTENANCE_REINDEX_ENABLED")
                .unwrap_or_else(|_| "true".to_string())
                .eq_ignore_ascii_case("true"),
            hnsw_maintenance_vacuum_enabled: std::env::var("HNSW_MAINTENANCE_VACUUM_ENABLED")
                .unwrap_or_else(|_| "true".to_string())
                .eq_ignore_ascii_case("true"),
            hnsw_maintenance_vacuum_analyze: std::env::var("HNSW_MAINTENANCE_VACUUM_ANALYZE")
                .unwrap_or_else(|_| "true".to_string())
                .eq_ignore_ascii_case("true"),
            hnsw_maintenance_advisory_lock_id: std::env::var("HNSW_MAINTENANCE_ADVISORY_LOCK_ID")
                .unwrap_or_else(|_| "173456781234".to_string())
                .parse()
                .context("HNSW_MAINTENANCE_ADVISORY_LOCK_ID must be a signed integer")?,
            hnsw_maintenance_work_mem: {
                let raw = std::env::var("HNSW_MAINTENANCE_WORK_MEM")
                    .unwrap_or_else(|_| "".to_string())
                    .trim()
                    .to_string();
                if raw.is_empty() {
                    None
                } else {
                    Some(raw)
                }
            },
        })
    }

    pub fn extraction_outbox_config(&self) -> Result<ExtractionOutboxConfig> {
        Ok(ExtractionOutboxConfig {
            enabled: Self::env_bool("EXTRACTION_OUTBOX_ENABLED", true),
            worker_poll_secs: std::env::var("EXTRACTION_WORKER_POLL_SECS")
                .unwrap_or_else(|_| "1".to_string())
                .parse()
                .context("EXTRACTION_WORKER_POLL_SECS must be a positive integer")?,
            max_attempts: std::env::var("EXTRACTION_MAX_ATTEMPTS")
                .unwrap_or_else(|_| "5".to_string())
                .parse()
                .context("EXTRACTION_MAX_ATTEMPTS must be a positive integer")?,
            backoff_base_secs: std::env::var("EXTRACTION_BACKOFF_BASE_SECS")
                .unwrap_or_else(|_| "2".to_string())
                .parse()
                .context("EXTRACTION_BACKOFF_BASE_SECS must be a positive integer")?,
        })
    }

    pub fn extraction_outbox_enabled(&self) -> bool {
        Self::env_bool("EXTRACTION_OUTBOX_ENABLED", true)
    }

    fn env_bool(name: &str, default: bool) -> bool {
        std::env::var(name)
            .map(|v| v.eq_ignore_ascii_case("true"))
            .unwrap_or(default)
    }
}

#[cfg(test)]
mod tests {
    use super::ProcessRole;

    #[test]
    fn process_role_parses_supported_values() {
        assert_eq!(
            ProcessRole::from_env_value(Some("proxy")).unwrap(),
            ProcessRole::Proxy
        );
        assert_eq!(
            ProcessRole::from_env_value(Some("PROXY")).unwrap(),
            ProcessRole::Proxy
        );
        assert_eq!(
            ProcessRole::from_env_value(Some("worker")).unwrap(),
            ProcessRole::Worker
        );
        assert_eq!(
            ProcessRole::from_env_value(Some("all")).unwrap(),
            ProcessRole::All
        );
        assert_eq!(
            ProcessRole::from_env_value(Some("")).unwrap(),
            ProcessRole::All
        );
        assert_eq!(ProcessRole::from_env_value(None).unwrap(), ProcessRole::All);
    }

    #[test]
    fn process_role_rejects_unknown_values() {
        let error = ProcessRole::from_env_value(Some("bogus")).unwrap_err();

        assert!(
            error.to_string().contains("MEMORYOS_ROLE"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn process_role_serves_proxy_and_runs_workers_truth_table() {
        assert!(ProcessRole::Proxy.serves_proxy());
        assert!(!ProcessRole::Proxy.runs_workers());

        assert!(!ProcessRole::Worker.serves_proxy());
        assert!(ProcessRole::Worker.runs_workers());

        assert!(ProcessRole::All.serves_proxy());
        assert!(ProcessRole::All.runs_workers());
    }
}
