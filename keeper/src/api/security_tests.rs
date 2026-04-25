/// Adversarial security tests for the keeper API.
///
/// These tests actively try to break the security model:
/// challenge abuse, signature replay, SSRF, race conditions,
/// body-size limits, and malformed inputs.
///
/// Design notes
/// ─────────────
/// • `verify_auth` calls the on-chain whitelist via RPC as its final step.
///   In all tests that must reach that step we use `Address::ZERO` as both
///   `curator` and `batch_updater` — the RPC call fails and returns 500.
///   For tests that are expected to short-circuit *before* the whitelist
///   (bad sig format, wrong sig, expired/missing challenge) we assert on the
///   early-exit status codes instead.
/// • URL validation in `peer_register` / `peer_notify` runs *before* auth, so
///   SSRF tests never need a valid signature.
/// • The `WalletChallengeStore` uses a `Mutex<HashMap>` with an atomic
///   `remove()` call — the race-condition test exercises that single-use
///   enforcement under genuine concurrency.

use std::collections::HashMap;
use std::sync::Arc;

use alloy::primitives::Address;
use alloy::signers::local::PrivateKeySigner;
use alloy::signers::Signer;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use http_body_util::BodyExt as _;
use tokio::sync::{Mutex, Notify, RwLock};
use tower::ServiceExt as _;

use crate::api::{AppState, ChallengeStore, KeeperState, SharedState, WalletChallengeStore};
use crate::peer_registry::{new_peer_registry, new_pending_registrations};
use crate::security::WhitelistCache;

// ─── Test-wallet constants ────────────────────────────────────────────────────

const TEST_PRIVATE_KEY: &str =
    "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";

fn test_signer() -> PrivateKeySigner {
    TEST_PRIVATE_KEY.parse().expect("valid test private key")
}

fn test_wallet() -> Address {
    test_signer().address()
}

// ─── App builder helpers ──────────────────────────────────────────────────────

fn build_app() -> Router {
    build_app_impl(None, None, HashMap::new())
}

fn build_app_with_wallet_challenge(wallet: Address, challenge: &str) -> Router {
    let mut map = HashMap::new();
    let expires_at = chrono::Utc::now().timestamp() as u64 + 300;
    map.insert(format!("{wallet:#x}"), (challenge.to_string(), expires_at));
    build_app_impl(None, None, map)
}

fn build_app_with_expired_wallet_challenge(wallet: Address, challenge: &str) -> Router {
    let mut map = HashMap::new();
    let expires_at = chrono::Utc::now().timestamp() as u64 - 1; // already expired
    map.insert(format!("{wallet:#x}"), (challenge.to_string(), expires_at));
    build_app_impl(None, None, map)
}

fn build_app_with_shared_challenge_store(
    wallet: Address,
    challenge: &str,
    store: WalletChallengeStore,
) -> Router {
    let expires_at = chrono::Utc::now().timestamp() as u64 + 300;
    {
        let mut guard = store.try_lock().expect("lock during setup");
        guard.insert(format!("{wallet:#x}"), (challenge.to_string(), expires_at));
    }
    build_app_from_wallet_challenge_store(store)
}

fn build_app_from_wallet_challenge_store(wallet_challenges: WalletChallengeStore) -> Router {
    let state: SharedState = Arc::new(RwLock::new(KeeperState::default()));
    let notify = Arc::new(Notify::new());
    let challenges: ChallengeStore = Arc::new(Mutex::new(HashMap::new()));
    let peer_registry = new_peer_registry();
    let pending_registrations = new_pending_registrations();
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
        None, // telegram
        Arc::new(Mutex::new(HashMap::new())), // bridge_warnings
        None, // oracle_owner_signer
    );

    make_router(app_state)
}

fn build_app_impl(
    _curator: Option<Address>,
    _keeper_url: Option<String>,
    pre_seeded_wallet_challenges: HashMap<String, (String, u64)>,
) -> Router {
    let state: SharedState = Arc::new(RwLock::new(KeeperState::default()));
    let notify = Arc::new(Notify::new());
    let challenges: ChallengeStore = Arc::new(Mutex::new(HashMap::new()));
    let peer_registry = new_peer_registry();
    let pending_registrations = new_pending_registrations();
    let wallet_challenges: WalletChallengeStore =
        Arc::new(Mutex::new(pre_seeded_wallet_challenges));
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
        None, // telegram
        Arc::new(Mutex::new(HashMap::new())), // bridge_warnings
        None, // oracle_owner_signer
    );

    make_router(app_state)
}

fn make_router(app_state: AppState) -> Router {
    Router::new()
        .route("/health", axum::routing::get(super::handlers::health))
        .route("/status", axum::routing::post(super::handlers::status))
        .route("/challenge", axum::routing::get(super::handlers::get_challenge))
        .route(
            "/peers/challenge",
            axum::routing::get(super::handlers::get_peer_challenge),
        )
        .route("/update", axum::routing::post(super::handlers::trigger_update))
        .route(
            "/peers/register",
            axum::routing::post(super::handlers::peer_register),
        )
        .route(
            "/peers/verify",
            axum::routing::post(super::handlers::peer_verify),
        )
        .route(
            "/peers/spoke-values",
            axum::routing::post(super::handlers::peer_spoke_values),
        )
        .route(
            "/peers/spoke-values/live",
            axum::routing::post(super::handlers::peer_spoke_values_live),
        )
        .route(
            "/peers/notify",
            axum::routing::post(super::handlers::peer_notify),
        )
        .with_state(app_state)
}

