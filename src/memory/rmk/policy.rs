use serde::{Deserialize, Serialize};

/// Policy vector θ — the six AMP parameters optimised by the meta-learner.
///
/// Defaults are the same as the AMP component defaults so that enabling RMK
/// without prior training is equivalent to running with static AMP.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolicyParams {
    /// Recency coefficient in pressure formula: `a·days_stale`.
    pub pressure_a: f64,
    /// Utility coefficient in pressure formula: `b·(1 − utility_ema)`.
    pub pressure_b: f64,
    /// PI controller proportional gain.
    pub kp: f64,
    /// PI controller integral gain.
    pub ki: f64,
    /// Weight applied to co-access graph bonus during retrieval scoring.
    pub graph_bonus_weight: f64,
    /// Retrieval similarity threshold; memories below this are dropped.
    pub retrieval_threshold: f64,
}

impl Default for PolicyParams {
    fn default() -> Self {
        Self {
            pressure_a: 0.02,
            pressure_b: 0.4,
            kp: 0.15,
            ki: 0.02,
            graph_bonus_weight: 0.15,
            // A freshly seeded RMK policy must not silently change retrieval
            // strictness relative to a non-RMK deployment; the meta-learner
            // explores from here within BOUNDS.
            retrieval_threshold: crate::config::DEFAULT_RETRIEVAL_THRESHOLD,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seeded_policy_threshold_matches_static_default() {
        assert_eq!(
            PolicyParams::default().retrieval_threshold,
            crate::config::DEFAULT_RETRIEVAL_THRESHOLD,
        );
    }
}
