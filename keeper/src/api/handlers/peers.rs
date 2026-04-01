use alloy::primitives::{Address, Signature};
use axum::{extract::State, http::StatusCode, response::Json};
use serde::{Deserialize, Serialize};

use crate::oracle;
use crate::peer_registry::PeerInfo;
use crate::peers;

use super::super::{AppState, PEER_CHALLENGE_TTL_SECS};
use super::update::UpdateResponse;

#[derive(Deserialize)]
pub struct PeerRegisterRequest {
    url: String,
}

#[derive(Serialize)]
pub(crate) struct PeerRegisterResponse {
    challenge: String,
    expires_at: u64,
}

#[derive(Deserialize)]
pub struct PeerVerifyRequest {
    url: String,
    signature: String,
}

#[derive(Serialize)]
pub(crate) struct PeerVerifyResponse {
    api_key: String,
}

#[derive(Deserialize)]
pub struct PeerNotifyRequest {
    url: String,
    wallet: String,
}

pub async fn peer_register(
    State(app): State<AppState>,
    Json(body): Json<PeerRegisterRequest>,
) -> Result<Json<PeerRegisterResponse>, (StatusCode, Json<UpdateResponse>)> {
    let nonce = uuid::Uuid::new_v4().to_string();
    let challenge = format!("peer-register:{nonce}");
    let expires_at = chrono::Utc::now().timestamp() as u64 + PEER_CHALLENGE_TTL_SECS;

    let mut pending = app.5.lock().await;
    // Purge expired entries
    let now = chrono::Utc::now().timestamp() as u64;
    pending.retain(|_, (_, exp)| *exp > now);
    pending.insert(body.url.clone(), (challenge.clone(), expires_at));

    tracing::info!(peer_url = %body.url, "peer registration challenge issued");

    Ok(Json(PeerRegisterResponse {
        challenge,
        expires_at,
    }))
}

pub async fn peer_verify(
    State(app): State<AppState>,
    Json(body): Json<PeerVerifyRequest>,
) -> Result<Json<PeerVerifyResponse>, (StatusCode, Json<UpdateResponse>)> {
    let now = chrono::Utc::now().timestamp() as u64;
    let batch_updater = app.6;
    let flow_rpc = &app.7;

    // 1. Look up and consume the pending challenge
    let challenge = {
        let mut pending = app.5.lock().await;
        match pending.remove(&body.url) {
            None => {
                return Err((
                    StatusCode::BAD_REQUEST,
                    Json(UpdateResponse {
                        status: "error",
                        message: "no pending registration for this URL".into(),
                    }),
                ));
            }
            Some((_challenge, expires_at)) if now > expires_at => {
                return Err((
                    StatusCode::UNAUTHORIZED,
                    Json(UpdateResponse {
                        status: "error",
                        message: "peer challenge expired".into(),
                    }),
                ));
            }
            Some((challenge, _)) => challenge,
        }
    };

    // 2. Parse signature and recover address
    let sig: Signature = body.signature.parse().map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            Json(UpdateResponse {
                status: "error",
                message: "invalid signature format".into(),
            }),
        )
    })?;

    let recovered = sig
        .recover_address_from_msg(challenge.as_bytes())
        .map_err(|_| {
            (
                StatusCode::UNAUTHORIZED,
                Json(UpdateResponse {
                    status: "error",
                    message: "could not recover signer from peer signature".into(),
                }),
            )
        })?;

    // 3. Check on-chain whitelist
    let whitelisted = oracle::is_whitelisted(batch_updater, recovered, flow_rpc)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "failed to check isWhitelisted on-chain");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(UpdateResponse {
                    status: "error",
                    message: "failed to verify whitelist status on-chain".into(),
                }),
            )
        })?;

    if !whitelisted {
        tracing::warn!(
            peer_wallet = %recovered,
            peer_url = %body.url,
            "peer wallet not whitelisted on-chain"
        );
        return Err((
            StatusCode::FORBIDDEN,
            Json(UpdateResponse {
                status: "error",
                message: format!("wallet {recovered} is not whitelisted on OracleBatchUpdater"),
            }),
        ));
    }

    // 4. Generate API key and store in registry
    let api_key = uuid::Uuid::new_v4().to_string();

    {
        let mut registry = app.4.lock().await;
        registry.insert(
            body.url.clone(),
            PeerInfo {
                url: body.url.clone(),
                wallet: recovered,
                incoming_api_key: api_key.clone(),
                outgoing_api_key: None,
                registered_at: now,
                last_seen: Some(now),
                last_push_at: None,
                active: true,
            },
        );
    }

    tracing::info!(
        peer_url = %body.url,
        peer_wallet = %recovered,
        "peer registered successfully"
    );

    // 5. Broadcast new peer to existing peers
    {
        let registry = app.4.lock().await;
        let other_peers: Vec<(String, Option<String>)> = registry
            .iter()
            .filter(|(url, _)| **url != body.url)
            .map(|(url, info)| (url.clone(), info.outgoing_api_key.clone()))
            .collect();
        let new_peer_url = body.url.clone();
        let new_peer_wallet = format!("{recovered:#x}");
        tokio::spawn(async move {
            for (peer_url, _) in other_peers {
                let notify_url = format!("{}/peers/notify", peer_url.trim_end_matches('/'));
                let client = reqwest::Client::builder()
                    .timeout(std::time::Duration::from_secs(5))
                    .build();
                let client = match client {
                    Ok(c) => c,
                    Err(_) => continue,
                };
                let payload = serde_json::json!({
                    "url": new_peer_url,
                    "wallet": new_peer_wallet,
                });
                match client.post(&notify_url).json(&payload).send().await {
                    Ok(_) => {
                        tracing::info!(
                            peer = %peer_url,
                            new_peer = %new_peer_url,
                            "notified peer about new peer"
                        );
                    }
                    Err(e) => {
                        tracing::warn!(
                            peer = %peer_url,
                            error = %e,
                            "failed to notify peer about new peer"
                        );
                    }
                }
            }
        });
    }

    // 6. Initiate reverse registration if we have a keeper_url configured
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
                Ok(outgoing_key) => {
                    let mut registry = peer_registry_for_reverse.lock().await;
                    if let Some(info) = registry.get_mut(&peer_url_for_reverse) {
                        info.outgoing_api_key = Some(outgoing_key);
                    }
                    tracing::info!(
                        peer_url = %peer_url_for_reverse,
                        "reverse registration succeeded"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        peer_url = %peer_url_for_reverse,
                        error = %e,
                        "reverse registration failed"
                    );
                }
            }
        });
    }

    Ok(Json(PeerVerifyResponse { api_key }))
}

