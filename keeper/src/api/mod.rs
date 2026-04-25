mod handlers;

#[cfg(test)]
mod tests;

#[cfg(test)]
mod security_tests;

use alloy::primitives::Address;
use axum::{
    body::Body,
    http::{header, HeaderValue, Request},
    middleware::{self, Next},
    response::Response,
    routing::{get, post},
    Router,
};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, sync::Arc};
use tokio::sync::{Mutex, Notify, RwLock};
use tower::ServiceBuilder;
use tower_http::{limit::RequestBodyLimitLayer, timeout::TimeoutLayer};

use crate::config::SpokeConfig;
use crate::peer_registry::{PeerRegistry, PendingRegistrations};
use crate::security::WhitelistCache;
use crate::telegram::TelegramNotifier;

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

/// Keyed by challenge string -> expires_at. Used for the legacy curator flow.
pub type ChallengeStore = Arc<Mutex<HashMap<String, u64>>>;

/// Keyed by wallet address hex -> (challenge, expires_at). Used by all authenticated endpoints.
pub type WalletChallengeStore = Arc<Mutex<HashMap<String, (String, u64)>>>;

/// State for an active bridge warning on a specific spoke.
/// Stored keyed by spoke name in `BridgeWarningStore`.
#[derive(Debug, Clone)]
pub struct BridgeWarning {
    pub spoke_name: String,
    /// storedTotalAssets at the time the warning was issued.
    pub pre_bridge_value: u128,
    /// Expected absolute token delta from the bridge (e.g. 480_000_000 USDC-raw).
    pub expected_delta: u128,
    /// Unix timestamp after which the warning auto-expires regardless of delta.
    pub timeout_at: u64,
    /// The oracle's maxChangeBps before we set it to 0. None if we didn't touch it.
    pub original_max_change_bps: Option<u64>,
}

/// Keyed by spoke name -> active bridge warning.
pub type BridgeWarningStore = Arc<Mutex<HashMap<String, BridgeWarning>>>;

pub type AppState = (
    SharedState,                                     // 0
    Arc<Notify>,                                     // 1
    Address,                                         // 2 curator (for POST /update)
    ChallengeStore,                                  // 3
    PeerRegistry,                                    // 4
    PendingRegistrations,                            // 5
    Address,                                         // 6 batch_updater
    String,                                          // 7 flow_rpc URL
    Option<String>,                                  // 8 this keeper's own URL
    Option<alloy::signers::local::PrivateKeySigner>, // 9 keeper signer
    Vec<SpokeConfig>,                                // 10
    WalletChallengeStore,                            // 11
    WhitelistCache,                                  // 12
    Option<TelegramNotifier>,                        // 13
    BridgeWarningStore,                              // 14
    Option<alloy::signers::local::PrivateKeySigner>, // 15 oracle owner signer (optional)
);

pub(crate) const CHALLENGE_TTL_SECS: u64 = 300;
pub(crate) const PEER_CHALLENGE_TTL_SECS: u64 = 60;
pub(crate) const WALLET_CHALLENGE_TTL_SECS: u64 = 120;
/// Bridge warnings auto-expire after 6 hours if the expected delta is never detected.
pub(crate) const BRIDGE_WARNING_TIMEOUT_SECS: u64 = 21_600;

/// Axum middleware that adds security headers to every response.
async fn security_headers(request: Request<Body>, next: Next) -> Response {
    let mut response = next.run(request).await;
    let headers = response.headers_mut();

    headers.insert(
        header::HeaderName::from_static("x-content-type-options"),
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::HeaderName::from_static("x-frame-options"),
        HeaderValue::from_static("DENY"),
    );
    headers.insert(
        header::HeaderName::from_static("content-security-policy"),
        HeaderValue::from_static("default-src 'none'"),
    );
    headers.insert(
        header::HeaderName::from_static("cache-control"),
        HeaderValue::from_static("no-store"),
    );
    // Note: HSTS is meaningful only over TLS; we include it so that
    // any TLS-terminating proxy forwards it correctly.
    headers.insert(
        header::HeaderName::from_static("strict-transport-security"),
        HeaderValue::from_static("max-age=31536000; includeSubDomains"),
    );

    response
}

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
    telegram: Option<TelegramNotifier>,
    bridge_warnings: BridgeWarningStore,
    oracle_owner_signer: Option<alloy::signers::local::PrivateKeySigner>,
) {
    let challenges: ChallengeStore = Arc::new(Mutex::new(HashMap::new()));
    let wallet_challenges: WalletChallengeStore = Arc::new(Mutex::new(HashMap::new()));
    let whitelist_cache = WhitelistCache::new();

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
        wallet_challenges,
        whitelist_cache,
        telegram,
        bridge_warnings,
        oracle_owner_signer,
    );

    let app = Router::new()
        .route("/health", get(handlers::health))
        .route("/challenge", get(handlers::get_challenge))
        .route("/peers/challenge", get(handlers::get_peer_challenge))
        .route("/status", post(handlers::status))
        .route("/update", post(handlers::trigger_update))
        .route("/peers/register", post(handlers::peer_register))
        .route("/peers/verify", post(handlers::peer_verify))
        .route("/peers/spoke-values", post(handlers::peer_spoke_values))
        .route("/peers/spoke-values/live", post(handlers::peer_spoke_values_live))
        .route("/peers/notify", post(handlers::peer_notify))
        .route("/curator/bridge-challenge", get(handlers::get_bridge_challenge))
        .route("/curator/bridge-warning", post(handlers::curator_bridge_warning))
        .route("/peers/bridge-warning", post(handlers::peer_bridge_warning))
        .with_state(app_state)
        .layer(
            ServiceBuilder::new()
                // Security headers on every response
                .layer(middleware::from_fn(security_headers))
                // Request body size limit: 64 KiB
                .layer(RequestBodyLimitLayer::new(65_536))
                // Overall request timeout: 30 seconds
                .layer(TimeoutLayer::with_status_code(
                    axum::http::StatusCode::REQUEST_TIMEOUT,
                    std::time::Duration::from_secs(30),
                ))
                // Global concurrency cap: shed load when >256 in-flight requests.
                // Acts as a simple circuit breaker against request storms.
                .layer(tower::limit::ConcurrencyLimitLayer::new(256)),
        );

    let addr = std::net::SocketAddr::from(([0, 0, 0, 0], port));
    tracing::info!(port, "HTTP API listening");

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .expect("Failed to bind HTTP API port");

    axum::serve(listener, app)
        .await
        .expect("HTTP API server error");
}
