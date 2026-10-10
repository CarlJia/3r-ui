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
use crate::web::middleware::{csrf_from_cookies, url_encode, CurrentAdmin, SECURITY_HEADERS};
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
}

#[derive(Template)]
#[template(path = "clients_form.html", escape = "html")]
struct ClientsFormTemplate {
    csrf: String,
    client: Option<ClientView>,
    error: Option<String>,
}

#[derive(Template)]
#[template(path = "clients_detail.html", escape = "html")]
struct ClientsDetailTemplate {
    client: ClientView,
    inbound: Inbound,
    uri: String,
    qr_path: String,
    warning: Option<String>,
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
}

/// `GET /admin/clients` — list every non-deleted client, newest first.
pub async fn list(
    State(state): State<AppState>,
    Extension(_admin): Extension<CurrentAdmin>,
    jar: CookieJar,
) -> Response {
    if let Err(e) = disable_expired(&state.pool).await {
        tracing::warn!(error = %e, "expired-client sweep failed before list");
    }
    let has_inbound = current_inbound(&state).await.is_ok_and(|i| i.is_some());
    let rows = match sqlx::query_as::<_, ClientTrafficRow>(
        "SELECT c.id, c.inbound_id, c.uuid, c.name, c.quota_bytes, c.expiry_at, \
                c.status, c.created_at, COALESCE(t.uplink, 0) AS uplink, \
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
    }
    .render()
    .unwrap_or_else(|e| format!("template error: {e}"));
    page(jar, StatusCode::OK, html)
}

/// `GET /admin/clients/new` — render an empty create form.
pub async fn new_form(
    State(state): State<AppState>,
    Extension(_admin): Extension<CurrentAdmin>,
    jar: CookieJar,
) -> Response {
    let has_inbound = current_inbound(&state).await.is_ok_and(|i| i.is_some());
    let html = ClientsFormTemplate {
        csrf: csrf_from_cookies(&jar),
        client: None,
        error: if has_inbound {
            None
        } else {
            Some("请先在入站页面创建入站后再添加用户".to_string())
        },
    }
    .render()
    .unwrap_or_else(|e| format!("template error: {e}"));
    page(jar, StatusCode::OK, html)
}

/// `POST /admin/clients` — mint a UUID, validate, persist, reload.
pub async fn create(
    State(state): State<AppState>,
    Extension(_admin): Extension<CurrentAdmin>,
    jar: CookieJar,
    axum::extract::Form(form): axum::extract::Form<ClientForm>,
) -> Response {
    let inbound = match current_inbound(&state).await {
        Ok(Some(i)) => i,
        Ok(None) => return render_form_error(&jar, None, "请先在入站页面创建入站".to_string()),
        Err(e) => {
            tracing::error!(error = %e, "failed to read inbound for client create");
            return render_form_error(&jar, None, "数据库错误".to_string());
        }
    };

    let parsed = match parse_client_form(&form) {
        Ok(p) => p,
        Err(msg) => return render_form_error(&jar, None, msg),
    };

    let new_uuid = match uuid::v4() {
        Ok(u) => u,
        Err(e) => {
            tracing::error!(error = %e, "UUID generation failed");
            return render_form_error(&jar, None, "UUID 生成失败".to_string());
        }
    };

    let name = if parsed.name.is_empty() {
        None
    } else {
        Some(parsed.name.as_str())
    };

    let result = sqlx::query(
        "INSERT INTO clients (inbound_id, uuid, name, quota_bytes, expiry_at, status, created_at) \
         VALUES (?, ?, ?, ?, ?, 'active', unixepoch())",
    )
    .bind(inbound.id)
    .bind(&new_uuid)
    .bind(name)
    .bind(parsed.quota_bytes)
    .bind(parsed.expiry_at)
    .execute(&state.pool)
    .await;

    if let Err(e) = result {
        tracing::error!(error = %e, "client insert failed");
        return render_form_error(&jar, None, "保存失败".to_string());
    }

    reload(&state).await;
    redirect_to_list(&jar)
}

/// `GET /admin/clients/:id/edit` — render the form pre-filled with the row.
pub async fn edit_form(
    State(state): State<AppState>,
    Extension(_admin): Extension<CurrentAdmin>,
    jar: CookieJar,
    Path(id): Path<i64>,
) -> Response {
    match load_client(&state.pool, id).await {
        Ok(Some(c)) => {
            let html = ClientsFormTemplate {
                csrf: csrf_from_cookies(&jar),
                client: Some(ClientView::from(&c)),
                error: None,
            }
            .render()
            .unwrap_or_else(|e| format!("template error: {e}"));
            page(jar, StatusCode::OK, html)
        }
        Ok(None) => not_found_response(),
        Err(e) => {
            tracing::error!(error = %e, "client lookup failed");
            not_found_response()
        }
    }
}

