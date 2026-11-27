//! Network interface detection, public IP discovery, and host resolution utilities.

use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use std::sync::OnceLock;
use tokio::sync::RwLock;

use crate::db::models::SystemSettings;
use crate::error::{AppError, AppResult};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NetworkInterfaceInfo {
    pub name: String,
    pub ip: String,
    pub is_ipv6: bool,
}

#[cfg(unix)]
pub fn get_local_interfaces() -> Vec<NetworkInterfaceInfo> {
    use std::ffi::CStr;
    use std::net::{Ipv4Addr, Ipv6Addr};

    let mut result = Vec::new();
    let mut ifaddr_ptr: *mut libc::ifaddrs = std::ptr::null_mut();

    let ret = unsafe { libc::getifaddrs(&mut ifaddr_ptr) };
    if ret != 0 || ifaddr_ptr.is_null() {
        return result;
    }

    let mut curr = ifaddr_ptr;
    while !curr.is_null() {
        let ifa = unsafe { &*curr };
        if !ifa.ifa_addr.is_null() && !ifa.ifa_name.is_null() {
            let flags = ifa.ifa_flags as i32;
            let is_loopback = (flags & libc::IFF_LOOPBACK) != 0;
            let is_up = (flags & libc::IFF_UP) != 0;

            if is_up && !is_loopback {
                let name = unsafe { CStr::from_ptr(ifa.ifa_name) }
                    .to_string_lossy()
                    .into_owned();
                let family = unsafe { (*ifa.ifa_addr).sa_family as i32 };

                if family == libc::AF_INET {
                    let addr_in = unsafe { *(ifa.ifa_addr as *const libc::sockaddr_in) };
                    let ip = Ipv4Addr::from(u32::from_be(addr_in.sin_addr.s_addr));
                    if !ip.is_loopback() && !ip.is_unspecified() {
                        result.push(NetworkInterfaceInfo {
                            name,
                            ip: ip.to_string(),
                            is_ipv6: false,
                        });
                    }
                } else if family == libc::AF_INET6 {
                    let addr_in6 = unsafe { *(ifa.ifa_addr as *const libc::sockaddr_in6) };
                    let ip = Ipv6Addr::from(addr_in6.sin6_addr.s6_addr);
                    if !ip.is_loopback()
                        && !ip.is_unspecified()
                        && (ip.segments()[0] & 0xffc0) != 0xfe80
                    {
                        result.push(NetworkInterfaceInfo {
                            name,
                            ip: ip.to_string(),
                            is_ipv6: true,
                        });
                    }
                }
            }
        }
        curr = ifa.ifa_next;
    }

    unsafe {
        libc::freeifaddrs(ifaddr_ptr);
    }

    result
}

#[cfg(not(unix))]
pub fn get_local_interfaces() -> Vec<NetworkInterfaceInfo> {
    Vec::new()
}

/// Query public IP from custom endpoints.
pub async fn detect_ip_from_endpoints(endpoints: &[&str], is_v6: bool) -> Option<String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(3))
        .user_agent("curl/8.0")
        .build()
        .ok()?;

    for url in endpoints {
        if let Ok(resp) = client.get(*url).send().await {
            if let Ok(text) = resp.text().await {
                let ip_str = text.trim();
                if is_v6 {
                    if let Ok(ip) = ip_str.parse::<std::net::Ipv6Addr>() {
                        if !ip.is_loopback()
                            && !ip.is_unspecified()
                            && (ip.segments()[0] & 0xffc0) != 0xfe80
                        {
                            return Some(ip.to_string());
                        }
                    }
                } else if let Ok(ip) = ip_str.parse::<std::net::Ipv4Addr>() {
                    if !ip.is_loopback() && !ip.is_unspecified() {
                        return Some(ip.to_string());
                    }
                }
            }
        }
    }
    None
}

