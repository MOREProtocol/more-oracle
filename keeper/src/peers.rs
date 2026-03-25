/// On startup, query all peer /status endpoints and calculate the optimal
/// startup sleep so this keeper fills the largest gap in the push schedule.
///
/// Algorithm:
/// 1. Query GET {peer}/status for each configured peer (parallel, 5s timeout)
/// 2. Filter peers that have the SAME update_interval as this keeper
/// 3. If no matching peers → return 0 (push immediately)
/// 4. Collect their last_cycle_at timestamps, map to cycle position (% interval)
/// 5. Find the largest gap in the circular schedule
/// 6. Return sleep duration = middle of largest gap - current position in cycle
use serde::Deserialize;
use std::time::Duration;
use tracing::{info, warn};

/// Minimal shape of the /status response we care about from a peer.
#[derive(Deserialize)]
struct PeerStatus {
    last_cycle_at: Option<u64>,
    update_interval_secs: Option<u64>,
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
                                    "Peer has different update_interval — ignoring"
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
/// `oracle_last_timestamp` is the value from `latestTimestamp()` on-chain —
/// it represents the last time *any* keeper pushed an update, used as an
/// additional position in the gap calculation.
pub async fn calculate_startup_sleep(
    peers: &[String],
    update_interval_secs: u64,
    oracle_last_timestamp: u64,
) -> Duration {
    if peers.is_empty() {
        info!("No peers configured — starting immediately");
        return Duration::ZERO;
    }

    // 1. Query all peers in parallel with 5 s timeout each
    let peer_last_cycles = query_peers(peers, update_interval_secs).await;

    // 2. If no matching peers respond → start immediately
    if peer_last_cycles.is_empty() {
        info!("No reachable peers with matching interval — starting immediately");
        return Duration::ZERO;
    }

    let interval = update_interval_secs;

    // 3. Collect cycle positions (seconds into the current interval window)
    // Include the oracle's last timestamp as a virtual "unknown peer" position
    let mut positions: Vec<u64> = peer_last_cycles
        .iter()
        .map(|ts| ts % interval)
        .collect();

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
