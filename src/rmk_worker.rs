use std::time::Duration;

use tracing::{info, warn};

use crate::{
    memory::{
        amp::{co_access::CoAccessGraph, pi_controller::PIController, pressure::PressureManager},
        rmk::{meta_learner::MetaLearner, policy::PolicyParams, store},
    },
    AppState,
};

/// Periodically runs the RMK policy update cycle.
///
/// For every agent that has accumulated at least `min_episodes_before_update`
/// episodes since RMK was enabled, the meta-learner generates a new candidate
/// policy via ε-greedy exploration and persists it to `rmk_policies`.  The
/// next retrieval for that agent will pick up the new threshold automatically.
pub async fn run_policy_update_job(state: AppState) {
    let cooldown = state.config.rmk_config.update_cooldown_secs;
    let min_episodes = state.config.rmk_config.min_episodes_before_update;
    let epsilon = state.config.rmk_config.epsilon;

    info!(
        cooldown_secs = cooldown,
        min_episodes, epsilon, "RMK policy-update worker started"
    );

    loop {
        tokio::time::sleep(Duration::from_secs(cooldown)).await;

        let agents = match store::list_all_agent_ids_with_episodes(&state.db).await {
            Ok(a) => a,
            Err(e) => {
                warn!("RMK worker: failed to list agents: {}", e);
                continue;
            }
        };

        for agent_id in agents {
            if let Err(e) = update_policy_for_agent(&state, &agent_id, min_episodes, epsilon).await
            {
                warn!(agent_id = %agent_id, "RMK: policy update error: {}", e);
            }
        }
    }
}

/// Minimum episodes a policy must have accumulated before its mean reward is
/// treated as a meaningful hill-climb signal.  Below this the comparison is
/// skipped (too noisy to reject on).
const MIN_EPISODES_PER_POLICY: i64 = 5;

/// Pure hill-climbing acceptance rule: the currently serving policy survives
/// only if its measured mean reward did not regress against its
/// predecessor's.  Extracted as a free function for unit testing.
pub(crate) fn decide_accept(current_mean: f64, previous_mean: f64) -> bool {
    current_mean >= previous_mean
}

async fn update_policy_for_agent(
    state: &AppState,
    agent_id: &str,
    min_episodes: usize,
    epsilon: f64,
) -> anyhow::Result<()> {
    let count = store::count_episodes(&state.db, agent_id).await?;
    if count < min_episodes as i64 {
        return Ok(());
    }

    // Load the current serving policy; seed the defaults on first run and
    // return — exploration starts on the next cycle, once the seeded policy
    // has episodes of its own to be judged by.
    let (current_id, current_params) = match store::get_latest_policy(&state.db, agent_id).await? {
        Some(pair) => pair,
        None => {
            let seeded = PolicyParams::default();
            store::insert_policy(&state.db, agent_id, &seeded).await?;
            info!(agent_id = %agent_id, "RMK: seeded default policy");
            return Ok(());
        }
    };

    // Hill-climb accept/reject: compare the serving policy's measured mean
    // episode reward against its predecessor's.  A regression is rejected —
    // the policy stops serving and the next candidate explores from the last
    // known-good policy instead of compounding the regression.
    let mut base_params = current_params;
    let current_perf = store::get_mean_reward_for_policy(&state.db, current_id).await?;
    if let Some((current_mean, current_n)) = current_perf {
        if current_n >= MIN_EPISODES_PER_POLICY {
            if let Some((prev_id, prev_params)) =
                store::get_previous_accepted_policy(&state.db, agent_id, current_id).await?
            {
                if let Some((prev_mean, prev_n)) =
                    store::get_mean_reward_for_policy(&state.db, prev_id).await?
                {
                    if prev_n >= MIN_EPISODES_PER_POLICY && !decide_accept(current_mean, prev_mean)
                    {
                        store::mark_policy_rejected(&state.db, current_id).await?;
                        info!(
                            agent_id = %agent_id,
                            current_mean, prev_mean,
                            "RMK: rejected regressing policy; re-rolling from predecessor"
                        );
                        base_params = prev_params;
                    }
                }
            }
        }
    }

    // ε-greedy: generate a (possibly perturbed) candidate from the accepted base.
    let learner = MetaLearner::new(epsilon, base_params);
    let candidate = learner.suggest_explore();

    store::insert_policy(&state.db, agent_id, &candidate).await?;
    let mean_reward = current_perf.map(|(m, _)| m);
    info!(agent_id = %agent_id, ?mean_reward, "RMK: policy updated");
    Ok(())
}