/// Query public IPv4 address from external echo services with a short timeout.
pub async fn detect_public_ipv4() -> Option<String> {
    if std::env::var_os("THREE_R_UI_NO_IP_DETECT").is_some() {
        return None;
    }
    let endpoints = [
        "https://api4.ipify.org",
        "https://4.icanhazip.com",
        "https://api-ipv4.ip.sb/ip",
        "https://api.ipify.org",
    ];
    detect_ip_from_endpoints(&endpoints, false).await
}

/// Query public IPv6 address from external echo services with a short timeout.
pub async fn detect_public_ipv6() -> Option<String> {
    if std::env::var_os("THREE_R_UI_NO_IP_DETECT").is_some() {
        return None;
    }
    let endpoints = [
        "https://api6.ipify.org",
        "https://6.icanhazip.com",
        "https://api-ipv6.ip.sb/ip",
    ];
    detect_ip_from_endpoints(&endpoints, true).await
}

/// Query public IP from external echo services with a short timeout.
pub async fn detect_public_ip() -> Option<String> {
    if let Some(ipv4) = detect_public_ipv4().await {
        return Some(ipv4);
    }
    detect_public_ipv6().await
}

static CACHED_PUBLIC_IP: OnceLock<RwLock<Option<String>>> = OnceLock::new();

/// Retrieve cached public IP or query it on first call.
pub async fn get_or_detect_public_ip() -> Option<String> {
    // If running in test harness, skip external network call to keep tests fast and sandboxed
    if std::env::var_os("CARGO_TARGET_TMPDIR").is_some()
        || std::env::var_os("THREE_R_UI_NO_IP_DETECT").is_some()
    {
        return None;
    }

    let lock = CACHED_PUBLIC_IP.get_or_init(|| RwLock::new(None));
    {
        let read = lock.read().await;
        if let Some(ref ip) = *read {
            return Some(ip.clone());
        }
    }

    let detected = detect_public_ip().await;
    if let Some(ref ip) = detected {
        let mut write = lock.write().await;
        *write = Some(ip.clone());
    }
    detected
}

/// Parsed effective host information for share URIs and subscriptions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EffectiveHost {
    pub raw: String,
    pub scheme: &'static str,
    pub host: String,
    pub is_fallback: bool,
}

impl EffectiveHost {
    pub fn parse(raw: &str, is_fallback: bool) -> Self {
        let trimmed = raw.trim();
        if let Some(rest) = trimmed.strip_prefix("https://") {
            Self {
                raw: trimmed.to_string(),
                scheme: "https",
                host: rest.trim_matches('/').to_string(),
                is_fallback,
            }
        } else if let Some(rest) = trimmed.strip_prefix("http://") {
            Self {
                raw: trimmed.to_string(),
                scheme: "http",
                host: rest.trim_matches('/').to_string(),
                is_fallback,
            }
        } else if trimmed.is_empty() {
            Self {
                raw: String::new(),
                scheme: "http",
                host: "your-server.example.com".to_string(),
                is_fallback: true,
            }
        } else {
            Self {
                raw: trimmed.to_string(),
                scheme: "http",
                host: trimmed.trim_matches('/').to_string(),
                is_fallback,
            }
        }
    }
}

/// Fetch system settings singleton record (id = 1).
pub async fn get_system_settings(pool: &SqlitePool) -> AppResult<SystemSettings> {
    let settings = sqlx::query_as::<_, SystemSettings>(
        r#"
        SELECT id, public_host, updated_at
        FROM system_settings
        WHERE id = 1
        "#,
    )
    .fetch_optional(pool)
    .await
    .map_err(AppError::Database)?;

    match settings {
        Some(s) => Ok(s),
        None => {
            sqlx::query(
                r#"
                INSERT OR IGNORE INTO system_settings (id, public_host)
                VALUES (1, '')
                "#,
            )
            .execute(pool)
            .await
            .map_err(AppError::Database)?;

            sqlx::query_as::<_, SystemSettings>(
                r#"
                SELECT id, public_host, updated_at
                FROM system_settings
                WHERE id = 1
                "#,
            )
            .fetch_one(pool)
            .await
            .map_err(AppError::Database)
        }
    }
}

