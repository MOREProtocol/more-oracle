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
use crate::security::validate_peer_url;

/// Minimal shape of the /status response we care about from a peer.
#[derive(Deserialize)]
struct PeerStatus {
    last_cycle_at: Option<u64>,
    update_interval_secs: Option<u64>,
    #[serde(default)]
    #[allow(dead_code)]
    last_push_at: Option<u64>,
}

/// Response from GET /peers/challenge?wallet=...
#[derive(Deserialize)]
struct ChallengeResponse {
    challenge: String,
    #[allow(dead_code)]
    expires_at: u64,
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

/// Fetch a challenge from a peer for our wallet, then sign it.
/// Returns (wallet_hex, signature_hex).
async fn fetch_and_sign_challenge(
    client: &reqwest::Client,
    peer_base: &str,
    signer: &PrivateKeySigner,
) -> Result<(String, String)> {
    let our_wallet = signer.address();
    let challenge_url = format!(
        "{}/peers/challenge?wallet={our_wallet:#x}",
        peer_base.trim_end_matches('/')
    );

    let resp = client
        .get(&challenge_url)
        .send()
        .await
        .with_context(|| format!("GET {challenge_url} failed"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        eyre::bail!("GET {challenge_url} returned {status}: {body}");
    }

    let cr: ChallengeResponse = resp
        .json()
        .await
        .context("failed to parse challenge response")?;

    let sig = signer
        .sign_message(cr.challenge.as_bytes())
        .await
        .context("failed to sign challenge")?;

    let sig_hex = format!("0x{}", alloy::hex::encode(sig.as_bytes()));
    Ok((format!("{our_wallet:#x}"), sig_hex))
}

/// Query all configured peer POST /status endpoints in parallel (5 s timeout each).
/// Returns only peers whose `update_interval_secs` matches ours.
async fn query_peers(peers: &[String], update_interval_secs: u64, signer: &PrivateKeySigner) -> Vec<u64> {
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

    let mut results = Vec::new();
    for peer_url in peers {
        if let Err(reason) = validate_peer_url(peer_url) {
            warn!(peer_url = %peer_url, reason = %reason, "query_peers: skipping invalid URL");
            continue;
        }
        let base = peer_url.trim_end_matches('/');
        let (wallet_hex, sig_hex) = match fetch_and_sign_challenge(&client, base, signer).await {
            Ok(pair) => pair,
            Err(e) => {
                warn!(peer = %peer_url, error = %e, "query_peers: failed to get challenge");
                continue;
            }
        };
        let url = format!("{base}/status");
        let body = serde_json::json!({ "wallet": wallet_hex, "signature": sig_hex });
        match client.post(&url).json(&body).send().await {
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
                        continue;
                    }
                    if let Some(ts) = status.last_cycle_at {
                        results.push(ts);
                    }
                }
                Err(e) => warn!(peer = %peer_url, error = %e, "Failed to parse peer /status response"),
            },
            Err(e) => warn!(peer = %peer_url, error = %e, "Failed to reach peer /status"),
        }
    }
    results
}

