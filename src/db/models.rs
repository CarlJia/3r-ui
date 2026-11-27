use sqlx::FromRow;

#[derive(Debug, Clone, FromRow)]
pub struct Admin {
    pub id: i64,
    pub username: String,
    pub password_hash: String,
    pub created_at: i64,
}

#[derive(Debug, Clone, FromRow)]
pub struct Inbound {
    pub id: i64,
    pub listen_ip: String,
    pub listen_port: i64,
    pub dest: String,
    pub sni: String,
    pub server_name: Option<String>,
    pub private_key: String,
    pub public_key: String,
    pub short_id: String,
    pub xhttp_path: Option<String>,
    pub protocol: String,
    pub cert_mode: String,
    pub custom_cert_path: String,
    pub custom_key_path: String,
    pub masquerade_url: String,
    pub port_range: String,
    pub transport_protocol: String,
    pub skip_cert_verify: i64,
    pub udp: i64,
    pub status: String,
    pub created_at: Option<i64>,
    pub updated_at: Option<i64>,
}

#[derive(Debug, Clone, FromRow)]
pub struct Client {
    pub id: i64,
    pub inbound_id: i64,
    pub uuid: String,
    pub name: Option<String>,
    pub quota_bytes: i64,
    pub expiry_at: Option<i64>,
    pub status: String,
    pub sub_token: Option<String>,
    pub mieru_username: Option<String>,
    pub mieru_password: Option<String>,
    pub created_at: Option<i64>,
}

#[derive(Debug, Clone, FromRow)]
pub struct TrafficSnapshot {
    pub client_id: i64,
    pub uplink: i64,
    pub downlink: i64,
    pub xray_uplink: i64,
    pub xray_downlink: i64,
    pub recorded_at: i64,
}

#[derive(Debug, Clone, FromRow)]
pub struct Session {
    pub session_id: String,
    pub admin_id: i64,
    pub expires_at: i64,
    pub created_at: i64,
}

/// 一条 xray 访问日志（来自 xray stderr 的 access log 行，由
/// [`crate::xray::logs`] 解析后落库）。见 `migrations/0003_connection_logs.sql`。
#[derive(Debug, Clone, FromRow)]
pub struct ConnectionLog {
    pub id: i64,
    pub logged_at: i64,
    pub uuid: String,
    pub src_addr: Option<String>,
    pub dest: Option<String>,
    pub status: String,
    pub reason: Option<String>,
    pub raw_message: String,
    pub received_at: i64,
}

/// Pingora 反向代理与证书管理配置记录。
#[derive(Debug, Clone, FromRow, serde::Serialize, serde::Deserialize)]
pub struct ProxySettings {
    pub id: i64,
    pub enabled: i64,
    pub listen_port: i64,
    pub domain: String,
    pub path: String,
    pub sub_path: String,
    pub cert_mode: String,
    pub cf_api_token: String,
    pub cf_zone_id: String,
    pub custom_cert_path: String,
    pub custom_key_path: String,
    pub cert_expires_at: Option<i64>,
    pub last_renewed_at: Option<i64>,
    pub last_error: Option<String>,
    pub updated_at: i64,
}

/// 规范化面板访问路径：去除首尾空格与斜杠，以单个 `/` 开头且不以 `/` 结尾。
/// 如果输入为空或只有斜杠，返回空字符串 `""`。
pub fn normalize_panel_path(p: &str) -> String {
    let trimmed = p.trim().trim_matches('/');
    if trimmed.is_empty() {
        String::new()
    } else {
        format!("/{trimmed}")
    }
}

/// 规范化订阅访问路径：去除首尾空格与斜杠，以单个 `/` 开头且不以 `/` 结尾。
/// 如果输入为空或只有斜杠，默认返回 `"/sub"`。
pub fn normalize_sub_path(p: &str) -> String {
    let trimmed = p.trim().trim_matches('/');
    if trimmed.is_empty() {
        "/sub".to_string()
    } else {
        format!("/{trimmed}")
    }
}

