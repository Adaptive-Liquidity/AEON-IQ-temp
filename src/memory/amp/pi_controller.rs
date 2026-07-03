use super::types::ControllerParams;

/// PI controller that drives eviction aggressiveness based on the gap
/// between current and target active-memory count.
///
/// Returns `(threshold_high, threshold_low)`: memories with pressure above
/// `threshold_high` are soft-evicted; memories with pressure below
/// `threshold_low` are restored.  Hysteresis between the two thresholds
/// prevents oscillations.
pub struct PIController {
    params: ControllerParams,
    aggressiveness: f64,
    integral_error: f64,
}

impl PIController {
    /// Reconstruct a controller from persisted state (see
    /// `amp_controller_state`), so integral action survives across
    /// pressure-sweep cycles.  Inputs are clamped to the controller's own
    /// invariant ranges (Proposition 7 of the white paper), so corrupted or
    /// hand-edited rows cannot produce out-of-range behaviour.
    pub fn with_state(params: ControllerParams, aggressiveness: f64, integral_error: f64) -> Self {
        Self {
            params,
            aggressiveness: aggressiveness.clamp(0.0, 1.0),
            integral_error: integral_error.clamp(-10.0, 10.0),
        }
    }

    /// Current state for persistence: (aggressiveness, integral_error).
    pub fn state(&self) -> (f64, f64) {
        (self.aggressiveness, self.integral_error)
    }

    /// Update the controller given the current vs target active count.
    /// `dt` is elapsed time in seconds since the last call (use 1.0 for
    /// periodic per-cycle updates).
    pub fn update(
        &mut self,
        current_active_count: u64,
        target_active_count: u64,
        dt: f64,
    ) -> (f64, f64) {
        let error = (current_active_count as f64 - target_active_count as f64)
            / target_active_count.max(1) as f64;

        if error.abs() >= self.params.deadband {
            let saturated_high = self.aggressiveness >= 1.0;
            let saturated_low = self.aggressiveness <= 0.0;
            // Anti-windup: don't accumulate integral when already saturated in
            // the same direction as the error.
            if !(saturated_high && error > 0.0 || saturated_low && error < 0.0) {
                self.integral_error += error * dt;
                self.integral_error = self.integral_error.clamp(-10.0, 10.0);
            }
            let delta = self.params.kp * error + self.params.ki * self.integral_error;
            let change = delta.clamp(
                -self.params.max_change_per_cycle,
                self.params.max_change_per_cycle,
            );
            self.aggressiveness = (self.aggressiveness + change).clamp(0.0, 1.0);
        }

        let threshold_high = 1.0 - self.aggressiveness;
        let threshold_low = (threshold_high - self.params.delta_hysteresis).max(0.0);
        (threshold_high, threshold_low)
    }
}
