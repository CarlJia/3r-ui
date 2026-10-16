//! Render the xray-core configuration from SQLite state.
//!
//! SQLite is the single source of truth. Whenever an inbound or client
//! changes, [`render`] rebuilds `config.json` from the database and the caller
//! asks the supervisor to apply it — live when only the user list moved, by
//! restarting xray otherwise (see [`crate::xray::process`]).
//!
//! The rendered file contains the REALITY private key, so it is written with
//! mode 0600.

use std::path::Path;

use serde_json::{json, Value};
use sqlx::SqlitePool;

use crate::error::{AppError, AppResult};

use crate::db::models::{Client, Inbound};

/// Where xray listens for its gRPC API (traffic counters, live user edits).
/// Bound to loopback: the API is unauthenticated, so exposing it would let
/// anyone who can reach the port read per-user traffic and edit users.
pub const API_ADDR: &str = "127.0.0.1:10085";

/// Tag of the inbound the gRPC API attaches to. `api.tag` names it, and API
/// calls target it.
pub const API_TAG: &str = "api";

/// Re-read every active inbound and its active clients, and write
/// `${data_dir}/xray/config.json` at mode 0600.
///
/// Returns the number of *user* inbounds written (the API inbound is not
/// counted), so callers can log or test.
pub async fn render(pool: &SqlitePool, config_path: &Path) -> AppResult<usize> {
    let inbounds = active_inbounds(pool).await?;

    let mut inbound_values: Vec<Value> = Vec::with_capacity(inbounds.len() + 1);
    for inbound in &inbounds {
        let clients = active_clients(pool, inbound.id).await?;
        inbound_values.push(render_inbound(inbound, &clients));
    }
    // Last, so `inbounds[0]` stays the user's inbound for readers of the file.
    inbound_values.push(api_inbound()?);

    // The API rule comes first: `api.tag` names the synthetic *outbound* the
    // API handlers hide behind, so an API call only reaches them when this
    // inbound is routed there. Behind the catch-all below it would be proxied
    // as ordinary traffic — into a loop, since the API inbound points at itself.
    let mut rules = vec![api_routing_rule()];
    rules.push(json!({ "type": "field", "outboundTag": "blocked", "domain": ["geosite:category-ads-all"] }));
    rules.push(json!({ "type": "field", "outboundTag": "direct", "network": "tcp,udp" }));

    let config = json!({
        // 空字符串是 xray 唯一"写标准输出"的取值（`none` 才是关闭），面板
        // process.rs 捕获子进程 stdout 后逐行消费：access 行由 src/xray/logs.rs
        // 解析后落库 connection_logs，其余行写 $DATA_DIR/xray.log。
        // 注意：xray 不认 `@stderr` 这类伪路径——它会被当作文件名，在 cwd
        // 生成一个名为 `@stderr` 的杂散文件，而真正的管道什么都收不到。
        "log": {
            "loglevel": "warning",
            "access": "",
            "error": "",
        },
        "stats": {},
        "api": {
            // HandlerService is what applies a user-list change to the running
            // xray without a restart (see `xray::api`); StatsService is the
            // traffic counters.
            "services": ["HandlerService", "StatsService"],
            "tag": API_TAG,
        },
        "policy": policy(),
        "inbounds": inbound_values,
        "outbounds": [
            { "tag": "direct", "protocol": "freedom", "settings": {} },
            { "tag": "blocked", "protocol": "blackhole", "settings": {} },
        ],
        "routing": {
            "rules": rules,
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

/// The inbound xray serves its gRPC API on.
///
/// xray has no standalone API listener: the API attaches to an inbound tagged
/// [`API_TAG`], whose traffic [`render`] routes to the synthetic outbound of
/// the same name. Miss either half — the inbound or the routing rule — and xray
/// accepts the config but the port is dead: nothing listens, or connections are
/// proxied as ordinary traffic. In xray's schema `listen` is a bare host and
/// the port is a sibling field, so the shared [`API_ADDR`] is split here — the
/// panel's client dials the same address.
fn api_inbound() -> AppResult<Value> {
    let (host, port) = API_ADDR
        .rsplit_once(':')
        .and_then(|(host, port)| port.parse::<u16>().ok().map(|port| (host, port)))
        .ok_or_else(|| AppError::Xray(format!("API_ADDR is not host:port: {API_ADDR}")))?;

    Ok(json!({
        "tag": API_TAG,
        "listen": host,
        "port": port,
        "protocol": "dokodemo-door",
        "settings": { "address": host },
    }))
}

/// xray's traffic policy.
///
/// `statsUserUplink`/`statsUserDownlink` are what make xray create the per-user
/// counters the panel polls (`user>>><email>>>traffic>>><direction>`, see
/// [`crate::xray::stats`]). They default to false, and without them the counters
/// never exist at all: `QueryStats` comes back empty for every client,
/// `traffic_snapshots` stays empty and quota enforcement has nothing to compare
/// against. `statsOutbound*` only covers outbound counters, a different
/// namespace.
fn policy() -> Value {
    json!({
        "levels": {
            "0": {
                "handshake": 4,
                "connIdle": 300,
                "uplinkOnly": 1,
                "downlinkOnly": 1,
                "statsUserUplink": true,
                "statsUserDownlink": true,
            }
        },
        "system": { "statsOutboundUplink": true, "statsOutboundDownlink": true },
    })
}

/// The routing rule that hands [`api_inbound`]'s traffic to xray's API handlers.
///
/// Without it the API inbound is just a dokodemo-door pointing at itself: the
/// port accepts connections, which then loop back as proxy traffic, and every
/// call hangs until it times out.
fn api_routing_rule() -> Value {
    json!({ "type": "field", "inboundTag": [API_TAG], "outboundTag": API_TAG })
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
                // Must be the path a client actually requests. A client posts
                // its own `path` with a trailing slash appended when it has
                // none, and the server matches exactly — so `/xhttp` here
                // never matches the `/xhttp/` a client sends, and the inbound
                // accepts the connection only to drop it (no access-log line,
                // client hangs). We keep the default a client falls back to
                // when its share link carries no `path` (see src/share/vless.rs,
                // which omits it), which makes the two halves agree.
                "path": "/",
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
        // The client's share link (src/share/vless.rs) carries no `path`, so a
        // client requests the default `/`; a server path of `/xhttp` would
        // never match it and the tunnel would silently drop every connection.
        assert_eq!(ss["xhttpSettings"]["path"], "/");
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

    /// Without these flags xray never creates the `user>>>` counters the stats
    /// poll reads, so every client would silently report zero traffic forever.
    #[test]
    fn policy_enables_per_user_traffic_counters() {
        let p = policy();
        assert_eq!(p["levels"]["0"]["statsUserUplink"], true);
        assert_eq!(p["levels"]["0"]["statsUserDownlink"], true);
    }

    #[test]
    fn api_routing_rule_sends_api_traffic_to_the_api_outbound() {
        let rule = api_routing_rule();
        assert_eq!(rule["inboundTag"][0], API_TAG);
        assert_eq!(rule["outboundTag"], API_TAG);
    }

    #[test]
    fn api_inbound_listens_where_the_api_client_dials() {
        let v = api_inbound().unwrap();
        assert_eq!(v["tag"], API_TAG);
        assert_eq!(v["protocol"], "dokodemo-door");
        assert_eq!(v["settings"]["address"], "127.0.0.1");
        assert_eq!(
            format!("{}:{}", v["listen"].as_str().unwrap(), v["port"]),
            API_ADDR,
            "the API inbound must listen where the API clients connect"
        );
    }
}
