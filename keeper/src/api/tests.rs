use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{Mutex, Notify, RwLock};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use tower::ServiceExt as _;

use crate::api::{AppState, ChallengeStore, KeeperState, SharedState, WalletChallengeStore};
use crate::peer_registry::{new_peer_registry, new_pending_registrations, PeerInfo};
use crate::security::WhitelistCache;
use alloy::primitives::Address;

fn build_test_app() -> Router {
    let state: SharedState = Arc::new(RwLock::new(KeeperState::default()));
    let notify = Arc::new(Notify::new());
    let curator = Address::ZERO;
    let challenges: ChallengeStore = Arc::new(Mutex::new(HashMap::new()));
    let peer_registry = new_peer_registry();
    let pending_registrations = new_pending_registrations();
    let batch_updater = Address::ZERO;
    let flow_rpc = "http://localhost:8545".to_string();
    let keeper_url: Option<String> = None;
    let signer: Option<alloy::signers::local::PrivateKeySigner> = None;
    let spokes: Vec<crate::config::SpokeConfig> = vec![];
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
        None,
    );

    Router::new()
        .route("/health", axum::routing::get(super::handlers::health))
        .route("/status", axum::routing::post(super::handlers::status))
        .route("/challenge", axum::routing::get(super::handlers::get_challenge))
        .route("/peers/challenge", axum::routing::get(super::handlers::get_peer_challenge))
        .route("/update", axum::routing::post(super::handlers::trigger_update))
        .route("/peers/register", axum::routing::post(super::handlers::peer_register))
        .route("/peers/verify", axum::routing::post(super::handlers::peer_verify))
        .route("/peers/spoke-values", axum::routing::post(super::handlers::peer_spoke_values))
        .route("/peers/spoke-values/live", axum::routing::post(super::handlers::peer_spoke_values_live))
        .route("/peers/notify", axum::routing::post(super::handlers::peer_notify))
        .with_state(app_state)
}

fn build_test_app_with_challenge(wallet: Address, challenge: &str) -> Router {
    let state: SharedState = Arc::new(RwLock::new(KeeperState::default()));
    let notify = Arc::new(Notify::new());
    let curator = Address::ZERO;
    let challenges: ChallengeStore = Arc::new(Mutex::new(HashMap::new()));
    let peer_registry = new_peer_registry();
    let pending_registrations = new_pending_registrations();
    let batch_updater = Address::ZERO;
    let flow_rpc = "http://localhost:8545".to_string();
    let keeper_url: Option<String> = None;
    let signer: Option<alloy::signers::local::PrivateKeySigner> = None;
    let spokes: Vec<crate::config::SpokeConfig> = vec![];
    let wallet_challenges: WalletChallengeStore = Arc::new(Mutex::new(HashMap::new()));
    let whitelist_cache = WhitelistCache::new();

    {
        let mut guard = wallet_challenges.try_lock().expect("lock during setup");
        let expires_at = chrono::Utc::now().timestamp() as u64 + 300;
        guard.insert(
            format!("{wallet:#x}"),
            (challenge.to_string(), expires_at),
        );
    }

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
        None,
    );

    Router::new()
        .route("/health", axum::routing::get(super::handlers::health))
        .route("/status", axum::routing::post(super::handlers::status))
        .route("/challenge", axum::routing::get(super::handlers::get_challenge))
        .route("/peers/challenge", axum::routing::get(super::handlers::get_peer_challenge))
        .route("/update", axum::routing::post(super::handlers::trigger_update))
        .route("/peers/register", axum::routing::post(super::handlers::peer_register))
        .route("/peers/verify", axum::routing::post(super::handlers::peer_verify))
        .route("/peers/spoke-values", axum::routing::post(super::handlers::peer_spoke_values))
        .route("/peers/spoke-values/live", axum::routing::post(super::handlers::peer_spoke_values_live))
        .route("/peers/notify", axum::routing::post(super::handlers::peer_notify))
        .with_state(app_state)
}

