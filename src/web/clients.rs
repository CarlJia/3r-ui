//! Client (VLESS user) CRUD backed by SQLite, with xray config re-render
//! and reload after every mutation.
//!
//! The panel supports exactly one inbound, so clients are implicitly scoped
//! to that one. The schema permits multiple clients per inbound; quota and
//! expiry are enforced by xray after a reload (clients whose status is not
//! `'active'` are omitted from the rendered config — see
//! [`crate::xray::config`]).

use askama::Template;
use axum::extract::{Extension, Path, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum_extra::extract::cookie::CookieJar;

use crate::crypto::session::parse_expiry;
use crate::crypto::uuid;
use crate::db::models::{Client, Inbound};
use crate::web::middleware::{
    csrf_from_cookies, url_encode, BasePath, CurrentAdmin, SECURITY_HEADERS,
};
use crate::web::AppState;
use crate::xray::config;
use crate::xray::SupervisorCommand;

/// 1 GiB. The form takes GB so the user is not asked to count 9s.
pub const BYTES_PER_GB: i64 = 1024 * 1024 * 1024;

#[derive(Template)]
#[template(path = "clients_list.html", escape = "html")]
struct ClientsListTemplate {
    csrf: String,
    clients: Vec<ClientListView>,
    has_inbound: bool,
    base_path: String,
}

#[derive(Template)]
#[template(path = "clients_form.html", escape = "html")]
struct ClientsFormTemplate {
    csrf: String,
    client: Option<ClientView>,
    inbounds: Vec<crate::web::inbound::InboundView>,
    bound_inbound_ids: Vec<i64>,
    error: Option<String>,
    base_path: String,
}

#[derive(Clone, Debug)]
pub struct ClientNodeCard {
    pub inbound_id: i64,
    pub protocol: String,
    pub protocol_label: String,
    pub listen_port: i64,
    pub sni: String,
    pub uri: String,
    pub qr_path: String,
}

#[derive(Template)]
#[template(path = "clients_detail.html", escape = "html")]
struct ClientsDetailTemplate {
    client: ClientView,
    inbound: Inbound,
    uri: String,
    qr_path: String,
    warning: Option<String>,
    sub_url: String,
    sub_qr_path: String,
    nodes: Vec<ClientNodeCard>,
    base_path: String,
}

/// What the templates render. Decoupled from [`Client`] so we can layer in
/// human-friendly formatting (quota GB, expiry as a date string) without
/// changing the storage model.
#[derive(Clone, Debug)]
pub struct ClientView {
    pub id: i64,
    pub uuid: String,
    pub name: String,
    pub quota_gb: i64,
    pub expiry_date: Option<String>,
    pub status: String,
    pub active: bool,
}

#[derive(sqlx::FromRow)]
struct ClientTrafficRow {
    #[sqlx(flatten)]
    client: Client,
    uplink: i64,
    downlink: i64,
}

struct ClientListView {
    base: ClientView,
    used: String,
    uplink: String,
    downlink: String,
}

impl ClientView {
    fn from(c: &Client) -> Self {
        Self {
            id: c.id,
            uuid: c.uuid.clone(),
            name: c.name.clone().unwrap_or_default(),
            quota_gb: c.quota_bytes / BYTES_PER_GB,
            expiry_date: c.expiry_at.and_then(timestamp_to_iso_date),
            status: c.status.clone(),
            active: c.status == "active",
        }
    }
}

#[derive(serde::Deserialize, Default)]
pub struct ClientForm {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub quota_gb: String,
    #[serde(default)]
    pub expiry: String,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub inbound_ids: Option<String>,
}

pub fn generate_sub_token() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 16];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    let mut s = String::with_capacity(32);
    for b in bytes {
        use std::fmt::Write;
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// `GET /admin/clients` — list every non-deleted client, newest first.
pub async fn list(
    State(state): State<AppState>,
    Extension(_admin): Extension<CurrentAdmin>,
    base_path_ext: Option<Extension<BasePath>>,
    jar: CookieJar,
) -> Response {
    let base_path = base_path_ext.map(|Extension(p)| p.0).unwrap_or_default();
    if let Err(e) = disable_expired(&state.pool).await {
        tracing::warn!(error = %e, "expired-client sweep failed before list");
    }
    let has_inbound = current_inbound(&state).await.is_ok_and(|i| i.is_some());
    let rows = match sqlx::query_as::<_, ClientTrafficRow>(
        "SELECT c.id, c.inbound_id, c.uuid, c.name, c.quota_bytes, c.expiry_at, \
                c.status, c.sub_token, c.mieru_username, c.mieru_password, c.created_at, COALESCE(t.uplink, 0) AS uplink, \
                COALESCE(t.downlink, 0) AS downlink \
         FROM clients c LEFT JOIN traffic_snapshots t ON t.rowid = ( \
           SELECT s.rowid FROM traffic_snapshots s WHERE s.client_id = c.id \
           ORDER BY s.recorded_at DESC, s.rowid DESC LIMIT 1 \
         ) WHERE c.status != 'deleted' ORDER BY c.id DESC",
    )
    .fetch_all(&state.pool)
    .await
    {
        Ok(rows) => rows,
        Err(e) => {
            tracing::error!(error = %e, "failed to read client traffic");
            return page(
                jar,
                StatusCode::INTERNAL_SERVER_ERROR,
                "无法读取用户流量".to_string(),
            );
        }
    };
    let clients = rows
        .iter()
        .map(|row| ClientListView {
            base: ClientView::from(&row.client),
            uplink: format_traffic(row.uplink),
            downlink: format_traffic(row.downlink),
            used: format_traffic(row.uplink.saturating_add(row.downlink)),
        })
        .collect();

    let html = ClientsListTemplate {
        csrf: csrf_from_cookies(&jar),
        clients,
        has_inbound,
        base_path,
    }
    .render()
    .unwrap_or_else(|e| format!("template error: {e}"));
    page(jar, StatusCode::OK, html)
}

/// `GET /admin/clients/new` — render an empty create form.
pub async fn new_form(
    State(state): State<AppState>,
    Extension(_admin): Extension<CurrentAdmin>,
    base_path_ext: Option<Extension<BasePath>>,
    jar: CookieJar,
) -> Response {
    let base_path = base_path_ext.map(|Extension(p)| p.0).unwrap_or_default();
    let inbounds = list_all_active_inbounds(&state.pool)
        .await
        .unwrap_or_default();
    let has_inbound = !inbounds.is_empty();
    let bound_inbound_ids: Vec<i64> = inbounds.iter().map(|i| i.id).collect();
    let html = ClientsFormTemplate {
        csrf: csrf_from_cookies(&jar),
        client: None,
        inbounds: inbounds
            .iter()
            .map(crate::web::inbound::InboundView::from)
            .collect(),
        bound_inbound_ids,
        error: if has_inbound {
            None
        } else {
            Some("请先在入站页面创建入站后再添加用户".to_string())
        },
        base_path,
    }
    .render()
    .unwrap_or_else(|e| format!("template error: {e}"));
    page(jar, StatusCode::OK, html)
}

/// `POST /admin/clients` — mint a UUID, validate, persist, reload.
pub async fn create(
    State(state): State<AppState>,
    Extension(_admin): Extension<CurrentAdmin>,
    base_path_ext: Option<Extension<BasePath>>,
    jar: CookieJar,
    axum::extract::Form(form): axum::extract::Form<ClientForm>,
) -> Response {
    let base_path = base_path_ext.map(|Extension(p)| p.0).unwrap_or_default();
    let all_inbounds = list_all_active_inbounds(&state.pool)
        .await
        .unwrap_or_default();
    if all_inbounds.is_empty() {
        return render_form_error(
            &jar,
            None,
            &all_inbounds,
            vec![],
            "请先在入站页面创建入站".to_string(),
            &base_path,
        );
    }

    let parsed = match parse_client_form(&form) {
        Ok(p) => p,
        Err(msg) => return render_form_error(&jar, None, &all_inbounds, vec![], msg, &base_path),
    };

    let new_uuid = match uuid::v4() {
        Ok(u) => u,
        Err(e) => {
            tracing::error!(error = %e, "UUID generation failed");
            return render_form_error(
                &jar,
                None,
                &all_inbounds,
                vec![],
                "UUID 生成失败".to_string(),
                &base_path,
            );
        }
    };

    let sub_token = generate_sub_token();
    let name = if parsed.name.is_empty() {
        None
    } else {
        Some(parsed.name.as_str())
    };

    let selected_inbound_ids: Vec<i64> = form
        .inbound_ids
        .as_deref()
        .unwrap_or("")
        .split(',')
        .filter_map(|s| s.trim().parse::<i64>().ok())
        .collect();

    let final_inbound_ids = if selected_inbound_ids.is_empty() {
        all_inbounds.iter().map(|i| i.id).collect::<Vec<_>>()
    } else {
        selected_inbound_ids
    };

    let primary_inbound_id = final_inbound_ids
        .first()
        .copied()
        .unwrap_or(all_inbounds[0].id);

    let mieru_username = name
        .map(|n| n.trim().to_lowercase().replace(' ', "_"))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| format!("user_{}", &new_uuid[..8.min(new_uuid.len())]));
    let mieru_password = format!("{:016x}", rand::random::<u64>());

    let res = sqlx::query(
        "INSERT INTO clients (inbound_id, uuid, name, quota_bytes, expiry_at, status, sub_token, mieru_username, mieru_password, created_at) \
         VALUES (?, ?, ?, ?, ?, 'active', ?, ?, ?, unixepoch())",
    )
    .bind(primary_inbound_id)
    .bind(&new_uuid)
    .bind(name)
    .bind(parsed.quota_bytes)
    .bind(parsed.expiry_at)
    .bind(&sub_token)
    .bind(&mieru_username)
    .bind(&mieru_password)
    .execute(&state.pool)
    .await;

    let client_id = match res {
        Ok(r) => r.last_insert_rowid(),
        Err(e) => {
            tracing::error!(error = %e, "client insert failed");
            return render_form_error(
                &jar,
                None,
                &all_inbounds,
                vec![],
                "保存失败".to_string(),
                &base_path,
            );
        }
    };

    for ib_id in final_inbound_ids {
        let _ = sqlx::query(
            "INSERT OR IGNORE INTO client_inbounds (client_id, inbound_id) VALUES (?, ?)",
        )
        .bind(client_id)
        .bind(ib_id)
        .execute(&state.pool)
        .await;
    }

    reload(&state).await;
    redirect_to_list(&jar, &base_path)
}

