mod api;
mod config;
mod metrics;
mod oracle;
mod peers;
mod spoke;

use alloy::{primitives::Address, signers::local::PrivateKeySigner};
use api::{KeeperState, SharedState, SpokeState};
use eyre::{Context, Result};
use std::{str::FromStr, sync::Arc, time::Duration};
use tokio::{sync::Notify, time::sleep};
use tracing::{error, info, warn};

use config::{RuntimeConfig, SpokeConfig};
use metrics::*;

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize structured logging. RUST_LOG controls verbosity; default: info.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    info!("vault-oracle-keeper starting");

    let cfg = RuntimeConfig::load().context("Failed to load configuration")?;

    info!(
        flow_rpc = cfg.flow_rpc(),
        update_interval_secs = cfg.hub.update_interval_secs,
        min_update_interval_secs = cfg.hub.min_update_interval_secs,
        batch_updater = %cfg.hub.batch_updater,
        max_retries = cfg.hub.max_retries,
        api_port = cfg.hub.api_port,
        "configuration loaded"
    );

    let active_spokes: Vec<SpokeConfig> = cfg
        .spokes
        .iter()
        .filter(|s| s.active)
        .cloned()
        .collect();

    if active_spokes.is_empty() {
        warn!("No active spokes found. Set active = true in config.toml.");
        return Ok(());
    }

    // Validate that every active spoke has an oracle address configured
    for spoke in &active_spokes {
        if spoke.oracle_address.is_none() {
            warn!(
                spoke = spoke.name,
                "oracle address not configured — set ORACLE_{} in .env",
                spoke.name.to_uppercase()
            );
        }
    }

    info!("Running multicall loop for {} active spoke(s)", active_spokes.len());

    // Shared state between HTTP API and main loop
    let shared_state: SharedState = Arc::new(tokio::sync::RwLock::new(KeeperState {
        update_interval_secs: cfg.hub.update_interval_secs,
        ..KeeperState::default()
    }));

    // Notify used by POST /update to wake the main loop early
    let notify = Arc::new(Notify::new());

    // Spawn HTTP API server
    let api_port = cfg.hub.api_port;
    tokio::spawn(api::start_server(
        api_port,
        shared_state.clone(),
        notify.clone(),
    ));

    // Peer-aware startup coordination: find the optimal position in the push schedule
    let oracle_ts = read_latest_timestamp(&cfg, &active_spokes).await;
    let startup_sleep = peers::calculate_startup_sleep(
        &cfg.hub.peers,
        cfg.hub.update_interval_secs,
        oracle_ts,
    )
    .await;

    if startup_sleep.as_secs() > 0 {
        info!(
            sleep_secs = startup_sleep.as_secs(),
            "Positioning in schedule gap, sleeping before first push"
        );
        sleep(startup_sleep).await;
    }

    run_multicall_loop(&active_spokes, &cfg, shared_state, notify).await;

    Ok(())
}

/// Single main loop: reads all spokes in parallel, checks staleness, then
/// sends one Multicall3 transaction batching all oracle updates on Flow EVM.
async fn run_multicall_loop(
    spokes: &[SpokeConfig],
    cfg: &RuntimeConfig,
    shared_state: SharedState,
    notify: Arc<Notify>,
) {
    loop {
        log_cycle_start("multicall");

        let mut attempt: u32 = 0;
        let mut cycle_tx: Option<String> = None;

        loop {
            // 1. Read totalAssets() from ALL active spokes IN PARALLEL (always fresh on each attempt)
            info!("Reading totalAssets from {} spoke(s)…", spokes.len());
            let spoke_values = spoke::read_all_spokes(spokes).await;

            for (name, val) in &spoke_values {
                info!(spoke = %name, total_assets = val, "read spoke value");
            }

            // 2. Check staleness against the first spoke's oracle (representative for all)
            //    Skip the entire batch if the oracle was updated recently.
            let should_skip = check_staleness_and_skip(spokes, cfg).await;
            if should_skip {
                break; // fresh — skip this batch, wait full interval
            }

            // 3. Build the calls list: (oracle_address, total_assets)
            let calls = match build_oracle_calls(spokes, &spoke_values) {
                Some(c) => c,
                None => {
                    warn!("No oracle addresses configured for any active spoke — skipping");
                    break;
                }
            };

            info!(num_calls = calls.len(), "sending oracle update transactions");

            // 4. Build signer
            let signer = match PrivateKeySigner::from_str(&cfg.keeper_private_key) {
                Ok(s) => s,
                Err(e) => {
                    error!(error = %e, "Invalid KEEPER_PRIVATE_KEY — cannot send tx");
                    break;
                }
            };

            // 5. Single batchUpdate() tx via OracleBatchUpdater
            match oracle::send_batch_update(cfg.flow_rpc(), cfg.hub.batch_updater, signer, calls).await {
                Ok(tx_hash) => {
                    info!(
                        tx_hash = %tx_hash,
                        num_spokes = spokes.len(),
                        "batchUpdate succeeded"
                    );
                    log_multicall_success(spokes.len(), tx_hash);
                    cycle_tx = Some(format!("{tx_hash:#x}"));
                    break; // success — exit retry loop
                }
                Err(err) => {
                    attempt += 1;
                    if attempt >= cfg.hub.max_retries {
                        error!(
                            error = %err,
                            attempts = attempt,
                            "CRITICAL: oracle updates failed after max retries — giving up this cycle"
                        );
                        break;
                    } else {
                        warn!(
                            error = %err,
                            attempt,
                            max_retries = cfg.hub.max_retries,
                            retry_delay_secs = cfg.hub.retry_delay_secs,
                            "batchUpdate failed — will retry"
                        );
                        sleep(Duration::from_secs(cfg.hub.retry_delay_secs)).await;
                    }
                }
            }
        }

        // Update shared state after each cycle
        {
            let now_ts = chrono::Utc::now().timestamp() as u64;
            let flow_rpc = cfg.flow_rpc().to_string();
            let spoke_states = build_spoke_states(spokes, &flow_rpc).await;

            let mut state = shared_state.write().await;
            state.last_cycle_at = Some(now_ts);
            if cycle_tx.is_some() {
                state.last_cycle_tx = cycle_tx;
            }
            state.spoke_states = spoke_states;
        }

        // Wait for the next interval OR an early-wake from POST /update
        tokio::select! {
            _ = sleep(Duration::from_secs(cfg.hub.update_interval_secs)) => {},
            _ = notify.notified() => {
                info!("woken early by POST /update — starting cycle immediately");
            }
        }
    }
}