/// Periodically backfills `task_success` on recent episodes from real
/// retrieval feedback (`POST /api/v1/feedback`), then recomputes the reward.
///
/// Episodes are logged synchronously per proxied turn with the documented
/// default `task_success = 1.0` ("assumed"); feedback arrives asynchronously
/// and optionally.  This job attributes feedback to episodes via the injected
/// memory set recorded on each episode: a feedback event for memory M counts
/// toward every recent episode that injected M after which the feedback was
/// given (24 h attribution window).  Episodes that never receive feedback
/// keep the assumed value — deliberately, so reward magnitudes for agents
/// that never call /feedback are unchanged.
pub async fn run_task_success_aggregation_job(state: AppState) {
    const INTERVAL_SECS: u64 = 120;
    info!(
        interval_secs = INTERVAL_SECS,
        "RMK task-success aggregation worker started"
    );
    loop {
        tokio::time::sleep(Duration::from_secs(INTERVAL_SECS)).await;
        match aggregate_task_success(&state).await {
            Ok(0) => {}
            Ok(n) => info!(episodes = n, "RMK: backfilled task_success from feedback"),
            Err(e) => warn!("RMK task-success aggregation error: {}", e),
        }
    }
}

/// One aggregation pass; returns the number of episodes updated.
/// Idempotent: re-running with the same feedback recomputes the same values.
pub async fn aggregate_task_success(state: &AppState) -> anyhow::Result<u64> {
    let w = &state.config.rmk_config.reward_weights;
    let result = sqlx::query(
        "UPDATE rmk_episodes e SET
            task_success = f.avg_feedback,
            task_success_source = 'feedback',
            reward = $1 * f.avg_feedback
                   + $2 * e.token_savings
                   + $3 * e.retrieval_precision
                   + $4 * e.eviction_cost
         FROM (
            SELECT e2.id, AVG(rf.feedback) AS avg_feedback
            FROM rmk_episodes e2
            JOIN retrieval_feedback rf
              ON rf.agent_id = e2.agent_id
             AND rf.memory_id = ANY(e2.injected_memory_ids)
             AND rf.created_at >= e2.created_at
             AND rf.created_at <= e2.created_at + INTERVAL '24 hours'
            WHERE e2.created_at > NOW() - INTERVAL '48 hours'
              AND e2.injected_memory_ids IS NOT NULL
            GROUP BY e2.id
         ) f
         WHERE e.id = f.id",
    )
    .bind(w.task)
    .bind(w.token_savings)
    .bind(w.precision)
    .bind(w.eviction_cost)
    .execute(&state.db)
    .await?;
    Ok(result.rows_affected())
}

/// Periodically computes per-memory pressure scores and applies soft-eviction.
///
/// For each agent that has active memories, a fresh PIController is driven
/// by the gap between current and target active count.  Memories whose
/// computed pressure exceeds `threshold_high` are soft-evicted; soft-evicted
/// memories whose pressure has fallen below `threshold_low` are restored.
///
/// Runs every 5 minutes when AMP or RMK is enabled.
/// Classifier for the per-agent advisory lock that serializes AMP pressure
/// sweeps.  Used as the first key of `pg_advisory_xact_lock(int4, int4)`; the
/// second key is `hashtext(agent_id)`, so sweeps for different agents proceed
/// concurrently while sweeps for the SAME agent (background timer vs.
/// management-triggered force-sweep vs. an external orchestrator) serialize.
///
/// External drivers that mutate an agent's `memories` rows out-of-band MUST take
/// this same lock with the same classifier to avoid deadlocking against a sweep.
/// The longitudinal benchmark harness does exactly this (see
/// `benchmarks/scripts/run_longitudinal.py`, `AMP_SWEEP_LOCK_CLASSIFIER`).
pub(crate) const AMP_SWEEP_LOCK_CLASSIFIER: i32 = 0x0041_4D50; // 'A','M','P'

pub async fn run_pressure_sweep_job(state: AppState) {
    info!("AMP pressure sweep worker started (interval=5min)");

    loop {
        tokio::time::sleep(Duration::from_secs(5 * 60)).await;

        // Fetch all distinct agent IDs that have at least one active memory.
        let agents: Vec<String> = match sqlx::query_scalar(
            "SELECT DISTINCT agent_id FROM memories WHERE archived_at IS NULL",
        )
        .fetch_all(&state.db)
        .await
        {
            Ok(a) => a,
            Err(e) => {
                warn!("Pressure sweep: failed to list agents: {}", e);
                continue;
            }
        };

        for agent_id in agents {
            if let Err(e) = run_pressure_sweep_for_agent(&state, &agent_id).await {
                warn!(agent_id = %agent_id, "Pressure sweep error: {}", e);
            }
        }
    }
}

