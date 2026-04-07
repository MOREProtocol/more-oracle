use alloy::primitives::Address;
use eyre::{Context, Result};
use serde::Deserialize;
use std::collections::HashMap;

/// Top-level config loaded from config.toml
#[derive(Debug, Deserialize, Clone)]
pub struct AppConfig {
    pub hub: HubConfig,
    #[serde(default)]
    pub spokes: Vec<SpokeConfig>,
}

#[derive(Debug, Deserialize, Clone)]
#[allow(dead_code)]
pub struct HubConfig {
    pub oracle_registry: String,
    pub vault_address: String,
    pub rpc_url: String,
    pub chain_id: u64,
    /// How often to attempt an update (seconds)
    pub update_interval_secs: u64,
    /// Minimum gap between on-chain updates (seconds); skip push if oracle is fresher
    pub min_update_interval_secs: u64,
    /// OracleBatchUpdater contract address on Flow EVM
    pub batch_updater: Address,
    /// Maximum number of retry attempts on failure
    pub max_retries: u32,
    /// Seconds to wait between retries
    pub retry_delay_secs: u64,
    /// HTTP API port (default 8080)
    #[serde(default = "default_api_port")]
    pub api_port: u16,
    /// Peer keeper URLs — used at startup to find optimal push position
    #[serde(default)]
    pub peers: Vec<String>,
    /// Monitor loop interval in seconds (default 60).
    /// How often to check spokes for significant totalAssets changes.
    #[serde(default = "default_monitor_interval_secs")]
    pub monitor_interval_secs: u64,
    /// Threshold in basis points to trigger an early push from the monitor loop.
    /// 25 bps = 0.25%. Set to 0 to disable monitor-driven pushes.
    #[serde(default = "default_early_push_threshold_bps")]
    pub early_push_threshold_bps: u64,
    /// Skip a spoke in the scheduled batch if it was pushed within this many seconds.
    /// 0 = never skip (always include in batch).
    #[serde(default = "default_skip_if_fresh_secs")]
    pub skip_if_fresh_secs: u64,
    /// Cumulative drift window in seconds (default 86400 = 24h).
    /// The keeper tracks value changes within this sliding window.
    #[serde(default = "default_drift_window_secs")]
    pub drift_window_secs: u64,
    /// Maximum cumulative drift from the anchor value in basis points
    /// (default 1500 = 15%). If exceeded, the oracle update is skipped.
    #[serde(default = "default_max_cumulative_drift_bps")]
    pub max_cumulative_drift_bps: u64,
    /// This keeper's own public URL (announced to peers during registration).
    /// Example: KEEPER_URL=https://keeper1.example.com:8080
    #[serde(default)]
    pub keeper_url: Option<String>,
}

fn default_api_port() -> u16 {
    8080
}

fn default_monitor_interval_secs() -> u64 {
    60
}

fn default_early_push_threshold_bps() -> u64 {
    25
}

fn default_skip_if_fresh_secs() -> u64 {
    3600
}

fn default_drift_window_secs() -> u64 {
    86400 // 24 hours
}

fn default_max_cumulative_drift_bps() -> u64 {
    1500 // 15%
}

#[derive(Debug, Deserialize, Clone)]
pub struct SpokeConfig {
    pub name: String,
    pub eid: u32,
    pub chain_id: u64,
    pub rpc_url: String,
    pub vault_address: String,
    /// Set at runtime from env var ORACLE_<NAME_UPPER>
    #[serde(default)]
    pub oracle_address: Option<String>,
    /// Only active spokes are processed
    pub active: bool,
}

/// Runtime configuration assembled from config.toml + .env
#[derive(Debug, Clone)]
pub struct RuntimeConfig {
    pub hub: HubConfig,
    pub spokes: Vec<SpokeConfig>,
    pub keeper_private_key: String,
    /// Optional RPC overrides from env (RPC_FLOW, RPC_ARBITRUM, …)
    pub rpc_overrides: HashMap<String, String>,
    /// Telegram notifier — None if TELEGRAM_BOT_TOKEN / TELEGRAM_CHAT_ID not set
    pub telegram: Option<crate::telegram::TelegramNotifier>,
}

impl RuntimeConfig {
    pub fn load() -> Result<Self> {
        // Load .env (if present — not required in production where env is injected)
        let _ = dotenvy::dotenv();

        // Parse config.toml
        let raw = std::fs::read_to_string("config.toml")
            .context("config.toml not found — run keeper from the keeper/ directory")?;
        let mut cfg: AppConfig = toml::from_str(&raw).context("Failed to parse config.toml")?;

        // Keeper private key — mandatory
        let keeper_private_key = std::env::var("KEEPER_PRIVATE_KEY")
            .context("KEEPER_PRIVATE_KEY must be set in .env or environment")?;

        // Inject oracle addresses from env: ORACLE_<NAME_UPPER>
        for spoke in cfg.spokes.iter_mut() {
            let env_key = format!("ORACLE_{}", spoke.name.to_uppercase());
            if let Ok(addr) = std::env::var(&env_key) {
                if !addr.is_empty() && addr != "0x..." {
                    spoke.oracle_address = Some(addr);
                }
            }

            // Optional RPC override: RPC_<NAME_UPPER>
            let rpc_key = format!("RPC_{}", spoke.name.to_uppercase());
            if let Ok(rpc) = std::env::var(&rpc_key) {
                if !rpc.is_empty() {
                    spoke.rpc_url = rpc;
                }
            }
        }

        // Hub RPC override
        let mut rpc_overrides = HashMap::new();
        if let Ok(flow_rpc) = std::env::var("RPC_FLOW") {
            if !flow_rpc.is_empty() {
                rpc_overrides.insert("flow".to_string(), flow_rpc.clone());
            }
        }

        let telegram = crate::telegram::TelegramNotifier::from_env();
        if telegram.is_some() {
            tracing::info!("telegram: notifications enabled");
        } else {
            tracing::info!("telegram: TELEGRAM_BOT_TOKEN / TELEGRAM_CHAT_ID not set — notifications disabled");
        }

        Ok(RuntimeConfig {
            hub: cfg.hub,
            spokes: cfg.spokes,
            keeper_private_key,
            rpc_overrides,
            telegram,
        })
    }

    /// Returns the effective Flow EVM RPC URL (env override wins)
    pub fn flow_rpc(&self) -> &str {
        self.rpc_overrides
            .get("flow")
            .map(|s| s.as_str())
            .unwrap_or(&self.hub.rpc_url)
    }
}