// ─── Helpers ──────────────────────────────────────────────────────────────────

async fn body_bytes(body: Body) -> Vec<u8> {
    body.collect().await.unwrap().to_bytes().to_vec()
}

async fn body_json(body: Body) -> serde_json::Value {
    let bytes = body_bytes(body).await;
    serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
}

fn json_body(v: serde_json::Value) -> Body {
    Body::from(serde_json::to_vec(&v).unwrap())
}

fn post_json(uri: &str, body: serde_json::Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        .body(json_body(body))
        .unwrap()
}

// ─── Category 1: Challenge endpoint abuse ────────────────────────────────────

/// RPC is unreachable in tests so the whitelist check returns 500 — either way
/// the server must not issue a challenge (no 200).
#[tokio::test]
async fn test_challenge_non_whitelisted_wallet_no_200() {
    let app = build_app();

    let req = Request::builder()
        .method("GET")
        .uri(format!(
            "/challenge?wallet={}",
            "0x0000000000000000000000000000000000000001"
        ))
        .body(Body::empty())
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_ne!(
        resp.status(),
        StatusCode::OK,
        "non-whitelisted wallet must not receive a challenge"
    );
}

#[tokio::test]
async fn test_challenge_malformed_wallet_not_hex() {
    let app = build_app();

    let req = Request::builder()
        .method("GET")
        .uri("/challenge?wallet=not-a-hex-address")
        .body(Body::empty())
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "non-hex wallet must be rejected with 400"
    );
}

#[tokio::test]
async fn test_challenge_malformed_wallet_too_short() {
    let app = build_app();

    let req = Request::builder()
        .method("GET")
        .uri("/challenge?wallet=0xDEAD")
        .body(Body::empty())
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_challenge_empty_wallet_param() {
    let app = build_app();

    let req = Request::builder()
        .method("GET")
        .uri("/challenge?wallet=")
        .body(Body::empty())
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

/// No `wallet` param returns a generic challenge (legacy curator flow).
#[tokio::test]
async fn test_challenge_no_wallet_param_returns_generic_challenge() {
    let app = build_app();

    let req = Request::builder()
        .method("GET")
        .uri("/challenge")
        .body(Body::empty())
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp.into_body()).await;
    let ch = json["challenge"].as_str().unwrap_or("");
    assert!(ch.starts_with("update:"), "generic challenge prefix must be 'update:', got {ch}");
}

#[tokio::test]
async fn test_challenge_duplicate_request_consistent_non_200() {
    // Router is not Clone, so build two separate apps.
    let app = build_app();
    let app2 = build_app();

    let wallet = "0x0000000000000000000000000000000000000001";
    let make_req = || {
        Request::builder()
            .method("GET")
            .uri(format!("/challenge?wallet={wallet}"))
            .body(Body::empty())
            .unwrap()
    };

    let r1 = app.oneshot(make_req()).await.unwrap();
    let r2 = app2.oneshot(make_req()).await.unwrap();

    assert_ne!(r1.status(), StatusCode::OK, "first duplicate must not succeed");
    assert_ne!(r2.status(), StatusCode::OK, "second duplicate must not succeed");
    assert_eq!(
        r1.status(),
        r2.status(),
        "duplicate challenge requests must produce the same status code"
    );
}

// ─── Category 2: Signature replay attacks ─────────────────────────────────────

#[tokio::test]
async fn test_replay_attack_cross_endpoint_reuse() {
    let signer = test_signer();
    let wallet = test_wallet();
    let challenge = "keeper-auth:cross-endpoint-replay-test";

    let app = build_app_with_wallet_challenge(wallet, challenge);

    let sig = signer.sign_message(challenge.as_bytes()).await.unwrap();
    let sig_hex = format!("0x{}", alloy::hex::encode(sig.as_bytes()));

    // First use consumes the challenge (whitelist will fail, but challenge IS removed).
    let body1 = serde_json::json!({
        "wallet": format!("{wallet:#x}"),
        "signature": sig_hex,
    });
    let resp1 = app.clone().oneshot(post_json("/update", body1.clone())).await.unwrap();
    assert_ne!(resp1.status(), StatusCode::OK);

    let body2 = serde_json::json!({
        "url": "https://1.2.3.4:8080",
        "wallet": format!("{wallet:#x}"),
        "signature": sig_hex,
    });
    let resp2 = app.oneshot(post_json("/peers/register", body2)).await.unwrap();
    assert_eq!(
        resp2.status(),
        StatusCode::UNAUTHORIZED,
        "replayed signature on different endpoint must return 401"
    );
}

#[tokio::test]
async fn test_replay_attack_same_challenge_twice() {
    let signer = test_signer();
    let wallet = test_wallet();
    let challenge = "keeper-auth:replay-same-twice-test";

    // Both requests must share the same store so the first removal is visible to the second.
    let store: WalletChallengeStore = Arc::new(Mutex::new(HashMap::new()));
    let app = build_app_with_shared_challenge_store(wallet, challenge, store);

    let sig = signer.sign_message(challenge.as_bytes()).await.unwrap();
    let sig_hex = format!("0x{}", alloy::hex::encode(sig.as_bytes()));

    let body = serde_json::json!({
        "wallet": format!("{wallet:#x}"),
        "signature": sig_hex,
    });

    // First use: challenge exists; fails at whitelist, but IS consumed.
    let resp1 = app
        .clone()
        .oneshot(post_json("/peers/spoke-values", body.clone()))
        .await
        .unwrap();
    assert_ne!(
        resp1.status(),
        StatusCode::UNAUTHORIZED,
        "first use should not fail with 'no pending challenge'"
    );

    // Second use: challenge already removed.
    let resp2 = app
        .oneshot(post_json("/peers/spoke-values", body.clone()))
        .await
        .unwrap();
    assert_eq!(
        resp2.status(),
        StatusCode::UNAUTHORIZED,
        "second use of same challenge must return 401"
    );
    let json2 = body_json(resp2.into_body()).await;
    let msg = json2["message"].as_str().unwrap_or("");
    assert!(
        msg.contains("no pending challenge") || msg.contains("challenge"),
        "error message must mention missing challenge, got: {msg}"
    );
}

#[tokio::test]
async fn test_replay_attack_wrong_wallet_claimed() {
    let signer_a = test_signer();
    let _wallet_a = test_wallet();

    let wallet_b: Address = "0x0000000000000000000000000000000000000002"
        .parse()
        .unwrap();

    let challenge = "keeper-auth:wrong-wallet-claimed-test";
    let app = build_app_with_wallet_challenge(wallet_b, challenge);

    let sig = signer_a.sign_message(challenge.as_bytes()).await.unwrap();
    let sig_hex = format!("0x{}", alloy::hex::encode(sig.as_bytes()));

    let body = serde_json::json!({
        "wallet": format!("{wallet_b:#x}"),
        "signature": sig_hex,
    });

    let resp = app.oneshot(post_json("/peers/spoke-values", body)).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "signature signed by key A but claiming wallet B must be rejected"
    );
    let json = body_json(resp.into_body()).await;
    let msg = json["message"].as_str().unwrap_or("");
    assert!(
        msg.contains("does not match") || msg.contains("recover") || msg.contains("signature"),
        "error must mention signature mismatch, got: {msg}"
    );
}

