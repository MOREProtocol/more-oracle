use alloy::primitives::{Address, PrimitiveSignature as Signature};
use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::Json,
};
use serde::{Deserialize, Serialize};

use super::super::{AppState, CHALLENGE_TTL_SECS, WALLET_CHALLENGE_TTL_SECS};

#[derive(Deserialize)]
pub struct ChallengeQuery {
    pub wallet: Option<String>,
}

#[derive(Serialize)]
pub(crate) struct ChallengeResponse {
    challenge: String,
    expires_at: u64,
}

/// Auth request body for protected endpoints: wallet + signature over the challenge.
#[derive(Deserialize)]
pub struct UpdateRequest {
    pub wallet: String,
    pub signature: String,
}

#[derive(Serialize)]
pub struct UpdateResponse {
    pub status: &'static str,
    pub message: String,
}

/// GET /challenge?wallet=0x...
///
/// If `wallet` is provided, checks on-chain whitelist (with 5-min cache) and
/// issues a per-wallet challenge.  If wallet is omitted, issues a generic
/// challenge used only by the curator-based POST /update flow.
pub async fn get_challenge(
    State(app): State<AppState>,
    Query(params): Query<ChallengeQuery>,
) -> Result<Json<ChallengeResponse>, (StatusCode, Json<UpdateResponse>)> {
    let now = chrono::Utc::now().timestamp() as u64;

    if let Some(wallet_str) = params.wallet {
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
                tracing::error!(error = %e, "whitelist check failed in GET /challenge");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(UpdateResponse {
                        status: "error",
                        message: "failed to verify whitelist status".into(),
                    }),
                )
            })?;

        if !whitelisted {
            tracing::warn!(wallet = %wallet, "GET /challenge: wallet not whitelisted");
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
            return Ok(Json(ChallengeResponse {
                challenge: existing_challenge.clone(),
                expires_at: *existing_expires_at,
            }));
        }

        let nonce = uuid::Uuid::new_v4().to_string();
        let challenge = format!("keeper-auth:{nonce}");
        let expires_at = now + WALLET_CHALLENGE_TTL_SECS;

        store.insert(wallet_key, (challenge.clone(), expires_at));

        Ok(Json(ChallengeResponse { challenge, expires_at }))
    } else {
        let nonce = uuid::Uuid::new_v4().to_string();
        let challenge = format!("update:{nonce}");
        let expires_at = now + CHALLENGE_TTL_SECS;

        let mut store = app.3.lock().await;
        store.retain(|_, exp| *exp > now);
        store.insert(challenge.clone(), expires_at);

        Ok(Json(ChallengeResponse { challenge, expires_at }))
    }
}

/// POST /update — trigger a manual update cycle.
///
/// Requires unified challenge-response: `{ wallet, signature }`.
/// The wallet must be whitelisted AND must match the curator address.
pub async fn trigger_update(
    State(app): State<AppState>,
    Json(body): Json<UpdateRequest>,
) -> Result<Json<UpdateResponse>, (StatusCode, Json<UpdateResponse>)> {
    let curator = app.2;
    let batch_updater = app.6;
    let flow_rpc = &app.7;
    let whitelist_cache = &app.12;

    let (wallet, _sig_bytes) = verify_auth_for(
        &body.wallet,
        &body.signature,
        &app.11,
        batch_updater,
        flow_rpc,
        whitelist_cache,
        "/update",
    )
    .await?;

    if wallet != curator {
        tracing::warn!(
            recovered = %wallet,
            curator = %curator,
            "unauthorized /update attempt — not the curator"
        );
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(UpdateResponse {
                status: "error",
                message: format!("wallet {wallet} is not the vault curator"),
            }),
        ));
    }

    tracing::info!(curator = %curator, "curator-signed update triggered");
    app.1.notify_one();

    Ok(Json(UpdateResponse {
        status: "triggered",
        message: "update cycle triggered".into(),
    }))
}

