mod api;
mod config;
mod drift;
mod metrics;
mod oracle;
mod peer_registry;
mod peers;
mod security;
mod spoke;
mod telegram;

use alloy::{primitives::Address, signers::local::PrivateKeySigner};
use api::{KeeperState, SharedState, SpokeReading, SpokeState};
use drift::DriftTracker;
use eyre::{Context, Result};
use std::{
    collections::HashMap,
    str::FromStr,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{sync::Notify, time::sleep};
use tracing::{error, info, warn};

use config::{RuntimeConfig, SpokeConfig};
use metrics::*;
use peer_registry::{new_peer_registry, new_pending_registrations, PeerRegistry};

/// Per-spoke tracking of the last value pushed on-chain and when it was pushed.
/// Shared between the monitor loop and the scheduled loop.
#[derive(Debug, Clone)]
struct LastPushedInfo {
    /// The totalAssets value that was last pushed on-chain for this spoke.
    last_pushed_value: u128,
    /// When it was last pushed (monotonic clock).
    last_pushed_at: Instant,
}

/// Thread-safe map from spoke name to its last-pushed state.
type LastPushedState = Arc<tokio::sync::Mutex<HashMap<String, LastPushedInfo>>>;

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
        drift_window_secs = cfg.hub.drift_window_secs,
        max_cumulative_drift_bps = cfg.hub.max_cumulative_drift_bps,
        monitor_interval_secs = cfg.hub.monitor_interval_secs,
        early_push_threshold_bps = cfg.hub.early_push_threshold_bps,
        skip_if_fresh_secs = cfg.hub.skip_if_fresh_secs,
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
                "oracle address not configured -- set ORACLE_{} in .env",
                spoke.name.to_uppercase()
            );
        }
    }

    info!("Running multicall loop for {} active spoke(s)", active_spokes.len());

    // Startup Telegram notification
    if let Some(tg) = &cfg.telegram {
        let spoke_names: Vec<&str> = active_spokes.iter().map(|s| s.name.as_str()).collect();
        tg.send(format!(
            "✅ <b>Keeper started</b>\n\
             Spokes: <code>{}</code>\n\
             Update interval: {}s | Monitor: {}s\n\
             Circuit breaker: on-chain | Drift: {} bps / 24 h",
            spoke_names.join(", "),
            cfg.hub.update_interval_secs,
            cfg.hub.monitor_interval_secs,
            cfg.hub.max_cumulative_drift_bps,
        ));
    }

    // Startup discrepancy check: compare storedTotalAssets on-chain vs current spoke value.
    // Alerts if the keeper was down during a bridge or significant yield event.
    // Does NOT block the push — circuit breaker handles that.
    {
        const STARTUP_WARN_BPS: u64 = 100; // warn if >1% difference
        let spoke_values = spoke::read_all_spokes(&active_spokes).await;
        for spoke in &active_spokes {
            let oracle_addr = match &spoke.oracle_address {
                Some(a) => a.clone(),
                None => continue,
            };
            let current = match spoke_values.iter().find(|(n, _, _)| n == &spoke.name) {
                Some((_, v, _)) if *v > 1 => *v,
                _ => continue, // spoke unreadable or empty vault — skip
            };
            match oracle::stored_total_assets(&oracle_addr, cfg.flow_rpc()).await {
                Ok(stored_u256) => {
                    let stored: u128 = stored_u256.try_into().unwrap_or(0);
                    if stored <= 1 {
                        continue; // oracle never pushed yet — normal at launch
                    }
                    let delta_bps = if current > stored {
                        ((current - stored) as u128 * 10_000 / stored as u128) as u64
                    } else {
                        ((stored - current) as u128 * 10_000 / stored as u128) as u64
                    };
                    if delta_bps > STARTUP_WARN_BPS {
                        warn!(
                            spoke = %spoke.name,
                            stored,
                            current,
                            delta_bps,
                            "startup: oracle vs spoke discrepancy detected"
                        );
                        if let Some(tg) = &cfg.telegram {
                            tg.startup_discrepancy(&spoke.name, stored, current, delta_bps);
                        }
                    } else {
                        info!(spoke = %spoke.name, stored, current, delta_bps, "startup: oracle in sync");
                    }
                }
                Err(e) => {
                    warn!(spoke = %spoke.name, error = %e, "startup: could not read storedTotalAssets");
                }
            }
        }
    }

    // Shared state between HTTP API and main loop
    let shared_state: SharedState = Arc::new(tokio::sync::RwLock::new(KeeperState {
        update_interval_secs: cfg.hub.update_interval_secs,
        ..KeeperState::default()
    }));

    // Notify used by POST /update to wake the main loop early
    let notify = Arc::new(Notify::new());

    // Read curator address from vault contract
    let curator = oracle::read_curator(&cfg.hub.vault_address, cfg.flow_rpc())
        .await
        .context("Failed to read curator() from vault")?;
    info!(curator = %curator, "vault curator loaded");

    // Build signer for peer registration
    let signer = PrivateKeySigner::from_str(&cfg.keeper_private_key)
        .context("Invalid KEEPER_PRIVATE_KEY")?;

    // Create peer registry and pending registrations
    let peer_registry: PeerRegistry = new_peer_registry();
    let pending_registrations = new_pending_registrations();

    // Spawn HTTP API server
    let api_port = cfg.hub.api_port;
    tokio::spawn(api::start_server(
        api_port,
        shared_state.clone(),
        notify.clone(),
        curator,
        peer_registry.clone(),
        pending_registrations.clone(),
        cfg.hub.batch_updater,
        cfg.flow_rpc().to_string(),
        cfg.hub.keeper_url.clone(),
        Some(signer.clone()),
        active_spokes.clone(),
        cfg.telegram.clone(),
    ));

    // Peer-aware startup coordination: find the optimal position in the push schedule
    let oracle_ts = read_latest_timestamp(&cfg, &active_spokes).await;
    let startup_sleep = peers::calculate_startup_sleep(
        &cfg.hub.peers,
        cfg.hub.update_interval_secs,
        oracle_ts,
        &signer,
    )
    .await;

    if startup_sleep.as_secs() > 0 {
        info!(
            sleep_secs = startup_sleep.as_secs(),
            "Positioning in schedule gap, sleeping before first push"
        );
        sleep(startup_sleep).await;
    }

    // Attempt peer registrations after startup sleep
    peers::attempt_peer_registrations(&cfg, &signer, &peer_registry).await;

    // Shared last-pushed state: tracks per-spoke what was last pushed and when.
    let last_pushed: LastPushedState = Arc::new(tokio::sync::Mutex::new(HashMap::new()));

    // Drift tracker: in-memory, resets on keeper restart.
    let drift_tracker = Arc::new(tokio::sync::Mutex::new(DriftTracker::new(
        cfg.hub.drift_window_secs,
        cfg.hub.max_cumulative_drift_bps,
    )));

    // Spawn peer sync loop
    {
        let sync_cfg = cfg.clone();
        let sync_registry = peer_registry.clone();
        let sync_notify = notify.clone();
        let sync_signer = signer.clone();
        tokio::spawn(async move {
            peers::run_peer_sync_loop(sync_cfg, sync_registry, sync_notify, sync_signer).await;
        });
        info!("peer sync loop spawned");
    }

    // Spawn the monitor loop if early_push_threshold_bps > 0
    if cfg.hub.early_push_threshold_bps > 0 {
        let monitor_spokes = active_spokes.clone();
        let monitor_cfg = cfg.clone();
        let monitor_last_pushed = last_pushed.clone();
        let monitor_drift_tracker = drift_tracker.clone();
        tokio::spawn(async move {
            run_monitor_loop(
                &monitor_spokes,
                &monitor_cfg,
                monitor_last_pushed,
                monitor_drift_tracker,
            )
            .await;
        });
        info!(
            interval_secs = cfg.hub.monitor_interval_secs,
            threshold_bps = cfg.hub.early_push_threshold_bps,
            "monitor loop spawned"
        );
    } else {
        info!("monitor loop disabled (early_push_threshold_bps = 0)");
    }

    // Run the scheduled batch loop on the main task
    run_scheduled_loop(
        &active_spokes,
        &cfg,
        shared_state,
        notify,
        drift_tracker,
        last_pushed,
        peer_registry,
    )
    .await;

    Ok(())
}

