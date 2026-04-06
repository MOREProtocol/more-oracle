use alloy::primitives::Address;
use axum::{extract::State, http::StatusCode, response::Json};
use serde::{Deserialize, Serialize};

use crate::oracle;
use crate::peer_registry::PeerInfo;
use crate::peers;
use crate::security::validate_peer_url;

use super::super::{AppState, PEER_CHALLENGE_TTL_SECS, WALLET_CHALLENGE_TTL_SECS};
use super::update::{verify_auth_for, UpdateResponse};

// ── Challenge for peer-specific endpoints ─────────────────────────────────────

#[derive(Serialize)]
pub(crate) struct PeerChallengeResponse {
    challenge: String,
    expires_at: u64,
}

/// GET /peers/challenge?wallet=0x...
///
/// Issues a per-wallet challenge for peer-endpoint auth.  Checks whitelist
/// before issuing (with 5-min cache).  This is an alias of the main
/// GET /challenge?wallet=... but scoped to the peer challenge store so
/// peer challenge TTLs can differ.
pub async fn get_peer_challenge(
    State(app): State<AppState>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Result<Json<PeerChallengeResponse>, (StatusCode, Json<UpdateResponse>)> {
    let now = chrono::Utc::now().timestamp() as u64;

    let wallet_str = params.get("wallet").cloned().ok_or_else(|| {
        (
            StatusCode::BAD_REQUEST,
            Json(UpdateResponse {
                status: "error",
                message: "missing ?wallet= query parameter".into(),
            }),
        )
    })?;

    let wallet: Address = wallet_str.parse().map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            Json(UpdateResponse {
                status: "error",
                message: "invalid wallet address".into(),
            }),
        )
    })?;

    let batch_updater = app.6;
    let flow_rpc = &app.7;
    let whitelist_cache = &app.12;

    let whitelisted = whitelist_cache
        .is_whitelisted(batch_updater, wallet, flow_rpc)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "whitelist check failed in GET /peers/challenge");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(UpdateResponse {
                    status: "error",
                    message: "failed to verify whitelist status".into(),
                }),
            )
        })?;

    if !whitelisted {
        tracing::warn!(wallet = %wallet, "GET /peers/challenge: wallet not whitelisted");
        return Err((
            StatusCode::FORBIDDEN,
            Json(UpdateResponse {
                status: "error",
                message: format!("wallet {wallet} is not whitelisted on OracleBatchUpdater"),
            }),
        ));
    }

    let wallet_key = format!("{wallet:#x}");
    let mut store = app.11.lock().await;
    store.retain(|_, (_, exp)| *exp > now);

    // Return the existing non-expired challenge if one exists.
    // This bounds the store to at most N entries (N = whitelisted wallets)
    // and prevents memory exhaustion via challenge spam.
    if let Some((existing_challenge, existing_expires_at)) = store.get(&wallet_key) {
        return Ok(Json(PeerChallengeResponse {
            challenge: existing_challenge.clone(),
            expires_at: *existing_expires_at,
        }));
    }

    let nonce = uuid::Uuid::new_v4().to_string();
    let challenge = format!("peer-auth:{nonce}");
    let expires_at = now + PEER_CHALLENGE_TTL_SECS.max(WALLET_CHALLENGE_TTL_SECS);

    store.insert(wallet_key, (challenge.clone(), expires_at));

    Ok(Json(PeerChallengeResponse { challenge, expires_at }))
}

// ── /peers/register ───────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct PeerRegisterRequest {
    /// The registering peer's public URL.
    url: String,
    /// Wallet address of the registering peer.
    wallet: String,
    /// Signature over the challenge obtained from GET /peers/challenge?wallet=.
    signature: String,
}

