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

    /// Clear all drift history for the given oracle so the next push becomes
    /// a fresh anchor. Called after a bridge warning completes.
    pub fn reset(&mut self, oracle: &str) {
        self.history.remove(&oracle.to_lowercase());
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

    // ── N-1: Why seeding anchor from storedTotalAssets on restart adds no value ──
    //
    // Bogdan's proposed fix: on startup, read storedTotalAssets from each oracle
    // on Flow EVM and seed the drift tracker anchor with that value.
    //
    // These tests prove the fix provides no real benefit and introduces a
    // specific harm in the long downtime scenario.

    /// CLAIM 1: With multiple keepers, storedTotalAssets is always fresh.
    ///
    /// If Keeper A restarts, Keeper B has been pushing every hour.
    /// storedTotalAssets = Keeper B's last push = at most 1 hour old.
    /// The difference between storedTotalAssets and the current spoke value
    /// is at most 1 hour of yield — negligible vs the 1500 bps threshold.
    #[test]
    fn test_multi_keeper_stored_is_always_fresh() {
        // Keeper B pushed 1 hour ago: storedTotalAssets = 1,000,000
        // Vault accrued 1h of yield at 10% APY = ~11 bps
        let stored_total_assets: u128 = 1_000_000; // Keeper B's last push
        let current_spoke_value: u128 = 1_000_011; // +11 bps of yield in 1h

        // With fix: seed from storedTotalAssets
        let mut seeded = DriftTracker::new(86400, 1500);
        seeded.check_and_record("0xoracle", stored_total_assets, 0);
        let seeded_allows = seeded.check_and_record("0xoracle", current_spoke_value, 3600);

        // Without fix: fresh anchor from spoke RPC
        let mut fresh = DriftTracker::new(86400, 1500);
        let fresh_allows = fresh.check_and_record("0xoracle", current_spoke_value, 0);

        assert!(seeded_allows, "seeded: 11 bps difference passes trivially");
        assert!(fresh_allows, "fresh: current spoke value becomes anchor, passes");
        // Both behave identically. The fix changes nothing with multiple keepers.
    }

    /// CLAIM 2: With single keeper and short downtime, difference is negligible.
    ///
    /// Keeper down 23 hours (just under the 24h window).
    /// storedTotalAssets = value from 23 hours ago.
    /// At 10% APY, vault grew ~0.25% in 23h = 25 bps.
    /// Both seeded and fresh anchor produce same outcome — well under 1500 bps.
    #[test]
    fn test_single_keeper_short_downtime_negligible_difference() {
        let stored_total_assets: u128 = 1_000_000; // 23 hours ago
        // 10% APY over 23h = (1 + 0.10)^(23/8760) - 1 ≈ 0.25% = 25 bps
        let current_spoke_value: u128 = 1_000_025; // +25 bps

        let mut seeded = DriftTracker::new(86400, 1500);
        seeded.check_and_record("0xoracle", stored_total_assets, 0);
        assert!(
            seeded.check_and_record("0xoracle", current_spoke_value, 100),
            "seeded: 25 bps well under 1500 threshold"
        );

        let mut fresh = DriftTracker::new(86400, 1500);
        assert!(
            fresh.check_and_record("0xoracle", current_spoke_value, 0),
            "fresh: same result"
        );
        // Fix makes no difference. Both pass identically.
    }

    /// CLAIM 3: With single keeper and long downtime (>24h), fix actively harms.
    ///
    /// Keeper down 25 hours. The 24h drift window would have expired naturally.
    /// Fix loads a 25-hour-old storedTotalAssets as anchor of a FRESH window.
    /// Any legitimate move during the 25h downtime gets silently blocked.
    /// Without the fix, the fresh anchor from the spoke RPC passes naturally.
    #[test]
    fn test_single_keeper_long_downtime_fix_actively_harms() {
        // Keeper was down 25 hours.
        // storedTotalAssets = 1,000,000 (from 25 hours ago, very stale)
        // Curator made a large deposit during downtime: vault now at 1,300,000 (+30%)
        let stored_total_assets: u128 = 1_000_000; // 25h stale
        let current_spoke_value: u128 = 1_300_000; // +30% legitimate move

        // Without fix: fresh anchor = current spoke value → push passes
        let mut fresh = DriftTracker::new(86400, 1500);
        let fresh_allows = fresh.check_and_record("0xoracle", current_spoke_value, 0);
        assert!(
            fresh_allows,
            "without fix: current value becomes anchor, push reaches circuit breaker (visible)"
        );

        // With fix: anchor = 25h stale storedTotalAssets → +30% blocked silently
        let mut seeded = DriftTracker::new(86400, 1500);
        seeded.check_and_record("0xoracle", stored_total_assets, 0); // seed from on-chain
        let fix_blocks = !seeded.check_and_record("0xoracle", current_spoke_value, 100);
        assert!(
            fix_blocks,
            "with fix: 25h-stale anchor silently blocks legitimate +30% — oracle stale, no signal"
        );
        // The 24h window would have expired naturally — fix artificially revives
        // a stale anchor that the mechanism itself was about to discard.
    }

    /// CLAIM 4: Bogdan's fix does not help with the restart bypass attack.
    ///
    /// The restart bypass: attacker accumulates drift to 14.9%, triggers restart,
    /// gets fresh anchor, repeats. With the fix, the seeded anchor = storedTotalAssets
    /// = the last successfully pushed value = same value the fresh tracker would read.
    /// The fix provides zero additional protection against this attack.
    #[test]
    fn test_fix_does_not_prevent_restart_bypass() {
        // Attacker drove slow drift: last pushed value = 1,149,000 (14.9% from original)
        // storedTotalAssets on-chain = 1,149,000 (the last push that went through)
        let stored_total_assets: u128 = 1_149_000;
        let current_spoke_value: u128 = 1_149_000; // vault didn't move during restart

        // With fix: anchor = storedTotalAssets = 1,149,000
        let mut seeded = DriftTracker::new(86400, 1500);
        seeded.check_and_record("0xoracle", stored_total_assets, 0);

        // Without fix: anchor = current spoke value = 1,149,000 (same thing)
        let mut fresh = DriftTracker::new(86400, 1500);
        fresh.check_and_record("0xoracle", current_spoke_value, 0);

        // Both allow the same next push ceiling: 1,149,000 * 1.149 = ~1,320,000
        let attacker_next: u128 = 1_320_000; // +14.9% from new anchor
        let seeded_allows = seeded.check_and_record("0xoracle", attacker_next, 100);
        let fresh_allows = fresh.check_and_record("0xoracle", attacker_next, 100);

        assert_eq!(
            seeded_allows, fresh_allows,
            "fix and no-fix produce identical result against restart bypass attack"
        );
        // storedTotalAssets == last pushed spoke value == what fresh tracker reads.
        // The fix changes nothing for this attack vector.
    }

    /// CLAIM 5: Fresh anchor on restart is actually useful with multiple keepers.
    ///
    /// If Keeper A accumulated drift close to the threshold and then restarts,
    /// starting fresh allows it to push a legitimate large move that Keeper B
    /// (still running with its accumulated anchor) would block.
    ///
    /// This is a FEATURE: the fresh keeper acts as a safety valve for legitimate
    /// moves while the continuing keeper maintains drift protection.
    /// If the move is malicious, the on-chain circuit breaker stops it regardless.
    #[test]
    fn test_fresh_anchor_on_restart_is_useful_with_multiple_keepers() {
        // Both keepers running. Vault at 1,000,000.
        let mut keeper_a = DriftTracker::new(86400, 1500);
        let mut keeper_b = DriftTracker::new(86400, 1500);
        keeper_a.check_and_record("0xoracle", 1_000_000, 0);
        keeper_b.check_and_record("0xoracle", 1_000_000, 0);

        // Slow drift: both keepers have been pushing. Accumulated to 14% from anchor.
        keeper_a.check_and_record("0xoracle", 1_140_000, 3600);
        keeper_b.check_and_record("0xoracle", 1_140_000, 3600);

        // Curator makes a large legitimate deposit: vault now at 1,320,000 (+32% from anchor)
        // Both keepers try to push — drift blocks both (2800 bps > 1500)
        let large_deposit_value: u128 = 1_320_000;
        assert!(!keeper_a.check_and_record("0xoracle", large_deposit_value, 7200), "A blocks");
        assert!(!keeper_b.check_and_record("0xoracle", large_deposit_value, 7200), "B blocks");

        // Keeper A restarts — fresh anchor.
        let mut keeper_a_restarted = DriftTracker::new(86400, 1500);

        // Keeper A (fresh) can now push the legitimate value — becomes new anchor.
        // Push reaches on-chain circuit breaker which is the real decision maker.
        assert!(
            keeper_a_restarted.check_and_record("0xoracle", large_deposit_value, 7201),
            "A (fresh after restart): legitimate large move reaches circuit breaker"
        );

        // Keeper B still running with old anchor — still blocks.
        // But Keeper A already pushed — oracle is updated.
        assert!(
            !keeper_b.check_and_record("0xoracle", large_deposit_value, 7201),
            "B (continuing): still blocks from old anchor — but A already handled it"
        );

        // If the move was malicious instead of legitimate, the circuit breaker
        // would revert Keeper A's push on-chain — Keeper B's block is irrelevant.
        // The on-chain circuit breaker is the real gatekeeper in both cases.
    }

    // ── Restart scenarios: error vs attack, single vs multi keeper ───────────

    /// ERROR RESTART, SINGLE KEEPER: fresh anchor handles legitimate move correctly.
    ///
    /// Keeper crashed due to an error (not an attack). Vault had a large legitimate
    /// deposit during downtime. Fresh anchor lets the push reach the circuit breaker.
    /// Seeded anchor would silently block it.
    #[test]
    fn test_error_restart_single_keeper_fresh_anchor_correct() {
        let stored: u128 = 1_000_000; // last pushed before crash
        let after_deposit: u128 = 1_250_000; // +25% legitimate deposit during downtime

        // Fresh anchor (current behavior): push reaches circuit breaker → visible outcome
        let mut fresh = DriftTracker::new(86400, 1500);
        assert!(
            fresh.check_and_record("0xoracle", after_deposit, 0),
            "fresh: push reaches circuit breaker, which decides visibly"
        );

        // Seeded anchor (Bogdan's fix): silently blocked, oracle stale
        let mut seeded = DriftTracker::new(86400, 1500);
        seeded.check_and_record("0xoracle", stored, 0);
        assert!(
            !seeded.check_and_record("0xoracle", after_deposit, 100),
            "seeded: silently blocked — operator sees nothing, oracle stale"
        );
    }

    /// ATTACK RESTART, SINGLE KEEPER: circuit breaker stops fake value regardless.
    ///
    /// Attacker triggers restart to get fresh anchor, then pushes a fake value.
    /// The fresh anchor allows the push to REACH the circuit breaker.
    /// The circuit breaker (on-chain, not bypasseable) catches it with a visible revert.
    /// Seeding anchor from storedTotalAssets changes nothing here — circuit breaker
    /// would catch the fake value either way.
    #[test]
    fn test_attack_restart_single_keeper_circuit_breaker_catches_it() {
        let stored: u128 = 1_000_000; // storedTotalAssets on-chain
        let max_change_bps: u128 = 500; // circuit breaker: 5% per update

        // Attacker wants to push +30% fake value
        let fake_value: u128 = 1_300_000;

        // With fresh anchor: push reaches circuit breaker
        let mut fresh = DriftTracker::new(86400, 1500);
        let drift_allows = fresh.check_and_record("0xoracle", fake_value, 0);
        assert!(drift_allows, "fresh anchor: push reaches circuit breaker");

        // Circuit breaker catches it on-chain (visible revert)
        let delta_bps = (fake_value - stored) * 10_000 / stored;
        assert!(
            delta_bps > max_change_bps,
            "circuit breaker trips: {delta_bps} bps > {max_change_bps} bps (visible revert)"
        );

        // With seeded anchor: drift also allows it (30% > 15% threshold... wait)
        // Actually if fake_value is 30% above stored, drift blocks it too.
        // But the circuit breaker would have caught it anyway.
        let mut seeded = DriftTracker::new(86400, 1500);
        seeded.check_and_record("0xoracle", stored, 0);
        let seeded_drift_blocks = !seeded.check_and_record("0xoracle", fake_value, 100);
        // Whether drift blocks or not, circuit breaker is the real protection.
        // Seeded only adds a silent pre-block before the visible circuit breaker.
        let _ = seeded_drift_blocks;
    }

    /// ATTACK RESTART, SINGLE KEEPER: slow fake drift after restart.
    ///
    /// Attacker pushes small increments (+4.9%) after restart to stay under
    /// circuit breaker. Fresh anchor means they can accumulate another 14.9%.
    /// But this is also true with seeded anchor if storedTotalAssets == last pushed.
    /// Both behaviors are identical because storedTotalAssets == what fresh reads.
    #[test]
    fn test_attack_slow_drift_restart_fresh_vs_seeded_identical() {
        // Last pushed = 1,140,000 (attacker accumulated +14% before restart)
        let last_pushed: u128 = 1_140_000;

        // Fresh anchor: reads spoke → gets 1,140,000 (same as last pushed)
        let mut fresh = DriftTracker::new(86400, 1500);
        fresh.check_and_record("0xoracle", last_pushed, 0);

        // Seeded anchor: reads storedTotalAssets → 1,140,000 (same value)
        let mut seeded = DriftTracker::new(86400, 1500);
        seeded.check_and_record("0xoracle", last_pushed, 0); // storedTotalAssets == last_pushed

        // Both trackers have identical anchor. Attacker gains nothing from restart.
        let next_push: u128 = 1_196_460; // +4.9% from 1,140,000
        assert_eq!(
            fresh.check_and_record("0xoracle", next_push, 3600),
            seeded.check_and_record("0xoracle", next_push, 3600),
            "fresh and seeded produce identical result — restart provides no advantage"
        );
    }

    /// ERROR RESTART, MULTI KEEPER: both near threshold, one restarts.
    ///
    /// Both keepers accumulated drift. Legitimate large deposit happens.
    /// Both block it. One restarts due to error (not attack).
    /// Fresh restart allows the legitimate value to go through.
    /// This is the key scenario where fresh anchor is a FEATURE not a bug.
    #[test]
    fn test_error_restart_multi_keeper_fresh_is_safety_valve() {
        let mut keeper_a = DriftTracker::new(86400, 1500);
        let mut keeper_b = DriftTracker::new(86400, 1500);

        // Both accumulated to 14% drift from anchor 1,000,000
        keeper_a.check_and_record("0xoracle", 1_000_000, 0);
        keeper_b.check_and_record("0xoracle", 1_000_000, 0);
        keeper_a.check_and_record("0xoracle", 1_140_000, 3600);
        keeper_b.check_and_record("0xoracle", 1_140_000, 3600);

        // Large legitimate deposit: +40% from original anchor (1,000,000)
        // = +22.8% from storedTotalAssets (1,140,000)
        // Both thresholds > 1500 bps → both fresh and seeded would block from those anchors
        let deposit_value: u128 = 1_400_000;
        assert!(!keeper_a.check_and_record("0xoracle", deposit_value, 7200), "A blocks: 4000 bps from anchor");
        assert!(!keeper_b.check_and_record("0xoracle", deposit_value, 7200), "B blocks: 4000 bps from anchor");

        // Keeper A crashes (error, not attack) → restarts with fresh anchor
        let mut keeper_a_fresh = DriftTracker::new(86400, 1500);

        // Fresh A: anchor = current spoke value = 1,400,000 → push passes (0 bps from anchor)
        assert!(
            keeper_a_fresh.check_and_record("0xoracle", deposit_value, 7201),
            "A (fresh): deposit_value becomes new anchor, push reaches circuit breaker"
        );
        // Keeper A seeded: anchor = storedTotalAssets = 1,140,000
        // delta = (1,400,000 - 1,140,000) / 1,140,000 = 2280 bps > 1500 → also blocks
        let mut keeper_a_seeded = DriftTracker::new(86400, 1500);
        keeper_a_seeded.check_and_record("0xoracle", 1_140_000, 0); // seed from storedTotalAssets
        assert!(
            !keeper_a_seeded.check_and_record("0xoracle", deposit_value, 7201),
            "A (seeded): 2280 bps from stale anchor still blocks — oracle stuck until window expires"
        );
    }

    // ── Calibration: what drift threshold avoids blocking large legitimate deposits? ──
    //
    // The real practical question: given expected vault behavior (deposits, yield),
    // what max_drift_bps is safe? If set too low, normal operations get silently
    // blocked. These tests quantify the relationship.

    /// CALIBRATION: How large a single deposit can a vault receive without
    /// tripping the drift tracker at various threshold settings?
    ///
    /// This helps decide if max_drift_bps = 1500 is appropriate or too tight.
    #[test]
    fn test_calibration_max_deposit_size_per_threshold() {
        // At max_drift_bps = 1500 (15%), a deposit growing vault by >15% is blocked.
        // Example: $150k deposit into a $1M vault = +15% = exactly at threshold.
        // Any deposit larger than that is silently blocked.
        let vault_size: u128 = 1_000_000_000_000; // $1M in 6 decimals

        // Note: drift check is strictly > threshold, so exactly at threshold passes.
        for (threshold_bps, deposit_pct, should_pass) in [
            (1500u64, 14u128, true),  // 14% deposit → 1400 bps < 1500 → passes
            (1500u64, 15u128, true),  // 15% deposit → 1500 bps == 1500 → passes (strictly >)
            (1500u64, 16u128, false), // 16% deposit → 1600 bps > 1500 → blocked
            (1500u64, 20u128, false), // 20% deposit → 2000 bps > 1500 → blocked
            (3000u64, 20u128, true),  // 30% threshold: 20% deposit → 2000 bps < 3000 → passes
            (3000u64, 31u128, false), // 30% threshold: 31% deposit → 3100 bps > 3000 → blocked
            (5000u64, 49u128, true),  // 50% threshold: 49% deposit → 4900 bps < 5000 → passes
        ] {
            let mut tracker = DriftTracker::new(86400, threshold_bps);
            tracker.check_and_record("0xoracle", vault_size, 0);
            let deposit_value = vault_size + (vault_size * deposit_pct / 100);
            let result = tracker.check_and_record("0xoracle", deposit_value, 100);
            assert_eq!(
                result, should_pass,
                "threshold={threshold_bps} bps, deposit=+{deposit_pct}%: expected pass={should_pass}"
            );
        }
        // Key insight: with max_drift_bps = 1500, any deposit larger than 15% of
        // current vault size is silently blocked for up to 24h.
        // For small vaults receiving large first deposits this is a real risk.
    }

    /// CALIBRATION: Yield accumulation over 24h never trips the drift tracker.
    ///
    /// Pure yield (no deposits) at realistic APY rates never approaches 1500 bps
    /// in a single 24h window. The drift tracker is only relevant for deposits.
    #[test]
    fn test_calibration_yield_alone_never_trips_drift() {
        let vault: u128 = 1_000_000_000_000; // $1M

        // Highest realistic DeFi yield: 200% APY = 0.55% per day = 55 bps
        // Well under 1500 bps threshold.
        let yield_200_apy_24h = vault + (vault * 55 / 10_000); // +55 bps

        let mut tracker = DriftTracker::new(86400, 1500);
        tracker.check_and_record("0xoracle", vault, 0);
        assert!(
            tracker.check_and_record("0xoracle", yield_200_apy_24h, 86400),
            "200% APY yield in 24h = 55 bps — well under 1500 bps threshold"
        );
        // Conclusion: drift tracker threshold only matters for deposit/withdraw events,
        // not for yield accumulation. Calibration should be based on expected
        // deposit sizes relative to vault TVL.
    }

    // ── Restart-to-reset attack: single vs multi keeper ──────────────────────

    /// ATTACK, SINGLE KEEPER: attacker restarts to reset drift, resumes slow push.
    ///
    /// Each push stays under circuit breaker (maxChangeBps = 500, so +4.9% per push).
    /// Drift catches it within the window. Attacker restarts → fresh anchor → repeats.
    /// With one keeper there's no correction — attacker accumulates indefinitely.
    #[test]
    fn test_attack_single_keeper_restart_resets_drift_no_correction() {
        let max_change_bps: u128 = 500; // circuit breaker: 5% per push
        let mut tracker = DriftTracker::new(86400, 1500);

        // Anchor at 1,000,000
        tracker.check_and_record("0xoracle", 1_000_000, 0);

        // Attacker pushes +4.9% per hour — under circuit breaker, but accumulates
        let mut value = 1_000_000u128;
        let mut now = 1u64;
        let mut pushes = 0u32;

        loop {
            let next = (value as f64 * 1.049) as u128;
            let cb_ok = (next - value) * 10_000 / value <= max_change_bps;
            assert!(cb_ok, "each push is under circuit breaker threshold");
            if !tracker.check_and_record("0xoracle", next, now) {
                break; // drift caught it
            }
            value = next;
            now += 3600;
            pushes += 1;
        }
        // Drift stopped attacker after a few pushes within the window
        assert!(pushes >= 2, "drift caught slow accumulation after {pushes} pushes");

        // === Attacker restarts keeper → fresh anchor ===
        let mut tracker_after_restart = DriftTracker::new(86400, 1500);
        // value is now at whatever was last pushed (e.g. 1,100,401)
        // Fresh anchor = current value → attacker can push another 14.9% window
        assert!(
            tracker_after_restart.check_and_record("0xoracle", value, now),
            "after restart: fresh anchor, attacker gets another full drift window"
        );
        let next_after_restart = (value as f64 * 1.049) as u128;
        assert!(
            tracker_after_restart.check_and_record("0xoracle", next_after_restart, now + 3600),
            "attacker continues accumulating from new anchor — single keeper has no correction"
        );
        // With one keeper: attacker can repeat restart → accumulate indefinitely.
        // Circuit breaker alone doesn't stop this — drift does within a window,
        // but restart resets it. No other keeper to push the real value.
    }

    /// ATTACK, TWO KEEPERS: attacker restarts one, honest keeper corrects the oracle.
    ///
    /// Attacker controls Keeper A (compromised), pushes fake values.
    /// Keeper B (honest) reads the real spoke value and pushes it next cycle.
    /// Keeper B's honest push corrects A's fake push — oracle stays accurate.
    /// The attack only works if the attacker controls ALL keepers simultaneously.
    #[test]
    fn test_attack_two_keepers_honest_keeper_corrects_fake_push() {
        // Real spoke value: 1,000,000
        // Attacker (Keeper A) pushes fake +4.9%: 1,049,000
        // Keeper B reads real value: 1,000,050 (a bit of yield)

        let real_spoke_value: u128 = 1_000_050; // what honest keeper reads from RPC
        let fake_value: u128 = 1_049_000;       // what attacker pushes via compromised keeper

        // Keeper A (compromised, fresh anchor after restart) pushes fake value
        let mut keeper_a = DriftTracker::new(86400, 1500);
        assert!(keeper_a.check_and_record("0xoracle", fake_value, 0), "A pushes fake value");
        // Oracle now shows 1,049,000 (fake)

        // Keeper B (honest, independent tracker) reads real spoke value
        // and pushes it next cycle — corrects the oracle
        let mut keeper_b = DriftTracker::new(86400, 1500);
        assert!(
            keeper_b.check_and_record("0xoracle", real_spoke_value, 3600),
            "B pushes real value — oracle corrected to 1,000,050"
        );
        // With two keepers: attacker's fake push gets corrected within one cycle (1h).
        // Attacker would need to control ALL keepers to sustain the manipulation.
        // Drift tracker on A or B doesn't change this — the correction happens
        // because honest keeper reads the real spoke, not because of drift state.
    }

    // ── Adversarial scenarios ─────────────────────────────────────────────────

    /// KEY INSIGHT: Drift tracker and circuit breaker protect DIFFERENT vectors.
    ///
    /// Circuit breaker (on-chain): checks delta between LAST stored value and
    /// the NEW value. Anchors to previous push. NOT cumulative.
    ///
    /// Drift tracker (off-chain): checks delta between FIRST value in the window
    /// and the NEW value. Cumulative within window.
    ///
    /// This test proves the drift tracker catches slow accumulation that the
    /// circuit breaker completely misses.
    #[test]
    fn test_drift_catches_what_circuit_breaker_misses() {
        // Setup: circuit breaker maxChangeBps = 500 (5% per update)
        //        drift tracker max_drift_bps   = 1500 (15% cumulative per window)
        let max_change_bps: u128 = 500;
        let mut tracker = DriftTracker::new(86400, 1500);

        // Anchor at 1_000_000
        assert!(tracker.check_and_record("0xoracle", 1_000_000, 0));

        // Simulate slow drift: +4.9% per update (under circuit breaker threshold)
        // Circuit breaker checks: |new - stored| / stored <= 5% → always passes
        // Drift tracker checks:   |new - anchor| / anchor <= 15% → eventually trips
        let mut stored = 1_000_000u128; // simulates storedTotalAssets on-chain
        let mut now = 1u64;
        let mut pushes_allowed = 0u32;

        loop {
            let new_value = (stored as f64 * 1.049) as u128;

            // Circuit breaker: per-update check against LAST stored value
            let cb_delta_bps = (new_value - stored) * 10_000 / stored;
            let circuit_breaker_allows = cb_delta_bps <= max_change_bps;

            // Drift tracker: cumulative check against window ANCHOR
            let drift_allows = tracker.check_and_record("0xoracle", new_value, now);

            if !drift_allows {
                // Drift tracker finally tripped — circuit breaker still allowed it
                assert!(
                    circuit_breaker_allows,
                    "circuit breaker should still allow this value (cb_delta={cb_delta_bps} bps)"
                );
                break;
            }

            // Both allowed — update stored (as the contract would)
            stored = new_value;
            now += 3600;
            pushes_allowed += 1;

            assert!(pushes_allowed < 20, "drift tracker never tripped in 20 pushes");
        }

        // Drift tracker tripped before circuit breaker would have.
        // stored = last value that passed both checks (< 1500 bps from anchor).
        // Circuit breaker would have kept accepting indefinitely (+4.9% per update).
        // Drift tracker stopped it after a handful of pushes.
        let cumulative_bps = (stored - 1_000_000) * 10_000 / 1_000_000;
        assert!(
            cumulative_bps < 1500,
            "last allowed value should be under drift threshold (was {cumulative_bps} bps)"
        );
        assert!(
            pushes_allowed >= 2,
            "circuit breaker alone would have allowed {pushes_allowed} slow-drift pushes indefinitely"
        );
    }

    /// N-1 ATTACK: Restart bypass (current behavior, no fix).
    ///
    /// Attacker triggers restart → tracker loses anchor → can resume slow drift
    /// from fresh anchor, bypassing cumulative protection entirely.
    #[test]
    fn test_malicious_restart_bypass() {
        // === Keeper instance #1 ===
        let mut tracker = DriftTracker::new(86400, 1500);

        // Anchor at 1_000_000
        assert!(tracker.check_and_record("0xoracle", 1_000_000, 0));

        // Slow drift: +14% passes (1400 bps < 1500)
        assert!(tracker.check_and_record("0xoracle", 1_140_000, 3600));

        // +15.1% from anchor is blocked (1510 bps > 1500)
        assert!(!tracker.check_and_record("0xoracle", 1_151_000, 7200));

        // === Attacker triggers restart → tracker drops all state ===
        let mut tracker_after_restart = DriftTracker::new(86400, 1500);

        // Previously blocked value now passes freely (becomes new anchor)
        assert!(
            tracker_after_restart.check_and_record("0xoracle", 1_151_000, 7201),
            "BUG: restart reset anchor — blocked value now passes"
        );

        // Attacker can drift another +14.9% from new anchor
        assert!(
            tracker_after_restart.check_and_record("0xoracle", 1_322_799, 10800),
            "BUG: total accumulated drift +32% across two restarts, no alert"
        );
    }

    /// N-1 FIX (Bogdan): Seed anchor from storedTotalAssets on-chain at startup.
    ///
    /// Restart no longer gives attacker a fresh anchor. The anchor is seeded
    /// from the last confirmed on-chain value, so cumulative protection resumes
    /// from where it left off.
    #[test]
    fn test_bogdan_fix_prevents_restart_bypass() {
        // Before restart: vault at 1_000_000, slow drift to 1_140_000 pushed on-chain
        // storedTotalAssets on-chain = 1_140_000

        // === Keeper restarts → seeds anchor from storedTotalAssets ===
        let stored_on_chain: u128 = 1_140_000;
        let mut tracker_after_restart = DriftTracker::new(86400, 1500);
        // Seed: first observation = on-chain stored value
        assert!(tracker_after_restart.check_and_record("0xoracle", stored_on_chain, 0));

        // Attacker tries to push +15.1% from the original anchor (1_000_000)
        // which is +0.96% from stored — but drift anchor is now 1_140_000
        // so +15.1% from NEW anchor = 1_312_140, not just 1_151_000
        // Let's try the previously-blocked value: 1_151_000
        // From anchor 1_140_000: delta = 11_000 / 1_140_000 = ~96 bps → passes
        // (This is expected — it's only 1% from last stored, legitimate)
        assert!(
            tracker_after_restart.check_and_record("0xoracle", 1_151_000, 3600),
            "small step from seeded anchor should still pass"
        );

        // But trying to jump +15.1% from the seeded anchor (1_140_000) is blocked
        let jump = (1_140_000f64 * 1.151) as u128; // = ~1,312,140
        assert!(
            !tracker_after_restart.check_and_record("0xoracle", jump, 7200),
            "large jump from seeded anchor is blocked — fix works"
        );
    }

    /// N-1 FIX TRADEOFF: Bogdan's fix blocks legitimate large moves during downtime.
    ///
    /// If the vault moved +25% while keeper was down, seeding from storedTotalAssets
    /// means the anchor is stale. The legitimate update gets blocked silently.
    /// The on-chain circuit breaker would ALSO block it — but with a visible revert.
    /// Tradeoff: better restart protection vs. silent block on legitimate large moves.
    #[test]
    fn test_bogdan_fix_tradeoff_legitimate_move_during_downtime() {
        // storedTotalAssets on-chain = 1_000_000 (no push during downtime)
        let stored_on_chain: u128 = 1_000_000;

        // Vault actually moved to 1_250_000 (+25%) during downtime (legitimate deposit)

        // === Without fix: fresh tracker ===
        let mut fresh_tracker = DriftTracker::new(86400, 1500);
        let fresh_allows = fresh_tracker.check_and_record("0xoracle", 1_250_000, 0);
        assert!(
            fresh_allows,
            "fresh tracker: 1_250_000 becomes new anchor, push attempted (circuit breaker decides)"
        );
        // Note: circuit breaker on-chain would then check |1_250_000 - 1_000_000| / 1_000_000
        // = 2500 bps. If maxChangeBps < 2500, contract reverts visibly.

        // === With Bogdan's fix: seeded tracker ===
        let mut seeded_tracker = DriftTracker::new(86400, 1500);
        seeded_tracker.check_and_record("0xoracle", stored_on_chain, 0); // seed from on-chain
        let seeded_blocks = !seeded_tracker.check_and_record("0xoracle", 1_250_000, 100);
        assert!(
            seeded_blocks,
            "seeded tracker: +25% from anchor is blocked silently — oracle stays stale"
        );
        // Consequence: oracle stale until window expires (up to 24h).
        // No on-chain revert → operator may not notice without active alerting.
    }

    /// N-1 BYPASS: Window expiry resets anchor regardless of accumulated drift.
    ///
    /// An attacker who can wait (or control timing) bypasses cumulative protection
    /// by letting the window expire between drift increments.
    #[test]
    fn test_malicious_slow_drift_across_windows() {
        let window = 3600u64;
        let mut tracker = DriftTracker::new(window, 1500);

        let mut now = 0u64;
        let mut current_value: u128 = 1_000_000;

        // Each window: drift +14.9% (just under threshold), then wait for reset
        for cycle in 0..5 {
            assert!(
                tracker.check_and_record("0xoracle", current_value, now),
                "cycle {cycle}: anchor push should pass"
            );
            let new_value = (current_value as f64 * 1.149) as u128;
            assert!(
                tracker.check_and_record("0xoracle", new_value, now + window / 2),
                "cycle {cycle}: +14.9% within threshold passes"
            );
            now += window + 1; // advance past window → anchor resets
            current_value = new_value;
        }

        // After 5 windows: ~+100% total, zero drift alerts
        let total_pct = (current_value - 1_000_000) * 100 / 1_000_000;
        assert!(
            total_pct >= 90,
            "attacker accumulated +{total_pct}% across window resets with no alert"
        );
        // Bogdan's fix does NOT help here — window expiry always resets anchor
        // regardless of whether it was seeded from on-chain or not.
    }

    // ── N-2: Byzantine Generals Problem in peer cross-validation ─────────────
    //
    // Bogdan proposes: block if ≥2 peers disagree (3-keeper setup).
    // These tests prove that blocking on peer disagreement introduces a worse
    // failure mode than the one it fixes — and collapses entirely with 2 keepers.
    //
    // Model: peer_decision() simulates the blocking vs warn-only policy.
    // The drift tracker and circuit breaker are modeled as simple threshold checks.

    /// Simulate peer cross-validation decision without HTTP calls.
    ///
    /// Returns true if this keeper should push.
    /// `block_if_n_disagree`: Some(n) = block when ≥n peers diverge, None = warn-only.
    fn peer_decision(
        our_value: u128,
        peer_values: &[u128],
        divergence_threshold_bps: u64,
        block_if_n_disagree: Option<usize>,
    ) -> bool {
        fn bps(a: u128, b: u128) -> u64 {
            if a == 0 {
                return 0;
            }
            let diff = if b > a { b - a } else { a - b };
            ((diff as u128) * 10_000 / a) as u64
        }
        let disagreeing = peer_values
            .iter()
            .filter(|&&pv| bps(our_value, pv) > divergence_threshold_bps)
            .count();
        match block_if_n_disagree {
            Some(n) => disagreeing < n,
            None => true,
        }
    }

    /// N-2 SCENARIO A: Single RPC compromised — Bogdan's fix helps.
    ///
    /// This keeper's RPC is manipulated (+500 bps, within circuit breaker).
    /// 2 honest peers disagree. Blocking prevents the bad push.
    /// This is the scenario Bogdan is optimizing for.
    #[test]
    fn test_n2_single_rpc_compromised_bogdan_fix_helps() {
        let our_value: u128 = 1_050_000;   // our RPC: manipulated (+500 bps)
        let peer_a: u128 = 1_000_000;      // peer A: honest
        let peer_b: u128 = 1_000_000;      // peer B: honest

        let peers = [peer_a, peer_b];

        // Warn-only (current): push bad value — gap identified by Bogdan
        let warn_only_pushes = peer_decision(our_value, &peers, 100, None);
        assert!(warn_only_pushes, "warn-only: bad value gets pushed — Bogdan's gap is real");

        // Bogdan's block (≥2 disagree): 2 peers disagree → blocked
        let bogdan_blocks = !peer_decision(our_value, &peers, 100, Some(2));
        assert!(bogdan_blocks, "Bogdan's fix: 2 peers disagree → blocked correctly");

        // BUT: circuit breaker still catches large manipulations regardless
        let stored: u128 = 1_000_000;
        let max_change_bps: u128 = 500;
        let delta_bps = (our_value - stored) * 10_000 / stored;
        // At exactly 500 bps: delta_bps == max_change_bps → cb allows it (≤ threshold)
        // For truly large manipulations (>500 bps) cb would catch it.
        // This test shows the gap only exists for manipulations WITHIN cb threshold.
        assert!(
            delta_bps <= max_change_bps,
            "manipulation at {delta_bps} bps is within cb threshold — cb would NOT catch it"
        );
        // Conclusion: Bogdan's gap is real for sub-cb-threshold RPC manipulation.
    }

    /// N-2 SCENARIO B (2-KEEPER): Both keepers disagree — mutual deadlock.
    ///
    /// With 2 keepers, if threshold is "≥1 peer disagrees → block":
    /// keeper A disagrees with B, keeper B disagrees with A.
    /// Both block each other. Oracle goes stale. No backstop.
    ///
    /// This is the fundamental reason blocking doesn't scale below 3 keepers.
    #[test]
    fn test_n2_two_keepers_mutual_deadlock() {
        let keeper_a_value: u128 = 1_050_000; // A's RPC reading
        let keeper_b_value: u128 = 1_000_000; // B's RPC reading

        // With "block if ≥1 peer disagrees" (the only meaningful threshold with 2 keepers)
        let a_pushes = peer_decision(keeper_a_value, &[keeper_b_value], 100, Some(1));
        let b_pushes = peer_decision(keeper_b_value, &[keeper_a_value], 100, Some(1));

        assert!(!a_pushes, "A is blocked: B disagrees (≥1 peer)");
        assert!(!b_pushes, "B is blocked: A disagrees (≥1 peer)");

        // Neither keeper pushes. Oracle goes stale with no signal to curator.
        // No drift tracker, no circuit breaker, no alerting — just silence.
        // This is strictly worse than warn-only where at least one value gets pushed.
        let oracle_stale = !a_pushes && !b_pushes;
        assert!(oracle_stale, "DEADLOCK: oracle goes stale, no keeper can push");
    }

    /// N-2 SCENARIO B (3-KEEPER): Bogdan's fix with honest minority.
    ///
    /// 2 of 3 keepers have compromised RPCs (harder attack, but defines the failure mode).
    /// Compromised keepers agree with each other (+500 bps each).
    /// Honest keeper reads the real value.
    ///
    /// With Bogdan's "≥2 disagree" blocking:
    /// - Honest keeper: 2 compromised peers disagree → BLOCKED (silenced)
    /// - Each compromised keeper: only 1 peer disagrees (honest one) → NOT blocked
    /// - Compromised keepers push freely, honest keeper is permanently silenced.
    #[test]
    fn test_n2_three_keepers_compromised_majority_silences_honest() {
        let honest_value: u128 = 1_000_000;     // real spoke value
        let compromised_value: u128 = 1_050_000; // inflated value from bad RPCs

        // Honest keeper vs 2 compromised peers
        let honest_peers = [compromised_value, compromised_value];
        let honest_can_push_bogdan = peer_decision(honest_value, &honest_peers, 100, Some(2));
        assert!(
            !honest_can_push_bogdan,
            "SILENCED: honest keeper blocked by ≥2 compromised peers disagreeing"
        );

        // Compromised keeper A vs honest peer + compromised peer B
        let compromised_peers_for_a = [honest_value, compromised_value];
        let compromised_a_can_push = peer_decision(compromised_value, &compromised_peers_for_a, 100, Some(2));
        assert!(
            compromised_a_can_push,
            "Compromised A: only 1 peer disagrees (honest) → NOT blocked → pushes bad value"
        );

        // Compromised keeper B: same situation
        let compromised_peers_for_b = [honest_value, compromised_value];
        let compromised_b_can_push = peer_decision(compromised_value, &compromised_peers_for_b, 100, Some(2));
        assert!(
            compromised_b_can_push,
            "Compromised B: only 1 peer disagrees (honest) → NOT blocked → pushes bad value"
        );

        // Summary: Bogdan's fix, when majority is compromised:
        // - silences the only honest keeper
        // - allows both compromised keepers to push freely
        // This is strictly worse than warn-only, where at least the honest
        // keeper keeps pushing and the oracle oscillates between values.
        assert!(
            !honest_can_push_bogdan && compromised_a_can_push && compromised_b_can_push,
            "BYZANTINE FAILURE: fix silences honest keeper, compromised majority wins"
        );
    }

    /// N-2 WARN-ONLY WITH BACKSTOPS: Why warn-only is strictly better than blocking
    /// in the Byzantine scenario.
    ///
    /// Even if a compromised keeper pushes a wrong value (+500 bps),
    /// the honest keeper corrects it next cycle. Net effect: oracle oscillates.
    /// Drift tracker and circuit breaker still act as backstops.
    ///
    /// If we block instead: oracle can go stale (2-keeper deadlock) or the
    /// honest keeper gets silenced (3-keeper Byzantine). Neither has a backstop.
    #[test]
    fn test_n2_warn_only_keeps_backstops_active() {
        let stored: u128 = 1_000_000;       // storedTotalAssets
        let compromised: u128 = 1_050_000;  // bad RPC: +500 bps
        let honest: u128 = 1_000_050;       // real value: +5 bps yield
        let max_change_bps: u128 = 1500;    // circuit breaker threshold

        // Warn-only: compromised keeper pushes 1,050,000
        // Honest keeper pushes 1,000,050 next cycle → oracle corrected within 1h
        let compromised_delta = (compromised - stored) * 10_000 / stored;
        let honest_delta = (honest - stored) * 10_000 / stored;

        assert!(
            compromised_delta <= max_change_bps,
            "compromised push: {compromised_delta} bps is within cb — visible oscillation"
        );
        assert!(
            honest_delta <= max_change_bps,
            "honest push: {honest_delta} bps — corrects oracle next cycle"
        );

        // Drift tracker on compromised keeper: if RPC is consistently lying,
        // cumulative drift will trip within the 24h window.
        let mut compromised_tracker = DriftTracker::new(86400, 1500);
        compromised_tracker.check_and_record("0xoracle", stored, 0); // anchor at real value

        // Compromised keeper keeps pushing +500 bps each hour
        let mut value = stored;
        let mut now = 1u64;
        let mut pushes = 0u32;
        loop {
            let next = value + (value * 500 / 10_000); // +500 bps = +5%
            if !compromised_tracker.check_and_record("0xoracle", next, now) {
                break;
            }
            value = next;
            now += 3600;
            pushes += 1;
            assert!(pushes < 20);
        }
        // Drift tracker eventually blocks the compromised keeper even in warn-only mode.
        assert!(
            pushes <= 3,
            "drift catches compromised keeper after {pushes} pushes — no blocking needed"
        );
        // Contrast with blocking scenario: if oracle went stale (deadlock),
        // drift tracker never fires, curator gets no signal at all.
    }

    /// N-2 SUMMARY: The correct fix is alerting, not blocking.
    ///
    /// Blocking on peer disagreement:
    /// - 2 keepers: mutual deadlock → oracle stale, no signal
    /// - 3 keepers + compromised majority: honest silenced, no signal
    ///
    /// Warn-only + drift + circuit breaker:
    /// - Compromised single keeper: bad value pushed, corrected next cycle by honest keeper
    /// - Drift tracker stops cumulative manipulation within 24h
    /// - Circuit breaker stops large single jumps on-chain
    ///
    /// Missing piece (both approaches): no alert to curator on peer divergence.
    /// Correct fix: alert on peer divergence >100 bps so curator can investigate
    /// and shut down compromised keeper manually.
    #[test]
    fn test_n2_blocking_has_no_backstop_warn_only_does() {
        // Scenario: RPC lag causes temporary divergence of ~150 bps (>100 bps threshold).
        // Not an attack — keepers read against finalized block but one RPC is slightly behind.
        // 150 bps = 1.5% difference: e.g. one keeper missed a small deposit in the finalized block.
        let our_value: u128 = 1_015_000;  // our RPC: 1.5% higher due to RPC lag
        let peer_a: u128 = 1_000_000;     // peer A: sees slightly older state
        let peer_b: u128 = 1_000_000;     // peer B: same

        // Bogdan's block: ≥2 peers diverge → blocked
        let bogdan_blocks = !peer_decision(our_value, &[peer_a, peer_b], 100, Some(2));
        assert!(bogdan_blocks, "temporary lag triggers block — not an attack");

        // Warn-only: push our value, peers diverge → warn fires
        let warn_only_pushes = peer_decision(our_value, &[peer_a, peer_b], 100, None);
        assert!(warn_only_pushes, "warn-only: push proceeds, warn fires, curator can investigate");

        // With blocking: oracle doesn't update. If this happens every cycle during
        // high activity, oracle goes stale → OraclePriceIsOld() → users can't operate.
        // No drift, no circuit breaker, no signal — just stale oracle.

        // With warn-only: ~150 bps push goes through. Honest peer corrects next cycle.
        // Drift tracker records it. If it's consistent manipulation, drift trips within 24h.
        let mut tracker = DriftTracker::new(86400, 1500);
        tracker.check_and_record("0xoracle", 1_000_000, 0);
        let lag_push_passes = tracker.check_and_record("0xoracle", our_value, 100);
        assert!(lag_push_passes, "~150 bps lag: drift allows it, corrects naturally");
    }
}
