use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{Mutex, Notify, RwLock};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use tower::ServiceExt as _;

use crate::api::{AppState, ChallengeStore, KeeperState, SharedState};
use crate::peer_registry::{new_peer_registry, new_pending_registrations, PeerInfo};
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

    Router::new()
        .route("/health", axum::routing::get(super::handlers::health))
        .route("/status", axum::routing::get(super::handlers::status))
        .route("/challenge", axum::routing::get(super::handlers::get_challenge))
        .route("/update", axum::routing::post(super::handlers::trigger_update))
        .route("/peers/register", axum::routing::post(super::handlers::peer_register))
        .route("/peers/verify", axum::routing::post(super::handlers::peer_verify))
        .route("/peers/spoke-values", axum::routing::get(super::handlers::peer_spoke_values))
        .route("/peers/spoke-values/live", axum::routing::get(super::handlers::peer_spoke_values_live))
        .route("/peers/notify", axum::routing::post(super::handlers::peer_notify))
        .with_state(app_state)
}

fn build_test_app_with_peer(incoming_api_key: &str) -> Router {
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

    // Manually insert a peer into the registry
    {
        let registry_clone = peer_registry.clone();
        let key = incoming_api_key.to_string();
        // We need a blocking insert; use try_lock since we're still in single-threaded setup
        let mut guard = registry_clone.try_lock().expect("registry lock during setup");
        let now = chrono::Utc::now().timestamp() as u64;
        guard.insert(
            "http://peer:8080".to_string(),
            PeerInfo {
                url: "http://peer:8080".to_string(),
                wallet: Address::ZERO,
                incoming_api_key: key,
                outgoing_api_key: None,
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
    );

    Router::new()
        .route("/health", axum::routing::get(super::handlers::health))
        .route("/status", axum::routing::get(super::handlers::status))
        .route("/challenge", axum::routing::get(super::handlers::get_challenge))
        .route("/update", axum::routing::post(super::handlers::trigger_update))
        .route("/peers/register", axum::routing::post(super::handlers::peer_register))
        .route("/peers/verify", axum::routing::post(super::handlers::peer_verify))
        .route("/peers/spoke-values", axum::routing::get(super::handlers::peer_spoke_values))
        .route("/peers/spoke-values/live", axum::routing::get(super::handlers::peer_spoke_values_live))
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
async fn test_status_empty() {
    let app = build_test_app();

    let req = Request::builder()
        .method("GET")
        .uri("/status")
        .body(Body::empty())
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let bytes = body_bytes(resp.into_body()).await;
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();

    assert!(json["spokes"].is_array());
    assert!(json["peers"].is_array());
    assert_eq!(json["spokes"].as_array().unwrap().len(), 0);
    assert_eq!(json["peers"].as_array().unwrap().len(), 0);
    assert!(json["update_interval_secs"].is_number());
}

#[tokio::test]
async fn test_challenge_returns_challenge() {
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
    assert!(challenge.starts_with("update:"), "challenge should start with 'update:', got: {challenge}");
    assert!(json["expires_at"].is_number());
}

#[tokio::test]
async fn test_update_requires_valid_signature() {
    let app = build_test_app();

    // First get a challenge
    let challenge_req = Request::builder()
        .method("GET")
        .uri("/challenge")
        .body(Body::empty())
        .unwrap();

    let challenge_resp = app.clone().oneshot(challenge_req).await.unwrap();
    let bytes = body_bytes(challenge_resp.into_body()).await;
    let challenge_json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let challenge = challenge_json["challenge"].as_str().unwrap();

    // POST /update with a garbage signature
    let body = serde_json::json!({
        "challenge": challenge,
        "signature": "0x000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000"
    });

    let update_req = Request::builder()
        .method("POST")
        .uri("/update")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();

    let update_resp = app.oneshot(update_req).await.unwrap();
    // Should fail — bad signature or wrong signer
    assert!(
        update_resp.status() == StatusCode::BAD_REQUEST
            || update_resp.status() == StatusCode::UNAUTHORIZED,
        "expected 400 or 401, got {}",
        update_resp.status()
    );
}

#[tokio::test]
async fn test_peer_register_returns_challenge() {
    let app = build_test_app();

    let body = serde_json::json!({ "url": "http://peer:8080" });

    let req = Request::builder()
        .method("POST")
        .uri("/peers/register")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let bytes = body_bytes(resp.into_body()).await;
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();

    assert!(json["challenge"].is_string());
    assert!(json["expires_at"].is_number());
}

#[tokio::test]
async fn test_peer_register_twice_same_url() {
    let app = build_test_app();

    let body = serde_json::json!({ "url": "http://peer:8080" });

    // First registration
    let req1 = Request::builder()
        .method("POST")
        .uri("/peers/register")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let resp1 = app.clone().oneshot(req1).await.unwrap();
    assert_eq!(resp1.status(), StatusCode::OK);
    let bytes1 = body_bytes(resp1.into_body()).await;
    let json1: serde_json::Value = serde_json::from_slice(&bytes1).unwrap();
    assert!(json1["challenge"].is_string());

    // Second registration — overwrites the first challenge
    let req2 = Request::builder()
        .method("POST")
        .uri("/peers/register")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let resp2 = app.oneshot(req2).await.unwrap();
    assert_eq!(resp2.status(), StatusCode::OK);
    let bytes2 = body_bytes(resp2.into_body()).await;
    let json2: serde_json::Value = serde_json::from_slice(&bytes2).unwrap();
    assert!(json2["challenge"].is_string());
}

#[tokio::test]
async fn test_peer_verify_invalid_signature() {
    let app = build_test_app();

    // First register to get a challenge
    let reg_body = serde_json::json!({ "url": "http://peer:8080" });
    let reg_req = Request::builder()
        .method("POST")
        .uri("/peers/register")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&reg_body).unwrap()))
        .unwrap();
    let reg_resp = app.clone().oneshot(reg_req).await.unwrap();
    assert_eq!(reg_resp.status(), StatusCode::OK);

    // Now verify with a garbage signature
    let verify_body = serde_json::json!({
        "url": "http://peer:8080",
        "signature": "0xdeadbeef"
    });

    let verify_req = Request::builder()
        .method("POST")
        .uri("/peers/verify")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&verify_body).unwrap()))
        .unwrap();

    let verify_resp = app.oneshot(verify_req).await.unwrap();
    assert!(
        verify_resp.status() == StatusCode::BAD_REQUEST
            || verify_resp.status() == StatusCode::UNAUTHORIZED,
        "expected 400 or 401, got {}",
        verify_resp.status()
    );
}

#[tokio::test]
async fn test_spoke_values_requires_auth() {
    let app = build_test_app();

    let req = Request::builder()
        .method("GET")
        .uri("/peers/spoke-values")
        .body(Body::empty())
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_spoke_values_live_requires_auth() {
    let app = build_test_app();

    let req = Request::builder()
        .method("GET")
        .uri("/peers/spoke-values/live")
        .body(Body::empty())
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_spoke_values_invalid_key() {
    let app = build_test_app_with_peer("correct-api-key");

    let req = Request::builder()
        .method("GET")
        .uri("/peers/spoke-values")
        .header("X-Keeper-Key", "wrong-api-key")
        .body(Body::empty())
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}
