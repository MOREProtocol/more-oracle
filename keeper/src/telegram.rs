//! Telegram alert notifications for the keeper.
//!
//! Reads TELEGRAM_BOT_TOKEN and TELEGRAM_CHAT_ID from env.
//! If either is missing the notifier is disabled — keeper runs normally without alerts.
//! All sends are fire-and-forget; a Telegram failure never blocks the keeper.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};
use tracing::{debug, warn};

const TELEGRAM_API: &str = "https://api.telegram.org";
/// Minimum seconds between repeated alerts for the same key.
const COOLDOWN_SECS: u64 = 3600; // 1 hour

#[derive(Debug, Clone)]
pub struct TelegramNotifier {
    token: String,
    chat_id: String,
    client: reqwest::Client,
    /// last-sent timestamps keyed by alert type + spoke/oracle identifier
    cooldown: Arc<Mutex<HashMap<String, u64>>>,
}

impl TelegramNotifier {
    /// Build a notifier from env vars. Returns None if not configured.
    pub fn from_env() -> Option<Self> {
        let token = std::env::var("TELEGRAM_BOT_TOKEN").ok()?;
        let chat_id = std::env::var("TELEGRAM_CHAT_ID").ok()?;
        if token.is_empty() || chat_id.is_empty() {
            return None;
        }
        Some(Self {
            token,
            chat_id,
            client: reqwest::Client::new(),
            cooldown: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    /// Returns true if enough time has passed since the last alert for this key.
    fn check_cooldown(&self, key: &str) -> bool {
        let now = chrono::Utc::now().timestamp() as u64;
        let mut map = self.cooldown.lock().unwrap();
        let last = map.get(key).copied().unwrap_or(0);
        if now.saturating_sub(last) >= COOLDOWN_SECS {
            map.insert(key.to_string(), now);
            true
        } else {
            false
        }
    }

    /// Send a message. Spawns a background task — never blocks the caller.
    pub fn send(&self, text: String) {
        let url = format!("{}/bot{}/sendMessage", TELEGRAM_API, self.token);
        let chat_id = self.chat_id.clone();
        let client = self.client.clone();
        tokio::spawn(async move {
            let payload = serde_json::json!({
                "chat_id": chat_id,
                "text": text,
                "parse_mode": "HTML"
            });
            match client.post(&url).json(&payload).send().await {
                Ok(r) if r.status().is_success() => {
                    debug!("telegram: message sent");
                }
                Ok(r) => {
                    warn!(status = %r.status(), "telegram: send failed (non-2xx)");
                }
                Err(e) => {
                    warn!(error = %e, "telegram: send error");
                }
            }
        });
    }

    // ── Alert helpers ────────────────────────────────────────────────────────

    pub fn drift_alert(&self, spoke: &str, oracle: &str, max_bps: u64) {
        if !self.check_cooldown(&format!("drift:{spoke}")) { return; }
        self.send(format!(
            "🚨 <b>DRIFT ALERT — oracle update blocked</b>\n\
             Spoke: <code>{spoke}</code>\n\
             Oracle: <code>{oracle}</code>\n\
             Threshold: <b>{max_bps} bps</b> cumulative (24 h window)\n\
             ⚠️ Push suppressed. May indicate manipulation or a large curator bridge.\n\
             Check spoke activity before manually overriding."
        ));
    }

    pub fn all_oracles_drift_blocked(&self) {
        self.send(
            "🚨 <b>ALL ORACLES BLOCKED — drift protection</b>\n\
             Every oracle exceeded the cumulative drift threshold this cycle.\n\
             No update was sent to the hub.\n\
             ⚠️ Check vault activity across all chains immediately."
                .to_string(),
        );
    }

    pub fn critical_all_updates_failed(&self, attempts: u32) {
        self.send(format!(
            "🔴 <b>CRITICAL — keeper cycle failed</b>\n\
             All oracle updates failed (batch + individual) after <b>{attempts}</b> retries.\n\
             Oracles may go stale soon.\n\
             ⚠️ Check Flow EVM RPC connectivity and keeper logs immediately."
        ));
    }

    pub fn critical_invalid_key(&self) {
        self.send(
            "🔴 <b>CRITICAL — invalid keeper key</b>\n\
             <code>KEEPER_PRIVATE_KEY</code> is misconfigured. Keeper cannot sign transactions.\n\
             ⚠️ Fix <code>.env</code> and restart the keeper."
                .to_string(),
        );
    }

    pub fn batch_reverted(&self, num_oracles: usize, error: &str) {
        self.send(format!(
            "⚠️ <b>Batch update reverted</b>\n\
             Oracles in batch: <b>{num_oracles}</b>\n\
             Error: <code>{error}</code>\n\
             Falling back to individual oracle updates."
        ));
    }

    pub fn individual_update_failed(&self, oracle: &str, error: &str) {
        if !self.check_cooldown(&format!("update_failed:{oracle}")) { return; }
        self.send(format!(
            "⚠️ <b>Oracle update failed</b>\n\
             Oracle: <code>{oracle}</code>\n\
             Error: <code>{error}</code>"
        ));
    }

    pub fn monitor_push_failed(&self, spoke: &str, error: &str) {
        self.send(format!(
            "⚠️ <b>Early push failed</b>\n\
             Spoke: <code>{spoke}</code>\n\
             Error: <code>{error}</code>\n\
             Scheduled push will retry on next cycle."
        ));
    }

    pub fn peer_divergence(&self, spoke: &str, our_value: u128, peer_value: u128, divergence_bps: u64) {
        if !self.check_cooldown(&format!("divergence:{spoke}")) { return; }
        self.send(format!(
            "⚠️ <b>Keeper divergence — spoke value mismatch</b>\n\
             Spoke: <code>{spoke}</code>\n\
             This keeper: <code>{our_value}</code>\n\
             Peer keeper: <code>{peer_value}</code>\n\
             Divergence: <b>{divergence_bps} bps</b>\n\
             May indicate a curator bridge, RPC lag, or stale peer. \
             Both keepers will push their own value — monitor which lands on-chain."
        ));
    }

    pub fn both_keepers_failed(&self, spoke: &str) {
        if !self.check_cooldown(&format!("both_failed:{spoke}")) { return; }
        self.send(format!(
            "🔴 <b>Both keepers failed to read spoke</b>\n\
             Spoke: <code>{spoke}</code>\n\
             Neither this keeper nor its peer could read <code>totalAssets()</code> from the RPC.\n\
             ⚠️ Oracle will not be updated for this spoke. Check RPC endpoints on both keepers."
        ));
    }

    pub fn retrying_cycle(&self, attempt: u32, max_retries: u32, delay_secs: u64) {
        self.send(format!(
            "⚠️ <b>Retry — attempt {attempt}/{max_retries}</b>\n\
             All individual updates failed. Retrying in {delay_secs}s."
        ));
    }
}
