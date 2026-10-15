use std::path::PathBuf;

use crate::error::AppResult;

#[derive(Clone, Debug)]
pub struct AppConfig {
    pub bind_addr: String,
    pub data_dir: PathBuf,
    pub stats_interval_secs: u64,
    pub xray_download_base: String,
    /// Host the panel advertises in VLESS share URIs. When unset, the share
    /// endpoint refuses with a 4xx rather than silently embedding `127.0.0.1`
    /// (a localhost URL is useless to a remote client).
    pub public_host: String,
}

impl AppConfig {
    pub fn from_env() -> AppResult<Self> {
        let bind_addr = std::env::var("BIND_ADDR").unwrap_or_else(|_| "127.0.0.1:2053".to_string());
        let data_dir = std::env::var("DATA_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("./data"));
        let stats_interval_secs = std::env::var("STATS_INTERVAL_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(30);
        // GitHub **API** root, not the web host: the installer appends
        // `/repos/XTLS/Xray-core/releases/...` and parses the release JSON.
        let xray_download_base = std::env::var("XRAY_DOWNLOAD_BASE")
            .unwrap_or_else(|_| "https://api.github.com".to_string());
        let public_host = std::env::var("PUBLIC_HOST").unwrap_or_default();

        // Per Assumptions: warn (not block) if binding to non-loopback.
        if !is_loopback_host(&bind_addr) {
            eprintln!(
                "WARNING: BIND_ADDR={bind_addr} is not loopback; admin panel will be served over plaintext HTTP. \
                 Use a TLS reverse proxy in front."
            );
        }

        Ok(Self {
            bind_addr,
            data_dir,
            stats_interval_secs,
            xray_download_base,
            public_host,
        })
    }
}

fn is_loopback_host(addr: &str) -> bool {
    // Strip optional port. Handles:
    //   - "127.0.0.1" / "127.0.0.1:port"  (IPv4 with/without port)
    //   - "::1" / "[::1]" / "[::1]:port"  (IPv6 with or without brackets)
    //   - "localhost"
    let host = if let Some(rest) = addr.strip_prefix('[') {
        let end = rest.find(']').unwrap_or(rest.len());
        &rest[..end]
    } else if addr.matches(':').count() > 1 {
        addr
    } else if let Some((h, _)) = addr.rsplit_once(':') {
        h
    } else {
        addr
    };
    matches!(host, "127.0.0.1" | "localhost" | "::1")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_v4_detected() {
        assert!(is_loopback_host("127.0.0.1:2053"));
        assert!(is_loopback_host("127.0.0.1:80"));
    }

    #[test]
    fn loopback_v6_detected() {
        assert!(is_loopback_host("[::1]:2053"));
        assert!(is_loopback_host("[::1]"));
        assert!(is_loopback_host("::1"));
    }

    #[test]
    fn non_loopback_detected() {
        assert!(!is_loopback_host("0.0.0.0:2053"));
        assert!(!is_loopback_host("192.168.1.5:2053"));
        assert!(!is_loopback_host("admin.example.com:443"));
    }
}