pub async fn peer_notify(
    State(app): State<AppState>,
    Json(body): Json<PeerNotifyRequest>,
) -> Result<Json<UpdateResponse>, (StatusCode, Json<UpdateResponse>)> {
    let batch_updater = app.6;
    let flow_rpc = app.7.clone();
    let keeper_url = app.8.clone();
    let signer = app.9.clone();
    let peer_registry = app.4.clone();

    // Parse wallet address
    let wallet: Address = body.wallet.parse().map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            Json(UpdateResponse {
                status: "error",
                message: "invalid wallet address format".into(),
            }),
        )
    })?;

    // Verify on-chain whitelist
    let whitelisted = oracle::is_whitelisted(batch_updater, wallet, &flow_rpc)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "failed to check isWhitelisted for notified peer");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(UpdateResponse {
                    status: "error",
                    message: "failed to verify whitelist".into(),
                }),
            )
        })?;

    if !whitelisted {
        return Err((
            StatusCode::FORBIDDEN,
            Json(UpdateResponse {
                status: "error",
                message: format!("wallet {wallet} is not whitelisted"),
            }),
        ));
    }

    let peer_url = body.url.clone();

    // Spawn registration task if we have keeper_url and signer
    if let (Some(our_url), Some(signer)) = (keeper_url, signer) {
        tokio::spawn(async move {
            // Check if we already have this peer registered
            {
                let registry = peer_registry.lock().await;
                if registry.contains_key(&peer_url) {
                    tracing::info!(peer_url = %peer_url, "peer already registered, skipping");
                    return;
                }
            }

            match peers::register_with_peer(
                &peer_url,
                &our_url,
                &signer,
                batch_updater,
                &flow_rpc,
            )
            .await
            {
                Ok(outgoing_key) => {
                    let mut registry = peer_registry.lock().await;
                    if let Some(info) = registry.get_mut(&peer_url) {
                        info.outgoing_api_key = Some(outgoing_key);
                    }
                    tracing::info!(peer_url = %peer_url, "registered with notified peer");
                }
                Err(e) => {
                    tracing::warn!(
                        peer_url = %peer_url,
                        error = %e,
                        "failed to register with notified peer"
                    );
                }
            }
        });
    }

    Ok(Json(UpdateResponse {
        status: "ok",
        message: "notification received".into(),
    }))
}