fn build_test_app_with_peer(wallet: Address) -> Router {
    let state: SharedState = Arc::new(RwLock::new(KeeperState::default()));
    let notify = Arc::new(Notify::new());
    let curator = Address::ZERO;
    let challenges: ChallengeStore = Arc::new(Mutex::new(HashMap::new()));
    let peer_registry = new_peer_registry();
    let pending_registrations = new_pending_registrations();
    let batch_updater = Address::ZERO;
    let flow_rpc = "http://localhost:8545".to_string();
    let keeper_url: Option<String> = None;
    let signer: Option<alloy::signers::local::PrivateKeySigner> = None;
    let spokes: Vec<crate::config::SpokeConfig> = vec![];
    let wallet_challenges: WalletChallengeStore = Arc::new(Mutex::new(HashMap::new()));
    let whitelist_cache = WhitelistCache::new();

    {
        let mut guard = peer_registry.try_lock().expect("registry lock during setup");
        let now = chrono::Utc::now().timestamp() as u64;
        guard.insert(
            "http://peer.example.com:8080".to_string(),
            PeerInfo {
                url: "http://peer.example.com:8080".to_string(),
                wallet,
                registered_at: now,
                last_seen: Some(now),
                last_push_at: None,
                active: true,
            },
        );
    }

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
        None,
    );

    Router::new()
        .route("/health", axum::routing::get(super::handlers::health))
        .route("/status", axum::routing::post(super::handlers::status))
        .route("/challenge", axum::routing::get(super::handlers::get_challenge))
        .route("/peers/challenge", axum::routing::get(super::handlers::get_peer_challenge))
        .route("/update", axum::routing::post(super::handlers::trigger_update))
        .route("/peers/register", axum::routing::post(super::handlers::peer_register))
        .route("/peers/verify", axum::routing::post(super::handlers::peer_verify))
        .route("/peers/spoke-values", axum::routing::post(super::handlers::peer_spoke_values))
        .route("/peers/spoke-values/live", axum::routing::post(super::handlers::peer_spoke_values_live))
        .route("/peers/notify", axum::routing::post(super::handlers::peer_notify))
        .with_state(app_state)
}

async fn body_bytes(body: axum::body::Body) -> Vec<u8> {
    use http_body_util::BodyExt;
    body.collect().await.unwrap().to_bytes().to_vec()
}

#[tokio::test]
async fn test_health() {
    let app = build_test_app();

    let req = Request::builder()
        .method("GET")
        .uri("/health")
        .body(Body::empty())
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let bytes = body_bytes(resp.into_body()).await;
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["status"], "ok");
}

#[tokio::test]
async fn test_status_requires_auth() {
    // GET /status no longer exists; POST without body → 4xx
    let app = build_test_app();

    let req = Request::builder()
        .method("GET")
        .uri("/status")
        .body(Body::empty())
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);
}

#[tokio::test]
async fn test_status_post_without_auth_returns_4xx() {
    // POST /status without a challenge pre-seeded → 401
    let app = build_test_app();

    let body = serde_json::json!({
        "wallet": "0x0000000000000000000000000000000000000001",
        "signature": "0x000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000"
    });

    let req = Request::builder()
        .method("POST")
        .uri("/status")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert!(
        resp.status() == StatusCode::UNAUTHORIZED || resp.status() == StatusCode::BAD_REQUEST,
        "expected 4xx, got {}",
        resp.status()
    );
}

#[tokio::test]
async fn test_challenge_returns_challenge_no_wallet() {
    let app = build_test_app();

    let req = Request::builder()
        .method("GET")
        .uri("/challenge")
        .body(Body::empty())
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let bytes = body_bytes(resp.into_body()).await;
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();

    let challenge = json["challenge"].as_str().unwrap();
    assert!(
        challenge.starts_with("update:"),
        "generic challenge should start with 'update:', got: {challenge}"
    );
    assert!(json["expires_at"].is_number());
}