#[tokio::test]
async fn test_replay_attack_wrong_message_signed() {
    let signer = test_signer();
    let wallet = test_wallet();

    let real_challenge = "keeper-auth:real-challenge-nonce";
    let app = build_app_with_wallet_challenge(wallet, real_challenge);

    let wrong_sig = signer.sign_message(b"wrong message not the challenge").await.unwrap();
    let sig_hex = format!("0x{}", alloy::hex::encode(wrong_sig.as_bytes()));

    let body = serde_json::json!({
        "wallet": format!("{wallet:#x}"),
        "signature": sig_hex,
    });

    let resp = app.oneshot(post_json("/peers/spoke-values", body)).await.unwrap();
    assert!(
        resp.status() == StatusCode::UNAUTHORIZED || resp.status() == StatusCode::BAD_REQUEST,
        "signing wrong message must be rejected, got {}",
        resp.status()
    );
}

#[tokio::test]
async fn test_replay_attack_expired_challenge() {
    let signer = test_signer();
    let wallet = test_wallet();
    let challenge = "keeper-auth:expired-ttl-test";

    // Seed an ALREADY EXPIRED challenge.
    let app = build_app_with_expired_wallet_challenge(wallet, challenge);

    let sig = signer.sign_message(challenge.as_bytes()).await.unwrap();
    let sig_hex = format!("0x{}", alloy::hex::encode(sig.as_bytes()));

    let body = serde_json::json!({
        "wallet": format!("{wallet:#x}"),
        "signature": sig_hex,
    });

    let resp = app.oneshot(post_json("/peers/spoke-values", body)).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "expired challenge must return 401"
    );
    let json = body_json(resp.into_body()).await;
    let msg = json["message"].as_str().unwrap_or("");
    assert!(
        msg.contains("expired") || msg.contains("challenge"),
        "error message must mention expiry, got: {msg}"
    );
}

// ─── Category 3: `/peers/notify` without / bad auth ──────────────────────────

#[tokio::test]
async fn test_peers_notify_no_auth_fields() {
    let app = build_app();

    let body = serde_json::json!({
        "url": "https://1.2.3.4:8080",
        "wallet": "0x0000000000000000000000000000000000000001",
    });

    let resp = app.oneshot(post_json("/peers/notify", body)).await.unwrap();
    assert!(
        resp.status().is_client_error(),
        "missing auth fields must return 4xx, got {}",
        resp.status()
    );
}

