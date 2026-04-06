use alloy::primitives::Address;
use std::{collections::HashMap, sync::Arc};
use tokio::sync::Mutex;

/// In-memory peer information -- lost on restart (by design, forces re-auth).
#[derive(Debug, Clone)]
pub struct PeerInfo {
    pub url: String,
    /// Recovered from their EIP-191 signature during registration.
    pub wallet: Address,
    /// Unix timestamp when this peer was first registered.
    pub registered_at: u64,
    /// Unix timestamp of the last successful /status poll.
    pub last_seen: Option<u64>,
    /// Unix timestamp of the peer's last push (from their /status response).
    pub last_push_at: Option<u64>,
    /// false after 6h without a successful /status poll.
    pub active: bool,
}

/// Thread-safe peer registry keyed by peer URL.
pub type PeerRegistry = Arc<Mutex<HashMap<String, PeerInfo>>>;

/// Peer URL -> (challenge, expires_at).
pub type PendingRegistrations = Arc<Mutex<HashMap<String, (String, u64)>>>;

pub fn new_peer_registry() -> PeerRegistry {
    Arc::new(Mutex::new(HashMap::new()))
}

pub fn new_pending_registrations() -> PendingRegistrations {
    Arc::new(Mutex::new(HashMap::new()))
}
