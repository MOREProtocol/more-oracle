use alloy::primitives::{Address, PrimitiveSignature as Signature};
use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::Json,
};
use serde::{Deserialize, Serialize};
use crate::api::BridgeWarning;
use crate::oracle;
use crate::peers;

use super::super::{AppState, BRIDGE_WARNING_TIMEOUT_SECS, CHALLENGE_TTL_SECS};
use super::update::UpdateResponse;

// ── GET /curator/bridge-challenge ────────────────────────────────────────────

#[derive(Deserialize)]
pub struct BridgeChallengeQuery {
    pub spoke: String,
    pub delta: String,
}

#[derive(Serialize)]
pub struct BridgeChallengeResponse {
    /// Sign this string. It encodes spoke + delta so the signature is bound to
    /// this specific operation, not a reusable generic nonce.
    pub challenge: String,
    pub expires_at: u64,
}

/// GET /curator/bridge-challenge?spoke=arbitrum&delta=480000000
///
/// Issues a challenge that embeds the specific spoke and expected delta.
/// The curator must sign the returned `challenge` string exactly.
/// No wallet parameter needed — the signer is recovered from the signature on POST.
pub async fn get_bridge_challenge(
    State(app): State<AppState>,
    Query(params): Query<BridgeChallengeQuery>,
) -> Result<Json<BridgeChallengeResponse>, (StatusCode, Json<UpdateResponse>)> {
    if params.spoke.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(UpdateResponse {
                status: "error",
                message: "missing spoke parameter".into(),
            }),
        ));
    }

    // Validate delta is a parseable u128 > 0 at challenge time so the curator
    // gets an early error rather than discovering it at POST time.
    let delta: u128 = params.delta.parse().map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            Json(UpdateResponse {
                status: "error",
                message: "delta must be a valid u128 decimal string".into(),
            }),
        )
    })?;

    if delta == 0 {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(UpdateResponse {
                status: "error",
                message: "delta must be > 0".into(),
            }),
        ));
    }

    let now = chrono::Utc::now().timestamp() as u64;
    let nonce = uuid::Uuid::new_v4().to_string();

    // The challenge embeds spoke and delta — signing it binds the signature to
    // this specific bridge operation.
    let challenge = format!("bridge-warning:{}:{}:{}", nonce, params.spoke, delta);
    let expires_at = now + CHALLENGE_TTL_SECS;

    {
        let mut store = app.3.lock().await;
        store.retain(|_, exp| *exp > now);
        store.insert(challenge.clone(), expires_at);
    }

    Ok(Json(BridgeChallengeResponse { challenge, expires_at }))
}

// ── POST /curator/bridge-warning ─────────────────────────────────────────────

#[derive(Deserialize)]
pub struct CuratorBridgeWarningRequest {
    /// The challenge string returned by GET /curator/bridge-challenge.
    pub challenge: String,
    /// EIP-191 signature over the challenge.
    pub signature: String,
    /// Must match what was encoded in the challenge.
    pub spoke: String,
    /// Must match what was encoded in the challenge, as a decimal string.
    pub expected_delta: String,
}