/// Shared auth helper: consumes the single-use per-wallet challenge, recovers
/// the signer via EIP-191, and verifies on-chain whitelist status (with cache).
/// Emits structured auth_ok / auth_fail tracing events on every attempt.
/// Pass the endpoint name for accurate audit log fields.
pub(crate) async fn verify_auth_for(
    wallet_str: &str,
    signature_str: &str,
    wallet_challenges: &super::super::WalletChallengeStore,
    batch_updater: Address,
    flow_rpc: &str,
    whitelist_cache: &crate::security::WhitelistCache,
    endpoint: &str,
) -> Result<(Address, Signature), (StatusCode, Json<UpdateResponse>)> {
    verify_auth_inner(wallet_str, signature_str, wallet_challenges, batch_updater, flow_rpc, whitelist_cache, endpoint).await
}

async fn verify_auth_inner(
    wallet_str: &str,
    signature_str: &str,
    wallet_challenges: &super::super::WalletChallengeStore,
    batch_updater: Address,
    flow_rpc: &str,
    whitelist_cache: &crate::security::WhitelistCache,
    endpoint: &str,
) -> Result<(Address, Signature), (StatusCode, Json<UpdateResponse>)> {
    let now = chrono::Utc::now().timestamp() as u64;

    macro_rules! auth_fail {
        ($wallet:expr, $reason:expr) => {
            tracing::warn!(
                wallet = $wallet,
                endpoint = endpoint,
                ts = now,
                reason = $reason,
                "auth_fail"
            );
        };
    }

    let wallet: Address = wallet_str.parse().map_err(|_| {
        auth_fail!("(unparseable)", "invalid wallet address");
        (
            StatusCode::BAD_REQUEST,
            Json(UpdateResponse {
                status: "error",
                message: "invalid wallet address".into(),
            }),
        )
    })?;

    let wallet_display = format!("{wallet:#x}");
    let wallet_key = wallet_display.clone();

    let challenge = {
        let mut store = wallet_challenges.lock().await;
        match store.remove(&wallet_key) {
            None => {
                auth_fail!(&wallet_display, "no pending challenge");
                return Err((
                    StatusCode::UNAUTHORIZED,
                    Json(UpdateResponse {
                        status: "error",
                        message: "no pending challenge for this wallet — call GET /challenge?wallet=".into(),
                    }),
                ));
            }
            Some((_ch, exp)) if now > exp => {
                auth_fail!(&wallet_display, "challenge expired");
                return Err((
                    StatusCode::UNAUTHORIZED,
                    Json(UpdateResponse {
                        status: "error",
                        message: "challenge expired".into(),
                    }),
                ));
            }
            Some((ch, _)) => ch,
        }
    };

    let sig: Signature = signature_str.parse().map_err(|_| {
        auth_fail!(&wallet_display, "invalid signature format");
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
            auth_fail!(&wallet_display, "signer recovery failed");
            (
                StatusCode::UNAUTHORIZED,
                Json(UpdateResponse {
                    status: "error",
                    message: "could not recover signer from signature".into(),
                }),
            )
        })?;

    if recovered != wallet {
        auth_fail!(&wallet_display, "signature does not match wallet");
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(UpdateResponse {
                status: "error",
                message: "signature does not match claimed wallet".into(),
            }),
        ));
    }

    let whitelisted = whitelist_cache
        .is_whitelisted(batch_updater, wallet, flow_rpc)
        .await
        .map_err(|e| {
            tracing::error!(
                error = %e,
                wallet = %wallet_display,
                endpoint,
                "whitelist check failed during auth"
            );
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(UpdateResponse {
                    status: "error",
                    message: "failed to verify whitelist status".into(),
                }),
            )
        })?;

    if !whitelisted {
        auth_fail!(&wallet_display, "wallet not whitelisted");
        return Err((
            StatusCode::FORBIDDEN,
            Json(UpdateResponse {
                status: "error",
                message: format!("wallet {wallet} is not whitelisted on OracleBatchUpdater"),
            }),
        ));
    }

    tracing::info!(
        wallet = %wallet_display,
        endpoint,
        ts = now,
        "auth_ok"
    );

    Ok((wallet, sig))
}