/// `GET /admin/clients/:id` — share / QR detail page.
pub async fn detail(
    State(state): State<AppState>,
    Extension(_admin): Extension<CurrentAdmin>,
    jar: CookieJar,
    Path(id): Path<i64>,
) -> Response {
    let client = match load_client(&state.pool, id).await {
        Ok(Some(c)) => c,
        Ok(None) => return not_found_response(),
        Err(e) => {
            tracing::error!(error = %e, "client lookup failed for detail");
            return not_found_response();
        }
    };
    let inbound = match load_active_inbound(&state.pool).await {
        Ok(Some(i)) => i,
        Ok(None) => {
            return render_detail_error(&jar, "尚未创建入站，无法生成分享链接".to_string()).await
        }
        Err(e) => {
            tracing::error!(error = %e, "inbound lookup failed for detail");
            return not_found_response();
        }
    };

    let public_host = state.config.public_host.clone();
    let warning = if public_host.is_empty() {
        Some("尚未配置 PUBLIC_HOST 环境变量，链接中的 host 占位为示例地址，客户端无法直接使用。请在面板环境配置 PUBLIC_HOST 后重新打开此页面。".to_string())
    } else {
        None
    };
    let host = if public_host.is_empty() {
        "your-server.example.com".to_string()
    } else {
        public_host
    };

    let uri = crate::share::vless::build_uri(&client, &inbound, &host);
    let qr_path = format!("/admin/clients/{id}/qr.png?host={}", url_encode(&host));

    let html = ClientsDetailTemplate {
        client: ClientView::from(&client),
        inbound,
        uri,
        qr_path,
        warning,
    }
    .render()
    .unwrap_or_else(|e| format!("template error: {e}"));
    page(jar, StatusCode::OK, html)
}

/// `GET /admin/clients/:id/qr.png` — stream a PNG of the VLESS URI. The
/// optional `host` query param overrides `PUBLIC_HOST` for one-off renders
/// (used by the detail page preview when no PUBLIC_HOST is set yet).
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
    let inbound = match load_active_inbound(&state.pool).await {
        Ok(Some(i)) => i,
        _ => return not_found_response(),
    };

    let host = params
        .get("host")
        .cloned()
        .filter(|s| !s.is_empty())
        .or_else(|| {
            let h = state.config.public_host.clone();
            if h.is_empty() {
                None
            } else {
                Some(h)
            }
        })
        .unwrap_or_else(|| "your-server.example.com".to_string());

    let uri = crate::share::vless::build_uri(&client, &inbound, &host);
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
        "SELECT id, listen_port, dest, sni, server_name, private_key, public_key, short_id, \
                status, created_at, updated_at \
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
    jar: CookieJar,
    Path(id): Path<i64>,
    axum::extract::Form(form): axum::extract::Form<ClientForm>,
) -> Response {
    let existing = match load_client(&state.pool, id).await {
        Ok(Some(c)) => c,
        Ok(None) => return not_found_response(),
        Err(e) => {
            tracing::error!(error = %e, "client lookup failed before update");
            return not_found_response();
        }
    };

    let parsed = match parse_client_form(&form) {
        Ok(p) => p,
        Err(msg) => {
            let view = ClientView::from(&existing);
            return render_form_error(&jar, Some(view), msg);
        }
    };

    let new_status = if parsed
        .expiry_at
        .is_some_and(|t| t < crate::crypto::session::now_unix())
    {
        "disabled"
    } else {
        existing.status.as_str()
    };

    let name = if parsed.name.is_empty() {
        None
    } else {
        Some(parsed.name.as_str())
    };

    let result = sqlx::query(
        "UPDATE clients SET name = ?, quota_bytes = ?, expiry_at = ?, status = ? \
         WHERE id = ?",
    )
    .bind(name)
    .bind(parsed.quota_bytes)
    .bind(parsed.expiry_at)
    .bind(new_status)
    .bind(id)
    .execute(&state.pool)
    .await;

    if let Err(e) = result {
        tracing::error!(error = %e, "client update failed");
        let view = ClientView::from(&existing);
        return render_form_error(&jar, Some(view), "保存失败".to_string());
    }

    reload(&state).await;
    redirect_to_list(&jar)
}

/// `POST /admin/clients/:id/delete` — soft-delete the client.
pub async fn delete(
    State(state): State<AppState>,
    Extension(_admin): Extension<CurrentAdmin>,
    jar: CookieJar,
    Path(id): Path<i64>,
) -> Response {
    let _ = sqlx::query("UPDATE clients SET status = 'deleted' WHERE id = ?")
        .bind(id)
        .execute(&state.pool)
        .await;
    reload(&state).await;
    redirect_to_list(&jar)
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
        "SELECT id, inbound_id, uuid, name, quota_bytes, expiry_at, status, created_at \
         FROM clients WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(pool)
    .await
    .map_err(crate::error::AppError::Database)
}

async fn current_inbound(state: &AppState) -> crate::error::AppResult<Option<Inbound>> {
    sqlx::query_as::<_, Inbound>(
        "SELECT id, listen_port, dest, sni, server_name, private_key, public_key, short_id, \
                status, created_at, updated_at \
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
}

fn render_form_error(jar: &CookieJar, client: Option<ClientView>, error: String) -> Response {
    let html = ClientsFormTemplate {
        csrf: csrf_from_cookies(jar),
        client,
        error: Some(error),
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

fn redirect_to_list(jar: &CookieJar) -> Response {
    decorate(Redirect::to("/admin/clients").into_response(), Some(jar))
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