/// Monitor loop: runs every `monitor_interval_secs`, reads totalAssets from all
/// spokes, and pushes individual oracle updates for any spoke whose value has
/// drifted more than `early_push_threshold_bps` from its last pushed value.
async fn run_monitor_loop(
    spokes: &[SpokeConfig],
    cfg: &RuntimeConfig,
    last_pushed: LastPushedState,
    drift_tracker: Arc<tokio::sync::Mutex<DriftTracker>>,
) {
    let interval = Duration::from_secs(cfg.hub.monitor_interval_secs);
    let threshold_bps = cfg.hub.early_push_threshold_bps;

    loop {
        sleep(interval).await;

        info!("monitor: reading totalAssets from {} spoke(s)", spokes.len());
        let spoke_values = spoke::read_all_spokes(spokes).await;

        for (name, total_assets, _) in &spoke_values {
            let spoke_cfg = match spokes.iter().find(|s| &s.name == name) {
                Some(s) => s,
                None => continue,
            };

            let oracle_addr_str = match &spoke_cfg.oracle_address {
                Some(a) => a.clone(),
                None => continue,
            };

            // Check if delta exceeds threshold compared to last pushed value
            let should_push = {
                let state = last_pushed.lock().await;
                match state.get(name) {
                    Some(info) => {
                        let delta_bps = calculate_delta_bps(info.last_pushed_value, *total_assets);
                        if delta_bps >= threshold_bps {
                            info!(
                                spoke = %name,
                                last_pushed_value = info.last_pushed_value,
                                current_value = total_assets,
                                delta_bps,
                                threshold_bps,
                                "monitor: delta exceeds threshold -- triggering early push"
                            );
                            true
                        } else {
                            false
                        }
                    }
                    None => {
                        // No previous push recorded -- record current value without pushing.
                        // The scheduled loop will handle the first push.
                        false
                    }
                }
            };

            if !should_push {
                continue;
            }

            // Apply drift protection if enabled
            let oracle_addr = match Address::from_str(&oracle_addr_str) {
                Ok(a) => a,
                Err(e) => {
                    warn!(spoke = %name, error = %e, "monitor: invalid oracle address");
                    continue;
                }
            };

            {
                let now_secs = chrono::Utc::now().timestamp() as u64;
                let oracle_key = format!("{oracle_addr:#x}");
                let mut tracker = drift_tracker.lock().await;
                if !tracker.check_and_record(&oracle_key, *total_assets, now_secs) {
                    warn!(
                        spoke = %name,
                        "monitor: skipping early push due to drift protection"
                    );
                    continue;
                }
            }

            // Send individual update
            let signer = match PrivateKeySigner::from_str(&cfg.keeper_private_key) {
                Ok(s) => s,
                Err(e) => {
                    error!(error = %e, "monitor: invalid KEEPER_PRIVATE_KEY");
                    if let Some(tg) = &cfg.telegram {
                        tg.critical_invalid_key();
                    }
                    continue; // don't break — other spokes may still push fine
                }
            };

            let total_assets_u256 = alloy::primitives::U256::from(*total_assets);
            match oracle::push_update(&oracle_addr_str, cfg.flow_rpc(), signer, total_assets_u256)
                .await
            {
                Ok(tx_hash) => {
                    info!(
                        spoke = %name,
                        tx_hash = %tx_hash,
                        total_assets = total_assets,
                        "monitor: early push succeeded"
                    );
                    // Update last-pushed state
                    let mut state = last_pushed.lock().await;
                    state.insert(
                        name.clone(),
                        LastPushedInfo {
                            last_pushed_value: *total_assets,
                            last_pushed_at: Instant::now(),
                        },
                    );
                }
                Err(e) => {
                    warn!(
                        spoke = %name,
                        error = %e,
                        "monitor: early push failed"
                    );
                    if let Some(tg) = &cfg.telegram {
                        tg.monitor_push_failed(name, &e.to_string());
                    }
                }
            }
        }
    }
}

