//! Client subscription engine.
//!
//! Provides `GET /sub/:token` to serve Base64 encoded node list for proxy clients
//! (Clash, Sing-box, v2rayN, NekoBox, etc.) with standard `Subscription-Userinfo` header.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use base64::Engine;

use crate::crypto::session::now_unix;
use crate::db::models::{Client, Inbound};
use crate::web::AppState;

/// Public endpoint for client subscription.
pub async fn handle_subscription(
    State(state): State<AppState>,
    Path(token): Path<String>,
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
        "SELECT i.id, i.listen_port, i.dest, i.sni, i.server_name, i.private_key, i.public_key, \
                i.short_id, i.xhttp_path, i.protocol, i.cert_mode, i.custom_cert_path, i.custom_key_path, \
                i.masquerade_url, i.port_range, i.transport_protocol, i.skip_cert_verify, i.udp, i.status, i.created_at, i.updated_at \
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
            "SELECT id, listen_port, dest, sni, server_name, private_key, public_key, \
                    short_id, xhttp_path, protocol, cert_mode, custom_cert_path, custom_key_path, \
                    masquerade_url, port_range, transport_protocol, skip_cert_verify, udp, status, created_at, updated_at \
             FROM inbounds WHERE id = ? AND status = 'active'",
        )
        .bind(client.inbound_id)
        .fetch_optional(&state.pool)
        .await
        {
            inbounds.push(single);
        }
    }

    let raw_public_host = state.config.public_host.trim();
    let host = if !raw_public_host.is_empty() {
        if raw_public_host.starts_with("https://") {
            raw_public_host
                .trim_start_matches("https://")
                .trim_matches('/')
                .to_string()
        } else if raw_public_host.starts_with("http://") {
            raw_public_host
                .trim_start_matches("http://")
                .trim_matches('/')
                .to_string()
        } else {
            raw_public_host.trim_matches('/').to_string()
        }
    } else {
        let proxy_domain = crate::proxy::config::get_proxy_settings(&state.pool)
            .await
            .ok()
            .map(|s| s.domain)
            .filter(|d| !d.is_empty());
        proxy_domain.unwrap_or_else(|| "127.0.0.1".to_string())
    };

    let base_name = client.name.as_deref().unwrap_or("3r-ui");
    let mut uris = Vec::with_capacity(inbounds.len());
    for inbound in &inbounds {
        let mut client_clone = client.clone();
        let proto_label = match inbound.protocol.as_str() {
            "hysteria2" => "Hy2",
            "mieru" => "Mieru",
            "vless" => "VLESS",
            p => p,
        };
        client_clone.name = Some(format!("{base_name} - {proto_label}"));

        let uri = match inbound.protocol.as_str() {
            "hysteria2" => crate::share::hy2::build_uri(&client_clone, inbound, &host),
            "mieru" => crate::share::mieru::build_uri(&client_clone, inbound, &host),
            _ => crate::share::vless::build_uri(&client_clone, inbound, &host),
        };
        uris.push(uri);
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