impl ProxySettings {
    pub fn is_enabled(&self) -> bool {
        self.enabled == 1
    }

    /// 返回规范化后的面板访问路径，如 `"/xxjjye"` 或 `""`。
    pub fn normalized_path(&self) -> String {
        normalize_panel_path(&self.path)
    }

    /// 返回规范化后的订阅访问路径，如 `"/sub"` 或 `"/mysub"`。
    pub fn normalized_sub_path(&self) -> String {
        normalize_sub_path(&self.sub_path)
    }

    /// 返回完整的反代访问入口 URL，例如 `https://jp2.856668.xyz:8443/xxjjye` 或 `https://jp2.856668.xyz/xxjjye`。
    pub fn full_url(&self) -> String {
        if self.domain.is_empty() {
            return String::new();
        }
        let port_part = if self.listen_port == 443 {
            String::new()
        } else {
            format!(":{}", self.listen_port)
        };
        let p = self.normalized_path();
        format!("https://{}{}{}", self.domain, port_part, p)
    }

    /// 返回完整的反代订阅访问入口 URL，例如 `https://jp2.856668.xyz:8443/sub/token123` 或 `https://jp2.856668.xyz/sub/token123`。
    pub fn full_sub_url(&self, token: &str) -> String {
        if self.domain.is_empty() {
            return String::new();
        }
        let port_part = if self.listen_port == 443 {
            String::new()
        } else {
            format!(":{}", self.listen_port)
        };
        let sub_p = self.normalized_sub_path();
        if token.is_empty() {
            format!("https://{}{}{}", self.domain, port_part, sub_p)
        } else {
            format!("https://{}{}{}/{}", self.domain, port_part, sub_p, token)
        }
    }

    /// 返回脱敏后的 Cloudflare API Token 用于前端回显
    pub fn masked_cf_token(&self) -> String {
        if self.cf_api_token.is_empty() {
            String::new()
        } else if self.cf_api_token.len() <= 8 {
            "********".to_string()
        } else {
            format!(
                "{}****{}",
                &self.cf_api_token[..3],
                &self.cf_api_token[self.cf_api_token.len() - 4..]
            )
        }
    }
}

/// 系统安全与 Fail2ban 设置（单例，id = 1）
#[derive(Debug, Clone, FromRow, serde::Serialize, serde::Deserialize)]
pub struct SecuritySettings {
    pub id: i64,
    pub fail2ban_enabled: i64,
    pub ssh_protection: i64,
    pub web_protection: i64,
    pub maxretry: i64,
    pub findtime_secs: i64,
    pub bantime_secs: i64,
    pub whitelist_ips: String,
    pub updated_at: i64,
}

impl SecuritySettings {
    pub fn is_fail2ban_enabled(&self) -> bool {
        self.fail2ban_enabled == 1
    }

    pub fn is_ssh_protected(&self) -> bool {
        self.ssh_protection == 1
    }

    pub fn is_web_protected(&self) -> bool {
        self.web_protection == 1
    }
}

#[derive(Clone, Debug, sqlx::FromRow, serde::Serialize, serde::Deserialize)]
pub struct SystemSettings {
    pub id: i64,
    pub public_host: String,
    pub updated_at: i64,
}

#[derive(Clone, Debug, sqlx::FromRow, serde::Serialize, serde::Deserialize)]
pub struct DdnsSettings {
    pub id: i64,
    pub enabled: i64,
    pub cf_api_token: String,
    pub ipv4_enabled: i64,
    pub ipv4_domains: String,
    pub ipv6_enabled: i64,
    pub ipv6_domains: String,
    pub check_interval_mins: i64,
    pub cf_proxied: i64,
    pub auto_sync_public_host: i64,
    pub last_ipv4: String,
    pub last_ipv6: String,
    pub last_sync_at: i64,
    pub last_status: String,
    pub updated_at: i64,
}

