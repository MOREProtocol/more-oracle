/// URL validation and whitelist cache for the security layer.
use alloy::primitives::Address;
use std::{
    collections::HashMap,
    net::{IpAddr, ToSocketAddrs},
    sync::Arc,
};
use tokio::sync::Mutex;

// ── URL Validation ────────────────────────────────────────────────────────────

/// Validate that a peer URL is safe to make an outbound HTTP request to.
///
/// Rejects:
/// - Non-http/https schemes
/// - Loopback addresses (127.x.x.x, ::1)
/// - RFC 1918 private ranges (10.x, 172.16–31.x, 192.168.x)
/// - Link-local (169.254.x.x, fe80::/10)
/// - Hostnames that resolve to any of the above
pub fn validate_peer_url(url: &str) -> Result<(), String> {
    let parsed = url
        .parse::<reqwest::Url>()
        .map_err(|e| format!("invalid URL '{url}': {e}"))?;

    let scheme = parsed.scheme();
    if scheme != "http" && scheme != "https" {
        return Err(format!(
            "rejected URL '{url}': only http/https schemes are allowed (got '{scheme}')"
        ));
    }

    let host = parsed
        .host_str()
        .ok_or_else(|| format!("rejected URL '{url}': missing host"))?;

    if let Ok(ip) = host.parse::<IpAddr>() {
        return check_ip(url, ip);
    }

    let port = parsed.port_or_known_default().unwrap_or(80);
    let lookup = format!("{host}:{port}");
    match lookup.to_socket_addrs() {
        Ok(addrs) => {
            for addr in addrs {
                check_ip(url, addr.ip())?;
            }
        }
        Err(e) => {
            return Err(format!("rejected URL '{url}': DNS resolution failed: {e}"));
        }
    }

    Ok(())
}

/// Check a single IP for loopback, private, and link-local ranges.
fn check_ip(url: &str, ip: IpAddr) -> Result<(), String> {
    match ip {
        IpAddr::V4(v4) => {
            let octets = v4.octets();
            if v4.is_loopback() {
                return Err(format!(
                    "rejected URL '{url}': loopback address ({ip}) is not allowed"
                ));
            }
            // RFC 1918 private: 10/8, 172.16/12, 192.168/16
            if octets[0] == 10 {
                return Err(format!(
                    "rejected URL '{url}': RFC1918 private address ({ip}) is not allowed"
                ));
            }
            if octets[0] == 172 && octets[1] >= 16 && octets[1] <= 31 {
                return Err(format!(
                    "rejected URL '{url}': RFC1918 private address ({ip}) is not allowed"
                ));
            }
            if octets[0] == 192 && octets[1] == 168 {
                return Err(format!(
                    "rejected URL '{url}': RFC1918 private address ({ip}) is not allowed"
                ));
            }
            // Link-local: 169.254/16
            if octets[0] == 169 && octets[1] == 254 {
                return Err(format!(
                    "rejected URL '{url}': link-local address ({ip}) is not allowed"
                ));
            }
        }
        IpAddr::V6(v6) => {
            if v6.is_loopback() {
                return Err(format!(
                    "rejected URL '{url}': loopback address ({ip}) is not allowed"
                ));
            }
            let segments = v6.segments();
            // Link-local: fe80::/10
            if segments[0] & 0xffc0 == 0xfe80 {
                return Err(format!(
                    "rejected URL '{url}': link-local address ({ip}) is not allowed"
                ));
            }
            // Unique-local: fc00::/7
            if segments[0] & 0xfe00 == 0xfc00 {
                return Err(format!(
                    "rejected URL '{url}': unique-local address ({ip}) is not allowed"
                ));
            }
        }
    }
    Ok(())
}

// ── Whitelist Cache ───────────────────────────────────────────────────────────

const WHITELIST_CACHE_TTL_SECS: u64 = 300; // 5 minutes

#[derive(Debug, Clone)]
struct CacheEntry {
    whitelisted: bool,
    expires_at: u64,
}

/// Thread-safe cache for on-chain whitelist lookups.
/// Keyed by wallet address; values expire after 5 minutes.
#[derive(Clone)]
pub struct WhitelistCache(Arc<Mutex<HashMap<Address, CacheEntry>>>);

impl WhitelistCache {
    pub fn new() -> Self {
        WhitelistCache(Arc::new(Mutex::new(HashMap::new())))
    }

    pub async fn is_whitelisted(
        &self,
        batch_updater: Address,
        wallet: Address,
        flow_rpc: &str,
    ) -> eyre::Result<bool> {
        let now = chrono::Utc::now().timestamp() as u64;

        {
            let cache = self.0.lock().await;
            if let Some(entry) = cache.get(&wallet) {
                if entry.expires_at > now {
                    return Ok(entry.whitelisted);
                }
            }
        }

        let whitelisted =
            crate::oracle::is_whitelisted(batch_updater, wallet, flow_rpc).await?;

        {
            let mut cache = self.0.lock().await;
            // Evict expired entries to keep the map bounded
            cache.retain(|_, v| v.expires_at > now);
            cache.insert(
                wallet,
                CacheEntry {
                    whitelisted,
                    expires_at: now + WHITELIST_CACHE_TTL_SECS,
                },
            );
        }

        Ok(whitelisted)
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_valid_https_url() {
        assert!(validate_peer_url("https://1.2.3.4:8080").is_ok());
    }

    #[test]
    fn test_invalid_scheme() {
        let err = validate_peer_url("ftp://1.2.3.4").unwrap_err();
        assert!(err.contains("only http/https"));
    }

    #[test]
    fn test_loopback_v4() {
        let err = validate_peer_url("http://127.0.0.1:8080").unwrap_err();
        assert!(err.contains("loopback"));
    }

    #[test]
    fn test_loopback_v6() {
        let err = validate_peer_url("http://[::1]:8080").unwrap_err();
        assert!(err.contains("loopback"));
    }

    #[test]
    fn test_private_10() {
        let err = validate_peer_url("http://10.0.0.1:8080").unwrap_err();
        assert!(err.contains("RFC1918"));
    }

    #[test]
    fn test_private_172() {
        let err = validate_peer_url("http://172.16.0.1:8080").unwrap_err();
        assert!(err.contains("RFC1918"));
    }

    #[test]
    fn test_private_172_boundary_low() {
        assert!(validate_peer_url("http://172.15.0.1:8080").is_ok());
    }

    #[test]
    fn test_private_172_boundary_high() {
        assert!(validate_peer_url("http://172.32.0.1:8080").is_ok());
    }

    #[test]
    fn test_private_192_168() {
        let err = validate_peer_url("http://192.168.1.1:8080").unwrap_err();
        assert!(err.contains("RFC1918"));
    }

    #[test]
    fn test_link_local() {
        let err = validate_peer_url("http://169.254.0.1:8080").unwrap_err();
        assert!(err.contains("link-local"));
    }

    #[test]
    fn test_ipv6_link_local() {
        let err = validate_peer_url("http://[fe80::1]:8080").unwrap_err();
        assert!(err.contains("link-local"));
    }

    #[test]
    fn test_missing_scheme() {
        assert!(validate_peer_url("1.2.3.4:8080").is_err());
    }
}
