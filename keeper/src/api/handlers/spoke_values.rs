use axum::{extract::State, http::StatusCode, response::Json};
use serde::Serialize;

use crate::spoke;

use super::super::{AppState, SpokeReading};
use super::update::UpdateResponse;

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

pub async fn peer_spoke_values(
    State(app): State<AppState>,
    headers: axum::http::HeaderMap,
) -> Result<Json<PeerSpokeValuesResponse>, (StatusCode, Json<UpdateResponse>)> {
    // Authenticate via X-Keeper-Key header
    let api_key = headers
        .get("X-Keeper-Key")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| {
            (
                StatusCode::UNAUTHORIZED,
                Json(UpdateResponse {
                    status: "error",
                    message: "missing X-Keeper-Key header".into(),
                }),
            )
        })?;

    // Check if this key matches any registered peer's incoming_api_key
    let registry = app.4.lock().await;
    let is_valid = registry
        .values()
        .any(|p| p.incoming_api_key == api_key);

    if !is_valid {
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(UpdateResponse {
                status: "error",
                message: "invalid API key".into(),
            }),
        ));
    }
    drop(registry);

    let state = app.0.read().await;
    let readings = state.last_spoke_readings.clone();
    let last_push_at = state.last_cycle_at;

    Ok(Json(PeerSpokeValuesResponse {
        readings,
        last_push_at,
    }))
}

pub async fn peer_spoke_values_live(
    State(app): State<AppState>,
    headers: axum::http::HeaderMap,
) -> Result<Json<LiveSpokeValuesResponse>, (StatusCode, Json<UpdateResponse>)> {
    // Authenticate via X-Keeper-Key header
    let api_key = headers
        .get("X-Keeper-Key")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| {
            (
                StatusCode::UNAUTHORIZED,
                Json(UpdateResponse {
                    status: "error",
                    message: "missing X-Keeper-Key header".into(),
                }),
            )
        })?;

    let registry = app.4.lock().await;
    let is_valid = registry.values().any(|p| p.incoming_api_key == api_key);
    drop(registry);

    if !is_valid {
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(UpdateResponse {
                status: "error",
                message: "invalid API key".into(),
            }),
        ));
    }

    let spokes = &app.10;
    let now = chrono::Utc::now().timestamp() as u64;
    let raw = spoke::read_all_spokes(spokes).await;

    let readings = raw
        .into_iter()
        .map(|(name, value)| {
            // read_all_spokes returns 1 as sentinel for failed/zero reads
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
