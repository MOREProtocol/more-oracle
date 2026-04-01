use alloy::primitives::Address;
use std::{collections::HashMap, sync::Arc};
use tokio::sync::Mutex;

/// In-memory peer information -- lost on restart (by design, forces re-auth).
#[derive(Debug, Clone)]
pub struct PeerInfo {
    pub url: String,
    /// Recovered from their EIP-191 signature during registration.
    pub wallet: Address,
    /// API key we generated; the peer presents it when calling our endpoints.
    pub incoming_api_key: String,
    /// API key they generated for us; we present it when calling their endpoints.
    pub outgoing_api_key: Option<String>,
    /// Unix timestamp when this peer was first registered.
    pub registered_at: u64,
    /// Unix timestamp of the last successful /status poll.
    pub last_seen: Option<u64>,
    /// Unix timestamp of the peer's last push (from their /status response).
    pub last_push_at: Option<u64>,
}

/// Thread-safe peer registry keyed by peer URL.
pub type PeerRegistry = Arc<Mutex<HashMap<String, PeerInfo>>>;

/// Pending peer registrations: peer URL -> (challenge, expires_at).
pub type PendingRegistrations = Arc<Mutex<HashMap<String, (String, u64)>>>;

/// Create a new empty PeerRegistry.
pub fn new_peer_registry() -> PeerRegistry {
    Arc::new(Mutex::new(HashMap::new()))
}

/// Create a new empty PendingRegistrations store.
pub fn new_pending_registrations() -> PendingRegistrations {
    Arc::new(Mutex::new(HashMap::new()))
}
