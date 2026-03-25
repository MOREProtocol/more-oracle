use axum::{
    extract::State,
    response::Json,
    routing::{get, post},
    Router,
};
use serde::Serialize;
use std::sync::Arc;
use tokio::sync::{Notify, RwLock};

/// Shared state between the HTTP API and the main keeper loop.
#[derive(Debug, Default)]
pub struct KeeperState {
    /// Unix timestamp of the last completed cycle.
    pub last_cycle_at: Option<u64>,
    /// Transaction hash of the last successful multicall batch.
    pub last_cycle_tx: Option<String>,
    /// Per-spoke oracle state, populated after each cycle.
    pub spoke_states: Vec<SpokeState>,
    /// Update interval in seconds — published so peers can match on it.
    pub update_interval_secs: u64,
}

/// State for a single spoke oracle as read from Flow EVM.
#[derive(Debug, Clone)]
pub struct SpokeState {
    pub name: String,
    pub oracle: String,
    pub stored_total_assets: u128,
    pub last_updated: u64,
    pub active: bool,
}

// ─── JSON response shapes ────────────────────────────────────────────────────

#[derive(Serialize)]
struct HealthResponse {
    status: &'static str,
}

#[derive(Serialize)]
struct SpokeStatusJson {
    name: String,
    oracle: String,
    stored_total_assets: String,
    last_updated: u64,
    seconds_since_update: u64,
    active: bool,
}

#[derive(Serialize)]
struct StatusResponse {
    spokes: Vec<SpokeStatusJson>,
    last_cycle_at: Option<u64>,
    last_cycle_tx: Option<String>,
    update_interval_secs: u64,
}

#[derive(Serialize)]
struct TriggerResponse {
    status: &'static str,
    message: &'static str,
}

// ─── Shared state type alias ─────────────────────────────────────────────────

pub type SharedState = Arc<RwLock<KeeperState>>;

// ─── Handlers ────────────────────────────────────────────────────────────────

async fn health() -> Json<HealthResponse> {
    Json(HealthResponse { status: "ok" })
}

async fn status(
    State((state, _notify)): State<(SharedState, Arc<Notify>)>,
) -> Json<StatusResponse> {
    let now = chrono::Utc::now().timestamp() as u64;
    let locked = state.read().await;

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

    Json(StatusResponse {
        spokes,
        last_cycle_at: locked.last_cycle_at,
        last_cycle_tx: locked.last_cycle_tx.clone(),
        update_interval_secs: locked.update_interval_secs,
    })
}

async fn trigger_update(
    State((_state, notify)): State<(SharedState, Arc<Notify>)>,
) -> Json<TriggerResponse> {
    notify.notify_one();
    Json(TriggerResponse {
        status: "triggered",
        message: "Update cycle triggered, check /status for result",
    })
}

// ─── Server entry point ───────────────────────────────────────────────────────

pub async fn start_server(port: u16, state: SharedState, notify: Arc<Notify>) {
    let app = Router::new()
        .route("/health", get(health))
        .route("/status", get(status))
        .route("/update", post(trigger_update))
        .with_state((state, notify));

    let addr = std::net::SocketAddr::from(([0, 0, 0, 0], port));
    tracing::info!(port, "HTTP API listening");

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .expect("Failed to bind HTTP API port");

    axum::serve(listener, app)
        .await
        .expect("HTTP API server error");
}