/// POST /peers/register
///
/// Registers a peer keeper.  Requires unified challenge-response auth.
/// The wallet must be whitelisted on OracleBatchUpdater.
pub async fn peer_register(
    State(app): State<AppState>,
    Json(body): Json<PeerRegisterRequest>,
) -> Result<Json<UpdateResponse>, (StatusCode, Json<UpdateResponse>)> {
    let batch_updater = app.6;
    let flow_rpc = &app.7;
    let whitelist_cache = &app.12;

    if let Err(reason) = validate_peer_url(&body.url) {
        tracing::warn!(peer_url = %body.url, reason = %reason, "peer_register: rejected URL");
        return Err((
            StatusCode::BAD_REQUEST,
            Json(UpdateResponse {
                status: "error",
                message: reason,
            }),
        ));
    }

    let (wallet, _sig) = verify_auth_for(
        &body.wallet,
        &body.signature,
        &app.11,
        batch_updater,
        flow_rpc,
        whitelist_cache,
        "/peers/register",
    )
    .await?;

    let now = chrono::Utc::now().timestamp() as u64;

    {
        let mut registry = app.4.lock().await;
        registry.insert(
            body.url.clone(),
            PeerInfo {
                url: body.url.clone(),
                wallet,
                registered_at: now,
                last_seen: Some(now),
                last_push_at: None,
                active: true,
            },
        );
    }

    tracing::info!(
        peer_url = %body.url,
        peer_wallet = %wallet,
        "peer registered successfully"
    );

    {
        let registry = app.4.lock().await;
        let other_peers: Vec<String> = registry
            .keys()
            .filter(|url| **url != body.url)
            .cloned()
            .collect();
        let new_peer_url = body.url.clone();
        let new_peer_wallet = format!("{wallet:#x}");
        let our_url = app.8.clone();
        let signer = app.9.clone();
        let peer_registry = app.4.clone();
        let batch_updater_copy = app.6;
        let flow_rpc_copy = app.7.clone();

        tokio::spawn(async move {
            for peer_url in other_peers {
                if validate_peer_url(&peer_url).is_err() {
                    continue;
                }
                notify_peer_of_new_peer(&peer_url, &new_peer_url, &new_peer_wallet, &our_url, &signer, &peer_registry, batch_updater_copy, &flow_rpc_copy).await;
            }
        });
    }

    Ok(Json(UpdateResponse {
        status: "ok",
        message: "registered".into(),
    }))
}

// ── /peers/verify — kept for backward compat but now just an alias ────────────

/// Kept for backward compatibility with peers that still use the two-step
/// register/verify flow.  In the new model, /peers/register is the single step.
/// This endpoint accepts the same auth body and is a no-op success if the peer
/// is already registered.
#[derive(Deserialize)]
pub struct PeerVerifyRequest {
    pub url: String,
    pub wallet: String,
    pub signature: String,
}

#[derive(Serialize)]
pub(crate) struct PeerVerifyResponse {
    /// Empty string — API keys are no longer issued.
    pub api_key: String,
}

pub async fn peer_verify(
    State(app): State<AppState>,
    Json(body): Json<PeerVerifyRequest>,
) -> Result<Json<PeerVerifyResponse>, (StatusCode, Json<UpdateResponse>)> {
    let batch_updater = app.6;
    let flow_rpc = &app.7;
    let whitelist_cache = &app.12;

    if let Err(reason) = validate_peer_url(&body.url) {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(UpdateResponse {
                status: "error",
                message: reason,
            }),
        ));
    }

    let (wallet, _sig) = verify_auth_for(
        &body.wallet,
        &body.signature,
        &app.11,
        batch_updater,
        flow_rpc,
        whitelist_cache,
        "/peers/verify",
    )
    .await?;

    let now = chrono::Utc::now().timestamp() as u64;

    {
        let mut registry = app.4.lock().await;
        registry
            .entry(body.url.clone())
            .and_modify(|info| {
                info.wallet = wallet;
                info.last_seen = Some(now);
                info.active = true;
            })
            .or_insert_with(|| PeerInfo {
                url: body.url.clone(),
                wallet,
                registered_at: now,
                last_seen: Some(now),
                last_push_at: None,
                active: true,
            });
    }

    tracing::info!(peer_url = %body.url, peer_wallet = %wallet, "peer_verify: peer upserted");

    let keeper_url = app.8.clone();
    let signer = app.9.clone();
    let peer_url_for_reverse = body.url.clone();
    let peer_registry_for_reverse = app.4.clone();
    let batch_updater_for_reverse = app.6;
    let flow_rpc_for_reverse = app.7.clone();

    if let (Some(our_url), Some(signer)) = (keeper_url, signer) {
        tokio::spawn(async move {
            match peers::register_with_peer(
                &peer_url_for_reverse,
                &our_url,
                &signer,
                batch_updater_for_reverse,
                &flow_rpc_for_reverse,
            )
            .await
            {
                Ok(()) => {
                    tracing::info!(peer_url = %peer_url_for_reverse, "reverse registration succeeded");
                }
                Err(e) => {
                    tracing::warn!(peer_url = %peer_url_for_reverse, error = %e, "reverse registration failed");
                }
            }
            let _ = peer_registry_for_reverse; // keep alive
        });
    }

    Ok(Json(PeerVerifyResponse { api_key: String::new() }))
}

// ── /peers/notify ─────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct PeerNotifyRequest {
    /// URL of the newly-appeared peer being announced.
    url: String,
    /// Wallet of the newly-appeared peer (informational; we verify on-chain).
    wallet: String,
    /// Auth: our wallet that signed the challenge.
    auth_wallet: String,
    /// Auth: signature over the challenge for `auth_wallet`.
    auth_signature: String,
}

