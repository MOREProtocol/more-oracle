use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Json;
use serde::{Deserialize, Serialize};

use super::super::{AppState, PeerStatusJson};
use super::update::{verify_auth_for, UpdateResponse};

#[derive(Serialize)]
pub(crate) struct HealthResponse {
    status: &'static str,
}

#[derive(Serialize)]
pub(crate) struct SpokeStatusJson {
    name: String,
    oracle: String,
    stored_total_assets: String,
    last_updated: u64,
    seconds_since_update: u64,
    active: bool,
}

#[derive(Serialize)]
pub(crate) struct StatusResponse {
    spokes: Vec<SpokeStatusJson>,
    last_cycle_at: Option<u64>,
    last_cycle_tx: Option<String>,
    update_interval_secs: u64,
    peers: Vec<PeerStatusJson>,
}

#[derive(Deserialize)]
pub struct StatusRequest {
    pub wallet: String,
    pub signature: String,
}

pub async fn health() -> Json<HealthResponse> {
    Json(HealthResponse { status: "ok" })
}

/// POST /status — returns full keeper status.
///
/// Requires unified challenge-response auth: `{ wallet, signature }`.
/// The wallet must be whitelisted on OracleBatchUpdater.
pub async fn status(
    State(app): State<AppState>,
    Json(body): Json<StatusRequest>,
) -> Result<Json<StatusResponse>, (StatusCode, Json<UpdateResponse>)> {
    let batch_updater = app.6;
    let flow_rpc = &app.7;
    let whitelist_cache = &app.12;

    verify_auth_for(
        &body.wallet,
        &body.signature,
        &app.11,
        batch_updater,
        flow_rpc,
        whitelist_cache,
        "/status",
    )
    .await?;

    let now = chrono::Utc::now().timestamp() as u64;
    let locked = app.0.read().await;

    let spokes = locked
        .spoke_states
        .iter()
        .map(|s| SpokeStatusJson {
            name: s.name.clone(),
            oracle: s.oracle.clone(),
            stored_total_assets: s.stored_total_assets.to_string(),
            last_updated: s.last_updated,
            seconds_since_update: now.saturating_sub(s.last_updated),
            active: s.active,
        })
        .collect();

    let registry = app.4.lock().await;
    let peers_json: Vec<PeerStatusJson> = registry
        .values()
        .map(|p| {
            let healthy = p.active && p.last_seen.map_or(false, |ls| now.saturating_sub(ls) < 120);
            PeerStatusJson {
                url: p.url.clone(),
                wallet: format!("{:#x}", p.wallet),
                last_seen: p.last_seen,
                last_push_at: p.last_push_at,
                healthy,
            }
        })
        .collect();

    Ok(Json(StatusResponse {
        spokes,
        last_cycle_at: locked.last_cycle_at,
        last_cycle_tx: locked.last_cycle_tx.clone(),
        update_interval_secs: locked.update_interval_secs,
        peers: peers_json,
    }))
}