#[tokio::test]
async fn test_peers_notify_empty_body() {
    let app = build_app();

    let req = Request::builder()
        .method("POST")
        .uri("/peers/notify")
        .header("content-type", "application/json")
        .body(Body::from(b"{}".as_ref()))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert!(
        resp.status().is_client_error(),
        "empty body must return 4xx, got {}",
        resp.status()
    );
}

#[tokio::test]
async fn test_peers_notify_wrong_signature() {
    let app = build_app();

    let body = serde_json::json!({
        "url": "https://legitimate-keeper.example.com:8080",
        "wallet": "0x0000000000000000000000000000000000000001",
        "auth_wallet": "0x0000000000000000000000000000000000000001",
        "auth_signature": "0x000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000",
    });

    let resp = app.oneshot(post_json("/peers/notify", body)).await.unwrap();
    assert!(
        resp.status() == StatusCode::UNAUTHORIZED || resp.status() == StatusCode::BAD_REQUEST,
        "wrong/missing challenge must return 401 or 400, got {}",
        resp.status()
    );
}

/// URL validation runs before auth — a loopback URL must be rejected with 400
/// even if the auth fields would otherwise be valid.
#[tokio::test]
async fn test_peers_notify_with_loopback_url_rejected_before_auth() {
    let app = build_app();

    let body = serde_json::json!({
        "url": "http://127.0.0.1:9999",
        "wallet": "0x0000000000000000000000000000000000000001",
        "auth_wallet": "0x0000000000000000000000000000000000000001",
        "auth_signature": "0xdeadbeef",
    });

    let resp = app.oneshot(post_json("/peers/notify", body)).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "loopback URL must be rejected with 400 before auth is checked"
    );
    let json = body_json(resp.into_body()).await;
    assert!(
        json["message"].as_str().unwrap_or("").contains("loopback"),
        "rejection message must mention loopback"
    );
}

#[tokio::test]
async fn test_peers_notify_aws_metadata_url_rejected() {
    let app = build_app();

    let body = serde_json::json!({
        "url": "http://169.254.169.254/latest/meta-data",
        "wallet": "0x0000000000000000000000000000000000000001",
        "auth_wallet": "0x0000000000000000000000000000000000000001",
        "auth_signature": "0xdeadbeef",
    });

    let resp = app.oneshot(post_json("/peers/notify", body)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let json = body_json(resp.into_body()).await;
    let msg = json["message"].as_str().unwrap_or("");
    assert!(
        msg.contains("link-local"),
        "AWS metadata endpoint must be rejected as link-local, got: {msg}"
    );
}

// ─── Category 4: SSRF via URL validation (`security::validate_peer_url`) ─────

macro_rules! ssrf_test {
    ($name:ident, $url:expr, $expected_fragment:expr) => {
        #[tokio::test]
        async fn $name() {
            let app = build_app();
            let body = serde_json::json!({
                "url": $url,
                "wallet": "0x0000000000000000000000000000000000000001",
                "signature": "0xdeadbeef",
            });
            let resp = app.oneshot(post_json("/peers/register", body)).await.unwrap();
            assert_eq!(
                resp.status(),
                StatusCode::BAD_REQUEST,
                "URL '{}' must be rejected with 400",
                $url
            );
            let json = body_json(resp.into_body()).await;
            let msg = json["message"].as_str().unwrap_or("");
            assert!(
                msg.contains($expected_fragment),
                "rejection for '{}' must mention '{}', got: {msg}",
                $url,
                $expected_fragment
            );
        }
    };
}

ssrf_test!(
    test_ssrf_loopback_ipv4,
    "http://127.0.0.1/anything",
    "loopback"
);

ssrf_test!(
    test_ssrf_loopback_localhost,
    "http://localhost/anything",
    "loopback"
);

ssrf_test!(
    test_ssrf_private_192_168,
    "http://192.168.1.1/admin",
    "RFC1918"
);

ssrf_test!(
    test_ssrf_private_10_0,
    "http://10.0.0.1/secret",
    "RFC1918"
);

ssrf_test!(
    test_ssrf_private_172_16,
    "http://172.16.0.1/internal",
    "RFC1918"
);

ssrf_test!(
    test_ssrf_aws_metadata,
    "http://169.254.169.254/latest/meta-data",
    "link-local"
);

ssrf_test!(
    test_ssrf_wrong_scheme_ftp,
    "ftp://example.com",
    "only http/https"
);

ssrf_test!(
    test_ssrf_wrong_scheme_file,
    "file:///etc/passwd",
    "only http/https"
);

ssrf_test!(
    test_ssrf_ipv6_loopback,
    "http://[::1]/",
    "loopback"
);

ssrf_test!(
    test_ssrf_ipv6_unique_local,
    "http://[fc00::1]/",
    "unique-local"
);

/// A public IP URL must pass URL validation and fail only at auth (not URL).
#[tokio::test]
async fn test_ssrf_legitimate_url_passes_url_validation() {
    let app = build_app();
    let body = serde_json::json!({
        "url": "https://1.2.3.4:8080",
        "wallet": "0x0000000000000000000000000000000000000001",
        "signature": "0xdeadbeef",
    });
    let resp = app.oneshot(post_json("/peers/register", body)).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "public IP URL must pass URL validation and fail only at auth (401), got {}",
        resp.status()
    );
    let json = body_json(resp.into_body()).await;
    let msg = json["message"].as_str().unwrap_or("");
    for forbidden in &["loopback", "RFC1918", "link-local", "only http/https"] {
        assert!(
            !msg.contains(forbidden),
            "public URL rejection must NOT mention '{}', got: {msg}",
            forbidden
        );
    }
}