/// POST /curator/bridge-warning
///
/// Curator-only. Signals that a bridge is about to move a large amount into
/// a spoke vault, temporarily bypassing off-chain drift protection for that
/// spoke until the expected delta is detected (≥90%) or the 6h timeout expires.
///
/// Authentication: the caller signs the challenge returned by
/// GET /curator/bridge-challenge. The signer address is recovered from the
/// signature — no wallet parameter needed. The challenge encodes the spoke
/// and delta, so the signature is bound to this specific operation.
///
/// If this keeper holds ORACLE_OWNER_PRIVATE_KEY, it calls setMaxChangeBps(0)
/// on the oracle for the bridge duration, restoring the original value on completion.
///
/// The warning is propagated to all registered peer keepers.
pub async fn curator_bridge_warning(
    State(app): State<AppState>,
    Json(body): Json<CuratorBridgeWarningRequest>,
) -> Result<Json<UpdateResponse>, (StatusCode, Json<UpdateResponse>)> {
    let curator = app.2;
    let flow_rpc = app.7.clone();
    let batch_updater = app.6;

    let now = chrono::Utc::now().timestamp() as u64;

    // 1. Consume the single-use challenge from the store
    let challenge = {
        let mut store = app.3.lock().await;
        match store.remove(&body.challenge) {
            None => {
                tracing::warn!(
                    challenge = %body.challenge,
                    "/curator/bridge-warning: no pending challenge (expired or never issued)"
                );
                return Err((
                    StatusCode::UNAUTHORIZED,
                    Json(UpdateResponse {
                        status: "error",
                        message: "no pending challenge — call GET /curator/bridge-challenge first".into(),
                    }),
                ));
            }
            Some(exp) if now > exp => {
                tracing::warn!("/curator/bridge-warning: challenge expired");
                return Err((
                    StatusCode::UNAUTHORIZED,
                    Json(UpdateResponse {
                        status: "error",
                        message: "challenge expired".into(),
                    }),
                ));
            }
            Some(_) => body.challenge.clone(),
        }
    };

    // 2. Verify the challenge encodes the same spoke + delta as the request body.
    //    Format: "bridge-warning:{nonce}:{spoke}:{delta}"
    let expected_delta: u128 = body.expected_delta.parse().map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            Json(UpdateResponse {
                status: "error",
                message: "expected_delta must be a valid u128 decimal string".into(),
            }),
        )
    })?;

    let expected_suffix = format!(":{}:{}", body.spoke, expected_delta);
    if !challenge.starts_with("bridge-warning:") || !challenge.ends_with(&expected_suffix) {
        tracing::warn!(
            challenge = %challenge,
            spoke = %body.spoke,
            expected_delta,
            "/curator/bridge-warning: challenge does not match spoke/delta"
        );
        return Err((
            StatusCode::BAD_REQUEST,
            Json(UpdateResponse {
                status: "error",
                message: "challenge does not match the submitted spoke and expected_delta".into(),
            }),
        ));
    }

    // 3. Recover the signer from the signature — no wallet parameter needed.
    let sig: Signature = body.signature.parse().map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            Json(UpdateResponse {
                status: "error",
                message: "invalid signature format".into(),
            }),
        )
    })?;

    let recovered: Address = sig
        .recover_address_from_msg(challenge.as_bytes())
        .map_err(|_| {
            (
                StatusCode::UNAUTHORIZED,
                Json(UpdateResponse {
                    status: "error",
                    message: "could not recover signer from signature".into(),
                }),
            )
        })?;

    // 4. Verify the recovered address is the vault curator.
    if recovered != curator {
        tracing::warn!(
            recovered = %recovered,
            curator = %curator,
            "/curator/bridge-warning: signer is not the curator"
        );
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(UpdateResponse {
                status: "error",
                message: format!("signer {recovered} is not the vault curator"),
            }),
        ));
    }

    // 5. Find the spoke config
    let spokes = &app.10;
    let spoke_cfg = spokes.iter().find(|s| s.name == body.spoke).ok_or_else(|| {
        (
            StatusCode::BAD_REQUEST,
            Json(UpdateResponse {
                status: "error",
                message: format!("spoke '{}' not found in config", body.spoke),
            }),
        )
    })?;

    let oracle_addr_str = spoke_cfg.oracle_address.clone().ok_or_else(|| {
        (
            StatusCode::BAD_REQUEST,
            Json(UpdateResponse {
                status: "error",
                message: format!("spoke '{}' has no oracle address configured", body.spoke),
            }),
        )
    })?;

    // 6. Read current storedTotalAssets as the pre-bridge baseline
    let pre_bridge_value: u128 = match oracle::stored_total_assets(&oracle_addr_str, &flow_rpc).await {
        Ok(v) => v.try_into().unwrap_or(0),
        Err(e) => {
            tracing::warn!(spoke = %body.spoke, error = %e, "bridge-warning: could not read storedTotalAssets, using 0");
            0
        }
    };

    // 7. Optionally disable circuit breaker and record original value
    let oracle_owner_signer = app.15.clone();
    let original_max_change_bps: Option<u64> = if oracle_owner_signer.is_some() {
        match oracle::get_max_change_bps(&oracle_addr_str, &flow_rpc).await {
            Ok(bps) if bps > 0 => {
                let signer = oracle_owner_signer.as_ref().unwrap().clone();
                match oracle::set_max_change_bps(&oracle_addr_str, &flow_rpc, signer, 0).await {
                    Ok(tx) => tracing::info!(
                        spoke = %body.spoke, original_bps = bps, tx_hash = %tx,
                        "bridge-warning: circuit breaker disabled (setMaxChangeBps(0))"
                    ),
                    Err(e) => tracing::warn!(
                        spoke = %body.spoke, error = %e,
                        "bridge-warning: setMaxChangeBps(0) failed"
                    ),
                }
                Some(bps)
            }
            Ok(_) => None, // already 0 — nothing to restore
            Err(e) => {
                tracing::warn!(spoke = %body.spoke, error = %e, "bridge-warning: could not read maxChangeBps");
                None
            }
        }
    } else {
        None
    };

    let timeout_at = now + BRIDGE_WARNING_TIMEOUT_SECS;

    let warning = BridgeWarning {
        spoke_name: body.spoke.clone(),
        pre_bridge_value,
        expected_delta,
        timeout_at,
        original_max_change_bps,
    };

    {
        let mut store = app.14.lock().await;
        store.insert(body.spoke.clone(), warning);
    }

    tracing::info!(
        spoke = %body.spoke,
        curator = %recovered,
        expected_delta,
        pre_bridge_value,
        timeout_at,
        has_oracle_owner_key = oracle_owner_signer.is_some(),
        "bridge warning activated"
    );

    if let Some(tg) = &app.13 {
        tg.send(format!(
            "🌉 <b>Bridge warning active</b>\n\
             Spoke: <code>{}</code>\n\
             Expected delta: <code>{}</code>\n\
             Drift bypass: enabled | Timeout: 6h",
            body.spoke, expected_delta,
        ));
    }

    // 8. Propagate to peers asynchronously (best-effort)
    let signer = app.9.clone();
    let peer_registry = app.4.clone();
    let spoke_name = body.spoke.clone();
    let flow_rpc_copy = flow_rpc.clone();

    if let Some(signer) = signer {
        tokio::spawn(async move {
            peers::propagate_bridge_warning(
                &spoke_name,
                expected_delta,
                pre_bridge_value,
                timeout_at,
                original_max_change_bps,
                &signer,
                batch_updater,
                &flow_rpc_copy,
                &peer_registry,
            )
            .await;
        });
    }

    Ok(Json(UpdateResponse {
        status: "ok",
        message: format!("bridge warning activated for spoke '{}'", body.spoke),
    }))
}