/// On startup, query all peer POST /status endpoints and calculate the optimal
/// startup sleep so this keeper fills the largest gap in the push schedule.
pub async fn calculate_startup_sleep(
    peers: &[String],
    update_interval_secs: u64,
    oracle_last_timestamp: u64,
    signer: &PrivateKeySigner,
) -> Duration {
    if peers.is_empty() {
        info!("No peers configured -- starting immediately");
        return Duration::ZERO;
    }

    let peer_last_cycles = query_peers(peers, update_interval_secs, signer).await;

    if peer_last_cycles.is_empty() {
        info!("No reachable peers with matching interval -- starting immediately");
        return Duration::ZERO;
    }

    let interval = update_interval_secs;

    let mut positions: Vec<u64> = peer_last_cycles.iter().map(|ts| ts % interval).collect();
    if oracle_last_timestamp > 0 {
        positions.push(oracle_last_timestamp % interval);
    }

    positions.sort_unstable();
    positions.dedup();

    info!(
        positions = ?positions,
        interval,
        "Peer cycle positions (seconds into interval)"
    );

    let n = positions.len();
    let mut largest_gap_size: u64 = 0;
    let mut largest_gap_start: u64 = 0;

    for i in 0..n {
        let gap_start = positions[i];
        let gap_end = if i + 1 < n {
            positions[i + 1]
        } else {
            positions[0] + interval
        };
        let gap_size = gap_end.saturating_sub(gap_start);
        if gap_size > largest_gap_size {
            largest_gap_size = gap_size;
            largest_gap_start = gap_start;
        }
    }

    let target = (largest_gap_start + largest_gap_size / 2) % interval;
    let now_secs = chrono::Utc::now().timestamp() as u64;
    let current_position = now_secs % interval;

    let sleep_secs = if target >= current_position {
        target - current_position
    } else {
        interval - current_position + target
    };

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

// ── Peer coordination ─────────────────────────────────────────────────────────

/// Register this keeper with a remote peer using the new unified challenge-response flow.
///
/// 1. GET {peer_url}/peers/challenge?wallet={our_wallet} — check whitelist + get challenge
/// 2. Sign challenge with KEEPER_PRIVATE_KEY (EIP-191)
/// 3. POST {peer_url}/peers/register with { url, wallet, signature }
///
/// Returns () on success (no API key in new model).
pub async fn register_with_peer(
    peer_url: &str,
    our_url: &str,
    signer: &PrivateKeySigner,
    _batch_updater: Address,
    _flow_rpc: &str,
) -> Result<()> {
    // Validate the peer URL before making any request
    validate_peer_url(peer_url).map_err(|e| eyre::eyre!(e))?;

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .context("failed to build HTTP client")?;

    let base = peer_url.trim_end_matches('/');

    // Step 1: Fetch challenge and sign it
    let (wallet_hex, sig_hex) = fetch_and_sign_challenge(&client, base, signer)
        .await
        .with_context(|| format!("failed to obtain challenge from {base}"))?;

    // Step 2: POST /peers/register
    let register_url = format!("{base}/peers/register");
    let register_body = serde_json::json!({
        "url": our_url,
        "wallet": wallet_hex,
        "signature": sig_hex,
    });

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

    info!(peer_url = %peer_url, "peer registration succeeded");
    Ok(())
}

/// Attempt registration with all configured peers on startup.
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
        // Validate URL before attempting
        if let Err(reason) = validate_peer_url(peer_url) {
            warn!(peer_url = %peer_url, reason = %reason, "skipping peer with invalid URL");
            continue;
        }

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
            Ok(()) => {
                let now = chrono::Utc::now().timestamp() as u64;
                let mut registry = peer_registry.lock().await;
                registry
                    .entry(peer_url.clone())
                    .and_modify(|info| {
                        info.last_seen = Some(now);
                        info.active = true;
                    })
                    .or_insert_with(|| PeerInfo {
                        url: peer_url.clone(),
                        wallet: signer.address(), // approximate — peer may differ
                        registered_at: now,
                        last_seen: Some(now),
                        last_push_at: None,
                        active: true,
                    });
                info!(peer_url = %peer_url, "peer registration succeeded");
                if let Some(tg) = &cfg.telegram {
                    tg.peer_connected(peer_url);
                }
            }
            Err(e) => {
                warn!(peer_url = %peer_url, error = %e, "peer registration failed");
            }
        }
    }
}

