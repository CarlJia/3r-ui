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

/// `routing.domainStrategy`. Pulled out so the production code and the
/// `domain_strategy_is_as_is` test cannot drift apart.
///
/// `AsIs` (rather than `IPIfNonMatch`) means the proxy host never resolves
/// user destinations — `IPIfNonMatch` would put every user destination into
/// the proxy's resolver log alongside `connection_logs.dest`.
pub const DOMAIN_STRATEGY: &str = "AsIs";

/// Re-read every active inbound and its active clients, and write
/// `${data_dir}/xray/config.json` at mode 0600.
///
/// Returns the number of *user* inbounds written (the API inbound is not
/// counted), so callers can log or test.
pub async fn render(pool: &SqlitePool, config_path: &Path) -> AppResult<usize> {
    let inbounds = active_inbounds(pool).await?;
    let proxy_domain = crate::proxy::config::get_proxy_settings(pool)
        .await
        .ok()
        .map(|s| s.domain);
    let data_dir = config_path
        .parent()
        .and_then(|p| p.parent())
        .unwrap_or(Path::new("."));

    let mut inbound_values: Vec<Value> = Vec::with_capacity(inbounds.len() + 1);
    for inbound in &inbounds {
        let clients = active_clients(pool, inbound.id).await?;
        inbound_values.push(render_inbound(
            inbound,
            &clients,
            data_dir,
            proxy_domain.as_deref(),
        )?);
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
            "domainStrategy": DOMAIN_STRATEGY,
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
                xhttp_path, protocol, cert_mode, custom_cert_path, custom_key_path, masquerade_url, \
                port_range, transport_protocol, skip_cert_verify, udp, status, created_at, updated_at \
         FROM inbounds WHERE status = 'active' ORDER BY id",
    )
    .fetch_all(pool)
    .await
    .map_err(AppError::Database)
}

pub async fn active_clients(pool: &SqlitePool, inbound_id: i64) -> AppResult<Vec<Client>> {
    sqlx::query_as::<_, Client>(
        "SELECT c.id, c.inbound_id, c.uuid, c.name, c.quota_bytes, c.expiry_at, c.status, c.sub_token, c.mieru_username, c.mieru_password, c.created_at \
         FROM clients c \
         WHERE (c.inbound_id = ? OR EXISTS ( \
             SELECT 1 FROM client_inbounds ci WHERE ci.client_id = c.id AND ci.inbound_id = ? \
         )) AND c.status = 'active' \
         ORDER BY c.id",
    )
    .bind(inbound_id)
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

/// Resolve the path xray's `xhttpSettings.path` should carry.
///
/// `Inbound.xhttp_path` is nullable: the column was added by
/// `migrations/0004_xhttp_path.sql`, and any row that predates the backfill
/// (or somehow missed it) returns `None`. In that case we derive a
/// deterministic path from the inbound's stable fields rather than the bare
/// `/` the migration replaced — shipping `/` would silently reintroduce the
/// L4 leak. Determinism matters: `render_inbound`, `crate::share::vless`
/// and the inbound form all call this, and they must agree byte-for-byte or
/// the tunnel hangs on connect.
pub(crate) fn effective_xhttp_path(inbound: &Inbound) -> String {
    if let Some(path) = inbound
        .xhttp_path
        .as_deref()
        .filter(|p| !p.is_empty() && crate::xray::validate::validate_xhttp_path(p).is_ok())
    {
        return path.to_string();
    }
    deterministic_fallback_path(inbound)
}

/// Stable fallback when `xhttp_path` is NULL/invalid. Hashes the inbound's
/// stable identity so every caller computes the same `/r/<16hex>` without
/// needing a write. Uses splitmix64 — tiny, stable, no dependencies.
fn deterministic_fallback_path(inbound: &Inbound) -> String {
    // Mix id + short_id + public_key so a row is stable across restarts.
    let mut h: u64 = 0x9E37_79B9_7F4A_7C15;
    for byte in inbound.id.to_le_bytes() {
        h = splitmix64(h ^ byte as u64);
    }
    for byte in inbound.short_id.as_bytes() {
        h = splitmix64(h ^ *byte as u64);
    }
    for byte in inbound.public_key.as_bytes() {
        h = splitmix64(h ^ *byte as u64);
    }
    format!("/r/{h:016x}")
}

fn splitmix64(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = x;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

fn render_inbound(
    inbound: &Inbound,
    clients: &[Client],
    data_dir: &Path,
    proxy_domain: Option<&str>,
) -> AppResult<Value> {
    if inbound.protocol == "hysteria2" {
        render_hysteria2_inbound(inbound, clients, data_dir, proxy_domain)
    } else {
        Ok(render_vless_inbound(inbound, clients))
    }
}

/// Render one VLESS + REALITY + XHTTP inbound.
///
/// The client `email` field is load-bearing: xray keys its per-user traffic
/// counters by email, and a client without one is excluded from StatsService
/// entirely. We set it to the UUID so the stat names the panel can map back.
fn render_vless_inbound(inbound: &Inbound, clients: &[Client]) -> Value {
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
                // Per-inbound random path (migrations/0004_xhttp_path.sql),
                // not the hardcoded `/` xray's docs use as an example. A
                // bare `/` here means anyone scanning the port sees a
                // working endpoint at the root path — combined with the
                // REALITY fallback behaviour that turns this port into a
                // gateway to the `dest` host when auth fails, that lets a
                // passive scanner reach whatever the operator set as
                // `dest`. The path goes into the share URI so legitimate
                // clients can still reach the inbound; an out-of-the-box
                // share link that lost the path would silently hang on
                // `connect` rather than fail loudly, so any regenerated
                // path must round-trip through `share::vless::build_uri`.
                "path": effective_xhttp_path(inbound),
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
            // VLESS commands already carry the destination, so sniffing only
            // matters to recover an HTTP Host when the inner protocol is
            // plain HTTP. Including `tls` here would let xray pull an SNI
            // from any inner TLS ClientHello (a wrapped inner TLS tunnel, an
            // HTTP/2 `:authority`, an accidental nested TLS) and overwrite
            // the VLESS-specified destination with it — both routing and
            // `connection_logs.dest` then record the inner SNI rather than
            // the real target. `http` only is the minimum that lets the
            // freedom outbound handle bare HTTP/1.1 correctly.
            "destOverride": ["http"],
        },
    })
}