// ── POST /peers/bridge-warning ────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct PeerBridgeWarningRequest {
    pub auth_wallet: String,
    pub auth_signature: String,
    pub spoke: String,
    pub expected_delta: String,
    pub pre_bridge_value: String,
    pub timeout_at: u64,
    pub original_max_change_bps: Option<u64>,
}

/// POST /peers/bridge-warning
///
/// Peer-authenticated. Receives a bridge warning propagated from another keeper.
/// Stores it locally and optionally calls setMaxChangeBps(0) if this keeper
/// holds the oracle owner key. Does NOT propagate further (prevents loops).
pub async fn peer_bridge_warning(
    State(app): State<AppState>,
    Json(body): Json<PeerBridgeWarningRequest>,
) -> Result<Json<UpdateResponse>, (StatusCode, Json<UpdateResponse>)> {
    let batch_updater = app.6;
    let flow_rpc = app.7.clone();
    let whitelist_cache = &app.12;

    // Peer auth — wallet required here because multiple peers exist and each
    // needs its own per-wallet challenge from GET /peers/challenge?wallet=...
    super::update::verify_auth_for(
        &body.auth_wallet,
        &body.auth_signature,
        &app.11,
        batch_updater,
        &flow_rpc,
        whitelist_cache,
        "/peers/bridge-warning",
    )
    .await?;

    let expected_delta: u128 = body.expected_delta.parse().map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            Json(UpdateResponse {
                status: "error",
                message: "expected_delta must be a valid u128 decimal string".into(),
            }),
        )
    })?;

    let pre_bridge_value: u128 = body.pre_bridge_value.parse().map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            Json(UpdateResponse {
                status: "error",
                message: "pre_bridge_value must be a valid u128 decimal string".into(),
            }),
        )
    })?;

    // If this keeper holds the oracle owner PK, disable the circuit breaker
    // regardless of whether the propagating keeper already did it.
    // This handles the case where the curator sent the warning to a peer without
    // the oracle owner PK, which then propagated here with original_max_change_bps=None.
    let oracle_owner_signer = app.15.clone();
    let local_original_max_change_bps: Option<u64> = if let Some(signer) = oracle_owner_signer {
        let oracle_addr_str = app
            .10
            .iter()
            .find(|s| s.name == body.spoke)
            .and_then(|s| s.oracle_address.clone());

        if let Some(addr) = oracle_addr_str {
            // Use the propagated value if available; otherwise read from chain.
            let orig_bps = match body.original_max_change_bps {
                Some(bps) => Some(bps),
                None => oracle::get_max_change_bps(&addr, &flow_rpc).await.ok(),
            };

            if let Some(bps) = orig_bps {
                if bps > 0 {
                    let flow_rpc_copy = flow_rpc.clone();
                    let addr_copy = addr.clone();
                    let spoke_name = body.spoke.clone();
                    tokio::spawn(async move {
                        match oracle::set_max_change_bps(&addr_copy, &flow_rpc_copy, signer, 0).await {
                            Ok(tx) => tracing::info!(
                                spoke = %spoke_name, original_bps = bps, tx_hash = %tx,
                                "peer bridge-warning: circuit breaker disabled"
                            ),
                            Err(e) => tracing::warn!(
                                spoke = %spoke_name, error = %e,
                                "peer bridge-warning: setMaxChangeBps(0) failed"
                            ),
                        }
                    });
                    Some(bps)
                } else {
                    None // already disabled — nothing to restore
                }
            } else {
                None
            }
        } else {
            None
        }
    } else {
        // No oracle owner PK — use what the propagating keeper stored
        body.original_max_change_bps
    };

    let warning = BridgeWarning {
        spoke_name: body.spoke.clone(),
        pre_bridge_value,
        expected_delta,
        timeout_at: body.timeout_at,
        // Store the value we know is correct for this keeper
        original_max_change_bps: local_original_max_change_bps,
    };

    {
        let mut store = app.14.lock().await;
        store.insert(body.spoke.clone(), warning);
    }

    tracing::info!(
        spoke = %body.spoke,
        expected_delta,
        pre_bridge_value,
        timeout_at = body.timeout_at,
        "peer bridge warning stored"
    );

    Ok(Json(UpdateResponse {
        status: "ok",
        message: format!("bridge warning stored for spoke '{}'", body.spoke),
    }))
}

