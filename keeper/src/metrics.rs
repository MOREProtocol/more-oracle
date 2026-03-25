use alloy::primitives::TxHash;
use chrono::Utc;
use tracing::{error, info, warn};

/// Log a successful multicall batch push to all oracles.
pub fn log_multicall_success(num_spokes: usize, tx_hash: TxHash) {
    info!(
        num_spokes,
        tx_hash = %tx_hash,
        ts = %Utc::now().to_rfc3339(),
        "multicall batch pushed all oracle updates"
    );
}

/// Log a successful push to the oracle (single-spoke path).
#[allow(dead_code)]
pub fn log_push_success(spoke_name: &str, tx_hash: TxHash) {
    info!(
        spoke = spoke_name,
        tx_hash = %tx_hash,
        ts = %Utc::now().to_rfc3339(),
        "pushed update"
    );
}

/// Log a skipped push (oracle is fresh enough).
#[allow(dead_code)]
pub fn log_skipped(spoke_name: &str, oracle_age_secs: u64, min_interval: u64) {
    info!(
        spoke = spoke_name,
        oracle_age_secs,
        min_interval,
        ts = %Utc::now().to_rfc3339(),
        "skipped — oracle is fresh"
    );
}

/// Log that the oracle address is not yet configured for a spoke.
#[allow(dead_code)]
pub fn log_no_oracle(spoke_name: &str) {
    warn!(
        spoke = spoke_name,
        "oracle address not configured — set ORACLE_{} in .env",
        spoke_name.to_uppercase()
    );
}

/// Log an error that occurred during a cycle (non-fatal).
#[allow(dead_code)]
pub fn log_cycle_error(spoke_name: &str, err: &eyre::Report) {
    error!(
        spoke = spoke_name,
        error = %err,
        ts = %Utc::now().to_rfc3339(),
        "cycle error — will retry"
    );
}

/// Log the start of a new cycle.
pub fn log_cycle_start(label: &str) {
    info!(
        label,
        ts = %Utc::now().to_rfc3339(),
        "starting update cycle"
    );
}

/// Log when totalAssets is zero — keeper will push 1 instead.
#[allow(dead_code)]
pub fn log_zero_assets(spoke_name: &str) {
    warn!(
        spoke = spoke_name,
        "totalAssets() returned 0 — pushing 1 to avoid ValueNotPositive revert"
    );
}