pub async fn peer_notify(
    State(app): State<AppState>,
    Json(body): Json<PeerNotifyRequest>,
) -> Result<Json<UpdateResponse>, (StatusCode, Json<UpdateResponse>)> {
    let batch_updater = app.6;
    let flow_rpc = app.7.clone();
    let whitelist_cache = &app.12;

    if let Err(reason) = validate_peer_url(&body.url) {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(UpdateResponse {
                status: "error",
                message: reason,
            }),
        ));
    }

    verify_auth_for(
        &body.auth_wallet,
        &body.auth_signature,
        &app.11,
        batch_updater,
        &flow_rpc,
        whitelist_cache,
        "/peers/notify",
    )
    .await?;

    // The announced peer's wallet is verified independently — the caller's auth
    // does not vouch for the peer they're announcing.
    let peer_wallet: Address = body.wallet.parse().map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            Json(UpdateResponse {
                status: "error",
                message: "invalid peer wallet address".into(),
            }),
        )
    })?;

    let peer_whitelisted = oracle::is_whitelisted(batch_updater, peer_wallet, &flow_rpc)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "failed to check isWhitelisted for notified peer");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(UpdateResponse {
                    status: "error",
                    message: "failed to verify peer whitelist".into(),
                }),
            )
        })?;

    if !peer_whitelisted {
        return Err((
            StatusCode::FORBIDDEN,
            Json(UpdateResponse {
                status: "error",
                message: format!("peer wallet {peer_wallet} is not whitelisted"),
            }),
        ));
    }

    let peer_url = body.url.clone();
    let keeper_url = app.8.clone();
    let signer = app.9.clone();
    let peer_registry = app.4.clone();

    if let (Some(our_url), Some(signer)) = (keeper_url, signer) {
        let flow_rpc_copy = flow_rpc.clone();
        tokio::spawn(async move {
            {
                let registry = peer_registry.lock().await;
                if registry.contains_key(&peer_url) {
                    tracing::info!(peer_url = %peer_url, "peer already registered, skipping notify-triggered registration");
                    return;
                }
            }

            match peers::register_with_peer(
                &peer_url,
                &our_url,
                &signer,
                batch_updater,
                &flow_rpc_copy,
            )
            .await
            {
                Ok(()) => {
                    tracing::info!(peer_url = %peer_url, "registered with notified peer");
                }
                Err(e) => {
                    tracing::warn!(peer_url = %peer_url, error = %e, "failed to register with notified peer");
                }
            }
        });
    }

    Ok(Json(UpdateResponse {
        status: "ok",
        message: "notification received".into(),
    }))
}

// ── Internal helpers ──────────────────────────────────────────────────────────

/// Best-effort: notify `peer_url` about a new peer joining the mesh.
async fn notify_peer_of_new_peer(
    peer_url: &str,
    new_peer_url: &str,
    new_peer_wallet: &str,
    our_url: &Option<String>,
    signer: &Option<alloy::signers::local::PrivateKeySigner>,
    _peer_registry: &crate::peer_registry::PeerRegistry,
    batch_updater: Address,
    flow_rpc: &str,
) {
    let (our_url, signer) = match (our_url.as_deref(), signer.as_ref()) {
        (Some(u), Some(s)) => (u, s),
        _ => return, // can't auth without signer
    };

    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
    {
        Ok(c) => c,
        Err(_) => return,
    };

    let our_wallet = signer.address();
    let challenge_url = format!(
        "{}/peers/challenge?wallet={our_wallet:#x}",
        peer_url.trim_end_matches('/')
    );
    let challenge: String = match client.get(&challenge_url).send().await {
        Ok(resp) if resp.status().is_success() => {
            match resp.json::<serde_json::Value>().await {
                Ok(j) => match j["challenge"].as_str() {
                    Some(c) => c.to_string(),
                    None => return,
                },
                Err(_) => return,
            }
        }
        _ => return,
    };

    use alloy::signers::Signer;
    let signature = match signer.sign_message(challenge.as_bytes()).await {
        Ok(s) => format!("0x{}", alloy::hex::encode(s.as_bytes())),
        Err(_) => return,
    };

    let notify_url = format!("{}/peers/notify", peer_url.trim_end_matches('/'));
    let payload = serde_json::json!({
        "url": new_peer_url,
        "wallet": new_peer_wallet,
        "auth_wallet": format!("{our_wallet:#x}"),
        "auth_signature": signature,
    });
    let _ = client.post(&notify_url).json(&payload).send().await;
    let _ = (our_url, batch_updater, flow_rpc); // suppress unused warnings
}
