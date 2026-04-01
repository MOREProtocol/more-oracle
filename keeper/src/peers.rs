/// On startup, query all peer /status endpoints and calculate the optimal
/// startup sleep so this keeper fills the largest gap in the push schedule.
///
/// Algorithm:
/// 1. Query GET {peer}/status for each configured peer (parallel, 5s timeout)
/// 2. Filter peers that have the SAME update_interval as this keeper
/// 3. If no matching peers -> return 0 (push immediately)
/// 4. Collect their last_cycle_at timestamps, map to cycle position (% interval)
/// 5. Find the largest gap in the circular schedule
/// 6. Return sleep duration = middle of largest gap - current position in cycle
use alloy::primitives::Address;
use alloy::signers::local::PrivateKeySigner;
use alloy::signers::Signer;
use eyre::{Context, Result};
use serde::Deserialize;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;
use tracing::{info, warn};

use crate::api::SpokeReading;
use crate::config::RuntimeConfig;
use crate::peer_registry::{PeerInfo, PeerRegistry};

/// Minimal shape of the /status response we care about from a peer.
#[derive(Deserialize)]
struct PeerStatus {
    last_cycle_at: Option<u64>,
    update_interval_secs: Option<u64>,
    #[serde(default)]
    #[allow(dead_code)]
    last_push_at: Option<u64>,
}

/// Response from POST /peers/register.
#[derive(Deserialize)]
struct RegisterResponse {
    challenge: String,
    #[allow(dead_code)]
    expires_at: u64,
}

/// Response from POST /peers/verify.
#[derive(Deserialize)]
struct VerifyResponse {
    api_key: String,
}

/// Response from GET /peers/spoke-values (cached).
#[derive(Deserialize)]
struct SpokeValuesResponse {
    readings: Vec<SpokeReading>,
    #[allow(dead_code)]
    last_push_at: Option<u64>,
}

/// Response from GET /peers/spoke-values/live (fresh RPC read).
#[derive(Deserialize)]
struct LiveSpokeValuesResponse {
    readings: Vec<LiveSpokeReading>,
}

#[derive(Deserialize)]
struct LiveSpokeReading {
    spoke: String,
    value: String, // u128 as string
    source: String,
    #[allow(dead_code)]
    at: u64,
}

/// Query all configured peer /status endpoints in parallel (5 s timeout each).
/// Returns only peers whose `update_interval_secs` matches `update_interval_secs`.
async fn query_peers(peers: &[String], update_interval_secs: u64) -> Vec<u64> {
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            warn!(error = %e, "Failed to build HTTP client for peer queries");
            return Vec::new();
        }
    };

    let futures: Vec<_> = peers
        .iter()
        .map(|peer_url| {
            let client = client.clone();
            let url = format!("{}/status", peer_url.trim_end_matches('/'));
            let peer_url = peer_url.clone();
            async move {
                match client.get(&url).send().await {
                    Ok(resp) => match resp.json::<PeerStatus>().await {
                        Ok(status) => {
                            let peer_interval = status.update_interval_secs.unwrap_or(0);
                            if peer_interval != update_interval_secs {
                                info!(
                                    peer = %peer_url,
                                    peer_interval,
                                    our_interval = update_interval_secs,
                                    "Peer has different update_interval -- ignoring"
                                );
                                return None;
                            }
                            status.last_cycle_at
                        }
                        Err(e) => {
                            warn!(peer = %peer_url, error = %e, "Failed to parse peer /status response");
                            None
                        }
                    },
                    Err(e) => {
                        warn!(peer = %peer_url, error = %e, "Failed to reach peer /status");
                        None
                    }
                }
            }
        })
        .collect();

    futures::future::join_all(futures)
        .await
        .into_iter()
        .flatten()
        .collect()
}