/// Summary of one AMP pressure sweep, returned so the force-sweep management
/// endpoint (`POST /api/v1/agents/:id/amp/sweep`) can report convergence to a
/// caller driving sweeps deterministically (e.g. the longitudinal benchmark).
#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct SweepReport {
    pub active_count: i64,
    pub target: u64,
    pub aggressiveness: f64,
    pub soft_evicted_total: i64,
    pub newly_evicted: i64,
    pub newly_restored: i64,
}

pub(crate) async fn run_pressure_sweep_for_agent(
    state: &AppState,
    agent_id: &str,
) -> anyhow::Result<SweepReport> {
    let target = state.config.amp_config.target_active_count;
    let pressure_params = state.config.amp_config.pressure_params.clone();
    let controller_params = state.config.amp_config.controller_params.clone();
    let min_age_seconds = controller_params.min_age_seconds;

    // Serialize this sweep against (a) the autonomous 5-minute pressure-sweep
    // timer, (b) other management-triggered force-sweeps, and (c) any external
    // orchestrator mutating the same agent's rows (e.g. the longitudinal
    // benchmark harness, which takes this SAME per-agent advisory lock around its
    // aging SQL).  Without it, concurrent `UPDATE memories SET soft_evicted ...`
    // and `UPDATE memories SET created_at ...` acquire row locks in opposing
    // orders and deadlock.  The lock is transaction-scoped (auto-released on
    // commit/rollback) and spans the read→decide→write cycle, so the eviction
    // decision is taken against a snapshot no concurrent sweep can mutate.
    let mut tx = state.db.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock($1, hashtext($2))")
        .bind(AMP_SWEEP_LOCK_CLASSIFIER)
        .bind(agent_id)
        .execute(&mut *tx)
        .await?;

    // Count active (non-archived, non-soft-evicted) memories for this agent.
    let current_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM memories
         WHERE agent_id = $1 AND archived_at IS NULL AND soft_evicted = FALSE",
    )
    .bind(agent_id)
    .fetch_one(&mut *tx)
    .await?;

    // Restore persisted controller state so integral action accumulates
    // across sweeps (fresh construction discarded it every 5 minutes).
    let prior: Option<(f64, f64)> = sqlx::query_as(
        "SELECT aggressiveness, integral_error FROM amp_controller_state WHERE agent_id = $1",
    )
    .bind(agent_id)
    .fetch_optional(&mut *tx)
    .await?;
    let (agg0, int0) = prior.unwrap_or((0.0, 0.0));
    let mut pi = PIController::with_state(controller_params, agg0, int0);
    let (threshold_high, threshold_low) = pi.update(current_count as u64, target, 1.0);
    let (agg1, int1) = pi.state();
    // Inside the advisory-locked transaction a failed statement poisons the txn,
    // so this must propagate (abort the whole sweep) rather than warn-and-continue.
    sqlx::query(
        "INSERT INTO amp_controller_state (agent_id, aggressiveness, integral_error, updated_at)
         VALUES ($1, $2, $3, NOW())
         ON CONFLICT (agent_id) DO UPDATE
             SET aggressiveness = $2, integral_error = $3, updated_at = NOW()",
    )
    .bind(agent_id)
    .bind(agg1)
    .bind(int1)
    .execute(&mut *tx)
    .await?;

    // Fetch all non-archived memories (including currently soft-evicted ones so we
    // can restore them) with the pressure-relevant columns.
    let rows = sqlx::query(
        "SELECT id, last_accessed_at, created_at, utility_ema, soft_evicted
         FROM memories
         WHERE agent_id = $1 AND archived_at IS NULL",
    )
    .bind(agent_id)
    .fetch_all(&mut *tx)
    .await?;

    let pm = PressureManager::new(pressure_params);
    let now = chrono::Utc::now();

    let mut to_evict: Vec<uuid::Uuid> = Vec::new();
    let mut to_restore: Vec<uuid::Uuid> = Vec::new();
    let mut pressure_updates: Vec<(uuid::Uuid, f64)> = Vec::new();
    let mut initial_soft: i64 = 0;

    for row in rows {
        use sqlx::Row;
        let id: uuid::Uuid = row.get("id");
        let last_accessed: Option<chrono::DateTime<chrono::Utc>> = row.get("last_accessed_at");
        let created_at: chrono::DateTime<chrono::Utc> = row.get("created_at");
        let utility_ema: f64 = row.get("utility_ema");
        let soft_evicted: bool = row.get("soft_evicted");
        if soft_evicted {
            initial_soft += 1;
        }

        let pressure = pm.compute_pressure(last_accessed, created_at, utility_ema, now);
        pressure_updates.push((id, pressure));

        if soft_evicted {
            if PressureManager::should_restore(pressure, threshold_low) {
                to_restore.push(id);
            }
        } else if PressureManager::should_soft_evict(
            pressure,
            threshold_high,
            created_at,
            min_age_seconds,
            now,
        ) {
            to_evict.push(id);
        }
    }

    // Batch-update pressure scores in a single statement: the previous per-row
    // loop issued one pool acquire per memory, which starved hot-path queries
    // on large agents (observed as a p99 latency tail in the proxy benchmarks).
    if !pressure_updates.is_empty() {
        let (ids, pressures): (Vec<uuid::Uuid>, Vec<f64>) =
            pressure_updates.iter().cloned().unzip();
        sqlx::query(
            "UPDATE memories SET pressure = data.pressure
             FROM (SELECT UNNEST($1::uuid[]) AS id, UNNEST($2::float8[]) AS pressure) AS data
             WHERE memories.id = data.id",
        )
        .bind(&ids)
        .bind(&pressures)
        .execute(&mut *tx)
        .await?;
    }

    // Soft-evict.
    if !to_evict.is_empty() {
        sqlx::query(
            "UPDATE memories
             SET soft_evicted = TRUE, soft_evicted_at = NOW()
             WHERE id = ANY($1)",
        )
        .bind(&to_evict)
        .execute(&mut *tx)
        .await?;
        info!(agent_id = %agent_id, count = to_evict.len(), "AMP: soft-evicted memories");
    }

    // Restore.
    if !to_restore.is_empty() {
        sqlx::query(
            "UPDATE memories
             SET soft_evicted = FALSE, soft_evicted_at = NULL
             WHERE id = ANY($1)",
        )
        .bind(&to_restore)
        .execute(&mut *tx)
        .await?;
        info!(agent_id = %agent_id, count = to_restore.len(), "AMP: restored soft-evicted memories");
    }

    tx.commit().await?;

    let newly_evicted = to_evict.len() as i64;
    let newly_restored = to_restore.len() as i64;
    Ok(SweepReport {
        active_count: current_count - newly_evicted + newly_restored,
        target,
        aggressiveness: agg1,
        soft_evicted_total: initial_soft + newly_evicted - newly_restored,
        newly_evicted,
        newly_restored,
    })
}