/// `GET /admin/clients/:id/edit` — render the form pre-filled with the row.
pub async fn edit_form(
    State(state): State<AppState>,
    Extension(_admin): Extension<CurrentAdmin>,
    Path(id): Path<i64>,
    base_path_ext: Option<Extension<BasePath>>,
    jar: CookieJar,
) -> Response {
    let base_path = base_path_ext.map(|Extension(p)| p.0).unwrap_or_default();
    let client = match load_client(&state.pool, id).await {
        Ok(Some(c)) => c,
        Ok(None) => return not_found_response(),
        Err(e) => {
            tracing::error!(error = %e, "client lookup failed");
            return not_found_response();
        }
    };

    let inbounds = list_all_active_inbounds(&state.pool)
        .await
        .unwrap_or_default();
    let mut bound_inbound_ids: Vec<i64> =
        sqlx::query_scalar("SELECT inbound_id FROM client_inbounds WHERE client_id = ?")
            .bind(id)
            .fetch_all(&state.pool)
            .await
            .unwrap_or_default();

    if bound_inbound_ids.is_empty() && client.inbound_id > 0 {
        bound_inbound_ids.push(client.inbound_id);
    }

    let html = ClientsFormTemplate {
        csrf: csrf_from_cookies(&jar),
        client: Some(ClientView::from(&client)),
        inbounds: inbounds
            .iter()
            .map(crate::web::inbound::InboundView::from)
            .collect(),
        bound_inbound_ids,
        error: None,
        base_path,
    }
    .render()
    .unwrap_or_else(|e| format!("template error: {e}"));
    page(jar, StatusCode::OK, html)
}