/// On startup, query all peer /status endpoints and calculate the optimal
/// startup sleep so this keeper fills the largest gap in the push schedule.
///
/// `oracle_last_timestamp` is the value from `latestTimestamp()` on-chain --
/// it represents the last time *any* keeper pushed an update, used as an
/// additional position in the gap calculation.
pub async fn calculate_startup_sleep(
    peers: &[String],
    update_interval_secs: u64,
    oracle_last_timestamp: u64,
) -> Duration {
    if peers.is_empty() {
        info!("No peers configured -- starting immediately");
        return Duration::ZERO;
    }

    // 1. Query all peers in parallel with 5 s timeout each
    let peer_last_cycles = query_peers(peers, update_interval_secs).await;

    // 2. If no matching peers respond -> start immediately
    if peer_last_cycles.is_empty() {
        info!("No reachable peers with matching interval -- starting immediately");
        return Duration::ZERO;
    }

    let interval = update_interval_secs;

    // 3. Collect cycle positions (seconds into the current interval window)
    // Include the oracle's last timestamp as a virtual "unknown peer" position
    let mut positions: Vec<u64> = peer_last_cycles.iter().map(|ts| ts % interval).collect();

    if oracle_last_timestamp > 0 {
        positions.push(oracle_last_timestamp % interval);
    }

    // Deduplicate and sort
    positions.sort_unstable();
    positions.dedup();

    info!(
        positions = ?positions,
        interval,
        "Peer cycle positions (seconds into interval)"
    );

    // 4. Find the largest gap in the circular schedule
    let n = positions.len();
    let mut largest_gap_size: u64 = 0;
    let mut largest_gap_start: u64 = 0;

    for i in 0..n {
        let gap_start = positions[i];
        let gap_end = if i + 1 < n {
            positions[i + 1]
        } else {
            // Wrap-around: distance from last position back to first + interval
            positions[0] + interval
        };
        let gap_size = gap_end.saturating_sub(gap_start);
        if gap_size > largest_gap_size {
            largest_gap_size = gap_size;
            largest_gap_start = gap_start;
        }
    }

    // 5. Target = middle of the largest gap
    let target = (largest_gap_start + largest_gap_size / 2) % interval;

    // 6. current_position = now % interval
    let now_secs = chrono::Utc::now().timestamp() as u64;
    let current_position = now_secs % interval;

    // 7. Sleep = distance from current_position to target (forward in cycle)
    let sleep_secs = if target >= current_position {
        target - current_position
    } else {
        interval - current_position + target
    };

    // Cap at one full interval
    let sleep_secs = sleep_secs.min(interval);

    info!(
        largest_gap_start,
        largest_gap_size,
        target,
        current_position,
        sleep_secs,
        "Calculated startup sleep to fill schedule gap"
    );

    Duration::from_secs(sleep_secs)
}

// -- Peer coordination functions -----------------------------------------------

