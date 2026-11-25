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

impl ProxySettings {
    pub fn is_enabled(&self) -> bool {
        self.enabled == 1
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
