use alloy::primitives::Signature;
use axum::{extract::State, http::StatusCode, response::Json};
use serde::{Deserialize, Serialize};

use super::super::{AppState, CHALLENGE_TTL_SECS};

#[derive(Serialize)]
pub(crate) struct ChallengeResponse {
    challenge: String,
    expires_at: u64,
}

#[derive(Deserialize)]
pub struct UpdateRequest {
    pub challenge: String,
    pub signature: String,
}

#[derive(Serialize)]
pub struct UpdateResponse {
    pub status: &'static str,
    pub message: String,
}

pub async fn get_challenge(State(app): State<AppState>) -> Json<ChallengeResponse> {
    let nonce = uuid::Uuid::new_v4().to_string();
    let challenge = format!("update:{nonce}");
    let expires_at = chrono::Utc::now().timestamp() as u64 + CHALLENGE_TTL_SECS;

    let mut store = app.3.lock().await;
    let now = chrono::Utc::now().timestamp() as u64;
    store.retain(|_, exp| *exp > now);
    store.insert(challenge.clone(), expires_at);

    Json(ChallengeResponse { challenge, expires_at })
}

pub async fn trigger_update(
    State(app): State<AppState>,
    Json(body): Json<UpdateRequest>,
) -> Result<Json<UpdateResponse>, (StatusCode, Json<UpdateResponse>)> {
    let now = chrono::Utc::now().timestamp() as u64;
    let notify = &app.1;
    let curator = app.2;
    let challenges = &app.3;

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
            Some(_) => {}
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
    let recovered = sig
        .recover_address_from_msg(body.challenge.as_bytes())
        .map_err(|_| {
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