/// Register this keeper with a remote peer using the challenge/verify flow.
///
/// 1. POST {peer_url}/peers/register with our URL
/// 2. Receive challenge
/// 3. Sign challenge with our signer (EIP-191)
/// 4. POST {peer_url}/peers/verify with URL + signature
/// 5. Returns the API key the peer generated for us
pub async fn register_with_peer(
    peer_url: &str,
    our_url: &str,
    signer: &PrivateKeySigner,
    _batch_updater: Address,
    _flow_rpc: &str,
) -> Result<String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .context("failed to build HTTP client")?;

    let base = peer_url.trim_end_matches('/');

    // Step 1: POST /peers/register
    let register_url = format!("{base}/peers/register");
    let register_body = serde_json::json!({ "url": our_url });

    let resp = client
        .post(&register_url)
        .json(&register_body)
        .send()
        .await
        .with_context(|| format!("POST {register_url} failed"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        eyre::bail!("POST {register_url} returned {status}: {body}");
    }

    let register_resp: RegisterResponse = resp
        .json()
        .await
        .context("failed to parse register response")?;

    // Step 2: Sign the challenge (EIP-191 personal_sign)
    let signature = signer
        .sign_message(register_resp.challenge.as_bytes())
        .await
        .context("failed to sign peer challenge")?;

    let sig_hex = format!("0x{}", alloy::hex::encode(signature.as_bytes()));

    // Step 3: POST /peers/verify
    let verify_url = format!("{base}/peers/verify");
    let verify_body = serde_json::json!({
        "url": our_url,
        "signature": sig_hex,
    });

    let resp = client
        .post(&verify_url)
        .json(&verify_body)
        .send()
        .await
        .with_context(|| format!("POST {verify_url} failed"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        eyre::bail!("POST {verify_url} returned {status}: {body}");
    }

    let verify_resp: VerifyResponse = resp
        .json()
        .await
        .context("failed to parse verify response")?;

    Ok(verify_resp.api_key)
}

/// Attempt registration with all configured peers.
pub async fn attempt_peer_registrations(
    cfg: &RuntimeConfig,
    signer: &PrivateKeySigner,
    peer_registry: &PeerRegistry,
) {
    let our_url = match &cfg.hub.keeper_url {
        Some(url) => url.clone(),
        None => {
            info!("No keeper_url configured -- skipping peer registrations");
            return;
        }
    };

    for peer_url in &cfg.hub.peers {
        info!(peer_url = %peer_url, "attempting peer registration");

        match register_with_peer(
            peer_url,
            &our_url,
            signer,
            cfg.hub.batch_updater,
            cfg.flow_rpc(),
        )
        .await
        {
            Ok(api_key) => {
                let now = chrono::Utc::now().timestamp() as u64;
                let mut registry = peer_registry.lock().await;

                // If the peer already exists (from them registering with us), update outgoing key.
                // Otherwise create a new entry (we don't know their wallet yet).
                if let Some(info) = registry.get_mut(peer_url) {
                    info.outgoing_api_key = Some(api_key);
                    info!(
                        peer_url = %peer_url,
                        "peer registration succeeded (updated existing entry)"
                    );
                } else {
                    registry.insert(
                        peer_url.clone(),
                        PeerInfo {
                            url: peer_url.clone(),
                            wallet: Address::ZERO, // will be updated when they register with us
                            incoming_api_key: String::new(),
                            outgoing_api_key: Some(api_key),
                            registered_at: now,
                            last_seen: None,
                            last_push_at: None,
                            active: true,
                        },
                    );
                    info!(
                        peer_url = %peer_url,
                        "peer registration succeeded (created new entry)"
                    );
                }
            }
            Err(e) => {
                warn!(
                    peer_url = %peer_url,
                    error = %e,
                    "peer registration failed"
                );
            }
        }
    }
}

/// Query a peer's spoke values using the API key they gave us.
pub async fn query_peer_spoke_values(
    peer_url: &str,
    api_key: &str,
) -> Result<Vec<SpokeReading>> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .context("failed to build HTTP client")?;

    // Use /live endpoint — reads fresh from RPC right now, not cached values
    let url = format!("{}/peers/spoke-values/live", peer_url.trim_end_matches('/'));

    let resp = client
        .get(&url)
        .header("X-Keeper-Key", api_key)
        .send()
        .await
        .with_context(|| format!("GET {url} failed"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        eyre::bail!("GET {url} returned {status}: {body}");
    }

    let parsed: LiveSpokeValuesResponse = resp
        .json()
        .await
        .context("failed to parse live spoke-values response")?;

    // Convert to SpokeReading format used by cross_validate_and_resolve
    let readings = parsed
        .readings
        .into_iter()
        .map(|r| {
            let value: u128 = r.value.parse().unwrap_or(1);
            SpokeReading {
                spoke: r.spoke,
                value,
                source: r.source,
                at: chrono::Utc::now().timestamp() as u64,
            }
        })
        .collect();

    Ok(readings)
}

/// Cross-validate our readings against peer readings and resolve fallbacks.
///
/// - If our read failed ("failed" source or value == 1 with fallback): use peer value.
/// - If values diverge > 100 bps between us and a peer: log warning.
/// - Returns resolved `Vec<(spoke_name, value)>`.
pub async fn cross_validate_and_resolve(
    our_readings: &[(String, u128)],
    peer_registry: &PeerRegistry,
    _cfg: &RuntimeConfig,
) -> Vec<(String, u128)> {
    const DIVERGENCE_THRESHOLD_BPS: u64 = 100;

    // Collect peer readings
    let registry = peer_registry.lock().await;
    let mut peer_values: Vec<Vec<SpokeReading>> = Vec::new();

    for (_, peer_info) in registry.iter() {
        if !peer_info.active {
            continue;
        }
        let api_key = match &peer_info.outgoing_api_key {
            Some(k) => k.clone(),
            None => continue,
        };
        let peer_url = peer_info.url.clone();
        // Query peer spoke values (best effort)
        match query_peer_spoke_values(&peer_url, &api_key).await {
            Ok(readings) => {
                peer_values.push(readings);
            }
            Err(e) => {
                warn!(
                    peer_url = %peer_url,
                    error = %e,
                    "failed to query peer spoke values"
                );
            }
        }
    }
    drop(registry);

    let mut resolved: Vec<(String, u128)> = Vec::with_capacity(our_readings.len());

    for (name, our_value) in our_readings {
        let our_failed = *our_value <= 1; // read_all_spokes uses 1 as fallback for failures

        // Collect peer values for this spoke
        let peer_vals: Vec<u128> = peer_values
            .iter()
            .flat_map(|readings| {
                readings
                    .iter()
                    .filter(|r| r.spoke == *name && r.source != "failed" && r.value > 1)
                    .map(|r| r.value)
            })
            .collect();

        if our_failed && !peer_vals.is_empty() {
            // RPC fallback from peer
            let fallback_value = peer_vals[0]; // use first available peer value
            info!(
                spoke = %name,
                peer_value = fallback_value,
                "RPC fallback from peer"
            );
            resolved.push((name.clone(), fallback_value));
            continue;
        }

        // Check for divergence
        if !our_failed {
            for &pv in &peer_vals {
                let divergence = calculate_bps(*our_value, pv);
                if divergence > DIVERGENCE_THRESHOLD_BPS {
                    warn!(
                        spoke = %name,
                        our_value = our_value,
                        peer_value = pv,
                        divergence_bps = divergence,
                        "spoke value divergence with peer exceeds {} bps",
                        DIVERGENCE_THRESHOLD_BPS
                    );
                }
            }
        }

        resolved.push((name.clone(), *our_value));
    }

    resolved
}

/// Calculate absolute delta in basis points between two values.
fn calculate_bps(a: u128, b: u128) -> u64 {
    if a == 0 {
        return 0;
    }
    let diff = if b > a { b - a } else { a - b };
    let bps = (diff as u128)
        .checked_mul(10_000)
        .map(|n| n / a)
        .unwrap_or(u64::MAX as u128);
    if bps > u64::MAX as u128 {
        u64::MAX
    } else {
        bps as u64
    }
}

/// Peer sync loop: runs every 60s, polls peer /status, detects failover.
pub async fn run_peer_sync_loop(
    cfg: RuntimeConfig,
    peer_registry: PeerRegistry,
    notify: Arc<Notify>,
) {
    let interval = Duration::from_secs(60);
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            warn!(error = %e, "Failed to build HTTP client for peer sync loop");
            return;
        }
    };

    loop {
        tokio::time::sleep(interval).await;

        let registry = peer_registry.lock().await;
        let now = chrono::Utc::now().timestamp() as u64;

        let peer_urls: Vec<String> = registry.keys().cloned().collect();
        drop(registry);

        const SIX_HOURS_SECS: u64 = 6 * 60 * 60;

        for peer_url in &peer_urls {
            let url = format!("{}/status", peer_url.trim_end_matches('/'));
            match client.get(&url).send().await {
                Ok(resp) => {
                    if let Ok(status) = resp.json::<PeerStatus>().await {
                        let mut registry = peer_registry.lock().await;
                        if let Some(info) = registry.get_mut(peer_url) {
                            info.last_seen = Some(now);
                            if let Some(lca) = status.last_cycle_at {
                                info.last_push_at = Some(lca);
                            }
                        }
                    }
                }
                Err(e) => {
                    warn!(
                        peer_url = %peer_url,
                        error = %e,
                        "peer sync: failed to reach peer /status"
                    );
                }
            }
        }

        // Inactivity timeout: mark peers inactive if no successful poll for 6h
        {
            let mut registry = peer_registry.lock().await;
            for (url, info) in registry.iter_mut() {
                let stale = match info.last_seen {
                    Some(ls) => now.saturating_sub(ls) > SIX_HOURS_SECS,
                    None => now.saturating_sub(info.registered_at) > SIX_HOURS_SECS,
                };
                if stale && info.active {
                    info.active = false;
                    warn!(
                        peer_url = %url,
                        "peer marked inactive — no response for 6h, will re-register on next startup"
                    );
                }
            }
            // Evict inactive peers from the registry — they must re-register from scratch
            registry.retain(|_, v| v.active);
        }

        // Failover detection: if any peer's last_push_at is too old, wake scheduled loop
        // Only consider active peers
        let registry = peer_registry.lock().await;
        let failover_threshold = cfg.hub.update_interval_secs + 120;

        for (url, info) in registry.iter() {
            if let Some(last_push) = info.last_push_at {
                if now.saturating_sub(last_push) > failover_threshold {
                    warn!(
                        peer_url = %url,
                        last_push_at = last_push,
                        seconds_since_push = now.saturating_sub(last_push),
                        threshold = failover_threshold,
                        "peer appears stale -- triggering early update cycle"
                    );
                    notify.notify_one();
                    break; // one notification is enough
                }
            }
        }
    }
}