/// Scheduled batch loop: reads all spokes, skips any that were recently pushed
/// by the monitor loop, and pushes the rest via batch or individual updates.
async fn run_scheduled_loop(
    spokes: &[SpokeConfig],
    cfg: &RuntimeConfig,
    shared_state: SharedState,
    notify: Arc<Notify>,
    drift_tracker: Arc<tokio::sync::Mutex<DriftTracker>>,
    last_pushed: LastPushedState,
    peer_registry: PeerRegistry,
) {
    loop {
        log_cycle_start("scheduled");

        let mut attempt: u32 = 0;
        let mut cycle_tx: Option<String> = None;

        loop {
            // 1. Read totalAssets() from ALL active spokes IN PARALLEL
            info!("Reading totalAssets from {} spoke(s)...", spokes.len());
            let spoke_values = spoke::read_all_spokes(spokes).await;

            for (name, val, _) in &spoke_values {
                info!(spoke = %name, total_assets = val, "read spoke value");
            }

            // 1b. Cross-validate and resolve with peer readings
            let resolved_values =
                peers::cross_validate_and_resolve(&spoke_values, &peer_registry, cfg).await;

            // 1c. Store readings in shared state for peer sharing
            {
                let now_ts = chrono::Utc::now().timestamp() as u64;
                let readings: Vec<SpokeReading> = resolved_values
                    .iter()
                    .map(|(name, value)| {
                        let source = if spoke_values
                            .iter()
                            .any(|(n, v, failed)| n == name && *v == *value && *v > 1 && !failed)
                        {
                            "rpc".to_string()
                        } else if spoke_values
                            .iter()
                            .any(|(n, _, failed)| n == name && !failed)
                            && *value <= 1
                        {
                            "empty".to_string()
                        } else if *value > 1 {
                            "peer_fallback".to_string()
                        } else {
                            "failed".to_string()
                        };
                        SpokeReading {
                            spoke: name.clone(),
                            value: *value,
                            source,
                            at: now_ts,
                        }
                    })
                    .collect();

                let mut state = shared_state.write().await;
                state.last_spoke_readings = readings;
            }

            // 2. Check staleness against the first spoke's oracle (representative)
            let should_skip = check_staleness_and_skip(spokes, cfg).await;
            if should_skip {
                break;
            }

            // 3. Build the calls list, filtering out spokes that are "fresh"
            //    (recently pushed by the monitor loop).
            let all_calls = match build_oracle_calls(spokes, &resolved_values) {
                Some(c) => c,
                None => {
                    warn!("No oracle addresses configured for any active spoke -- skipping");
                    break;
                }
            };

            // Filter out spokes that were recently pushed by the monitor loop
            let skip_if_fresh = Duration::from_secs(cfg.hub.skip_if_fresh_secs);
            let (calls_to_push, skipped_fresh) = filter_fresh_spokes(
                spokes,
                &all_calls,
                &last_pushed,
                skip_if_fresh,
            )
            .await;

            if skipped_fresh > 0 {
                info!(
                    skipped_fresh,
                    remaining = calls_to_push.len(),
                    "skipped {} spoke(s) -- recently pushed by monitor loop",
                    skipped_fresh
                );
            }

            if calls_to_push.is_empty() {
                info!("All spokes are fresh (recently pushed by monitor) -- skipping entire batch");
                break;
            }

            // 4. Apply cumulative drift protection
            let now_secs = chrono::Utc::now().timestamp() as u64;
            let mut calls: Vec<(Address, u128)> = Vec::with_capacity(calls_to_push.len());
            let mut skipped_drift: usize = 0;

            {
                let mut tracker = drift_tracker.lock().await;
                for (oracle_addr, total_assets) in &calls_to_push {
                    let oracle_str = format!("{oracle_addr:#x}");
                    if tracker.check_and_record(&oracle_str, *total_assets, now_secs) {
                        calls.push((*oracle_addr, *total_assets));
                    } else {
                        skipped_drift += 1;
                        let spoke_name = spokes
                            .iter()
                            .find(|s| {
                                s.oracle_address.as_deref()
                                    .and_then(|a| Address::from_str(a).ok())
                                    .map(|a| a == *oracle_addr)
                                    .unwrap_or(false)
                            })
                            .map(|s| s.name.as_str())
                            .unwrap_or("unknown");
                        warn!(
                            oracle = %oracle_str,
                            spoke = %spoke_name,
                            total_assets,
                            "skipping oracle due to cumulative drift protection"
                        );
                        if let Some(tg) = &cfg.telegram {
                            tg.drift_alert(spoke_name, &oracle_str, cfg.hub.max_cumulative_drift_bps);
                        }
                    }
                }
            }

            if skipped_drift > 0 {
                info!(
                    skipped = skipped_drift,
                    remaining = calls.len(),
                    "drift protection filtered out {} oracle(s)", skipped_drift
                );
            }

            if calls.is_empty() {
                warn!("All oracles filtered by drift protection -- skipping cycle");
                if let Some(tg) = &cfg.telegram {
                    tg.all_oracles_drift_blocked();
                }
                break;
            }

            info!(num_calls = calls.len(), "sending oracle update transactions");

            // 5. Build signer
            let signer = match PrivateKeySigner::from_str(&cfg.keeper_private_key) {
                Ok(s) => s,
                Err(e) => {
                    error!(error = %e, "Invalid KEEPER_PRIVATE_KEY -- cannot send tx");
                    if let Some(tg) = &cfg.telegram {
                        tg.critical_invalid_key();
                    }
                    break;
                }
            };

            // 6. Try batchUpdate() first (only if most spokes need updating)
            if calls.len() > 1 {
                match oracle::send_batch_update(
                    cfg.flow_rpc(),
                    cfg.hub.batch_updater,
                    signer.clone(),
                    calls.clone(),
                )
                .await
                {
                    Ok(tx_hash) => {
                        info!(
                            tx_hash = %tx_hash,
                            num_oracles = calls.len(),
                            "batchUpdate succeeded"
                        );
                        log_multicall_success(calls.len(), tx_hash);
                        cycle_tx = Some(format!("{tx_hash:#x}"));

                        // Update last-pushed state for all spokes in the batch
                        update_last_pushed_for_calls(spokes, &calls, &last_pushed).await;
                        break;
                    }
                    Err(batch_err) => {
                        warn!(
                            error = %batch_err,
                            num_oracles = calls.len(),
                            "batchUpdate reverted -- falling back to individual oracle updates"
                        );
                        if let Some(tg) = &cfg.telegram {
                            tg.batch_reverted(calls.len(), &batch_err.to_string());
                        }
                        // Fall through to individual updates below
                    }
                }
            }

            // Individual updates (fallback from batch, or when only 1 spoke needs updating)
            let results =
                oracle::send_individual_updates(cfg.flow_rpc(), signer, calls.clone()).await;

            let mut succeeded = 0usize;
            let mut failed = 0usize;
            let mut last_tx: Option<String> = None;
            let mut succeeded_calls: Vec<(Address, u128)> = Vec::new();

            for r in &results {
                if r.success {
                    succeeded += 1;
                    if let Some(ref hash) = r.tx_hash {
                        info!(
                            oracle = %r.oracle,
                            tx_hash = %hash,
                            "individual update succeeded"
                        );
                        last_tx = Some(format!("{hash:#x}"));
                    }
                    // Find the matching call to record in last_pushed
                    if let Some(call) = calls.iter().find(|(addr, _)| *addr == r.oracle) {
                        succeeded_calls.push(*call);
                    }
                } else {
                    failed += 1;
                    let err_str = r.error.as_deref().unwrap_or("unknown");
                    warn!(
                        oracle = %r.oracle,
                        error = err_str,
                        "individual update failed"
                    );
                    if let Some(tg) = &cfg.telegram {
                        tg.individual_update_failed(&format!("{:#x}", r.oracle), err_str);
                    }
                }
            }

            info!(
                succeeded,
                failed,
                total = results.len(),
                "individual update summary"
            );

            if succeeded > 0 {
                cycle_tx = last_tx;
                // Update last-pushed state for succeeded calls
                update_last_pushed_for_calls(spokes, &succeeded_calls, &last_pushed).await;
            }

            if failed > 0 && succeeded == 0 {
                // All individual calls also failed -- apply retry logic
                attempt += 1;
                if attempt >= cfg.hub.max_retries {
                    error!(
                        attempts = attempt,
                        "CRITICAL: all oracle updates failed (batch + individual) after max retries -- giving up this cycle"
                    );
                    if let Some(tg) = &cfg.telegram {
                        tg.critical_all_updates_failed(attempt);
                    }
                    break;
                } else {
                    warn!(
                        attempt,
                        max_retries = cfg.hub.max_retries,
                        retry_delay_secs = cfg.hub.retry_delay_secs,
                        "all individual updates failed -- will retry full cycle"
                    );
                    if let Some(tg) = &cfg.telegram {
                        tg.retrying_cycle(attempt, cfg.hub.max_retries, cfg.hub.retry_delay_secs);
                    }
                    sleep(Duration::from_secs(cfg.hub.retry_delay_secs)).await;
                    continue;
                }
            }

            // At least some succeeded, consider cycle done
            break;
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
                info!("woken early by POST /update -- starting cycle immediately");
            }
        }
    }
}