/// Periodically decays co-access edge weights and prunes stale edges.
///
/// Runs once per day.  Decay prevents old co-access signals from dominating
/// the bonus score as conversation topics shift.
pub async fn run_co_access_decay_job(state: AppState) {
    info!("Co-access decay job started (interval=24h)");

    loop {
        tokio::time::sleep(Duration::from_secs(24 * 60 * 60)).await;

        let graph = CoAccessGraph::new(
            state.db.clone(),
            state.config.amp_config.co_access_params.clone(),
        );
        match graph.decay_all().await {
            Ok(()) => info!("Co-access decay pass completed"),
            Err(e) => warn!("Co-access decay failed: {}", e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::rmk::reward::EpisodeMetrics;
    use crate::test_support::test_state;
    use uuid::Uuid;

    #[test]
    fn accept_rule_is_non_regression() {
        assert!(decide_accept(1.0, 1.0), "ties survive");
        assert!(decide_accept(1.1, 1.0), "improvement survives");
        assert!(!decide_accept(0.9, 1.0), "regression is rejected");
    }

    async fn insert_episodes_for_policy(
        state: &crate::AppState,
        agent: &str,
        policy_id: Uuid,
        rewards: &[f64],
    ) {
        for &r in rewards {
            let metrics = EpisodeMetrics {
                task_success: 1.0,
                token_savings: 0.0,
                retrieval_precision: 0.0,
                eviction_cost: 0.0,
            };
            store::insert_episode(&state.db, agent, Some(policy_id), "s1", &[], &metrics, r)
                .await
                .unwrap();
        }
    }

    /// End-to-end hill-climb: a serving policy whose mean episode reward
    /// regresses against its predecessor is marked rejected, stops serving,
    /// and the next candidate re-rolls from the predecessor (ε = 0 makes the
    /// re-roll exact, so parameters can be compared directly).
    #[sqlx::test(migrations = "./migrations")]
    async fn regressing_policy_is_rejected_and_reroll_uses_predecessor(pool: sqlx::PgPool) {
        let state = test_state(pool);
        const AGENT: &str = "rmk-hillclimb-agent";

        // Policy A (good), then policy B (newer, worse).
        let params_a = PolicyParams {
            kp: 0.33, // distinctive marker for identity checks
            ..Default::default()
        };
        let id_a = store::insert_policy(&state.db, AGENT, &params_a)
            .await
            .unwrap();
        insert_episodes_for_policy(&state, AGENT, id_a, &[2.0; 6]).await;

        let params_b = PolicyParams {
            kp: 0.77,
            ..Default::default()
        };
        let id_b = store::insert_policy(&state.db, AGENT, &params_b)
            .await
            .unwrap();
        insert_episodes_for_policy(&state, AGENT, id_b, &[1.0; 6]).await;

        // Sanity: B currently serves.
        let (serving, _) = store::get_latest_policy(&state.db, AGENT)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(serving, id_b);

        // One worker cycle with ε = 0 (deterministic re-roll).
        update_policy_for_agent(&state, AGENT, 1, 0.0)
            .await
            .unwrap();

        // B is rejected; the new serving policy carries A's parameters.
        let status_b: String = sqlx::query_scalar("SELECT status FROM rmk_policies WHERE id = $1")
            .bind(id_b)
            .fetch_one(&state.db)
            .await
            .unwrap();
        assert_eq!(status_b, "rejected", "regressing policy must be rejected");

        let (new_id, new_params) = store::get_latest_policy(&state.db, AGENT)
            .await
            .unwrap()
            .unwrap();
        assert_ne!(new_id, id_b, "rejected policy must not serve");
        assert_eq!(
            new_params.kp, params_a.kp,
            "re-roll must start from the last known-good policy"
        );
    }

    /// A non-regressing serving policy survives and the next candidate
    /// explores from it, not from its predecessor.
    #[sqlx::test(migrations = "./migrations")]
    async fn improving_policy_is_kept(pool: sqlx::PgPool) {
        let state = test_state(pool);
        const AGENT: &str = "rmk-improve-agent";

        let params_a = PolicyParams {
            kp: 0.33,
            ..Default::default()
        };
        let id_a = store::insert_policy(&state.db, AGENT, &params_a)
            .await
            .unwrap();
        insert_episodes_for_policy(&state, AGENT, id_a, &[1.0; 6]).await;

        let params_b = PolicyParams {
            kp: 0.77,
            ..Default::default()
        };
        let id_b = store::insert_policy(&state.db, AGENT, &params_b)
            .await
            .unwrap();
        insert_episodes_for_policy(&state, AGENT, id_b, &[2.0; 6]).await;

        update_policy_for_agent(&state, AGENT, 1, 0.0)
            .await
            .unwrap();

        let status_b: String = sqlx::query_scalar("SELECT status FROM rmk_policies WHERE id = $1")
            .bind(id_b)
            .fetch_one(&state.db)
            .await
            .unwrap();
        assert_eq!(status_b, "accepted");

        let (_, new_params) = store::get_latest_policy(&state.db, AGENT)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            new_params.kp, params_b.kp,
            "candidate explores from the survivor"
        );
    }

    /// Controller state persists across pressure sweeps: with the active
    /// count held above target, integral error accumulates from one sweep to
    /// the next instead of resetting (the pre-fix behaviour).
    #[sqlx::test(migrations = "./migrations")]
    async fn controller_state_persists_across_sweeps(pool: sqlx::PgPool) {
        let mut state = test_state(pool);
        let mut cfg = (*state.config).clone();
        cfg.amp_config.target_active_count = 1;
        state.config = std::sync::Arc::new(cfg);
        const AGENT: &str = "amp-state-agent";

        let zero_vec = format!("[{}]", vec!["0"; 1536].join(","));
        for i in 0..3 {
            sqlx::query(
                "INSERT INTO memories (agent_id, content, memory_type, confidence, embedding)
                 VALUES ($1, $2, 'semantic', 1.0, $3::vector)",
            )
            .bind(AGENT)
            .bind(format!("m{i}"))
            .bind(&zero_vec)
            .execute(&state.db)
            .await
            .unwrap();
        }

        run_pressure_sweep_for_agent(&state, AGENT).await.unwrap();
        let (agg1, int1): (f64, f64) = sqlx::query_as(
            "SELECT aggressiveness, integral_error FROM amp_controller_state WHERE agent_id = $1",
        )
        .bind(AGENT)
        .fetch_one(&state.db)
        .await
        .unwrap();
        assert!(
            int1 > 0.0,
            "positive error must accumulate integral, got {int1}"
        );

        run_pressure_sweep_for_agent(&state, AGENT).await.unwrap();
        let (agg2, int2): (f64, f64) = sqlx::query_as(
            "SELECT aggressiveness, integral_error FROM amp_controller_state WHERE agent_id = $1",
        )
        .bind(AGENT)
        .fetch_one(&state.db)
        .await
        .unwrap();
        assert!(
            int2 > int1,
            "integral must keep accumulating across sweeps ({int1} -> {int2})"
        );
        assert!((0.0..=1.0).contains(&agg1) && (0.0..=1.0).contains(&agg2));
    }

    /// Feedback aggregation backfills task_success (and recomputes reward)
    /// only for episodes whose injected memories received feedback; episodes
    /// without feedback keep the assumed default.
    #[sqlx::test(migrations = "./migrations")]
    async fn feedback_backfills_task_success(pool: sqlx::PgPool) {
        let state = test_state(pool);
        const AGENT: &str = "rmk-feedback-agent";

        // Two real memory rows (retrieval_feedback has an FK to memories).
        let zero_vec = format!("[{}]", vec!["0"; 1536].join(","));
        let mut mem_ids = Vec::new();
        for i in 0..2 {
            let id: Uuid = sqlx::query_scalar(
                "INSERT INTO memories (agent_id, content, memory_type, confidence, embedding)
                 VALUES ($1, $2, 'semantic', 1.0, $3::vector) RETURNING id",
            )
            .bind(AGENT)
            .bind(format!("memory {i}"))
            .bind(&zero_vec)
            .fetch_one(&state.db)
            .await
            .unwrap();
            mem_ids.push(id);
        }

        let metrics = EpisodeMetrics {
            task_success: 1.0,
            token_savings: 0.4,
            retrieval_precision: 0.2,
            eviction_cost: 0.0,
        };
        // Episode 1 injected both memories; episode 2 injected none.
        store::insert_episode(&state.db, AGENT, None, "s1", &mem_ids, &metrics, 9.9)
            .await
            .unwrap();
        store::insert_episode(&state.db, AGENT, None, "s2", &[], &metrics, 9.9)
            .await
            .unwrap();

        // Feedback: 0.0 and 0.5 → avg 0.25.
        for (mem, fb) in mem_ids.iter().zip([0.0f64, 0.5f64]) {
            sqlx::query(
                "INSERT INTO retrieval_feedback (agent_id, memory_id, feedback) VALUES ($1, $2, $3)",
            )
            .bind(AGENT)
            .bind(mem)
            .bind(fb)
            .execute(&state.db)
            .await
            .unwrap();
        }

        let updated = aggregate_task_success(&state).await.unwrap();
        assert_eq!(
            updated, 1,
            "exactly the episode with injected memories updates"
        );

        use sqlx::Row;
        let rows = sqlx::query(
            "SELECT session_id, task_success, task_success_source, reward
             FROM rmk_episodes WHERE agent_id = $1 ORDER BY session_id",
        )
        .bind(AGENT)
        .fetch_all(&state.db)
        .await
        .unwrap();
        assert_eq!(rows.len(), 2);

        let fed: &sqlx::postgres::PgRow = &rows[0]; // s1
        assert_eq!(fed.get::<String, _>("task_success_source"), "feedback");
        let ts: f64 = fed.get("task_success");
        assert!(
            (ts - 0.25).abs() < 1e-9,
            "task_success = avg feedback, got {ts}"
        );
        // reward recomputed with default weights: 1.0*0.25 + 0.5*0.4 + 1.0*0.2 - 0.1*0.0
        let reward: f64 = fed.get("reward");
        assert!(
            (reward - 0.65).abs() < 1e-9,
            "reward recomputed, got {reward}"
        );

        let unfed: &sqlx::postgres::PgRow = &rows[1]; // s2
        assert_eq!(unfed.get::<String, _>("task_success_source"), "assumed");
        assert_eq!(unfed.get::<f64, _>("task_success"), 1.0);
        assert_eq!(unfed.get::<f64, _>("reward"), 9.9);
    }
}