/// Render one Hysteria 2 inbound using native Xray protocol.
fn render_hysteria2_inbound(
    inbound: &Inbound,
    clients: &[Client],
    data_dir: &Path,
    proxy_domain: Option<&str>,
) -> AppResult<Value> {
    let (cert_file, key_file) =
        crate::proxy::cert::resolve_inbound_cert(data_dir, inbound, proxy_domain)?;

    let cert_file_abs = if cert_file.is_absolute() {
        cert_file
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(&cert_file))
            .unwrap_or(cert_file)
    };
    let key_file_abs = if key_file.is_absolute() {
        key_file
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(&key_file))
            .unwrap_or(key_file)
    };

    let client_values: Vec<Value> = clients
        .iter()
        .map(|c| {
            json!({
                "auth": c.uuid,
                "email": c.uuid,
                "level": 0,
            })
        })
        .collect();

    let masquerade_url = if inbound.masquerade_url.trim().is_empty() {
        "https://news.ycombinator.com/"
    } else {
        inbound.masquerade_url.trim()
    };

    Ok(json!({
        "tag": format!("inbound-{}", inbound.id),
        "listen": "0.0.0.0",
        "port": inbound.listen_port,
        "protocol": "hysteria",
        "settings": {
            "version": 2,
            "users": client_values,
            "ignoreClientBandwidth": false,
        },
        "streamSettings": {
            "network": "hysteria",
            "security": "tls",
            "tlsSettings": {
                "certificates": [
                    {
                        "certificateFile": cert_file_abs.to_string_lossy(),
                        "keyFile": key_file_abs.to_string_lossy(),
                    }
                ],
                "alpn": ["h3"],
            },
            "hysteriaSettings": {
                "version": 2,
                "udpIdleTimeout": 60,
                "masquerade": {
                    "type": "proxy",
                    "url": masquerade_url,
                },
            },
        },
        "sniffing": {
            "enabled": true,
            "destOverride": ["http"],
        },
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inbound() -> Inbound {
        Inbound {
            id: 1,
            listen_port: 443,
            dest: "www.apple.com".into(),
            sni: "www.apple.com".into(),
            server_name: None,
            private_key: "PRIVATE".into(),
            public_key: "PUBLIC".into(),
            short_id: "abcd".into(),
            xhttp_path: Some("/r/abcdef0123456789".into()),
            protocol: "vless".into(),
            cert_mode: "reality".into(),
            custom_cert_path: "".into(),
            custom_key_path: "".into(),
            masquerade_url: "".into(),
            port_range: "".into(),
            transport_protocol: "TCP".into(),
            skip_cert_verify: 0,
            udp: 1,
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
            sub_token: Some("token123".into()),
            mieru_username: Some("user1".into()),
            mieru_password: Some("pass1".into()),
            created_at: None,
        }
    }

    #[test]
    fn inbound_uses_vless_reality_xhttp() {
        let v = render_inbound(&inbound(), &[], Path::new("."), None).unwrap();
        assert_eq!(v["protocol"], "vless");
        assert_eq!(v["port"], 443);
        let ss = &v["streamSettings"];
        assert_eq!(ss["network"], "xhttp");
        assert_eq!(ss["security"], "reality");
        // The path is per-inbound, not the bare `/` that used to ship by
        // default: a public root path is what let a passive scanner find the
        // REALITY endpoint (see migrations/0004_xhttp_path.sql). The share
        // URI carries the same value (src/share/vless.rs), so the client
        // requests the path the server actually answers on.
        assert_eq!(ss["xhttpSettings"]["path"], "/r/abcdef0123456789");
        assert_eq!(ss["realitySettings"]["dest"], "www.apple.com:443");
        assert_eq!(ss["realitySettings"]["serverNames"][0], "www.apple.com");
        assert_eq!(ss["realitySettings"]["shortIds"][0], "abcd");
    }

    #[test]
    fn inbound_uses_hysteria2() {
        let temp_dir = tempfile::tempdir().unwrap();
        let mut ib = inbound();
        ib.protocol = "hysteria2".into();
        ib.listen_port = 8443;
        ib.sni = "hy2.example.com".into();

        let v = render_inbound(&ib, &[client("abc-123")], temp_dir.path(), None).unwrap();
        assert_eq!(v["protocol"], "hysteria");
        assert_eq!(v["port"], 8443);
        assert_eq!(v["settings"]["version"], 2);
        assert_eq!(v["settings"]["users"][0]["auth"], "abc-123");
        assert_eq!(v["settings"]["users"][0]["email"], "abc-123");
        assert_eq!(v["streamSettings"]["network"], "hysteria");
        assert_eq!(v["streamSettings"]["security"], "tls");
        assert_eq!(v["streamSettings"]["hysteriaSettings"]["version"], 2);
    }

    /// A row that predates migration 0004 (or somehow missed the backfill)
    /// must not silently fall back to `/` — that would reintroduce the leak.
    #[test]
    fn missing_xhttp_path_falls_back_to_a_generated_one() {
        let mut ib = inbound();
        ib.xhttp_path = None;
        let v = render_inbound(&ib, &[], Path::new("."), None).unwrap();
        let path = v["streamSettings"]["xhttpSettings"]["path"]
            .as_str()
            .expect("path must be a string");
        assert_ne!(path, "/", "must not fall back to the leaked root path");
        assert!(
            crate::xray::validate::validate_xhttp_path(path).is_ok(),
            "generated path must be valid: {path}"
        );
    }

    /// An invalid stored path (someone hand-edited the DB) must not be
    /// rendered as-is: xray would accept it and clients would never match.
    #[test]
    fn invalid_stored_xhttp_path_is_replaced() {
        let mut ib = inbound();
        ib.xhttp_path = Some("/".into());
        let v = render_inbound(&ib, &[], Path::new("."), None).unwrap();
        let path = v["streamSettings"]["xhttpSettings"]["path"]
            .as_str()
            .expect("path must be a string");
        assert_ne!(path, "/");
    }

    /// The single most load-bearing detail in this unit: without `email`,
    /// xray does not count a client's traffic, so quota enforcement (U7)
    /// would never fire.
    #[test]
    fn client_carries_email_equal_to_uuid() {
        let v = render_inbound(&inbound(), &[client("abc-123")], Path::new("."), None).unwrap();
        let c = &v["settings"]["clients"][0];
        assert_eq!(c["id"], "abc-123");
        assert_eq!(c["email"], "abc-123");
        assert_eq!(c["level"], 0);
    }

    #[test]
    fn no_clients_yields_an_empty_client_array() {
        let v = render_inbound(&inbound(), &[], Path::new("."), None).unwrap();
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

    /// `tls` must stay out of `destOverride`: any inner TLS ClientHello would
    /// otherwise be parsed and its SNI used as the destination, leaking the
    /// real target into routing decisions and the access-log `dest` column.
    #[test]
    fn sniffing_does_not_override_with_tls() {
        let v = render_inbound(&inbound(), &[], Path::new("."), None).unwrap();
        let overrides: Vec<&str> = v["sniffing"]["destOverride"]
            .as_array()
            .unwrap()
            .iter()
            .map(|x| x.as_str().unwrap())
            .collect();
        assert!(
            overrides.contains(&"http"),
            "http override must remain, got {overrides:?}"
        );
        assert!(
            !overrides.contains(&"tls"),
            "tls override must be off to prevent inner-SNI leakage, got {overrides:?}"
        );
    }

    /// `AsIs` keeps destinations in domain form rather than letting the proxy
    /// resolve them itself, so the proxy's resolver never sees the user's
    /// destinations. Held in [`DOMAIN_STRATEGY`] so a future edit cannot drift
    /// between production and tests.
    #[test]
    fn domain_strategy_is_as_is() {
        assert_eq!(DOMAIN_STRATEGY, "AsIs");
    }
}