/// Filter out spokes from `all_calls` that were pushed by the monitor loop
/// within `skip_if_fresh`. Returns the filtered calls and the count of skipped spokes.
async fn filter_fresh_spokes(
    spokes: &[SpokeConfig],
    all_calls: &[(Address, u128)],
    last_pushed: &LastPushedState,
    skip_if_fresh: Duration,
) -> (Vec<(Address, u128)>, usize) {
    if skip_if_fresh.is_zero() {
        return (all_calls.to_vec(), 0);
    }

    let state = last_pushed.lock().await;
    let now = Instant::now();
    let mut filtered = Vec::with_capacity(all_calls.len());
    let mut skipped = 0usize;

    for (oracle_addr, total_assets) in all_calls {
        // Find which spoke this oracle belongs to
        let spoke_name = spokes.iter().find_map(|s| {
            let addr_str = s.oracle_address.as_deref()?;
            let addr = Address::from_str(addr_str).ok()?;
            if addr == *oracle_addr {
                Some(s.name.as_str())
            } else {
                None
            }
        });

        let is_fresh = match spoke_name {
            Some(name) => match state.get(name) {
                Some(info) => now.duration_since(info.last_pushed_at) < skip_if_fresh,
                None => false,
            },
            None => false,
        };

        if is_fresh {
            info!(
                spoke = spoke_name.unwrap_or("unknown"),
                oracle = %oracle_addr,
                "skipping spoke -- recently pushed by monitor loop"
            );
            skipped += 1;
        } else {
            filtered.push((*oracle_addr, *total_assets));
        }
    }

    (filtered, skipped)
}