/// Read oracle state (latestTimestamp + storedTotalAssets) for each spoke from
/// Flow EVM and return a Vec of SpokeState. Used to populate the /status endpoint.
async fn build_spoke_states(spokes: &[SpokeConfig], flow_rpc: &str) -> Vec<SpokeState> {
    let mut states = Vec::with_capacity(spokes.len());

    for spoke in spokes {
        let oracle_addr = match &spoke.oracle_address {
            Some(a) => a.clone(),
            None => {
                states.push(SpokeState {
                    name: spoke.name.clone(),
                    oracle: String::new(),
                    stored_total_assets: 0,
                    last_updated: 0,
                    active: spoke.active,
                });
                continue;
            }
        };

        let last_updated = match oracle::latest_timestamp(&oracle_addr, flow_rpc).await {
            Ok(ts) => ts.try_into().unwrap_or(0u64),
            Err(err) => {
                warn!(spoke = spoke.name, error = %err, "failed to read latestTimestamp for status");
                0u64
            }
        };

        let stored_total_assets =
            match oracle::stored_total_assets(&oracle_addr, flow_rpc).await {
                Ok(v) => {
                    let max = alloy::primitives::U256::from(u128::MAX);
                    if v > max { u128::MAX } else { v.to::<u128>() }
                }
                Err(err) => {
                    warn!(spoke = spoke.name, error = %err, "failed to read storedTotalAssets for status");
                    0u128
                }
            };

        states.push(SpokeState {
            name: spoke.name.clone(),
            oracle: oracle_addr,
            stored_total_assets,
            last_updated,
            active: spoke.active,
        });
    }

    states
}

/// Read the latestTimestamp from the first spoke's oracle. Returns 0 on failure.
/// Used during startup to calculate the peer-aware sleep.
async fn read_latest_timestamp(cfg: &RuntimeConfig, spokes: &[SpokeConfig]) -> u64 {
    let representative = spokes.iter().find(|s| s.oracle_address.is_some());
    let oracle_address = match representative.and_then(|s| s.oracle_address.as_deref()) {
        Some(addr) => addr,
        None => return 0,
    };

    match oracle::latest_timestamp(oracle_address, cfg.flow_rpc()).await {
        Ok(ts) => ts.try_into().unwrap_or(0u64),
        Err(err) => {
            warn!(error = %err, "Failed to read latestTimestamp for startup coordination");
            0
        }
    }
}

/// Check the latestTimestamp on the first spoke's oracle.
/// Returns `true` if the oracle is fresh enough to skip the batch.
async fn check_staleness_and_skip(spokes: &[SpokeConfig], cfg: &RuntimeConfig) -> bool {
    // Use the first spoke with a configured oracle address as the representative
    let representative = spokes
        .iter()
        .find(|s| s.oracle_address.is_some());

    let oracle_address = match representative.and_then(|s| s.oracle_address.as_deref()) {
        Some(addr) => addr,
        None => return false, // no oracle address → do not skip, let build step warn
    };

    let now_secs = chrono::Utc::now().timestamp() as u64;

    match oracle::latest_timestamp(oracle_address, cfg.flow_rpc()).await {
        Ok(oracle_ts) => {
            let oracle_ts_u64: u64 = oracle_ts.try_into().unwrap_or(0);
            let age_secs = now_secs.saturating_sub(oracle_ts_u64);

            if age_secs < cfg.hub.min_update_interval_secs {
                info!(
                    oracle_age_secs = age_secs,
                    min_update_interval_secs = cfg.hub.min_update_interval_secs,
                    "oracle is fresh — skipping batch"
                );
                true
            } else {
                info!(
                    oracle_age_secs = age_secs,
                    "oracle is stale — proceeding with batch update"
                );
                false
            }
        }
        Err(err) => {
            warn!(
                error = %err,
                oracle_address,
                "failed to read latestTimestamp — proceeding with batch update anyway"
            );
            false // on error, attempt the update
        }
    }
}

/// Build the Vec of (Address, u128) for direct oracle updates, one per spoke
/// with a configured oracle address. Returns None if no spokes have oracle addresses.
fn build_oracle_calls(
    spokes: &[SpokeConfig],
    spoke_values: &[(String, u128)],
) -> Option<Vec<(Address, u128)>> {
    let value_map: std::collections::HashMap<&str, u128> = spoke_values
        .iter()
        .map(|(name, val)| (name.as_str(), *val))
        .collect();

    let calls: Vec<(Address, u128)> = spokes
        .iter()
        .filter_map(|spoke| {
            let oracle_addr_str = spoke.oracle_address.as_deref()?;
            let oracle_addr = Address::from_str(oracle_addr_str).ok()?;
            let total_assets = *value_map.get(spoke.name.as_str()).unwrap_or(&1u128);
            Some((oracle_addr, total_assets))
        })
        .collect();

    if calls.is_empty() { None } else { Some(calls) }
}
