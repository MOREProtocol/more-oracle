//! Cumulative drift protection (off-chain).
//!
//! Tracks historical values per oracle within a configurable sliding window.
//! Before a new value is pushed, the keeper checks whether the cumulative
//! drift from the "anchor" (first value in the window) exceeds a configurable
//! threshold.  If it does, the push is suppressed and an alert is logged.
//!
//! NOTE: Drift history is held in-memory and resets on keeper restart.

use std::collections::HashMap;
use tracing::{error, info};

/// A single recorded observation for an oracle.
#[derive(Debug, Clone)]
struct Observation {
    /// Unix timestamp (seconds) when this observation was recorded.
    timestamp: u64,
    /// The `totalAssets` value observed.
    value: u128,
}

/// Per-oracle drift tracker.
#[derive(Debug, Default)]
pub struct DriftTracker {
    /// oracle_address (lower-case) -> list of observations, ordered by time.
    history: HashMap<String, Vec<Observation>>,
    /// Sliding window duration in seconds.
    window_secs: u64,
    /// Maximum cumulative drift in basis points (e.g. 1500 = 15%).
    max_drift_bps: u64,
}

impl DriftTracker {
    /// Create a new tracker with the given window and threshold.
    pub fn new(window_secs: u64, max_drift_bps: u64) -> Self {
        Self {
            history: HashMap::new(),
            window_secs,
            max_drift_bps,
        }
    }

    /// Check whether pushing `new_value` for `oracle` would violate the
    /// cumulative drift threshold.
    ///
    /// Returns `true` if the value is safe to push (drift within limits or no
    /// anchor exists yet), `false` if the push should be skipped.
    ///
    /// When the check passes the observation is recorded automatically.
    /// When it fails the observation is NOT recorded (the value was not pushed).
    pub fn check_and_record(&mut self, oracle: &str, new_value: u128, now_secs: u64) -> bool {
        let key = oracle.to_lowercase();

        // Prune observations outside the sliding window.
        let cutoff = now_secs.saturating_sub(self.window_secs);
        let observations = self.history.entry(key.clone()).or_default();
        observations.retain(|obs| obs.timestamp >= cutoff);

        // If there is no anchor (first observation in window), this value
        // becomes the anchor -- always safe to push.
        let anchor_value = match observations.first() {
            Some(obs) => obs.value,
            None => {
                observations.push(Observation {
                    timestamp: now_secs,
                    value: new_value,
                });
                info!(
                    oracle = %oracle,
                    anchor_value = new_value,
                    "drift tracker: new anchor established"
                );
                return true;
            }
        };

        // Calculate drift in basis points:  |new - anchor| / anchor * 10_000
        let drift_bps = Self::calculate_drift_bps(anchor_value, new_value);

        if drift_bps > self.max_drift_bps {
            error!(
                oracle = %oracle,
                anchor_value,
                new_value,
                drift_bps,
                max_drift_bps = self.max_drift_bps,
                window_secs = self.window_secs,
                observations_in_window = observations.len(),
                "DRIFT ALERT: cumulative drift exceeds threshold — skipping oracle update"
            );
            return false;
        }

        info!(
            oracle = %oracle,
            anchor_value,
            new_value,
            drift_bps,
            max_drift_bps = self.max_drift_bps,
            "drift check passed"
        );

        observations.push(Observation {
            timestamp: now_secs,
            value: new_value,
        });

        true
    }

    /// Compute the absolute drift in basis points between two values.
    /// Returns 0 if the anchor is zero (avoids division by zero).
    fn calculate_drift_bps(anchor: u128, current: u128) -> u64 {
        if anchor == 0 {
            return 0;
        }

        let diff = if current > anchor {
            current - anchor
        } else {
            anchor - current
        };

        // Use u128 arithmetic to avoid overflow: diff * 10_000 / anchor
        let bps = (diff as u128)
            .checked_mul(10_000)
            .map(|n| n / anchor)
            .unwrap_or(u64::MAX as u128);

        // Clamp to u64
        if bps > u64::MAX as u128 {
            u64::MAX
        } else {
            bps as u64
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_first_observation_always_passes() {
        let mut tracker = DriftTracker::new(86400, 1500);
        assert!(tracker.check_and_record("0xabc", 1_000_000, 1000));
    }

    #[test]
    fn test_within_threshold_passes() {
        let mut tracker = DriftTracker::new(86400, 1500);
        // anchor = 1_000_000
        tracker.check_and_record("0xabc", 1_000_000, 1000);
        // 10% drift = 1000 bps < 1500 bps threshold
        assert!(tracker.check_and_record("0xabc", 1_100_000, 2000));
    }

    #[test]
    fn test_exceeding_threshold_fails() {
        let mut tracker = DriftTracker::new(86400, 1500);
        tracker.check_and_record("0xabc", 1_000_000, 1000);
        // 20% drift = 2000 bps > 1500 bps threshold
        assert!(!tracker.check_and_record("0xabc", 1_200_000, 2000));
    }

    #[test]
    fn test_negative_drift_detected() {
        let mut tracker = DriftTracker::new(86400, 1500);
        tracker.check_and_record("0xabc", 1_000_000, 1000);
        // -20% drift = 2000 bps > 1500 bps threshold
        assert!(!tracker.check_and_record("0xabc", 800_000, 2000));
    }

    #[test]
    fn test_window_expiry_resets_anchor() {
        let mut tracker = DriftTracker::new(3600, 1500); // 1 hour window
        tracker.check_and_record("0xabc", 1_000_000, 1000);
        // After window expires, the old anchor is pruned.
        // The new value becomes the anchor regardless of drift from old anchor.
        assert!(tracker.check_and_record("0xabc", 2_000_000, 1000 + 3601));
    }

    #[test]
    fn test_different_oracles_independent() {
        let mut tracker = DriftTracker::new(86400, 1500);
        tracker.check_and_record("0xaaa", 1_000_000, 1000);
        // Different oracle, independent anchor
        assert!(tracker.check_and_record("0xbbb", 5_000_000, 1000));
    }

    #[test]
    fn test_drift_bps_calculation() {
        assert_eq!(DriftTracker::calculate_drift_bps(1_000_000, 1_150_000), 1500);
        assert_eq!(DriftTracker::calculate_drift_bps(1_000_000, 850_000), 1500);
        assert_eq!(DriftTracker::calculate_drift_bps(1_000_000, 1_000_000), 0);
        assert_eq!(DriftTracker::calculate_drift_bps(0, 1_000_000), 0);
    }
}
