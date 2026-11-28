//! Client subscription engine.
//!
//! Provides `GET /sub/:token` to serve Base64 encoded node list for proxy clients
//! (Clash, Sing-box, v2rayN, NekoBox, etc.) with standard `Subscription-Userinfo` header,
//! and serves an aesthetic web preview dashboard when accessed via a web browser.

use std::collections::HashMap;

use askama::Template;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use base64::Engine;

use crate::crypto::session::now_unix;
use crate::db::models::{Client, Inbound};
use crate::web::AppState;

#[derive(Clone, Debug)]
pub struct SubPreviewNode {
    pub protocol: String,
    pub protocol_label: String,
    pub name: String,
    pub listen_port: i64,
    pub host_or_sni: String,
    pub uri: String,
    pub qr_data_uri: String,
}

#[derive(Clone, Debug)]
pub struct SubPreviewTraffic {
    pub upload_formatted: String,
    pub download_formatted: String,
    pub used_formatted: String,
    pub total_formatted: String,
    pub remaining_formatted: String,
    pub percent: u32,
    pub has_quota: bool,
}

#[derive(Template)]
#[template(path = "sub_preview.html", escape = "html")]
pub struct SubPreviewTemplate {
    pub client_name: String,
    pub client_uuid: String,
    pub status: String,
    pub expiry_date: String,
    pub remaining_days: String,
    pub traffic: SubPreviewTraffic,
    pub sub_url: String,
    pub sub_qr_data_uri: String,
    pub nodes: Vec<SubPreviewNode>,
    pub raw_base64: String,
    pub base_path: String,
}

