use rand::Rng;

use super::policy::PolicyParams;

/// Hard parameter bounds (min, max) for each θ dimension.
/// Values outside these ranges are physically meaningless.
const BOUNDS: [(f64, f64); 6] = [
    (0.001, 0.5), // pressure_a
    (0.05, 2.0),  // pressure_b
    (0.01, 1.0),  // kp
    (0.001, 0.5), // ki
    (0.0, 1.0),   // graph_bonus_weight
    (0.05, 0.99), // retrieval_threshold
];

/// Perturbation magnitude as a fraction of each parameter's full range.
const PERTURBATION_SCALE: f64 = 0.10;

/// ε-greedy policy explorer.
///
/// The learner itself is stateless across worker cycles: it proposes a
/// (possibly perturbed) candidate from a base policy, and the hill-climbing
/// accept/reject decision is made in `rmk_worker` from *persisted* per-policy
/// mean rewards (`rmk_episodes.policy_id` grouping).  An earlier design kept
/// `previous`/`last_reward` state on this struct for in-memory hill-climbing,
/// but the worker constructs a fresh learner every cycle, so that state could
/// never survive; it has been removed in favour of the DB-backed loop.
///
/// Safety invariant: parameters are always clamped to `BOUNDS` after
/// perturbation, so every policy the learner can ever emit lies in the
/// compact box ∏[loᵢ, hiᵢ].
pub struct MetaLearner {
    /// Exploration rate: probability of returning a perturbed policy.
    pub epsilon: f64,
    pub current: PolicyParams,
}

impl MetaLearner {
    pub fn new(epsilon: f64, initial: PolicyParams) -> Self {
        Self {
            epsilon,
            current: initial,
        }
    }

    /// ε-greedy: with probability `epsilon` return a uniformly-perturbed
    /// policy bounded by `BOUNDS`; otherwise return the current policy.
    ///
    /// Each perturbed dimension is offset by ±`PERTURBATION_SCALE × range`
    /// where range = hi − lo for that parameter.
    pub fn suggest_explore(&self) -> PolicyParams {
        let mut rng = rand::thread_rng();
        if rng.gen::<f64>() >= self.epsilon {
            return self.current.clone();
        }

        let vals = [
            self.current.pressure_a,
            self.current.pressure_b,
            self.current.kp,
            self.current.ki,
            self.current.graph_bonus_weight,
            self.current.retrieval_threshold,
        ];

        let perturbed: Vec<f64> = vals
            .iter()
            .zip(BOUNDS.iter())
            .map(|(&v, &(lo, hi))| {
                let noise = rng.gen_range(-PERTURBATION_SCALE..PERTURBATION_SCALE) * (hi - lo);
                (v + noise).clamp(lo, hi)
            })
            .collect();

        PolicyParams {
            pressure_a: perturbed[0],
            pressure_b: perturbed[1],
            kp: perturbed[2],
            ki: perturbed[3],
            graph_bonus_weight: perturbed[4],
            retrieval_threshold: perturbed[5],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn in_bounds(p: &PolicyParams) -> bool {
        let vals = [
            p.pressure_a,
            p.pressure_b,
            p.kp,
            p.ki,
            p.graph_bonus_weight,
            p.retrieval_threshold,
        ];
        vals.iter()
            .zip(BOUNDS.iter())
            .all(|(&v, &(lo, hi))| v >= lo && v <= hi)
    }

    /// Every policy the learner emits stays within BOUNDS, even when
    /// exploring from a base policy sitting on a bound (Proposition 10 of the
    /// white paper: containment of policy exploration).
    #[test]
    fn suggest_explore_respects_bounds() {
        // ε = 1.0 forces a perturbation every call.
        let corner = PolicyParams {
            pressure_a: 0.5,
            pressure_b: 0.05,
            kp: 1.0,
            ki: 0.001,
            graph_bonus_weight: 0.0,
            retrieval_threshold: 0.99,
        };
        let learner = MetaLearner::new(1.0, corner);
        for _ in 0..1000 {
            let candidate = learner.suggest_explore();
            assert!(in_bounds(&candidate), "candidate escaped BOUNDS");
        }

        let default_learner = MetaLearner::new(1.0, PolicyParams::default());
        for _ in 0..1000 {
            assert!(in_bounds(&default_learner.suggest_explore()));
        }
    }

    /// With ε = 0 the learner is pure exploitation: the base policy is
    /// returned unchanged.
    #[test]
    fn zero_epsilon_never_perturbs() {
        let base = PolicyParams::default();
        let learner = MetaLearner::new(0.0, base.clone());
        for _ in 0..100 {
            let candidate = learner.suggest_explore();
            assert_eq!(candidate.pressure_a, base.pressure_a);
            assert_eq!(candidate.retrieval_threshold, base.retrieval_threshold);
        }
    }
}