/// `GET /admin/clients/:id` — share / QR detail page.
pub async fn detail(
    State(state): State<AppState>,
    Extension(_admin): Extension<CurrentAdmin>,
    Path(id): Path<i64>,
    base_path_ext: Option<Extension<BasePath>>,
    jar: CookieJar,
) -> Response {
    let base_path = base_path_ext.map(|Extension(p)| p.0).unwrap_or_default();
    let mut client = match load_client(&state.pool, id).await {
        Ok(Some(c)) => c,
        Ok(None) => return not_found_response(),
        Err(e) => {
            tracing::error!(error = %e, "client lookup failed for detail");
            return not_found_response();
        }
    };

    if client.sub_token.is_none() {
        let tok = generate_sub_token();
        let _ = sqlx::query("UPDATE clients SET sub_token = ? WHERE id = ?")
            .bind(&tok)
            .bind(id)
            .execute(&state.pool)
            .await;
        client.sub_token = Some(tok);
    }

    let bound_inbounds: Vec<Inbound> = sqlx::query_as(
        "SELECT i.id, i.listen_ip, i.listen_port, i.dest, i.sni, i.server_name, i.private_key, i.public_key, \
                i.short_id, i.xhttp_path, i.protocol, i.cert_mode, i.custom_cert_path, i.custom_key_path, \
                i.masquerade_url, i.port_range, i.transport_protocol, i.skip_cert_verify, i.udp, i.status, i.created_at, i.updated_at \
         FROM inbounds i \
         JOIN client_inbounds ci ON ci.inbound_id = i.id \
         WHERE ci.client_id = ? AND i.status = 'active' \
         ORDER BY i.id ASC",
    )
    .bind(id)
    .fetch_all(&state.pool)
    .await
    .unwrap_or_default();

    let inbounds = if bound_inbounds.is_empty() {
        if let Ok(Some(single)) = load_active_inbound(&state.pool).await {
            vec![single]
        } else {
            return render_detail_error(&jar, "尚未创建入站，无法生成分享链接".to_string()).await;
        }
    } else {
        bound_inbounds
    };

    let first_inbound = inbounds[0].clone();

    let effective =
        crate::net::resolve_effective_public_host(&state.pool, &state.config.public_host).await;
    let warning = if effective.is_fallback {
        Some("尚未配置节点连接 Host（可在「系统设置」页面配置或设置 PUBLIC_HOST 环境变量），链接中的 host 占位为示例地址，客户端无法直接使用。请在系统设置中配置节点连接地址后重新打开此页面。".to_string())
    } else {
        None
    };

    let sub_scheme = effective.scheme;
    let host = effective.host;

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
    let sub_qr_path = format!("{base_path}/admin/clients/{id}/qr.png?sub=1");

    let nodes: Vec<ClientNodeCard> = inbounds
        .iter()
        .map(|ib| {
            let is_hy2 = ib.protocol == "hysteria2";
            let is_mieru = ib.protocol == "mieru";
            let mut c_clone = client.clone();
            let base_name = client.name.as_deref().unwrap_or("3r-ui");
            let proto_label = if is_hy2 {
                "Hy2"
            } else if is_mieru {
                "Mieru"
            } else {
                "VLESS"
            };
            c_clone.name = Some(format!("{base_name} - {proto_label}"));

            let node_host = crate::share::resolve_inbound_host(ib, &host);

            let uri = if is_hy2 {
                crate::share::hy2::build_uri(&c_clone, ib, &node_host)
            } else if is_mieru {
                crate::share::mieru::build_uri(&c_clone, ib, &node_host)
            } else {
                crate::share::vless::build_uri(&c_clone, ib, &node_host)
            };
            let qr_path = format!("{base_path}/admin/clients/{id}/qr.png?inbound_id={}", ib.id);

            ClientNodeCard {
                inbound_id: ib.id,
                protocol: ib.protocol.clone(),
                protocol_label: proto_label.to_string(),
                listen_port: ib.listen_port,
                sni: ib.sni.clone(),
                uri,
                qr_path,
            }
        })
        .collect();

    let default_uri = nodes
        .iter()
        .find(|n| n.protocol != "hysteria2")
        .or_else(|| nodes.first())
        .map(|n| n.uri.clone())
        .unwrap_or_default();

    let qr_path = format!(
        "{base_path}/admin/clients/{id}/qr.png?host={}",
        url_encode(&host)
    );

    let html = ClientsDetailTemplate {
        client: ClientView::from(&client),
        inbound: first_inbound,
        uri: default_uri,
        qr_path,
        warning,
        sub_url,
        sub_qr_path,
        nodes,
        base_path,
    }
    .render()
    .unwrap_or_else(|e| format!("template error: {e}"));
    page(jar, StatusCode::OK, html)
}

