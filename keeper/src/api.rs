use alloy::primitives::{Address, Signature};
use axum::{
    extract::State,
    http::StatusCode,
    response::Json,
    routing::{get, post},
    Router,
};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, sync::Arc};
use tokio::sync::{Mutex, Notify, RwLock};

/// Shared state between the HTTP API and the main keeper loop.
#[derive(Debug, Default)]
pub struct KeeperState {
    pub last_cycle_at: Option<u64>,
    pub last_cycle_tx: Option<String>,
    pub spoke_states: Vec<SpokeState>,
    pub update_interval_secs: u64,
}

#[derive(Debug, Clone)]
pub struct SpokeState {
    pub name: String,
    pub oracle: String,
    pub stored_total_assets: u128,
    pub last_updated: u64,
    pub active: bool,
}

// ─── JSON shapes ─────────────────────────────────────────────────────────────

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
struct ChallengeResponse {
    /// The exact string the curator must sign with personal_sign (EIP-191)
    challenge: String,
    /// Unix timestamp when this challenge expires
    expires_at: u64,
}

#[derive(Deserialize)]
pub struct UpdateRequest {
    /// The challenge string returned by GET /challenge
    pub challenge: String,
    /// EIP-191 personal_sign signature of the challenge (0x-prefixed)
    pub signature: String,
}

#[derive(Serialize)]
struct UpdateResponse {
    status: &'static str,
    message: String,
}

// ─── Shared state type aliases ────────────────────────────────────────────────

pub type SharedState = Arc<RwLock<KeeperState>>;

/// In-memory challenge store: challenge string → expires_at (unix secs)
pub type ChallengeStore = Arc<Mutex<HashMap<String, u64>>>;

// Challenge TTL in seconds
const CHALLENGE_TTL_SECS: u64 = 300;

// ─── Handlers ────────────────────────────────────────────────────────────────

async fn health() -> Json<HealthResponse> {
    Json(HealthResponse { status: "ok" })
}

async fn status(
    State((state, _notify, _curator, _challenges)): State<(SharedState, Arc<Notify>, Address, ChallengeStore)>,
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

/// GET /challenge — returns a server-generated one-time challenge for the curator to sign.
async fn get_challenge(
    State((_state, _notify, _curator, challenges)): State<(SharedState, Arc<Notify>, Address, ChallengeStore)>,
) -> Json<ChallengeResponse> {
    let nonce = uuid::Uuid::new_v4().to_string();
    let challenge = format!("update:{nonce}");
    let expires_at = chrono::Utc::now().timestamp() as u64 + CHALLENGE_TTL_SECS;

    // Purge expired challenges while we have the lock
    let mut store = challenges.lock().await;
    let now = chrono::Utc::now().timestamp() as u64;
    store.retain(|_, exp| *exp > now);
    store.insert(challenge.clone(), expires_at);

    Json(ChallengeResponse { challenge, expires_at })
}

/// POST /update — curator signs the challenge from GET /challenge and submits it here.
async fn trigger_update(
    State((_state, notify, curator, challenges)): State<(SharedState, Arc<Notify>, Address, ChallengeStore)>,
    Json(body): Json<UpdateRequest>,
) -> Result<Json<UpdateResponse>, (StatusCode, Json<UpdateResponse>)> {
    let now = chrono::Utc::now().timestamp() as u64;

    // 1. Look up and consume the challenge (one-time use)
    {
        let mut store = challenges.lock().await;
        match store.remove(&body.challenge) {
            None => {
                return Err((
                    StatusCode::UNAUTHORIZED,
                    Json(UpdateResponse {
                        status: "error",
                        message: "unknown or already-used challenge".into(),
                    }),
                ));
            }
            Some(expires_at) if now > expires_at => {
                return Err((
                    StatusCode::UNAUTHORIZED,
                    Json(UpdateResponse {
                        status: "error",
                        message: "challenge expired".into(),
                    }),
                ));
            }
            Some(_) => {} // valid
        }
    }

    // 2. Parse signature
    let sig: Signature = body.signature.parse().map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            Json(UpdateResponse {
                status: "error",
                message: "invalid signature format".into(),
            }),
        )
    })?;

    // 3. Recover signer via EIP-191 personal_sign hash
    let recovered = sig.recover_address_from_msg(body.challenge.as_bytes()).map_err(|_| {
        (
            StatusCode::UNAUTHORIZED,
            Json(UpdateResponse {
                status: "error",
                message: "could not recover signer from signature".into(),
            }),
        )
    })?;

    // 4. Check recovered address matches curator
    if recovered != curator {
        tracing::warn!(
            recovered = %recovered,
            curator = %curator,
            "unauthorized /update attempt"
        );
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(UpdateResponse {
                status: "error",
                message: format!("signer {recovered} is not the vault curator"),
            }),
        ));
    }

    tracing::info!(curator = %curator, "curator-signed update triggered");
    notify.notify_one();

    Ok(Json(UpdateResponse {
        status: "triggered",
        message: "update cycle triggered".into(),
    }))
}

// ─── Server entry point ───────────────────────────────────────────────────────

pub async fn start_server(
    port: u16,
    state: SharedState,
    notify: Arc<Notify>,
    curator: Address,
) {
    let challenges: ChallengeStore = Arc::new(Mutex::new(HashMap::new()));

    let app = Router::new()
        .route("/health", get(health))
        .route("/status", get(status))
        .route("/challenge", get(get_challenge))
        .route("/update", post(trigger_update))
        .with_state((state, notify, curator, challenges));

    let addr = std::net::SocketAddr::from(([0, 0, 0, 0], port));
    tracing::info!(port, "HTTP API listening");

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .expect("Failed to bind HTTP API port");

    axum::serve(listener, app)
        .await
        .expect("HTTP API server error");
}