impl DdnsSettings {
    pub fn is_enabled(&self) -> bool {
        self.enabled == 1
    }

    pub fn is_ipv4_enabled(&self) -> bool {
        self.ipv4_enabled == 1
    }

    pub fn is_ipv6_enabled(&self) -> bool {
        self.ipv6_enabled == 1
    }

    pub fn is_cf_proxied(&self) -> bool {
        self.cf_proxied == 1
    }

    pub fn is_auto_sync_public_host(&self) -> bool {
        self.auto_sync_public_host == 1
    }

    pub fn masked_token(&self) -> String {
        if self.cf_api_token.is_empty() {
            "".to_string()
        } else if self.cf_api_token.len() <= 8 {
            "********".to_string()
        } else {
            format!(
                "{}***{}",
                &self.cf_api_token[..3],
                &self.cf_api_token[self.cf_api_token.len() - 4..]
            )
        }
    }

    pub fn parse_ipv4_domains(&self) -> Vec<String> {
        Self::parse_domains(&self.ipv4_domains)
    }

    pub fn parse_ipv6_domains(&self) -> Vec<String> {
        Self::parse_domains(&self.ipv6_domains)
    }

    fn parse_domains(raw: &str) -> Vec<String> {
        raw.split([',', '\n', '\r', ';'])
            .map(|s| s.trim().trim_end_matches('.').to_string())
            .filter(|s| !s.is_empty())
            .collect()
    }
}

/// Cloudflare Tunnels (cloudflared) 守护配置与状态（单例，id = 1）
#[derive(Clone, Debug, sqlx::FromRow, serde::Serialize, serde::Deserialize)]
pub struct TunnelSettings {
    pub id: i64,
    pub enabled: i64,
    pub tunnel_token: String,
    pub custom_path: String,
    pub last_status: String,
    pub updated_at: i64,
}

impl TunnelSettings {
    pub fn is_enabled(&self) -> bool {
        self.enabled == 1
    }

