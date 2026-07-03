use anyhow::Result;
use sqlx::{PgPool, Row};
use uuid::Uuid;

use super::policy::PolicyParams;
use super::reward::EpisodeMetrics;

fn row_to_policy(r: &sqlx::postgres::PgRow) -> (Uuid, PolicyParams) {
    (
        r.get("id"),
        PolicyParams {
            pressure_a: r.get("pressure_a"),
            pressure_b: r.get("pressure_b"),
            kp: r.get("kp"),
            ki: r.get("ki"),
            graph_bonus_weight: r.get("graph_bonus_weight"),
            retrieval_threshold: r.get("retrieval_threshold"),
        },
    )
}

/// Fetch the most recent *accepted* policy for `agent_id`, or `None` if none
/// exists.  Rejected policies (hill-climb regressions) never serve.
pub async fn get_latest_policy(
    pool: &PgPool,
    agent_id: &str,
) -> Result<Option<(Uuid, PolicyParams)>> {
    let row = sqlx::query(
        "SELECT id, pressure_a, pressure_b, kp, ki, graph_bonus_weight, retrieval_threshold
         FROM rmk_policies
         WHERE agent_id = $1 AND status = 'accepted'
         ORDER BY created_at DESC
         LIMIT 1",
    )
    .bind(agent_id)
    .fetch_optional(pool)
    .await?;

    Ok(row.map(|r| row_to_policy(&r)))
}

/// Fetch the accepted policy immediately preceding `exclude_id` for an agent
/// (the hill-climb comparison baseline), or `None` if there is no predecessor.
pub async fn get_previous_accepted_policy(
    pool: &PgPool,
    agent_id: &str,
    exclude_id: Uuid,
) -> Result<Option<(Uuid, PolicyParams)>> {
    let row = sqlx::query(
        "SELECT id, pressure_a, pressure_b, kp, ki, graph_bonus_weight, retrieval_threshold
         FROM rmk_policies
         WHERE agent_id = $1 AND status = 'accepted' AND id != $2
         ORDER BY created_at DESC
         LIMIT 1",
    )
    .bind(agent_id)
    .bind(exclude_id)
    .fetch_optional(pool)
    .await?;

    Ok(row.map(|r| row_to_policy(&r)))
}

/// Mean episode reward and episode count for a specific policy.
/// Returns `None` when the policy has no episodes yet.
pub async fn get_mean_reward_for_policy(
    pool: &PgPool,
    policy_id: Uuid,
) -> Result<Option<(f64, i64)>> {
    let row = sqlx::query(
        "SELECT AVG(reward) AS mean_reward, COUNT(*) AS n
         FROM rmk_episodes WHERE policy_id = $1",
    )
    .bind(policy_id)
    .fetch_one(pool)
    .await?;
    let mean: Option<f64> = row.get("mean_reward");
    let n: i64 = row.get("n");
    Ok(mean.map(|m| (m, n)))
}

/// Mark a policy as rejected (hill-climb regression).  It stops serving
/// immediately; the row is kept for auditability.
pub async fn mark_policy_rejected(pool: &PgPool, policy_id: Uuid) -> Result<()> {
    sqlx::query("UPDATE rmk_policies SET status = 'rejected' WHERE id = $1")
        .bind(policy_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Persist a new policy vector and return its UUID.
pub async fn insert_policy(pool: &PgPool, agent_id: &str, policy: &PolicyParams) -> Result<Uuid> {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO rmk_policies \
         (id, agent_id, pressure_a, pressure_b, kp, ki, graph_bonus_weight, retrieval_threshold) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
    )
    .bind(id)
    .bind(agent_id)
    .bind(policy.pressure_a)
    .bind(policy.pressure_b)
    .bind(policy.kp)
    .bind(policy.ki)
    .bind(policy.graph_bonus_weight)
    .bind(policy.retrieval_threshold)
    .execute(pool)
    .await?;
    Ok(id)
}

/// Record one episode's performance metrics and computed reward, together
/// with the correlation data (session + injected memory IDs) the feedback
/// aggregation job uses to backfill a real `task_success` later.
pub async fn insert_episode(
    pool: &PgPool,
    agent_id: &str,
    policy_id: Option<Uuid>,
    session_id: &str,
    injected_memory_ids: &[Uuid],
    metrics: &EpisodeMetrics,
    reward: f64,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO rmk_episodes \
         (agent_id, policy_id, session_id, injected_memory_ids, \
          task_success, token_savings, retrieval_precision, eviction_cost, reward) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
    )
    .bind(agent_id)
    .bind(policy_id)
    .bind(session_id)
    .bind(injected_memory_ids)
    .bind(metrics.task_success)
    .bind(metrics.token_savings)
    .bind(metrics.retrieval_precision)
    .bind(metrics.eviction_cost)
    .bind(reward)
    .execute(pool)
    .await?;
    Ok(())
}

/// Count all episodes recorded for an agent.
pub async fn count_episodes(pool: &PgPool, agent_id: &str) -> Result<i64> {
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM rmk_episodes WHERE agent_id = $1")
        .bind(agent_id)
        .fetch_one(pool)
        .await?;
    Ok(count)
}

/// Return every distinct agent_id that has at least one episode recorded.
pub async fn list_all_agent_ids_with_episodes(pool: &PgPool) -> Result<Vec<String>> {
    let rows = sqlx::query("SELECT DISTINCT agent_id FROM rmk_episodes")
        .fetch_all(pool)
        .await?;
    Ok(rows.into_iter().map(|r| r.get("agent_id")).collect())
}
