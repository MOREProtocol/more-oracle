use axum::{extract::State, http::StatusCode, response::Json};
use serde::{Deserialize, Serialize};

use crate::spoke;

use super::super::{AppState, SpokeReading};
use super::update::{verify_auth_for, UpdateResponse};

#[derive(Serialize)]
pub(crate) struct PeerSpokeValuesResponse {
    readings: Vec<SpokeReading>,
    last_push_at: Option<u64>,
}

#[derive(Serialize)]
pub(crate) struct LiveSpokeReading {
    spoke: String,
    value: String,
    source: String, // "rpc" or "failed"
    at: u64,
}

#[derive(Serialize)]
pub(crate) struct LiveSpokeValuesResponse {
    readings: Vec<LiveSpokeReading>,
}

/// Unified auth body shared by spoke-value endpoints.
#[derive(Deserialize)]
pub struct AuthBody {
    pub wallet: String,
    pub signature: String,
}

pub async fn peer_spoke_values(
    State(app): State<AppState>,
    Json(body): Json<AuthBody>,
) -> Result<Json<PeerSpokeValuesResponse>, (StatusCode, Json<UpdateResponse>)> {
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
        "/peers/spoke-values",
    )
    .await?;

    let state = app.0.read().await;
    let readings = state.last_spoke_readings.clone();
    let last_push_at = state.last_cycle_at;

    Ok(Json(PeerSpokeValuesResponse { readings, last_push_at }))
}

pub async fn peer_spoke_values_live(
    State(app): State<AppState>,
    Json(body): Json<AuthBody>,
) -> Result<Json<LiveSpokeValuesResponse>, (StatusCode, Json<UpdateResponse>)> {
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
        "/peers/spoke-values/live",
    )
    .await?;

    let spokes = &app.10;
    let now = chrono::Utc::now().timestamp() as u64;
    let raw = spoke::read_all_spokes(spokes).await;

    let readings = raw
        .into_iter()
        .map(|(name, value)| {
            let source = if value == 1 { "failed" } else { "rpc" };
            LiveSpokeReading {
                spoke: name,
                value: value.to_string(),
                source: source.to_string(),
                at: now,
            }
        })
        .collect();

    Ok(Json(LiveSpokeValuesResponse { readings }))
}