    pub fn masked_token(&self) -> String {
        if self.tunnel_token.is_empty() {
            "".to_string()
        } else if self.tunnel_token.len() <= 8 {
            "********".to_string()
        } else {
            format!(
                "{}***{}",
                &self.tunnel_token[..3],
                &self.tunnel_token[self.tunnel_token.len() - 4..]
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_panel_path() {
        assert_eq!(normalize_panel_path(""), "");
        assert_eq!(normalize_panel_path("/"), "");
        assert_eq!(normalize_panel_path("///"), "");
        assert_eq!(normalize_panel_path("/xxjjye"), "/xxjjye");
        assert_eq!(normalize_panel_path("xxjjye"), "/xxjjye");
        assert_eq!(normalize_panel_path("/xxjjye/"), "/xxjjye");
        assert_eq!(normalize_panel_path("///xxjjye///"), "/xxjjye");
    }

    #[test]
    fn test_normalize_sub_path() {
        assert_eq!(normalize_sub_path(""), "/sub");
        assert_eq!(normalize_sub_path("/"), "/sub");
        assert_eq!(normalize_sub_path("/mysub"), "/mysub");
        assert_eq!(normalize_sub_path("mysub"), "/mysub");
        assert_eq!(normalize_sub_path("/mysub/"), "/mysub");
        assert_eq!(normalize_sub_path("///mysub///"), "/mysub");
    }

    #[test]
    fn test_proxy_settings_full_url() {
        let mut settings = ProxySettings {
            id: 1,
            enabled: 1,
            domain: "jp2.856668.xyz".to_string(),
            listen_port: 8443,
            path: "/xxjjye".to_string(),
            sub_path: "/sub".to_string(),
            cert_mode: "cf_dns".to_string(),
            cf_api_token: "".to_string(),
            cf_zone_id: "".to_string(),
            custom_cert_path: "".to_string(),
            custom_key_path: "".to_string(),
            cert_expires_at: None,
            last_renewed_at: None,
            last_error: None,
            updated_at: 0,
        };
        assert_eq!(settings.full_url(), "https://jp2.856668.xyz:8443/xxjjye");

        // Standard 443 port omits :443
        settings.listen_port = 443;
        assert_eq!(settings.full_url(), "https://jp2.856668.xyz/xxjjye");

        // Root path
        settings.path = "".to_string();
        assert_eq!(settings.full_url(), "https://jp2.856668.xyz");
    }

    #[test]
    fn test_proxy_settings_full_sub_url() {
        let mut settings = ProxySettings {
            id: 1,
            enabled: 1,
            domain: "jp2.856668.xyz".to_string(),
            listen_port: 8443,
            path: "/xxjjye".to_string(),
            sub_path: "/sub".to_string(),
            cert_mode: "cf_dns".to_string(),
            cf_api_token: "".to_string(),
            cf_zone_id: "".to_string(),
            custom_cert_path: "".to_string(),
            custom_key_path: "".to_string(),
            cert_expires_at: None,
            last_renewed_at: None,
            last_error: None,
            updated_at: 0,
        };
        assert_eq!(
            settings.full_sub_url("token123"),
            "https://jp2.856668.xyz:8443/sub/token123"
        );

        // Standard 443 port omits :443
        settings.listen_port = 443;
        assert_eq!(
            settings.full_sub_url("token123"),
            "https://jp2.856668.xyz/sub/token123"
        );

        // Custom sub_path
        settings.sub_path = "/mysub".to_string();
        assert_eq!(
            settings.full_sub_url("token123"),
            "https://jp2.856668.xyz/mysub/token123"
        );

        // Empty token
        assert_eq!(settings.full_sub_url(""), "https://jp2.856668.xyz/mysub");
    }

    #[test]
    fn test_ddns_settings_masking_and_domains() {
        let mut ddns = DdnsSettings {
            id: 1,
            enabled: 1,
            cf_api_token: "sk-1234567890abcdef".to_string(),
            ipv4_enabled: 1,
            ipv4_domains: "vps1.example.com, vps2.example.com.\n vps3.example.com; ".to_string(),
            ipv6_enabled: 0,
            ipv6_domains: "".to_string(),
            check_interval_mins: 5,
            cf_proxied: 0,
            auto_sync_public_host: 1,
            last_ipv4: "1.2.3.4".to_string(),
            last_ipv6: "".to_string(),
            last_sync_at: 100,
            last_status: "Success".to_string(),
            updated_at: 100,
        };

        assert!(ddns.is_enabled());
        assert!(ddns.is_ipv4_enabled());
        assert!(!ddns.is_ipv6_enabled());
        assert!(!ddns.is_cf_proxied());
        assert!(ddns.is_auto_sync_public_host());

        assert_eq!(ddns.masked_token(), "sk-***cdef");
        ddns.cf_api_token = "short".to_string();
        assert_eq!(ddns.masked_token(), "********");
        ddns.cf_api_token = "".to_string();
        assert_eq!(ddns.masked_token(), "");

        let domains = ddns.parse_ipv4_domains();
        assert_eq!(
            domains,
            vec![
                "vps1.example.com".to_string(),
                "vps2.example.com".to_string(),
                "vps3.example.com".to_string()
            ]
        );
    }

    #[test]
    fn test_tunnel_settings_masking() {
        let mut tunnel = TunnelSettings {
            id: 1,
            enabled: 1,
            tunnel_token: "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.token".to_string(),
            custom_path: "".to_string(),
            last_status: "Running".to_string(),
            updated_at: 100,
        };

        assert!(tunnel.is_enabled());
        assert_eq!(tunnel.masked_token(), "eyJ***oken");

        tunnel.tunnel_token = "short".to_string();
        assert_eq!(tunnel.masked_token(), "********");

        tunnel.tunnel_token = "".to_string();
        assert_eq!(tunnel.masked_token(), "");
    }
}
