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
    pub created_at: Option<i64>,
}

#[derive(Debug, Clone, FromRow)]
pub struct TrafficSnapshot {
    pub client_id: i64,
    pub uplink: i64,
    pub downlink: i64,
    pub recorded_at: i64,
}

#[derive(Debug, Clone, FromRow)]
pub struct Session {
    pub session_id: String,
    pub admin_id: i64,
    pub expires_at: i64,
    pub created_at: i64,
}