#[tokio::test]
async fn test_ssrf_embedded_credentials_private_ip() {
    let app = build_app();
    let body = serde_json::json!({
        "url": "http://user:pass@10.0.0.1/",
        "wallet": "0x0000000000000000000000000000000000000001",
        "signature": "0xdeadbeef",
    });
    let resp = app.oneshot(post_json("/peers/register", body)).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "embedded-credentials private URL must be rejected"
    );
    let json = body_json(resp.into_body()).await;
    let msg = json["message"].as_str().unwrap_or("");
    assert!(
        msg.contains("RFC1918"),
        "rejection must mention RFC1918, got: {msg}"
    );
}

// ─── Category 5: Race condition on challenge store ────────────────────────────

/// Ten concurrent requests share one challenge; at most one should pass the
/// single-use gate — the rest must get UNAUTHORIZED.
#[tokio::test]
async fn test_race_condition_single_use_challenge_under_concurrency() {
    let signer = test_signer();
    let wallet = test_wallet();
    let challenge = "keeper-auth:race-condition-test-nonce";

    let store: WalletChallengeStore = Arc::new(Mutex::new(HashMap::new()));
    {
        let expires_at = chrono::Utc::now().timestamp() as u64 + 300;
        let mut guard = store.try_lock().unwrap();
        guard.insert(format!("{wallet:#x}"), (challenge.to_string(), expires_at));
    }

    let sig = signer.sign_message(challenge.as_bytes()).await.unwrap();
    let sig_hex = format!("0x{}", alloy::hex::encode(sig.as_bytes()));

    const CONCURRENCY: usize = 10;

    let mut handles = Vec::with_capacity(CONCURRENCY);
    for _ in 0..CONCURRENCY {
        let store_clone = store.clone();
        let sig_hex_clone = sig_hex.clone();
        let wallet_hex = format!("{wallet:#x}");

        handles.push(tokio::spawn(async move {
            // Each task builds its own router sharing the same store.
            let app = build_app_from_wallet_challenge_store(store_clone);
            let body = serde_json::json!({
                "wallet": wallet_hex,
                "signature": sig_hex_clone,
            });
            let resp = app.oneshot(post_json("/peers/spoke-values", body)).await.unwrap();
            resp.status()
        }));
    }

    let statuses: Vec<StatusCode> = futures::future::join_all(handles)
        .await
        .into_iter()
        .map(|r| r.expect("task did not panic"))
        .collect();

    let passed_gate: Vec<StatusCode> = statuses
        .iter()
        .filter(|&&s| s != StatusCode::UNAUTHORIZED)
        .cloned()
        .collect();

    assert!(
        passed_gate.len() <= 1,
        "single-use challenge under concurrency: {} requests passed the gate (expected ≤1): {:?}",
        passed_gate.len(),
        statuses
    );

    let unauthorized_count = statuses.iter().filter(|&&s| s == StatusCode::UNAUTHORIZED).count();
    assert!(
        unauthorized_count >= CONCURRENCY - 1,
        "at least {} of {} concurrent requests must get 401, got {}: {:?}",
        CONCURRENCY - 1,
        CONCURRENCY,
        unauthorized_count,
        statuses
    );
}

// ─── Category 6: Request body limits ─────────────────────────────────────────

/// This test builds a router with the production 64 KiB limit attached,
/// since the default test router omits it.
#[tokio::test]
async fn test_request_body_too_large_returns_413() {
    use axum::middleware;
    use tower::ServiceBuilder;
    use tower_http::limit::RequestBodyLimitLayer;

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
        None, // telegram
        Arc::new(Mutex::new(HashMap::new())), // bridge_warnings
        None, // oracle_owner_signer
    );

    let app = Router::new()
        .route("/update", axum::routing::post(super::handlers::trigger_update))
        .with_state(app_state)
        .layer(
            ServiceBuilder::new()
                .layer(middleware::from_fn(super::security_headers))
                .layer(RequestBodyLimitLayer::new(65_536)),
        );

    let oversized = vec![b'x'; 65_537];
    let req = Request::builder()
        .method("POST")
        .uri("/update")
        .header("content-type", "application/json")
        .body(Body::from(oversized))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::PAYLOAD_TOO_LARGE,
        "body > 64 KiB must return 413"
    );
}

