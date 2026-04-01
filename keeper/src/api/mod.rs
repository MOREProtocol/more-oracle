mod handlers;

#[cfg(test)]
mod tests;

use alloy::primitives::Address;
use axum::{
    routing::{get, post},
    Router,
};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, sync::Arc};
use tokio::sync::{Mutex, Notify, RwLock};

use crate::config::SpokeConfig;
use crate::peer_registry::{PeerRegistry, PendingRegistrations};

#[derive(Debug, Default)]
pub struct KeeperState {
    pub last_cycle_at: Option<u64>,
    pub last_cycle_tx: Option<String>,
    pub spoke_states: Vec<SpokeState>,
    pub update_interval_secs: u64,
    pub last_spoke_readings: Vec<SpokeReading>,
}

#[derive(Debug, Clone)]
pub struct SpokeState {
    pub name: String,
    pub oracle: String,
    pub stored_total_assets: u128,
    pub last_updated: u64,
    pub active: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpokeReading {
    pub spoke: String,
    pub value: u128,
    pub source: String,
    pub at: u64,
}

#[derive(Serialize)]
pub struct PeerStatusJson {
    pub url: String,
    pub wallet: String,
    pub last_seen: Option<u64>,
    pub last_push_at: Option<u64>,
    pub healthy: bool,
}

pub type SharedState = Arc<RwLock<KeeperState>>;

pub type ChallengeStore = Arc<Mutex<HashMap<String, u64>>>;

pub type AppState = (
    SharedState,
    Arc<Notify>,
    Address,
    ChallengeStore,
    PeerRegistry,
    PendingRegistrations,
    Address,
    String,
    Option<String>,
    Option<alloy::signers::local::PrivateKeySigner>,
    Vec<SpokeConfig>,
);

pub(crate) const CHALLENGE_TTL_SECS: u64 = 300;

pub(crate) const PEER_CHALLENGE_TTL_SECS: u64 = 60;

pub async fn start_server(
    port: u16,
    state: SharedState,
    notify: Arc<Notify>,
    curator: Address,
    peer_registry: PeerRegistry,
    pending_registrations: PendingRegistrations,
    batch_updater: Address,
    flow_rpc: String,
    keeper_url: Option<String>,
    signer: Option<alloy::signers::local::PrivateKeySigner>,
    spokes: Vec<SpokeConfig>,
) {
    let challenges: ChallengeStore = Arc::new(Mutex::new(HashMap::new()));

    let app_state: AppState = (
        state,
        notify,
        curator,
        challenges,
        peer_registry,
        pending_registrations,
        batch_updater,
        flow_rpc,
        keeper_url,
        signer,
        spokes,
    );

    let app = Router::new()
        .route("/health", get(handlers::health))
        .route("/status", get(handlers::status))
        .route("/challenge", get(handlers::get_challenge))
        .route("/update", post(handlers::trigger_update))
        .route("/peers/register", post(handlers::peer_register))
        .route("/peers/verify", post(handlers::peer_verify))
        .route("/peers/spoke-values", get(handlers::peer_spoke_values))
        .route("/peers/spoke-values/live", get(handlers::peer_spoke_values_live))
        .route("/peers/notify", post(handlers::peer_notify))
        .with_state(app_state);

    let addr = std::net::SocketAddr::from(([0, 0, 0, 0], port));
    tracing::info!(port, "HTTP API listening");

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .expect("Failed to bind HTTP API port");

    axum::serve(listener, app)
        .await
        .expect("HTTP API server error");
}
