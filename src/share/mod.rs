//! VLESS share-link and QR-code rendering.
//!
//! Both pieces are pure functions over already-loaded DB rows so they are
//! straightforward to exercise in tests without an HTTP boundary in between.

pub mod hy2;
pub mod mieru;
pub mod qr;
pub mod vless;

use crate::db::models::Inbound;

/// Resolve the effective connection host for an inbound.
///
/// Priority:
/// 1. `inbound.server_name` if specified (e.g. dedicated direct domain/DDNS for this NIC);
/// 2. `inbound.listen_ip` if specified and not wildcard (`0.0.0.0` or `::`);
/// 3. `fallback_host` (e.g. public host or global proxy domain).
pub fn resolve_inbound_host(inbound: &Inbound, fallback_host: &str) -> String {
    if let Some(ref sn) = inbound.server_name {
        let trimmed = sn.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    let ip = inbound.listen_ip.trim();
    if !ip.is_empty() && ip != "0.0.0.0" && ip != "::" {
        return ip.to_string();
    }
    fallback_host.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_inbound(listen_ip: &str, server_name: Option<&str>) -> Inbound {
        Inbound {
            id: 1,
            listen_ip: listen_ip.to_string(),
            listen_port: 443,
            dest: "".to_string(),
            sni: "".to_string(),
            server_name: server_name.map(|s| s.to_string()),
            private_key: "".to_string(),
            public_key: "".to_string(),
            short_id: "".to_string(),
            xhttp_path: None,
            protocol: "vless".to_string(),
            cert_mode: "auto".to_string(),
            custom_cert_path: "".to_string(),
            custom_key_path: "".to_string(),
            masquerade_url: "".to_string(),
            port_range: "".to_string(),
            transport_protocol: "TCP".to_string(),
            skip_cert_verify: 0,
            udp: 1,
            status: "active".to_string(),
            created_at: None,
            updated_at: None,
        }
    }

    #[test]
    fn test_resolve_inbound_host() {
        let fallback = "cdn.example.com";

        // 1. server_name takes highest priority
        let ib1 = sample_inbound("198.51.100.1", Some("direct.example.com"));
        assert_eq!(resolve_inbound_host(&ib1, fallback), "direct.example.com");

        // 2. Specific listen_ip takes second priority
        let ib2 = sample_inbound("198.51.100.1", None);
        assert_eq!(resolve_inbound_host(&ib2, fallback), "198.51.100.1");

        let ib3 = sample_inbound("203.0.113.2", Some("   ")); // whitespace ignored
        assert_eq!(resolve_inbound_host(&ib3, fallback), "203.0.113.2");

        // 3. 0.0.0.0 or :: falls back
        let ib4 = sample_inbound("0.0.0.0", None);
        assert_eq!(resolve_inbound_host(&ib4, fallback), "cdn.example.com");

        let ib5 = sample_inbound("::", None);
        assert_eq!(resolve_inbound_host(&ib5, fallback), "cdn.example.com");

        let ib6 = sample_inbound("", None);
        assert_eq!(resolve_inbound_host(&ib6, fallback), "cdn.example.com");
    }
}