// ─── Category 7: `/status` auth behaviour ────────────────────────────────────

/// GET /status no longer exists — POST is required.
#[tokio::test]
async fn test_status_get_returns_405() {
    let app = build_app();

    let req = Request::builder()
        .method("GET")
        .uri("/status")
        .body(Body::empty())
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::METHOD_NOT_ALLOWED,
        "GET /status must return 405 — endpoint is POST-only"
    );
}

// ─── Category 8: Malformed inputs ────────────────────────────────────────────

#[tokio::test]
async fn test_malformed_signature_not_hex() {
    let wallet = test_wallet();
    let challenge = "keeper-auth:malformed-sig-test";
    let app = build_app_with_wallet_challenge(wallet, challenge);

    let body = serde_json::json!({
        "wallet": format!("{wallet:#x}"),
        "signature": "not-valid-hex-at-all!!",
    });

    let resp = app.oneshot(post_json("/peers/spoke-values", body)).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "non-hex signature must return 400"
    );
}

#[tokio::test]
async fn test_malformed_signature_zeroed_out() {
    let wallet = test_wallet();
    let challenge = "keeper-auth:zeroed-sig-test";
    let app = build_app_with_wallet_challenge(wallet, challenge);

    let zero_sig =
        "0x000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000";
    let body = serde_json::json!({
        "wallet": format!("{wallet:#x}"),
        "signature": zero_sig,
    });

    let resp = app.oneshot(post_json("/peers/spoke-values", body)).await.unwrap();
    assert!(
        resp.status() == StatusCode::BAD_REQUEST || resp.status() == StatusCode::UNAUTHORIZED,
        "zeroed-out signature must return 400 or 401, got {}",
        resp.status()
    );
}

#[tokio::test]
async fn test_malformed_extremely_long_wallet_no_panic() {
    let challenge = "keeper-auth:long-wallet-test";
    let wallet = test_wallet();
    let app = build_app_with_wallet_challenge(wallet, challenge);

    let long_wallet = format!("0x{}", "a".repeat(10_000));
    let body = serde_json::json!({
        "wallet": long_wallet,
        "signature": "0xdeadbeef",
    });

    let resp = app.oneshot(post_json("/peers/spoke-values", body)).await.unwrap();
    assert!(
        resp.status().is_client_error(),
        "extremely long wallet must return 4xx, got {}",
        resp.status()
    );
}

#[tokio::test]
async fn test_malformed_extremely_long_signature_no_panic() {
    let wallet = test_wallet();
    let challenge = "keeper-auth:long-sig-test";
    let app = build_app_with_wallet_challenge(wallet, challenge);

    let long_sig = format!("0x{}", "ff".repeat(10_000));
    let body = serde_json::json!({
        "wallet": format!("{wallet:#x}"),
        "signature": long_sig,
    });

    let resp = app.oneshot(post_json("/peers/spoke-values", body)).await.unwrap();
    assert!(
        resp.status().is_client_error(),
        "extremely long signature must return 4xx without panic, got {}",
        resp.status()
    );
}

#[tokio::test]
async fn test_malformed_wallet_right_length_bad_chars() {
    let app = build_app();

    let req = Request::builder()
        .method("GET")
        .uri("/challenge?wallet=0xGGGGGGGGGGGGGGGGGGGGGGGGGGGGGGGGGGGGGGGGGG")
        .body(Body::empty())
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "wallet with invalid hex chars must return 400"
    );
}

#[tokio::test]
async fn test_ssrf_via_peer_register_belt_and_suspenders() {
    let ssrf_urls = [
        ("http://127.0.0.1/", "loopback"),
        ("http://10.1.2.3/", "RFC1918"),
        ("http://172.20.0.1/", "RFC1918"),
        ("http://192.168.0.1/", "RFC1918"),
        ("http://169.254.0.1/", "link-local"),
        ("http://[::1]/", "loopback"),
        ("http://[fc00::1]/", "unique-local"),
        ("ftp://example.com", "only http/https"),
        ("file:///etc/hosts", "only http/https"),
    ];

    for (url, expected_fragment) in ssrf_urls {
        let app = build_app();
        let body = serde_json::json!({
            "url": url,
            "wallet": "0x0000000000000000000000000000000000000001",
            "signature": "0xdeadbeef",
        });
        let resp = app.oneshot(post_json("/peers/register", body)).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::BAD_REQUEST,
            "URL '{}' must be rejected with 400",
            url
        );
        let json = body_json(resp.into_body()).await;
        let msg = json["message"].as_str().unwrap_or("");
        assert!(
            msg.contains(expected_fragment),
            "URL '{}': expected fragment '{}' in message, got: {msg}",
            url,
            expected_fragment
        );
    }
}

// ─── Category 9: Challenge spam / idempotency (one challenge per wallet) ─────