/// After successful pushes, update the last-pushed state for the given calls.
async fn update_last_pushed_for_calls(
    spokes: &[SpokeConfig],
    calls: &[(Address, u128)],
    last_pushed: &LastPushedState,
) {
    let mut state = last_pushed.lock().await;
    let now = Instant::now();

    for (oracle_addr, total_assets) in calls {
        // Find which spoke this oracle belongs to
        if let Some(spoke) = spokes.iter().find(|s| {
            s.oracle_address
                .as_deref()
                .and_then(|a| Address::from_str(a).ok())
                .map_or(false, |a| a == *oracle_addr)
        }) {
            state.insert(
                spoke.name.clone(),
                LastPushedInfo {
                    last_pushed_value: *total_assets,
                    last_pushed_at: now,
                },
            );
        }
    }
}

/// Calculate the absolute delta in basis points between two values.
/// Returns 0 if the old value is zero (avoids division by zero).
fn calculate_delta_bps(old_value: u128, new_value: u128) -> u64 {
    if old_value == 0 {
        return 0;
    }

    let diff = if new_value > old_value {
        new_value - old_value
    } else {
        old_value - new_value
    };

    let bps = (diff as u128)
        .checked_mul(10_000)
        .map(|n| n / old_value)
        .unwrap_or(u64::MAX as u128);

    if bps > u64::MAX as u128 {
        u64::MAX
    } else {
        bps as u64
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
    let representative = spokes
        .iter()
        .find(|s| s.oracle_address.is_some());

    let oracle_address = match representative.and_then(|s| s.oracle_address.as_deref()) {
        Some(addr) => addr,
        None => return false,
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
                    "oracle is fresh -- skipping batch"
                );
                true
            } else {
                info!(
                    oracle_age_secs = age_secs,
                    "oracle is stale -- proceeding with batch update"
                );
                false
            }
        }
        Err(err) => {
            warn!(
                error = %err,
                oracle_address,
                "failed to read latestTimestamp -- proceeding with batch update anyway"
            );
            false
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_calculate_delta_bps_no_change() {
        assert_eq!(calculate_delta_bps(1_000_000, 1_000_000), 0);
    }

    #[test]
    fn test_calculate_delta_bps_positive() {
        // 0.25% increase = 25 bps
        assert_eq!(calculate_delta_bps(1_000_000, 1_002_500), 25);
    }

    #[test]
    fn test_calculate_delta_bps_negative() {
        // 0.25% decrease = 25 bps
        assert_eq!(calculate_delta_bps(1_000_000, 997_500), 25);
    }

    #[test]
    fn test_calculate_delta_bps_zero_old() {
        assert_eq!(calculate_delta_bps(0, 1_000_000), 0);
    }

    #[test]
    fn test_calculate_delta_bps_large_change() {
        // 50% change = 5000 bps
        assert_eq!(calculate_delta_bps(1_000_000, 1_500_000), 5000);
    }
}
