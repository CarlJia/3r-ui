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
}