/// Update system settings singleton record.
pub async fn update_system_settings(
    pool: &SqlitePool,
    public_host: &str,
) -> AppResult<SystemSettings> {
    let trimmed = public_host.trim();
    sqlx::query(
        r#"
        UPDATE system_settings
        SET public_host = ?, updated_at = unixepoch()
        WHERE id = 1
        "#,
    )
    .bind(trimmed)
    .execute(pool)
    .await
    .map_err(AppError::Database)?;

    get_system_settings(pool).await
}

/// Resolve the effective public host for node connections and subscriptions.
///
/// Priority:
/// 1. Database `system_settings.public_host` (configured via Web UI);
/// 2. `PUBLIC_HOST` environment variable (for backward compatibility);
/// 3. Automatically detected server public IP;
/// 4. Fallback placeholder `your-server.example.com` (marks `is_fallback = true`).
pub async fn resolve_effective_public_host(
    pool: &SqlitePool,
    env_public_host: &str,
) -> EffectiveHost {
    // 1. Check database setting
    if let Ok(settings) = get_system_settings(pool).await {
        if !settings.public_host.trim().is_empty() {
            return EffectiveHost::parse(&settings.public_host, false);
        }
    }

    // 2. Check environment variable
    let trimmed_env = env_public_host.trim();
    if !trimmed_env.is_empty() {
        return EffectiveHost::parse(trimmed_env, false);
    }

    // 3. Try auto-detect public IP
    if let Some(ip) = get_or_detect_public_ip().await {
        if !ip.trim().is_empty() {
            return EffectiveHost::parse(&ip, false);
        }
    }

    // 4. Fallback placeholder
    EffectiveHost::parse("", true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_get_local_interfaces_no_loopback() {
        let ifaces = get_local_interfaces();
        for iface in ifaces {
            assert_ne!(iface.ip, "127.0.0.1");
            assert_ne!(iface.ip, "::1");
            assert!(!iface.ip.starts_with("fe80:"));
        }
    }

    #[test]
    fn test_effective_host_parsing() {
        let h1 = EffectiveHost::parse("198.51.100.1", false);
        assert_eq!(h1.scheme, "http");
        assert_eq!(h1.host, "198.51.100.1");
        assert!(!h1.is_fallback);

        let h2 = EffectiveHost::parse("https://vpn.example.com/", false);
        assert_eq!(h2.scheme, "https");
        assert_eq!(h2.host, "vpn.example.com");
        assert!(!h2.is_fallback);

        let h3 = EffectiveHost::parse("http://vpn.example.com:8443/", false);
        assert_eq!(h3.scheme, "http");
        assert_eq!(h3.host, "vpn.example.com:8443");
        assert!(!h3.is_fallback);

        let h4 = EffectiveHost::parse("", true);
        assert_eq!(h4.scheme, "http");
        assert_eq!(h4.host, "your-server.example.com");
        assert!(h4.is_fallback);
    }

    #[tokio::test]
    async fn test_detect_ip_from_endpoints() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server_ipv4 = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string(" 203.0.113.88 \n"))
            .mount(&server_ipv4)
            .await;

        let detected_v4 = detect_ip_from_endpoints(&[&server_ipv4.uri()], false).await;
        assert_eq!(detected_v4, Some("203.0.113.88".to_string()));

        let server_ipv6 = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string("2001:db8::1\n"))
            .mount(&server_ipv6)
            .await;

        let detected_v6 = detect_ip_from_endpoints(&[&server_ipv6.uri()], true).await;
        assert_eq!(detected_v6, Some("2001:db8::1".to_string()));

        // Invalid response ignored
        let server_bad = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string("<html>Error</html>"))
            .mount(&server_bad)
            .await;

        let detected_bad = detect_ip_from_endpoints(&[&server_bad.uri()], false).await;
        assert_eq!(detected_bad, None);
    }
}