/// `GET /admin/clients/:id/qr.png` — stream a PNG of the URI or subscription.
pub async fn qr_png(
    State(state): State<AppState>,
    Extension(_admin): Extension<CurrentAdmin>,
    Path(id): Path<i64>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Response {
    let client = match load_client(&state.pool, id).await {
        Ok(Some(c)) => c,
        Ok(None) => return not_found_response(),
        Err(e) => {
            tracing::error!(error = %e, "client lookup failed for qr");
            return not_found_response();
        }
    };

    let effective =
        crate::net::resolve_effective_public_host(&state.pool, &state.config.public_host).await;
    let sub_scheme = effective.scheme;
    let default_host = effective.host;

    let host = params
        .get("host")
        .cloned()
        .filter(|s| !s.is_empty())
        .unwrap_or(default_host);

    let uri = if params.contains_key("sub") {
        let tok = client.sub_token.clone().unwrap_or_default();
        let proxy_settings = crate::proxy::config::get_proxy_settings(&state.pool)
            .await
            .ok();
        if let Some(custom_host) = params.get("host").filter(|s| !s.is_empty()) {
            let sub_path = proxy_settings
                .as_ref()
                .map(|p| p.normalized_sub_path())
                .unwrap_or_else(|| "/sub".to_string());
            format!("{sub_scheme}://{custom_host}{sub_path}/{tok}")
        } else if let Some(ref p) = proxy_settings {
            if p.is_enabled() && !p.domain.is_empty() {
                p.full_sub_url(&tok)
            } else {
                let sub_path = p.normalized_sub_path();
                format!("{sub_scheme}://{host}{sub_path}/{tok}")
            }
        } else {
            format!("{sub_scheme}://{host}/sub/{tok}")
        }
    } else if let Some(ib_id) = params.get("inbound_id").and_then(|s| s.parse::<i64>().ok()) {
        let inbound = match sqlx::query_as::<_, Inbound>(
            "SELECT id, listen_ip, listen_port, dest, sni, server_name, private_key, public_key, short_id, \
                    xhttp_path, protocol, cert_mode, custom_cert_path, custom_key_path, masquerade_url, \
                    port_range, transport_protocol, skip_cert_verify, udp, status, created_at, updated_at \
             FROM inbounds WHERE id = ?",
        )
        .bind(ib_id)
        .fetch_optional(&state.pool)
        .await
        {
            Ok(Some(i)) => i,
            _ => return not_found_response(),
        };

        let node_host = crate::share::resolve_inbound_host(&inbound, &host);

        if inbound.protocol == "hysteria2" {
            crate::share::hy2::build_uri(&client, &inbound, &node_host)
        } else if inbound.protocol == "mieru" {
            crate::share::mieru::build_uri(&client, &inbound, &node_host)
        } else {
            crate::share::vless::build_uri(&client, &inbound, &node_host)
        }
    } else {
        let inbound = match load_active_inbound(&state.pool).await {
            Ok(Some(i)) => i,
            _ => return not_found_response(),
        };
        let node_host = crate::share::resolve_inbound_host(&inbound, &host);
        crate::share::vless::build_uri(&client, &inbound, &node_host)
    };

    match crate::share::qr::render_png(&uri) {
        Ok(bytes) => {
            let mut response = (
                StatusCode::OK,
                [(axum::http::header::CONTENT_TYPE, "image/png")],
                bytes,
            )
                .into_response();
            for (name, value) in SECURITY_HEADERS {
                response.headers_mut().insert(
                    axum::http::HeaderName::from_bytes(name.as_bytes()).expect("static name"),
                    axum::http::HeaderValue::from_static(value),
                );
            }
            response
        }
        Err(e) => {
            tracing::error!(error = %e, "qr render failed");
            (StatusCode::INTERNAL_SERVER_ERROR, "qr render failed").into_response()
        }
    }
}

async fn render_detail_error(jar: &CookieJar, message: String) -> Response {
    let html = format!(
        "<!DOCTYPE html><html lang=\"zh-CN\"><body><p style=\"font-family:sans-serif;padding:24px\">\
         <a href=\"/admin/clients\">返回用户列表</a></p><p>{message}</p></body></html>"
    );
    page(jar.clone(), StatusCode::OK, html)
}

async fn load_active_inbound(pool: &sqlx::SqlitePool) -> crate::error::AppResult<Option<Inbound>> {
    sqlx::query_as::<_, Inbound>(
        "SELECT id, listen_ip, listen_port, dest, sni, server_name, private_key, public_key, short_id, \
                xhttp_path, protocol, cert_mode, custom_cert_path, custom_key_path, masquerade_url, \
                port_range, transport_protocol, skip_cert_verify, udp, status, created_at, updated_at \
         FROM inbounds WHERE status = 'active' ORDER BY id LIMIT 1",
    )
    .fetch_optional(pool)
    .await
    .map_err(crate::error::AppError::Database)
}

/// `POST /admin/clients/:id` — update name / quota / expiry; UUID immutable.
pub async fn update(
    State(state): State<AppState>,
    Extension(_admin): Extension<CurrentAdmin>,
    Path(id): Path<i64>,
    base_path_ext: Option<Extension<BasePath>>,
    jar: CookieJar,
    axum::extract::Form(form): axum::extract::Form<ClientForm>,
) -> Response {
    let base_path = base_path_ext.map(|Extension(p)| p.0).unwrap_or_default();
    let existing = match load_client(&state.pool, id).await {
        Ok(Some(c)) => c,
        Ok(None) => return not_found_response(),
        Err(e) => {
            tracing::error!(error = %e, "client lookup failed before update");
            return not_found_response();
        }
    };

    let all_inbounds = list_all_active_inbounds(&state.pool)
        .await
        .unwrap_or_default();
    let current_bound_ids: Vec<i64> =
        sqlx::query_scalar("SELECT inbound_id FROM client_inbounds WHERE client_id = ?")
            .bind(id)
            .fetch_all(&state.pool)
            .await
            .unwrap_or_default();

    let parsed = match parse_client_form(&form) {
        Ok(p) => p,
        Err(msg) => {
            let view = ClientView::from(&existing);
            return render_form_error(
                &jar,
                Some(view),
                &all_inbounds,
                current_bound_ids,
                msg,
                &base_path,
            );
        }
    };

    let explicit_status = match form.status.as_deref() {
        Some("disabled") => "disabled",
        Some("active") => "active",
        _ => existing.status.as_str(),
    };

    let new_status = if parsed
        .expiry_at
        .is_some_and(|t| t < crate::crypto::session::now_unix())
    {
        "disabled"
    } else {
        explicit_status
    };

    let name = if parsed.name.is_empty() {
        None
    } else {
        Some(parsed.name.as_str())
    };

    let sub_token = existing
        .sub_token
        .clone()
        .unwrap_or_else(generate_sub_token);

    let result = sqlx::query(
        "UPDATE clients SET name = ?, quota_bytes = ?, expiry_at = ?, status = ?, sub_token = ? \
         WHERE id = ?",
    )
    .bind(name)
    .bind(parsed.quota_bytes)
    .bind(parsed.expiry_at)
    .bind(new_status)
    .bind(&sub_token)
    .bind(id)
    .execute(&state.pool)
    .await;

    if let Err(e) = result {
        tracing::error!(error = %e, "client update failed");
        let view = ClientView::from(&existing);
        return render_form_error(
            &jar,
            Some(view),
            &all_inbounds,
            current_bound_ids,
            "保存失败".to_string(),
            &base_path,
        );
    }

    if let Some(inbound_ids_str) = &form.inbound_ids {
        let selected: Vec<i64> = inbound_ids_str
            .split(',')
            .filter_map(|s| s.trim().parse::<i64>().ok())
            .collect();
        if !selected.is_empty() {
            let _ = sqlx::query("DELETE FROM client_inbounds WHERE client_id = ?")
                .bind(id)
                .execute(&state.pool)
                .await;
            for ib_id in &selected {
                let _ = sqlx::query(
                    "INSERT OR IGNORE INTO client_inbounds (client_id, inbound_id) VALUES (?, ?)",
                )
                .bind(id)
                .bind(*ib_id)
                .execute(&state.pool)
                .await;
            }
        }
    }

    reload(&state).await;
    redirect_to_list(&jar, &base_path)
}

/// `POST /admin/clients/:id/delete` — soft-delete the client.
pub async fn delete(
    State(state): State<AppState>,
    Extension(_admin): Extension<CurrentAdmin>,
    base_path_ext: Option<Extension<BasePath>>,
    jar: CookieJar,
    Path(id): Path<i64>,
) -> Response {
    let base_path = base_path_ext.map(|Extension(p)| p.0).unwrap_or_default();
    let _ = sqlx::query("UPDATE clients SET status = 'deleted' WHERE id = ?")
        .bind(id)
        .execute(&state.pool)
        .await;
    reload(&state).await;
    redirect_to_list(&jar, &base_path)
}

#[derive(serde::Deserialize, Default)]
pub struct ToggleQuery {
    #[serde(default)]
    pub redirect: Option<String>,
}

/// `POST /admin/clients/:id/toggle` — toggle a client between active and disabled.
pub async fn toggle(
    State(state): State<AppState>,
    Extension(_admin): Extension<CurrentAdmin>,
    base_path_ext: Option<Extension<BasePath>>,
    jar: CookieJar,
    Path(id): Path<i64>,
    axum::extract::Query(query): axum::extract::Query<ToggleQuery>,
) -> Response {
    let base_path = base_path_ext.map(|Extension(p)| p.0).unwrap_or_default();
    let client = match load_client(&state.pool, id).await {
        Ok(Some(c)) => c,
        _ => return redirect_to_list(&jar, &base_path),
    };

    let new_status = if client.status == "active" {
        "disabled"
    } else {
        "active"
    };

    let _ = sqlx::query("UPDATE clients SET status = ? WHERE id = ?")
        .bind(new_status)
        .bind(id)
        .execute(&state.pool)
        .await;

    reload(&state).await;

    if query.redirect.as_deref() == Some("detail") {
        let target = if base_path.is_empty() {
            format!("/admin/clients/{id}")
        } else {
            format!("{base_path}/admin/clients/{id}")
        };
        decorate(Redirect::to(&target).into_response(), Some(&jar))
    } else {
        redirect_to_list(&jar, &base_path)
    }
}

struct ParsedClientForm {
    name: String,
    quota_bytes: i64,
    expiry_at: Option<i64>,
}

fn parse_client_form(form: &ClientForm) -> Result<ParsedClientForm, String> {
    let name = form.name.trim().to_string();
    if name.chars().count() > 64 {
        return Err("名称最长 64 个字符".to_string());
    }

    let quota_gb: i64 = if form.quota_gb.trim().is_empty() {
        0
    } else {
        form.quota_gb
            .trim()
            .parse()
            .map_err(|_| "配额必须是整数 GB".to_string())?
    };
    if quota_gb < 0 {
        return Err("配额不能为负".to_string());
    }
    if quota_gb > 10_000_000 {
        return Err("配额过大（上限 1000 万 GB）".to_string());
    }

    let expiry_at = if form.expiry.trim().is_empty() {
        None
    } else {
        Some(
            parse_expiry(Some(&form.expiry))
                .ok_or_else(|| "过期时间必须是 YYYY-MM-DD 或 Unix 时间戳".to_string())?,
        )
    };

    Ok(ParsedClientForm {
        name,
        quota_bytes: quota_gb * BYTES_PER_GB,
        expiry_at,
    })
}

async fn load_client(pool: &sqlx::SqlitePool, id: i64) -> crate::error::AppResult<Option<Client>> {
    sqlx::query_as::<_, Client>(
        "SELECT id, inbound_id, uuid, name, quota_bytes, expiry_at, status, sub_token, mieru_username, mieru_password, created_at \
         FROM clients WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(pool)
    .await
    .map_err(crate::error::AppError::Database)
}

async fn current_inbound(state: &AppState) -> crate::error::AppResult<Option<Inbound>> {
    sqlx::query_as::<_, Inbound>(
        "SELECT id, listen_ip, listen_port, dest, sni, server_name, private_key, public_key, short_id, \
                xhttp_path, protocol, cert_mode, custom_cert_path, custom_key_path, masquerade_url, \
                port_range, transport_protocol, skip_cert_verify, udp, status, created_at, updated_at \
         FROM inbounds WHERE status = 'active' ORDER BY id LIMIT 1",
    )
    .fetch_optional(&state.pool)
    .await
    .map_err(crate::error::AppError::Database)
}

/// Flip any active client whose `expiry_at` has passed to `disabled`, so it
/// stops appearing in the rendered xray config on the next reload. Safe to
/// call repeatedly; nothing happens if no rows match.
async fn disable_expired(pool: &sqlx::SqlitePool) -> crate::error::AppResult<u64> {
    let now = crate::crypto::session::now_unix();
    let res = sqlx::query(
        "UPDATE clients SET status = 'disabled' \
         WHERE status = 'active' AND expiry_at IS NOT NULL AND expiry_at < ?",
    )
    .bind(now)
    .execute(pool)
    .await
    .map_err(crate::error::AppError::Database)?;
    if res.rows_affected() > 0 {
        tracing::info!(
            disabled = res.rows_affected(),
            "auto-disabled expired clients before reload"
        );
    }
    Ok(res.rows_affected())
}

/// Re-render the xray config and ask the supervisor to reload. Calls
/// [`disable_expired`] first so disabled-but-not-yet-swept clients fall out
/// of the rendered config in the same reload.
async fn reload(state: &AppState) {
    if let Err(e) = disable_expired(&state.pool).await {
        tracing::warn!(error = %e, "expired-client sweep failed before reload");
    }
    let config_path = state.config.data_dir.join("xray").join("config.json");
    match config::render(&state.pool, &config_path).await {
        Ok(n) => {
            tracing::info!(inbounds = n, "xray config re-rendered after client change");
            let _ = state.supervisor_tx.send(SupervisorCommand::Reload).await;
        }
        Err(e) => tracing::error!(error = %e, "failed to re-render xray config"),
    }
    let mieru_config_path = state.config.data_dir.join("mieru").join("config.json");
    let _ = crate::mieru::render_to_file(&state.pool, &mieru_config_path).await;
    let _ = state
        .mieru_tx
        .send(crate::mieru::MieruCommand::Reload)
        .await;
}

async fn list_all_active_inbounds(
    pool: &sqlx::SqlitePool,
) -> crate::error::AppResult<Vec<Inbound>> {
    sqlx::query_as::<_, Inbound>(
        "SELECT id, listen_ip, listen_port, dest, sni, server_name, private_key, public_key, short_id, \
                xhttp_path, protocol, cert_mode, custom_cert_path, custom_key_path, masquerade_url, \
                port_range, transport_protocol, skip_cert_verify, udp, status, created_at, updated_at \
         FROM inbounds WHERE status = 'active' ORDER BY id ASC",
    )
    .fetch_all(pool)
    .await
    .map_err(crate::error::AppError::Database)
}

fn render_form_error(
    jar: &CookieJar,
    client: Option<ClientView>,
    inbounds: &[Inbound],
    bound_inbound_ids: Vec<i64>,
    error: String,
    base_path: &str,
) -> Response {
    let html = ClientsFormTemplate {
        csrf: csrf_from_cookies(jar),
        client,
        inbounds: inbounds
            .iter()
            .map(crate::web::inbound::InboundView::from)
            .collect(),
        bound_inbound_ids,
        error: Some(error),
        base_path: base_path.to_string(),
    }
    .render()
    .unwrap_or_else(|e| format!("template error: {e}"));
    page(jar.clone(), StatusCode::BAD_REQUEST, html)
}

fn not_found_response() -> Response {
    page_status(StatusCode::NOT_FOUND, "用户不存在".to_string())
}

fn page(jar: CookieJar, status: StatusCode, body: String) -> Response {
    decorate((status, Html(body)).into_response(), Some(&jar))
}

fn page_status(status: StatusCode, body: String) -> Response {
    decorate((status, Html(body)).into_response(), None)
}

fn redirect_to_list(jar: &CookieJar, base_path: &str) -> Response {
    let target = if base_path.is_empty() {
        "/admin/clients".to_string()
    } else {
        format!("{base_path}/admin/clients")
    };
    decorate(Redirect::to(&target).into_response(), Some(jar))
}

fn decorate(mut response: Response, jar: Option<&CookieJar>) -> Response {
    if let Some(jar) = jar {
        for cookie in jar.iter() {
            if let Ok(v) = cookie.encoded().to_string().parse() {
                response
                    .headers_mut()
                    .append(axum::http::header::SET_COOKIE, v);
            }
        }
    }
    for (name, value) in SECURITY_HEADERS {
        response.headers_mut().insert(
            axum::http::HeaderName::from_bytes(name.as_bytes()).expect("static name"),
            axum::http::HeaderValue::from_static(value),
        );
    }
    response
}

fn format_traffic(bytes: i64) -> String {
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64;
    for unit in ["B", "KiB", "MiB", "GiB", "TiB"] {
        if value < 1024.0 || unit == "TiB" {
            return format!("{value:.2} {unit}");
        }
        value /= 1024.0;
    }
    unreachable!()
}

fn timestamp_to_iso_date(ts: i64) -> Option<String> {
    const FMT: &[time::format_description::BorrowedFormatItem<'_>] =
        time::macros::format_description!("[year]-[month]-[day]");
    time::OffsetDateTime::from_unix_timestamp(ts)
        .ok()
        .and_then(|t| t.format(&FMT).ok())
}