/// Query a peer's spoke values using unified challenge-response auth.
pub async fn query_peer_spoke_values(
    peer_url: &str,
    signer: &PrivateKeySigner,
) -> Result<Vec<SpokeReading>> {
    validate_peer_url(peer_url).map_err(|e| eyre::eyre!(e))?;

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .context("failed to build HTTP client")?;

    let base = peer_url.trim_end_matches('/');

    // Step 1: Fetch challenge from /peers/challenge
    let (wallet_hex, sig_hex) = fetch_and_sign_challenge(&client, base, signer)
        .await
        .with_context(|| format!("failed to obtain challenge from {base}"))?;

    // Step 2: POST /peers/spoke-values/live with auth body
    let url = format!("{base}/peers/spoke-values/live");
    let body = serde_json::json!({
        "wallet": wallet_hex,
        "signature": sig_hex,
    });

    let resp = client
        .post(&url)
        .json(&body)
        .send()
        .await
        .with_context(|| format!("POST {url} failed"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        eyre::bail!("POST {url} returned {status}: {body}");
    }

    let parsed: LiveSpokeValuesResponse = resp
        .json()
        .await
        .context("failed to parse live spoke-values response")?;

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
pub async fn cross_validate_and_resolve(
    our_readings: &[(String, u128, bool)],
    peer_registry: &PeerRegistry,
    cfg: &RuntimeConfig,
) -> Vec<(String, u128)>  {
    const DIVERGENCE_THRESHOLD_BPS: u64 = 100;

    // Build signer from config for peer requests
    let signer = match std::str::FromStr::from_str(&cfg.keeper_private_key) {
        Ok(s) => s,
        Err(e) => {
            warn!(error = %e, "cross_validate: invalid KEEPER_PRIVATE_KEY, skipping peer queries");
            return our_readings.iter().map(|(n, v, _)| (n.clone(), *v)).collect();
        }
    };
    let signer: PrivateKeySigner = signer;

    // Collect active peer URLs
    let peer_urls: Vec<String> = {
        let registry = peer_registry.lock().await;
        registry
            .values()
            .filter(|p| p.active)
            .map(|p| p.url.clone())
            .collect()
    };

    let mut peer_values: Vec<Vec<SpokeReading>> = Vec::new();

    for peer_url in &peer_urls {
        match query_peer_spoke_values(peer_url, &signer).await {
            Ok(readings) => {
                peer_values.push(readings);
            }
            Err(e) => {
                warn!(peer_url = %peer_url, error = %e, "failed to query peer spoke values");
            }
        }
    }

    let mut resolved: Vec<(String, u128)> = Vec::with_capacity(our_readings.len());

    for (name, our_value, rpc_failed) in our_readings {
        let our_failed = *rpc_failed;

        // Peer values with real assets (source "rpc" or "peer_fallback", value > 1)
        let peer_vals: Vec<u128> = peer_values
            .iter()
            .flat_map(|readings: &Vec<SpokeReading>| {
                readings
                    .iter()
                    .filter(|r| r.spoke == *name && r.source != "failed" && r.value > 1)
                    .map(|r| r.value)
            })
            .collect();

        if our_failed && !peer_vals.is_empty() {
            let fallback_value = peer_vals[0];
            info!(spoke = %name, peer_value = fallback_value, "RPC fallback from peer");
            resolved.push((name.clone(), fallback_value));
            continue;
        }

        // Alert only when both keepers had a real RPC failure — not when the vault
        // is simply empty (peer source "empty" means the peer read 0 successfully).
        let peer_confirmed_empty = peer_values.iter().any(|readings| {
            readings.iter().any(|r| r.spoke == *name && r.source == "empty")
        });
        if our_failed && peer_vals.is_empty() && !peer_urls.is_empty() && !peer_confirmed_empty {
            warn!(spoke = %name, "both keepers failed to read spoke — no reliable value");
            if let Some(tg) = &cfg.telegram {
                tg.both_keepers_failed(name);
            }
        }

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
                    if let Some(tg) = &cfg.telegram {
                        tg.peer_divergence(name, *our_value, pv, divergence);
                    }
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

/// Peer sync loop: runs every 60s, polls peer POST /status, detects failover.
pub async fn run_peer_sync_loop(
    cfg: RuntimeConfig,
    peer_registry: PeerRegistry,
    notify: Arc<Notify>,
    signer: PrivateKeySigner,
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

        let peer_urls: Vec<String> = {
            let registry = peer_registry.lock().await;
            registry.keys().cloned().collect()
        };

        let now = chrono::Utc::now().timestamp() as u64;
        const SIX_HOURS_SECS: u64 = 6 * 60 * 60;

        for peer_url in &peer_urls {
            if let Err(reason) = validate_peer_url(peer_url) {
                warn!(peer_url = %peer_url, reason = %reason, "peer sync: skipping invalid URL");
                continue;
            }

            let base = peer_url.trim_end_matches('/');
            let auth = fetch_and_sign_challenge(&client, base, &signer).await;
            let (wallet_hex, sig_hex) = match auth {
                Ok(pair) => pair,
                Err(e) => {
                    warn!(peer_url = %peer_url, error = %e, "peer sync: failed to get challenge for /status");
                    continue;
                }
            };
            let url = format!("{base}/status");
            let body = serde_json::json!({ "wallet": wallet_hex, "signature": sig_hex });
            match client.post(&url).json(&body).send().await {
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
                    warn!(peer_url = %peer_url, error = %e, "peer sync: failed to reach peer /status");
                }
            }
        }

        // Inactivity timeout: mark peers inactive after 6h, then evict
        {
            let mut registry = peer_registry.lock().await;
            for (url, info) in registry.iter_mut() {
                let stale = match info.last_seen {
                    Some(ls) => now.saturating_sub(ls) > SIX_HOURS_SECS,
                    None => now.saturating_sub(info.registered_at) > SIX_HOURS_SECS,
                };
                if stale && info.active {
                    info.active = false;
                    warn!(peer_url = %url, "peer marked inactive — no response for 6h");
                    if let Some(tg) = &cfg.telegram {
                        tg.peer_evicted(url);
                    }
                }
            }
            registry.retain(|_, v| v.active);
        }

        // Failover detection
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
                    break;
                }
            }
        }
    }
}
