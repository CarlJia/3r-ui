//! Render the xray-core configuration from SQLite state.
//!
//! SQLite is the single source of truth. Whenever an inbound or client
//! changes, [`render`] rebuilds `config.json` from the database and the caller
//! signals xray to reload (SIGUSR1 — see [`crate::xray::process`]).
//!
//! The rendered file contains the REALITY private key, so it is written with
//! mode 0600.

use std::path::Path;

use serde_json::{json, Value};
use sqlx::SqlitePool;

use crate::error::{AppError, AppResult};

use crate::db::models::{Client, Inbound};

/// Where xray listens for the StatsService gRPC API. Bound to loopback: the
/// API is unauthenticated, so exposing it would leak per-user traffic to
/// anyone who can reach the port.
pub const STATS_ADDR: &str = "127.0.0.1:10085";

/// Re-read every active inbound and its active clients, and write
/// `${data_dir}/xray/config.json` at mode 0600.
///
/// Returns the number of active inbounds written, so callers can log or test.
pub async fn render(pool: &SqlitePool, config_path: &Path) -> AppResult<usize> {
    let inbounds = active_inbounds(pool).await?;

    let mut inbound_values: Vec<Value> = Vec::with_capacity(inbounds.len());
    for inbound in &inbounds {
        let clients = active_clients(pool, inbound.id).await?;
        inbound_values.push(render_inbound(inbound, &clients));
    }

    let config = json!({
        "log": { "loglevel": "warning" },
        "stats": {},
        "api": {
            "services": ["StatsService"],
            "tag": "api",
        },
        "stats api": {
            "tag": "api",
            "listen": STATS_ADDR,
        },
        "policy": {
            "levels": { "0": { "handshake": 4, "connIdle": 300, "uplinkOnly": 1, "downlinkOnly": 1 } },
            "system": { "statsOutboundUplink": true, "statsOutboundDownlink": true },
        },
        "inbounds": inbound_values,
        "outbounds": [
            { "tag": "direct", "protocol": "freedom", "settings": {} },
            { "tag": "blocked", "protocol": "blackhole", "settings": {} },
        ],
        "routing": {
            "rules": [
                { "type": "field", "outboundTag": "blocked", "domain": ["geosite:category-ads-all"] },
                { "type": "field", "outboundTag": "direct", "network": "tcp,udp" },
            ],
            "domainStrategy": "IPIfNonMatch",
        },
    });

    let body = serde_json::to_vec_pretty(&config)
        .map_err(|e| AppError::Xray(format!("config serialization failed: {e}")))?;

    if let Some(parent) = config_path.parent() {
        std::fs::create_dir_all(parent).map_err(AppError::Io)?;
    }
    // Write to a sibling temp file and rename, so a crash mid-write cannot
    // leave xray with a truncated config on its next reload.
    let tmp_path = config_path.with_extension("json.tmp");
    std::fs::write(&tmp_path, &body).map_err(AppError::Io)?;
    set_owner_only(&tmp_path)?;
    std::fs::rename(&tmp_path, config_path).map_err(AppError::Io)?;

    Ok(inbounds.len())
}

#[cfg(unix)]
fn set_owner_only(path: &Path) -> AppResult<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(AppError::Io)
}

#[cfg(not(unix))]
fn set_owner_only(_path: &Path) -> AppResult<()> {
    Ok(())
}

pub async fn active_inbounds(pool: &SqlitePool) -> AppResult<Vec<Inbound>> {
    sqlx::query_as::<_, Inbound>(
        "SELECT id, listen_port, dest, sni, server_name, private_key, public_key, short_id, \
                status, created_at, updated_at \
         FROM inbounds WHERE status = 'active' ORDER BY id",
    )
    .fetch_all(pool)
    .await
    .map_err(AppError::Database)
}

pub async fn active_clients(pool: &SqlitePool, inbound_id: i64) -> AppResult<Vec<Client>> {
    sqlx::query_as::<_, Client>(
        "SELECT id, inbound_id, uuid, name, quota_bytes, expiry_at, status, created_at \
         FROM clients WHERE inbound_id = ? AND status = 'active' ORDER BY id",
    )
    .bind(inbound_id)
    .fetch_all(pool)
    .await
    .map_err(AppError::Database)
}

/// Render one VLESS + REALITY + XHTTP inbound.
///
/// The client `email` field is load-bearing: xray keys its per-user traffic
/// counters by email, and a client without one is excluded from StatsService
/// entirely. We set it to the UUID so the stat names the panel can map back.
fn render_inbound(inbound: &Inbound, clients: &[Client]) -> Value {
    let client_values: Vec<Value> = clients
        .iter()
        .map(|c| {
            json!({
                "id": c.uuid,
                "email": c.uuid,
                "level": 0,
            })
        })
        .collect();

    json!({
        "tag": format!("inbound-{}", inbound.id),
        "listen": "0.0.0.0",
        "port": inbound.listen_port,
        "protocol": "vless",
        "settings": {
            "clients": client_values,
            "decryption": "none",
        },
        "streamSettings": {
            "network": "xhttp",
            "xhttpSettings": {
                "path": "/xhttp",
                "mode": "auto",
            },
            "security": "reality",
            "realitySettings": {
                "show": false,
                "dest": format!("{}:443", inbound.dest),
                "xver": 0,
                "serverNames": [inbound.sni],
                "privateKey": inbound.private_key,
                "shortIds": [inbound.short_id],
            },
        },
        "sniffing": {
            "enabled": true,
            "destOverride": ["http", "tls"],
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inbound() -> Inbound {
        Inbound {
            id: 1,
            listen_port: 443,
            dest: "www.microsoft.com".into(),
            sni: "www.microsoft.com".into(),
            server_name: None,
            private_key: "PRIVATE".into(),
            public_key: "PUBLIC".into(),
            short_id: "abcd".into(),
            status: "active".into(),
            created_at: None,
            updated_at: None,
        }
    }

    fn client(uuid: &str) -> Client {
        Client {
            id: 1,
            inbound_id: 1,
            uuid: uuid.into(),
            name: None,
            quota_bytes: 0,
            expiry_at: None,
            status: "active".into(),
            created_at: None,
        }
    }

    #[test]
    fn inbound_uses_vless_reality_xhttp() {
        let v = render_inbound(&inbound(), &[]);
        assert_eq!(v["protocol"], "vless");
        assert_eq!(v["port"], 443);
        let ss = &v["streamSettings"];
        assert_eq!(ss["network"], "xhttp");
        assert_eq!(ss["security"], "reality");
        assert_eq!(ss["realitySettings"]["dest"], "www.microsoft.com:443");
        assert_eq!(ss["realitySettings"]["serverNames"][0], "www.microsoft.com");
        assert_eq!(ss["realitySettings"]["shortIds"][0], "abcd");
    }

    /// The single most load-bearing detail in this unit: without `email`,
    /// xray does not count a client's traffic, so quota enforcement (U7)
    /// would never fire.
    #[test]
    fn client_carries_email_equal_to_uuid() {
        let v = render_inbound(&inbound(), &[client("abc-123")]);
        let c = &v["settings"]["clients"][0];
        assert_eq!(c["id"], "abc-123");
        assert_eq!(c["email"], "abc-123");
        assert_eq!(c["level"], 0);
    }

    #[test]
    fn no_clients_yields_an_empty_client_array() {
        let v = render_inbound(&inbound(), &[]);
        assert_eq!(v["settings"]["clients"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn stats_api_is_bound_to_loopback() {
        assert!(STATS_ADDR.starts_with("127.0.0.1:"));
    }
}
