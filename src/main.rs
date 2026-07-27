mod api;
mod archival;
mod attestation;
mod auth;
mod config;
mod credentials;
mod db;
mod embeddings;
mod extraction_worker;
mod hnsw_maintenance;
mod log_utils;
mod memory;
mod metrics;
mod models;
mod providers;
mod proxy;
mod rate_limit;
mod rmk_worker;
mod tenancy;
#[cfg(test)]
mod test_support;
mod url_guard;

use std::sync::Arc;

use axum::extract::DefaultBodyLimit;
use axum::{
    middleware,
    routing::{delete, get, patch, post},
    Router,
};
use tower_http::{cors::CorsLayer, trace::TraceLayer};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

pub use config::Config;
pub use db::DbPool;

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub db: DbPool,
    pub http_client: reqwest::Client,
    pub metrics: Arc<metrics::Metrics>,
    pub provider: providers::Provider,
    pub rate_limiter: Arc<rate_limit::RateLimiter>,
    pub evidence_signer: Arc<attestation::EvidenceSigner>,
    /// `None` unless `AEON_CREDENTIAL_AUTH_ENABLED=true`, which is the V1
    /// default and the state every existing deployment starts in.
    ///
    /// Nothing reads this yet: per-route enforcement is plan step 5. It is
    /// carried on the state so the subsystem is constructed and its startup
    /// preconditions are exercised, rather than sitting unreachable until the
    /// step that needs it.
    pub credential_auth: Option<Arc<credentials::Authenticator>>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();

    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::new(
            std::env::var("RUST_LOG")
                .unwrap_or_else(|_| "memoryos_kernel=debug,tower_http=info".into()),
        ))
        .with(tracing_subscriber::fmt::layer())
        .init();

    let config = Arc::new(Config::from_env()?);
    let role = config::ProcessRole::from_env()?;
    // Parsed before anything touches the database so a half-specified migration
    // mode fails immediately rather than after partial work.
    let tenancy_settings = tenancy::TenancySettings::from_env()?;
    // Parsed here for the same reason: a malformed pepper, or credential
    // authentication enabled with no pepper at all, must fail before any work
    // is done rather than at the first request. With none of the
    // AEON_CREDENTIAL_* variables set this yields the inactive V1 default and
    // requires no pepper.
    let credential_auth_settings = credentials::CredentialAuthSettings::from_env()?;
    tracing::info!(
        "MemoryOS Kernel starting on port {} with role {:?}",
        config.port,
        role
    );

    // Refuse to start if the management API would be unauthenticated without
    // an explicit operator acknowledgement.  This prevents accidental exposure
    // of /api/v1/* routes in production when the env var is simply forgotten.
    if config.management_api_key.is_none() {
        if config.allow_unauth_management {
            tracing::warn!(
                "MANAGEMENT_API_KEY is not set and ALLOW_UNAUTH_MANAGEMENT=true — \
                 management endpoints are unauthenticated. Do not expose this service \
                 publicly without a management key."
            );
        } else {
            anyhow::bail!(
                "MANAGEMENT_API_KEY is not set. Either:\n  \
                 • Set MANAGEMENT_API_KEY=<secret> to require authentication, or\n  \
                 • Set ALLOW_UNAUTH_MANAGEMENT=true to explicitly allow unauthenticated \
                 access (development only)."
            );
        }
    }

    let provider = providers::Provider::from_str(&config.upstream_provider);
    tracing::info!("Upstream provider: {:?}", provider);

    if config.rate_limit_rpm > 0 {
        tracing::info!(
            "Rate limiting enabled: {} RPM per agent, burst {}",
            config.rate_limit_rpm,
            config.rate_limit_burst,
        );
    }

    let db = db::connect(
        &config.database_url,
        config.db_max_connections,
        config.db_acquire_timeout_secs,
        config.db_idle_timeout_secs,
    )
    .await?;
    db::run_migrations(&db).await?;
    tracing::info!("Database migrations applied");

    // ── Legacy agent tenancy ──────────────────────────────────────────────────
    // Runs only when an operator has explicitly selected a migration mode.  With
    // no mode set, no agent is assigned a tenant: there is no default tenant and
    // none is generated (plan §6 / decision A-2).
    if let Some(plan) = tenancy_settings.legacy_plan.as_ref() {
        let report = tenancy::run_legacy_backfill(&db, plan).await?;
        tracing::info!(
            mode = report.mode.as_str(),
            legacy_tenant_id = ?report.legacy_tenant_id,
            agents_total = report.agents_total,
            agents_assigned = report.agents_assigned,
            agents_unmapped = report.agents_unmapped,
            already_applied = report.already_applied,
            "Legacy agent tenancy migration applied"
        );
        if report.agents_unmapped > 0 {
            tracing::warn!(
                agents_unmapped = report.agents_unmapped,
                "Agents remain unmapped. They belong to no tenant and are served to \
                 nobody; multi-tenant mode cannot be enabled until every agent is mapped."
            );
        }
    }

    // Inert unless MULTI_TENANT_ENABLED=true, which nothing sets by default.
    tenancy::assert_multi_tenant_preconditions(
        &db,
        &tenancy_settings,
        tenancy::ManagementAuth {
            legacy_key_accepted: config.management_api_key.is_some(),
            // `check_management_key` skips the whole comparison when no key is
            // configured (auth.rs:22), so this combination leaves every
            // /api/v1/* route open rather than merely un-keyed.
            unauthenticated: config.management_api_key.is_none() && config.allow_unauth_management,
        },
    )
    .await?;

    // Also inert unless MULTI_TENANT_ENABLED=true.  Separate from the gate
    // above because it asks a different question: that one asks whether the
    // legacy path has been retired, this one asks whether anything exists to
    // replace it.  Enabling multi-tenancy with an empty registry would leave a
    // deployment nothing could authenticate against (plan §2 "Startup").
    credentials::assert_registry_ready_for_multi_tenant(&db, tenancy_settings.multi_tenant_enabled)
        .await?;

    // Before the authenticator exists, not after: `CREATE TABLE IF NOT EXISTS`
    // will happily leave a pre-existing `credentials` table in place with no
    // primary key or weakened CHECK constraints, and record the migration as
    // applied. Skipped entirely while the subsystem is off, so a V1 deployment
    // is unaffected.
    if credential_auth_settings.enabled {
        credentials::assert_schema_contract(&db).await?;
    }

    let credential_auth = credential_auth_settings.build(db.clone())?.map(Arc::new);
    match credential_auth.as_ref() {
        Some(_) => tracing::info!(
            cache_ttl_secs = credentials::MAX_TTL.as_secs(),
            cache_capacity = credential_auth_settings.cache_capacity,
            "Credential authentication subsystem loaded (registry available; \
             per-route enforcement is not yet active)"
        ),
        None => tracing::debug!(
            "Credential authentication is inactive (AEON_CREDENTIAL_AUTH_ENABLED is not true); \
             MANAGEMENT_API_KEY remains the only authentication in effect"
        ),
    }

    // Redirects are disabled: provider API calls (LLM, embeddings, extraction)
    // should never be redirected cross-host.  Following redirects by default
    // would allow a validated public URL to 302 to an internal address
    // (e.g. 169.254.169.254) at request time, bypassing the startup SSRF guard.
    let http_client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(300))
        .redirect(reqwest::redirect::Policy::none())
        .build()?;

    let m = metrics::Metrics::new()?;
    let rate_limiter = Arc::new(rate_limit::RateLimiter::new(
        config.rate_limit_rpm,
        config.rate_limit_burst,
    ));

    let evidence_signer = Arc::new(attestation::EvidenceSigner::from_env()?);
    if evidence_signer.persistent {
        tracing::info!(key_id = %evidence_signer.key_id(), "Evidence signing key loaded from env");
    } else {
        tracing::warn!(
            key_id = %evidence_signer.key_id(),
            "AEON_EVIDENCE_SIGNING_KEY not set — using an ephemeral evidence signing key (key_id changes on restart; Nexus verifiers must pin a persistent key in production)"
        );
    }

    let state = AppState {
        config: config.clone(),
        db,
        http_client,
        metrics: Arc::new(m),
        provider,
        rate_limiter,
        evidence_signer,
        credential_auth,
    };

    // ── Background jobs ───────────────────────────────────────────────────────
    if role.runs_workers() {
        let extraction_outbox_config = config.extraction_outbox_config()?;
        if config.archival_interval_hours > 0 {
            tokio::spawn(archival::run_job(state.clone()));
        }
        if config.hnsw_maintenance_enabled && config.hnsw_maintenance_interval_hours > 0 {
            tokio::spawn(hnsw_maintenance::run_job(state.clone()));
        }
        if extraction_outbox_config.enabled {
            tokio::spawn(extraction_worker::run_job(
                state.clone(),
                extraction_outbox_config.clone(),
            ));
        }
        if config.rmk_config.enabled {
            tokio::spawn(rmk_worker::run_policy_update_job(state.clone()));
            tokio::spawn(rmk_worker::run_task_success_aggregation_job(state.clone()));
            tokio::spawn(rmk_worker::run_co_access_decay_job(state.clone()));
            tokio::spawn(rmk_worker::run_pressure_sweep_job(state.clone()));
        } else if config.amp_config.enabled {
            // AMP co-access decay and pressure sweep run even without RMK.
            tokio::spawn(rmk_worker::run_co_access_decay_job(state.clone()));
            tokio::spawn(rmk_worker::run_pressure_sweep_job(state.clone()));
        }
    } else {
        tracing::info!("Background jobs disabled for role {:?}", role);
    }

    let app = if role.serves_proxy() {
        // ── Management sub-router (authenticated) ─────────────────────────────
        let management = Router::new()
            .route("/agents", get(api::list_agents))
            .route("/agents/:agent_id", delete(api::delete_agent))
            .route("/agents/:agent_id/memories", get(api::list_memories))
            .route("/agents/:agent_id/memories", post(api::create_memory))
            .route(
                "/agents/:agent_id/memories/at",
                get(api::memories_at_timestamp),
            )
            .route("/agents/:agent_id/memories/diff", get(api::memories_diff))
            .route(
                "/agents/:agent_id/timeline",
                post(api::record_hypervisor_timeline_event),
            )
            .route(
                "/agents/:agent_id/timeline/at",
                get(api::resolve_hypervisor_snapshot_at),
            )
            .route("/agents/:agent_id/memories/bulk", post(api::bulk_operation))
            .route(
                "/agents/:agent_id/memories/archived",
                get(api::list_archived_memories),
            )
            .route(
                "/agents/:agent_id/archival/batches",
                get(api::list_archival_batches),
            )
            .route(
                "/agents/:agent_id/archival/trigger",
                post(api::trigger_archival),
            )
            .route("/agents/:agent_id/amp/sweep", post(api::trigger_amp_sweep))
            .route("/agents/:agent_id/export", get(api::export_memories))
            .route("/agents/:agent_id/import", post(api::import_memories))
            .route("/agents/:agent_id/sessions", get(api::list_sessions))
            .route(
                "/agents/:agent_id/sessions/:session_id",
                get(api::get_session),
            )
            .route(
                "/agents/:agent_id/sessions/:session_id",
                delete(api::delete_session),
            )
            .route("/agents/:agent_id/retrievals", get(api::list_retrievals))
            .route("/agents/:agent_id/lifecycle", get(api::agent_lifecycle))
            .route("/agents/:agent_id/conflicts", get(api::list_conflicts))
            .route(
                "/conflicts/:conflict_id/resolve",
                post(api::resolve_conflict),
            )
            .route("/memories/search", post(api::search_memories_semantic))
            .route("/memories/:id", patch(api::patch_memory))
            .route("/memories/:id", delete(api::delete_memory))
            .route("/memories/:id/restore", post(api::restore_memory))
            .route("/memories/:id/versions", get(api::list_memory_versions))
            .route("/memories/:id/status", patch(api::patch_memory_status))
            .route(
                "/memories/:id/sensitivity",
                patch(api::patch_memory_sensitivity),
            )
            .route(
                "/archival/batches/:batch_id/restore",
                post(api::restore_archival_batch),
            )
            .route("/stats", get(api::get_stats))
            .route(
                "/evidence/verifying-key",
                get(api::get_evidence_verifying_key),
            )
            .route("/feedback", post(api::post_feedback))
            .layer(middleware::from_fn_with_state(
                state.clone(),
                auth::check_management_key,
            ));

        // ── Root router ───────────────────────────────────────────────────────
        Router::new()
            .route("/v1/chat/completions", post(proxy::handle_chat_completions))
            .route("/v1/models", get(proxy::handle_models))
            .nest("/api/v1", management)
            .route("/metrics", get(metrics::handle_metrics))
            .route("/health", get(|| async { "OK" }))
            .layer(middleware::from_fn_with_state(
                state.clone(),
                metrics::track_request,
            ))
            .layer(DefaultBodyLimit::max(config.max_body_bytes))
            .layer(CorsLayer::permissive())
            .layer(TraceLayer::new_for_http())
            .with_state(state)
    } else {
        tracing::info!("Proxy routes disabled for worker-only role");
        Router::new()
            .route("/metrics", get(metrics::handle_metrics))
            .route("/health", get(|| async { "OK" }))
            .layer(middleware::from_fn_with_state(
                state.clone(),
                metrics::track_request,
            ))
            .layer(DefaultBodyLimit::max(config.max_body_bytes))
            .layer(CorsLayer::permissive())
            .layer(TraceLayer::new_for_http())
            .with_state(state)
    };

    let addr = format!("0.0.0.0:{}", config.port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!("Listening on {}", addr);

    axum::serve(listener, app).await?;
    Ok(())
}