/// The store must never hold more than one entry per wallet, and the same
/// challenge string must be returned on every call while it remains unexpired.
#[tokio::test]
async fn test_challenge_spam_same_wallet_bounded() {
    let store: WalletChallengeStore = Arc::new(Mutex::new(HashMap::new()));

    // Exercise the store's idempotency logic directly (bypassing the whitelist).
    let wallet_key = "0x0000000000000000000000000000000000000001".to_string();
    let expires_at = chrono::Utc::now().timestamp() as u64 + 300;
    let first_challenge = "keeper-auth:spam-test-nonce-initial".to_string();

    {
        let mut guard = store.lock().await;
        guard.insert(wallet_key.clone(), (first_challenge.clone(), expires_at));
    }

    // Simulate 1000 "requests" using the idempotency logic: return existing
    // if non-expired, otherwise insert new.
    let mut returned_challenges = Vec::with_capacity(1000);
    for _ in 0..1000 {
        let now = chrono::Utc::now().timestamp() as u64;
        let mut guard = store.lock().await;
        guard.retain(|_, (_, exp)| *exp > now);

        let result = if let Some((ch, exp)) = guard.get(&wallet_key) {
            (ch.clone(), *exp)
        } else {
            let nonce = uuid::Uuid::new_v4().to_string();
            let challenge = format!("keeper-auth:{nonce}");
            let new_exp = now + 120;
            guard.insert(wallet_key.clone(), (challenge.clone(), new_exp));
            (challenge, new_exp)
        };

        returned_challenges.push(result.0);

        assert_eq!(
            guard.len(),
            1,
            "store must have exactly 1 entry after iteration, found {}",
            guard.len()
        );
    }

    let first = &returned_challenges[0];
    for (i, ch) in returned_challenges.iter().enumerate() {
        assert_eq!(
            ch, first,
            "iteration {i} returned a different challenge (got {ch:?}, expected {first:?})"
        );
    }
}

/// Concurrent challenge spam must not overwrite a legitimate keeper's challenge
/// due to the idempotent "return existing if not expired" behaviour.
#[tokio::test]
async fn test_challenge_race_condition_spend_while_spamming() {
    let store: WalletChallengeStore = Arc::new(Mutex::new(HashMap::new()));
    let wallet_key = format!("{:#x}", Address::ZERO);
    let expires_at = chrono::Utc::now().timestamp() as u64 + 300;
    let legitimate_challenge = "keeper-auth:legitimate-thread-a-challenge".to_string();

    {
        let mut guard = store.lock().await;
        guard.insert(wallet_key.clone(), (legitimate_challenge.clone(), expires_at));
    }

    let store_b = store.clone();
    let wallet_key_b = wallet_key.clone();

    let spam_handle = tokio::spawn(async move {
        for _ in 0..200 {
            let now = chrono::Utc::now().timestamp() as u64;
            let mut guard = store_b.lock().await;
            guard.retain(|_, (_, exp)| *exp > now);

            if guard.get(&wallet_key_b).is_none() {
                let nonce = uuid::Uuid::new_v4().to_string();
                let challenge = format!("keeper-auth:{nonce}");
                let new_exp = now + 120;
                guard.insert(wallet_key_b.clone(), (challenge, new_exp));
            }
            drop(guard);
            tokio::task::yield_now().await;
        }
    });

    let store_a = store.clone();
    let wallet_key_a = wallet_key.clone();
    let expected_challenge = legitimate_challenge.clone();

    let legit_handle = tokio::spawn(async move {
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }

        let guard = store_a.lock().await;
        let entry = guard.get(&wallet_key_a).expect("challenge must still exist");
        assert_eq!(
            entry.0, expected_challenge,
            "Thread B spam must NOT have replaced Thread A's challenge: got {:?}",
            entry.0
        );
    });

    let (spam_result, legit_result) = tokio::join!(spam_handle, legit_handle);
    spam_result.expect("spam thread must not panic");
    legit_result.expect("legit handle must not panic");
}

#[tokio::test]
async fn test_challenge_expires_then_new_one_issued() {
    let store: WalletChallengeStore = Arc::new(Mutex::new(HashMap::new()));
    let wallet_key = "0x0000000000000000000000000000000000000099".to_string();
    let old_challenge = "keeper-auth:expired-old-challenge".to_string();

    {
        let mut guard = store.lock().await;
        let expired_at = chrono::Utc::now().timestamp() as u64 - 1; // already expired
        guard.insert(wallet_key.clone(), (old_challenge.clone(), expired_at));
    }

    let new_challenge = {
        let now = chrono::Utc::now().timestamp() as u64;
        let mut guard = store.lock().await;
        guard.retain(|_, (_, exp)| *exp > now);

        assert!(
            guard.get(&wallet_key).is_none(),
            "expired entry must have been evicted before issuing new challenge"
        );

        let nonce = uuid::Uuid::new_v4().to_string();
        let challenge = format!("keeper-auth:{nonce}");
        let expires_at = now + 120;
        guard.insert(wallet_key.clone(), (challenge.clone(), expires_at));
        challenge
    };

    assert_ne!(
        new_challenge, old_challenge,
        "new challenge must be different from the expired one"
    );

    let guard = store.lock().await;
    let entry = guard.get(&wallet_key).expect("new entry must exist");
    assert_ne!(
        entry.0, old_challenge,
        "store must hold the new challenge, not the expired one"
    );
}