/// Decide whether the incoming request is asking for a human-friendly browser preview.
pub fn is_browser_preview(headers: &HeaderMap, query: &HashMap<String, String>) -> bool {
    // 1. Explicit query parameters take highest precedence (?preview=1 or ?html=1)
    if query.contains_key("preview") || query.contains_key("html") {
        return true;
    }

    // 2. Defensive exclusion for known proxy clients or CLI downloaders
    if let Some(ua) = headers
        .get(axum::http::header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
    {
        let ua_lower = ua.to_ascii_lowercase();
        let proxy_keywords = [
            "clash",
            "sing-box",
            "v2ray",
            "shadowrocket",
            "surge",
            "quantumult",
            "loon",
            "stash",
            "surfboard",
            "nekobox",
            "nekoray",
            "flclash",
            "hiddify",
            "karing",
            "curl",
            "wget",
        ];
        for kw in &proxy_keywords {
            if ua_lower.contains(kw) {
                return false;
            }
        }
    }

    // 3. Check Accept header: browser navigation typically sends `text/html`
    if let Some(accept) = headers
        .get(axum::http::header::ACCEPT)
        .and_then(|v| v.to_str().ok())
    {
        if accept.contains("text/html") {
            return true;
        }
    }

    false
}

/// Public endpoint for client subscription.
pub async fn handle_subscription(
    State(state): State<AppState>,
    Path(token): Path<String>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    if token.is_empty() || token.len() > 64 {
        return (StatusCode::NOT_FOUND, "Not Found").into_response();
    }

    let client = match sqlx::query_as::<_, Client>(
        "SELECT id, inbound_id, uuid, name, quota_bytes, expiry_at, status, sub_token, mieru_username, mieru_password, created_at \
         FROM clients WHERE sub_token = ? AND status = 'active'",
    )
    .bind(&token)
    .fetch_optional(&state.pool)
    .await
    {
        Ok(Some(c)) => c,
        _ => return (StatusCode::NOT_FOUND, "Not Found").into_response(),
    };

    // Check expiry
    if let Some(exp) = client.expiry_at {
        if exp < now_unix() {
            return (StatusCode::NOT_FOUND, "Subscription Expired").into_response();
        }
    }

    // Query all active inbounds mapped to this client
    let mut inbounds = match sqlx::query_as::<_, Inbound>(
        "SELECT i.id, i.listen_ip, i.listen_port, i.dest, i.sni, i.server_name, i.private_key, i.public_key, \
                i.short_id, i.xhttp_path, i.protocol, i.cert_mode, i.custom_cert_path, i.custom_key_path, \
                i.masquerade_url, i.port_range, i.transport_protocol, i.skip_cert_verify, i.udp, i.status, i.outbound_id, i.created_at, i.updated_at \
         FROM inbounds i \
         JOIN client_inbounds ci ON ci.inbound_id = i.id \
         WHERE ci.client_id = ? AND i.status = 'active' \
         ORDER BY i.id ASC",
    )
    .bind(client.id)
    .fetch_all(&state.pool)
    .await
    {
        Ok(ib) => ib,
        Err(e) => {
            tracing::error!(error = %e, "failed to query client inbounds for subscription");
            return (StatusCode::INTERNAL_SERVER_ERROR, "Database Error").into_response();
        }
    };

    // Fallback for legacy single inbound mapping if client_inbounds was empty
    if inbounds.is_empty() && client.inbound_id > 0 {
        if let Ok(Some(single)) = sqlx::query_as::<_, Inbound>(
            "SELECT id, listen_ip, listen_port, dest, sni, server_name, private_key, public_key, \
                    short_id, xhttp_path, protocol, cert_mode, custom_cert_path, custom_key_path, \
                    masquerade_url, port_range, transport_protocol, skip_cert_verify, udp, status, outbound_id, created_at, updated_at \
             FROM inbounds WHERE id = ? AND status = 'active'",
        )
        .bind(client.inbound_id)
        .fetch_optional(&state.pool)
        .await
        {
            inbounds.push(single);
        }
    }

    let effective =
        crate::net::resolve_effective_public_host(&state.pool, &state.config.public_host).await;
    let request_scheme = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .or_else(|| {
            headers
                .get("cf-visitor")
                .and_then(|v| v.to_str().ok())
                .and_then(|s| {
                    if s.contains("https") {
                        Some("https")
                    } else {
                        None
                    }
                })
        });
    let sub_scheme = request_scheme.unwrap_or(effective.scheme);
    let host = if !effective.is_fallback {
        effective.host
    } else {
        let proxy_domain = crate::proxy::config::get_proxy_settings(&state.pool)
            .await
            .ok()
            .map(|s| s.domain)
            .filter(|d| !d.is_empty());
        let request_host = headers
            .get("x-forwarded-host")
            .or_else(|| headers.get(axum::http::header::HOST))
            .and_then(|v| v.to_str().ok())
            .map(|s| s.split(':').next().unwrap_or(s).to_string());
        proxy_domain
            .or(request_host)
            .unwrap_or_else(|| "127.0.0.1".to_string())
    };

    let base_name = client.name.as_deref().unwrap_or("3r-ui");
    let mut uris = Vec::with_capacity(inbounds.len());
    let mut preview_nodes = Vec::with_capacity(inbounds.len());

    let healthy_node_ids = state.node_manager.get_healthy_node_ids().await;
    let all_nodes = crate::db::node::list_nodes(&state.pool)
        .await
        .unwrap_or_default();
    let eligible_nodes: Vec<_> = all_nodes
        .into_iter()
        .filter(|n| healthy_node_ids.contains(&n.id))
        .collect();

    let target_nodes = if eligible_nodes.is_empty() {
        vec![crate::db::node::Node {
            id: 1,
            name: "Master Local".to_string(),
            secret_key: "".to_string(),
            is_local: 1,
            status: "online".to_string(),
            server_address: Some(host.clone()),
            group_id: None,
            last_heartbeat_at: None,
            cpu_percent: None,
            memory_used_bytes: None,
            memory_total_bytes: None,
            disk_used_bytes: None,
            disk_total_bytes: None,
            services_status: None,
            created_at: 0,
            updated_at: 0,
        }]
    } else {
        eligible_nodes
    };

    for target_node in &target_nodes {
        let node_endpoint_host = if target_node.is_local == 1
            && (target_node.server_address.is_none()
                || target_node.server_address.as_deref() == Some("127.0.0.1"))
        {
            &host
        } else {
            target_node.server_address.as_deref().unwrap_or(&host)
        };

        for inbound in &inbounds {
            let mut client_clone = client.clone();
            let proto_label = match inbound.protocol.as_str() {
                "hysteria2" => "Hy2",
                "mieru" => "Mieru",
                "vless" => "VLESS",
                p => p,
            };
            let node_name = if target_node.is_local == 1 {
                format!("{base_name} - {proto_label}")
            } else {
                format!("{base_name} - [{}] {proto_label}", target_node.name)
            };
            client_clone.name = Some(node_name.clone());

            let node_host = if target_node.is_local == 0 {
                node_endpoint_host.to_string()
            } else {
                crate::share::resolve_inbound_host(inbound, node_endpoint_host)
            };

            let uri = match inbound.protocol.as_str() {
                "hysteria2" => crate::share::hy2::build_uri(&client_clone, inbound, &node_host),
                "mieru" => crate::share::mieru::build_uri(&client_clone, inbound, &node_host),
                _ => crate::share::vless::build_uri(&client_clone, inbound, &node_host),
            };
            uris.push(uri.clone());

            let host_or_sni = if !inbound.sni.is_empty() {
                inbound.sni.clone()
            } else if let Some(ref sn) = inbound.server_name {
                if !sn.is_empty() {
                    sn.clone()
                } else {
                    node_host.clone()
                }
            } else {
                node_host.clone()
            };

            let node_qr_data_uri = crate::share::qr::render_png(&uri)
                .map(|bytes| {
                    format!(
                        "data:image/png;base64,{}",
                        base64::engine::general_purpose::STANDARD.encode(&bytes)
                    )
                })
                .unwrap_or_default();

            preview_nodes.push(SubPreviewNode {
                protocol: inbound.protocol.clone(),
                protocol_label: proto_label.to_string(),
                name: node_name,
                listen_port: inbound.listen_port,
                host_or_sni,
                uri,
                qr_data_uri: node_qr_data_uri,
            });
        }
    }

    let raw_content = uris.join("\n");
    let b64 = base64::engine::general_purpose::STANDARD.encode(raw_content.as_bytes());

    // Query user traffic for Subscription-Userinfo header
    let traffic: Option<(i64, i64)> = sqlx::query_as(
        "SELECT COALESCE(uplink, 0), COALESCE(downlink, 0) FROM traffic_snapshots \
         WHERE client_id = ? ORDER BY recorded_at DESC, rowid DESC LIMIT 1",
    )
    .bind(client.id)
    .fetch_optional(&state.pool)
    .await
    .ok()
    .flatten();

    let (upload, download) = traffic.unwrap_or((0, 0));
    let total = client.quota_bytes;
    let expire = client.expiry_at.unwrap_or(0);
    let userinfo = format!("upload={upload}; download={download}; total={total}; expire={expire}");

    let proxy_settings = crate::proxy::config::get_proxy_settings(&state.pool)
        .await
        .ok();
    let sub_token = client.sub_token.clone().unwrap_or_default();
    let sub_url = if let Some(ref p) = proxy_settings {
        if p.is_enabled() && !p.domain.is_empty() {
            p.full_sub_url(&sub_token)
        } else {
            let sub_path = p.normalized_sub_path();
            format!("{sub_scheme}://{host}{sub_path}/{sub_token}")
        }
    } else {
        format!("{sub_scheme}://{host}/sub/{sub_token}")
    };

    // If request indicates a browser navigation / preview mode, render HTML dashboard
    if is_browser_preview(&headers, &query) {
        let sub_qr_data_uri = crate::share::qr::render_png(&sub_url)
            .map(|bytes| {
                format!(
                    "data:image/png;base64,{}",
                    base64::engine::general_purpose::STANDARD.encode(&bytes)
                )
            })
            .unwrap_or_default();

        let expiry_date = match client.expiry_at {
            Some(ts) => crate::web::logs::timestamp_to_date(ts),
            None => "永不到期".to_string(),
        };

        let now = now_unix();
        let remaining_days = match client.expiry_at {
            Some(ts) => {
                let diff = ts.saturating_sub(now);
                if ts < now {
                    "已到期".to_string()
                } else {
                    let days = diff / 86400;
                    if days > 0 {
                        format!("剩余 {days} 天")
                    } else {
                        "剩余不到 1 天".to_string()
                    }
                }
            }
            None => "永久有效".to_string(),
        };

        let used_bytes = (upload + download) as u64;
        let total_bytes = total as u64;
        let has_quota = total_bytes > 0;
        let percent = if has_quota {
            std::cmp::min(100, (used_bytes * 100) / total_bytes) as u32
        } else {
            0
        };

        let remaining_formatted = if has_quota {
            let rem = total_bytes.saturating_sub(used_bytes);
            crate::web::system::format_bytes_human(rem)
        } else {
            "不限流量".to_string()
        };

        let traffic_view = SubPreviewTraffic {
            upload_formatted: crate::web::system::format_bytes_human(upload as u64),
            download_formatted: crate::web::system::format_bytes_human(download as u64),
            used_formatted: crate::web::system::format_bytes_human(used_bytes),
            total_formatted: if has_quota {
                crate::web::system::format_bytes_human(total_bytes)
            } else {
                "不限流量".to_string()
            },
            remaining_formatted,
            percent,
            has_quota,
        };

        let base_path = crate::web::middleware::base_path_from_headers(&headers);
        let template = SubPreviewTemplate {
            client_name: client.name.unwrap_or_else(|| "未命名用户".to_string()),
            client_uuid: client.uuid,
            status: if client.status == "active" {
                "正常启用".to_string()
            } else {
                client.status
            },
            expiry_date,
            remaining_days,
            traffic: traffic_view,
            sub_url,
            sub_qr_data_uri,
            nodes: preview_nodes,
            raw_base64: b64,
            base_path,
        };

        match template.render() {
            Ok(html) => {
                let mut resp = (StatusCode::OK, html).into_response();
                resp.headers_mut().insert(
                    axum::http::header::CONTENT_TYPE,
                    axum::http::HeaderValue::from_static("text/html; charset=utf-8"),
                );
                return resp;
            }
            Err(e) => {
                tracing::error!(error = %e, "failed to render subscription preview template");
                return (StatusCode::INTERNAL_SERVER_ERROR, "Template Error").into_response();
            }
        }
    }

    // Default proxy client response: Base64 node list with download headers
    let mut response = (StatusCode::OK, b64).into_response();
    let headers = response.headers_mut();
    headers.insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    headers.insert(
        axum::http::header::CONTENT_DISPOSITION,
        axum::http::HeaderValue::from_static("attachment; filename*=UTF-8''subscription.txt"),
    );
    if let Ok(v) = axum::http::HeaderValue::from_str(&userinfo) {
        headers.insert(
            axum::http::HeaderName::from_static("subscription-userinfo"),
            v,
        );
    }
    headers.insert(
        axum::http::HeaderName::from_static("profile-update-interval"),
        axum::http::HeaderValue::from_static("24"),
    );

    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{HeaderMap, HeaderValue};

    #[test]
    fn test_is_browser_preview_explicit_query() {
        let headers = HeaderMap::new();
        let mut query = HashMap::new();
        query.insert("preview".to_string(), "1".to_string());
        assert!(is_browser_preview(&headers, &query));

        let mut query_html = HashMap::new();
        query_html.insert("html".to_string(), "true".to_string());
        assert!(is_browser_preview(&headers, &query_html));
    }

    #[test]
    fn test_is_browser_preview_browser_accept() {
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::ACCEPT,
            HeaderValue::from_static(
                "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
            ),
        );
        headers.insert(
            axum::http::header::USER_AGENT,
            HeaderValue::from_static(
                "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) Chrome/120.0.0.0",
            ),
        );
        let query = HashMap::new();
        assert!(is_browser_preview(&headers, &query));
    }

    #[test]
    fn test_is_browser_preview_excludes_proxy_clients() {
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::ACCEPT,
            HeaderValue::from_static("text/html,text/plain,*/*"),
        );
        headers.insert(
            axum::http::header::USER_AGENT,
            HeaderValue::from_static("ClashforWindows/0.20.39"),
        );
        let query = HashMap::new();
        // Proxy client excluded even if accept contains text/html
        assert!(!is_browser_preview(&headers, &query));

        let mut headers_singbox = HeaderMap::new();
        headers_singbox.insert(
            axum::http::header::USER_AGENT,
            HeaderValue::from_static("sing-box 1.8.0"),
        );
        assert!(!is_browser_preview(&headers_singbox, &query));

        let mut headers_curl = HeaderMap::new();
        headers_curl.insert(
            axum::http::header::USER_AGENT,
            HeaderValue::from_static("curl/7.88.1"),
        );
        assert!(!is_browser_preview(&headers_curl, &query));
    }

    #[test]
    fn test_is_browser_preview_defaults_to_false() {
        let mut headers = HeaderMap::new();
        headers.insert(axum::http::header::ACCEPT, HeaderValue::from_static("*/*"));
        let query = HashMap::new();
        assert!(!is_browser_preview(&headers, &query));
    }
}