#[tokio::test]
async fn test_update_missing_challenge_returns_401() {
    let app = build_test_app();

    let body = serde_json::json!({
        "wallet": "0x0000000000000000000000000000000000000001",
        "signature": "0x000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000"
    });

    let req = Request::builder()
        .method("POST")
        .uri("/update")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_update_wrong_signature_returns_4xx() {
    let wallet = Address::ZERO;
    let app = build_test_app_with_challenge(wallet, "keeper-auth:test-nonce-1234");

    let body = serde_json::json!({
        "wallet": format!("{wallet:#x}"),
        "signature": "0x000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000"
    });

    let req = Request::builder()
        .method("POST")
        .uri("/update")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert!(
        resp.status() == StatusCode::BAD_REQUEST || resp.status() == StatusCode::UNAUTHORIZED,
        "expected 400 or 401, got {}",
        resp.status()
    );
}

#[tokio::test]
async fn test_peer_register_returns_400_without_challenge() {
    let app = build_test_app();

    let body = serde_json::json!({
        "url": "https://1.2.3.4:8080",
        "wallet": "0x0000000000000000000000000000000000000001",
        "signature": "0xdeadbeef"
    });

    let req = Request::builder()
        .method("POST")
        .uri("/peers/register")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert!(
        resp.status() == StatusCode::UNAUTHORIZED || resp.status() == StatusCode::BAD_REQUEST,
        "expected 401 or 400, got {}",
        resp.status()
    );
}

#[tokio::test]
async fn test_peer_register_rejects_private_url() {
    // URL validation runs before auth, so no valid sig is needed here.
    let app = build_test_app();

    let body = serde_json::json!({
        "url": "http://192.168.1.1:8080",
        "wallet": "0x0000000000000000000000000000000000000001",
        "signature": "0xdeadbeef"
    });

    let req = Request::builder()
        .method("POST")
        .uri("/peers/register")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    let bytes = body_bytes(resp.into_body()).await;
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert!(
        json["message"].as_str().unwrap_or("").contains("RFC1918"),
        "expected RFC1918 rejection, got: {}",
        json["message"]
    );
}

#[tokio::test]
async fn test_spoke_values_get_returns_405() {
    let app = build_test_app();

    let req = Request::builder()
        .method("GET")
        .uri("/peers/spoke-values")
        .body(Body::empty())
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);
}

#[tokio::test]
async fn test_spoke_values_post_without_challenge_returns_401() {
    let app = build_test_app();

    let body = serde_json::json!({
        "wallet": "0x0000000000000000000000000000000000000001",
        "signature": "0x000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000"
    });

    let req = Request::builder()
        .method("POST")
        .uri("/peers/spoke-values")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_peer_notify_rejects_loopback_url() {
    let app = build_test_app();

    let body = serde_json::json!({
        "url": "http://127.0.0.1:8080",
        "wallet": "0x0000000000000000000000000000000000000001",
        "auth_wallet": "0x0000000000000000000000000000000000000002",
        "auth_signature": "0xdeadbeef"
    });

    let req = Request::builder()
        .method("POST")
        .uri("/peers/notify")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    let bytes = body_bytes(resp.into_body()).await;
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert!(
        json["message"].as_str().unwrap_or("").contains("loopback"),
        "expected loopback rejection"
    );
}

#[tokio::test]
async fn test_security_headers_on_health() {
    use axum::middleware;
    use crate::api::AppState;

    let state: SharedState = Arc::new(RwLock::new(KeeperState::default()));
    let notify = Arc::new(Notify::new());
    let challenges: ChallengeStore = Arc::new(Mutex::new(HashMap::new()));
    let peer_registry = new_peer_registry();
    let pending_registrations = new_pending_registrations();
    let wallet_challenges: WalletChallengeStore = Arc::new(Mutex::new(HashMap::new()));
    let whitelist_cache = WhitelistCache::new();

    let app_state: AppState = (
        state,
        notify,
        Address::ZERO,
        challenges,
        peer_registry,
        pending_registrations,
        Address::ZERO,
        "http://localhost:8545".to_string(),
        None,
        None,
        vec![],
        wallet_challenges,
        whitelist_cache,
        None,
    );

    let app = Router::new()
        .route("/health", axum::routing::get(super::handlers::health))
        .with_state(app_state)
        .layer(middleware::from_fn(super::security_headers));

    let req = Request::builder()
        .method("GET")
        .uri("/health")
        .body(Body::empty())
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers().get("x-content-type-options").map(|v| v.to_str().unwrap()),
        Some("nosniff")
    );
    assert_eq!(
        resp.headers().get("x-frame-options").map(|v| v.to_str().unwrap()),
        Some("DENY")
    );
}