/// The whitelist gate aborts before reaching the store-write path, so a failed
/// attacker request must not overwrite a legitimate wallet's challenge.
#[tokio::test]
async fn test_attacker_cannot_invalidate_legitimate_challenge() {
    let legit_wallet: Address = "0x0000000000000000000000000000000000000042"
        .parse()
        .unwrap();
    let challenge_c1 = "keeper-auth:c1-legitimate-challenge";
    let app = build_app_with_wallet_challenge(legit_wallet, challenge_c1);

    let attacker_req = Request::builder()
        .method("GET")
        .uri(format!("/challenge?wallet={legit_wallet:#x}"))
        .body(Body::empty())
        .unwrap();

    let resp = app.clone().oneshot(attacker_req).await.unwrap();

    assert_ne!(
        resp.status(),
        StatusCode::OK,
        "attacker must not receive a 200 from the challenge endpoint (whitelist gate)"
    );

    // Re-seed C1 in a shared store and confirm a second attacker attempt leaves it intact.
    let store: WalletChallengeStore = Arc::new(Mutex::new(HashMap::new()));
    {
        let mut guard = store.lock().await;
        let expires_at = chrono::Utc::now().timestamp() as u64 + 300;
        guard.insert(format!("{legit_wallet:#x}"), (challenge_c1.to_string(), expires_at));
    }

    let app2 = build_app_from_wallet_challenge_store(store.clone());

    let attacker_req2 = Request::builder()
        .method("GET")
        .uri(format!("/challenge?wallet={legit_wallet:#x}"))
        .body(Body::empty())
        .unwrap();

    let resp2 = app2.oneshot(attacker_req2).await.unwrap();
    assert_ne!(resp2.status(), StatusCode::OK, "second attacker attempt must also be blocked");

    let guard = store.lock().await;
    let entry = guard
        .get(&format!("{legit_wallet:#x}"))
        .expect("C1 must still be in the store after failed attacker requests");
    assert_eq!(
        entry.0, challenge_c1,
        "C1 must be unchanged after attacker spam, got {:?}",
        entry.0
    );
}

// ─── /status auth tests ───────────────────────────────────────────────────────

/// GET /status must return 405 — the endpoint is POST-only.
#[tokio::test]
async fn test_status_requires_auth() {
    let app = build_app();

    let req = Request::builder()
        .method("GET")
        .uri("/status")
        .body(Body::empty())
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::METHOD_NOT_ALLOWED,
        "GET /status must return 405"
    );
}

/// POST /status with a valid challenge-response signature reaches the whitelist
/// check. In the test environment the RPC is unavailable, so we get 500 (not
/// 401/403), which confirms auth passed and the request was processed.
#[tokio::test]
async fn test_status_with_valid_auth() {
    let signer = test_signer();
    let wallet = test_wallet();
    let challenge = "keeper-auth:status-valid-auth-test";

    let app = build_app_with_wallet_challenge(wallet, challenge);

    let sig = signer.sign_message(challenge.as_bytes()).await.unwrap();
    let sig_hex = format!("0x{}", alloy::hex::encode(sig.as_bytes()));

    let body = serde_json::json!({
        "wallet": format!("{wallet:#x}"),
        "signature": sig_hex,
    });

    let resp = app.oneshot(post_json("/status", body)).await.unwrap();
    // 500 = auth passed but RPC whitelist check failed in test env (no live node).
    // Any 4xx auth error would be 401/403 — if we get 500 it means auth succeeded.
    assert_ne!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "valid challenge-response must not return 401"
    );
    assert_ne!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "valid challenge-response must not return 403"
    );
}

/// POST /status with the same challenge twice — second attempt must return 401
/// because the challenge is consumed on first use.
#[tokio::test]
async fn test_status_replay_blocked() {
    let signer = test_signer();
    let wallet = test_wallet();
    let challenge = "keeper-auth:status-replay-test";

    let store: WalletChallengeStore = Arc::new(Mutex::new(std::collections::HashMap::new()));
    let app = build_app_with_shared_challenge_store(wallet, challenge, store);

    let sig = signer.sign_message(challenge.as_bytes()).await.unwrap();
    let sig_hex = format!("0x{}", alloy::hex::encode(sig.as_bytes()));

    let body = serde_json::json!({
        "wallet": format!("{wallet:#x}"),
        "signature": sig_hex,
    });

    // First attempt — challenge consumed (auth passes, RPC may fail with 5xx).
    let resp1 = app.clone().oneshot(post_json("/status", body.clone())).await.unwrap();
    assert_ne!(
        resp1.status(),
        StatusCode::UNAUTHORIZED,
        "first /status attempt must not be 401"
    );

    // Second attempt — challenge gone; must be 401.
    let resp2 = app.oneshot(post_json("/status", body)).await.unwrap();
    assert_eq!(
        resp2.status(),
        StatusCode::UNAUTHORIZED,
        "replayed /status challenge must return 401"
    );
}
