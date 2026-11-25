//! Inbound CRUD backed by SQLite, with an xray config re-render and reload
//! after every mutation.

use askama::Template;
use axum::extract::{Extension, Path, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum_extra::extract::cookie::CookieJar;
use rand::RngCore;

use crate::crypto::reality::generate_keypair;
use crate::db::models::Inbound;
use crate::web::middleware::{csrf_from_cookies, BasePath, CurrentAdmin, SECURITY_HEADERS};
use crate::web::AppState;
use crate::xray::config;
use crate::xray::validate::{
    generate_xhttp_path, validate_dest, validate_short_id, validate_xhttp_path,
};
use crate::xray::SupervisorCommand;

#[derive(Template)]
#[template(path = "inbound_list.html", escape = "html")]
struct InboundListTemplate {
    csrf: String,
    inbounds: Vec<InboundView>,
    error: Option<String>,
    base_path: String,
}

#[derive(Template)]
#[template(path = "inbound_form.html", escape = "html")]
struct InboundFormTemplate {
    csrf: String,
    inbound: Option<InboundView>,
    /// Pre-generated 16-hex-char default used when there is no inbound to edit
    /// yet — the form is never shown with an empty shortId, and the random
    /// button next to the input lets the operator re-roll on demand.
    default_short_id: String,
    /// Pre-generated `/r/<16hex>` xhttp path used when no inbound exists to
    /// edit yet. Mirrors the default-short_id rationale: the form must never
    /// submit a blank path, and the random button lets the operator re-roll
    /// without a server round-trip.
    default_xhttp_path: String,
    error: Option<String>,
    base_path: String,
}

#[derive(Clone, Debug)]
pub struct InboundView {
    pub id: i64,
    pub listen_port: i64,
    pub protocol: String,
    pub cert_mode: String,
    pub dest: String,
    pub sni: String,
    pub public_key: String,
    pub short_id: String,
    pub xhttp_path: String,
    pub custom_cert_path: String,
    pub custom_key_path: String,
    pub masquerade_url: String,
    pub port_range: String,
    pub transport_protocol: String,
    pub skip_cert_verify: bool,
    pub udp: bool,
    pub status: String,
    pub is_active: bool,
}

impl From<&Inbound> for InboundView {
    fn from(i: &Inbound) -> Self {
        Self {
            id: i.id,
            listen_port: i.listen_port,
            protocol: i.protocol.clone(),
            cert_mode: i.cert_mode.clone(),
            dest: i.dest.clone(),
            sni: i.sni.clone(),
            public_key: i.public_key.clone(),
            short_id: i.short_id.clone(),
            xhttp_path: crate::xray::config::effective_xhttp_path(i),
            custom_cert_path: i.custom_cert_path.clone(),
            custom_key_path: i.custom_key_path.clone(),
            masquerade_url: i.masquerade_url.clone(),
            port_range: i.port_range.clone(),
            transport_protocol: i.transport_protocol.clone(),
            skip_cert_verify: i.skip_cert_verify == 1,
            udp: i.udp == 1,
            status: i.status.clone(),
            is_active: i.status == "active",
        }
    }
}

#[derive(serde::Deserialize, Default)]
pub struct InboundForm {
    #[serde(default)]
    pub id: Option<i64>,
    #[serde(default)]
    pub action: Option<String>,
    #[serde(default)]
    pub listen_port: String,
    #[serde(default = "default_protocol")]
    pub protocol: String,
    #[serde(default = "default_cert_mode")]
    pub cert_mode: String,
    #[serde(default)]
    pub dest: String,
    #[serde(default)]
    pub sni: String,
    #[serde(default)]
    pub short_id: String,
    #[serde(default)]
    pub xhttp_path: String,
    #[serde(default)]
    pub custom_cert_path: String,
    #[serde(default)]
    pub custom_key_path: String,
    #[serde(default)]
    pub masquerade_url: String,
    #[serde(default)]
    pub port_range: String,
    #[serde(default = "default_transport_protocol")]
    pub transport_protocol: String,
    #[serde(default = "default_skip_cert_verify")]
    pub skip_cert_verify: String,
    #[serde(default = "default_udp")]
    pub udp: String,
    #[serde(default = "default_status")]
    pub status: String,
}

fn default_skip_cert_verify() -> String {
    "0".to_string()
}

fn default_udp() -> String {
    "1".to_string()
}

fn default_transport_protocol() -> String {
    "TCP".to_string()
}

fn default_status() -> String {
    "active".to_string()
}

fn default_protocol() -> String {
    "vless".to_string()
}

fn default_cert_mode() -> String {
    "auto".to_string()
}

#[derive(serde::Deserialize, Default)]
pub struct DeleteInboundForm {
    #[serde(default)]
    pub id: Option<i64>,
}

/// `GET /admin/inbound` — list all active inbounds.
pub async fn list(
    State(state): State<AppState>,
    Extension(_admin): Extension<CurrentAdmin>,
    base_path_ext: Option<Extension<BasePath>>,
    jar: CookieJar,
    axum::extract::Query(query): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Response {
    let base_path = base_path_ext.map(|Extension(p)| p.0).unwrap_or_default();
    if query.contains_key("new") {
        let target = if base_path.is_empty() {
            "/admin/inbound/new".to_string()
        } else {
            format!("{base_path}/admin/inbound/new")
        };
        return decorate(Redirect::to(&target).into_response(), Some(&jar));
    }
    if let Some(id) = query.get("id") {
        let target = if base_path.is_empty() {
            format!("/admin/inbound/{id}/edit")
        } else {
            format!("{base_path}/admin/inbound/{id}/edit")
        };
        return decorate(Redirect::to(&target).into_response(), Some(&jar));
    }

    let all = list_inbounds(&state.pool).await.unwrap_or_default();
    let inbounds = all.iter().map(InboundView::from).collect();

    let html = InboundListTemplate {
        csrf: csrf_from_cookies(&jar),
        inbounds,
        error: None,
        base_path,
    }
    .render()
    .unwrap_or_else(|e| format!("template error: {e}"));
    page(jar, StatusCode::OK, html)
}

/// Backwards-compatibility alias for [`list`].
pub use list as show;

/// `GET /admin/inbound/new` — show an empty create form.
pub async fn new_form(
    State(_state): State<AppState>,
    Extension(_admin): Extension<CurrentAdmin>,
    base_path_ext: Option<Extension<BasePath>>,
    jar: CookieJar,
) -> Response {
    let base_path = base_path_ext.map(|Extension(p)| p.0).unwrap_or_default();
    let default_short_id = random_short_id();
    let default_xhttp_path = generate_xhttp_path();
    let html = InboundFormTemplate {
        csrf: csrf_from_cookies(&jar),
        inbound: None,
        default_short_id,
        default_xhttp_path,
        error: None,
        base_path,
    }
    .render()
    .unwrap_or_else(|e| format!("template error: {e}"));
    page(jar, StatusCode::OK, html)
}

/// `GET /admin/inbound/:id/edit` or `GET /admin/inbound/:id` — show edit form for an inbound.
pub async fn edit_form(
    State(state): State<AppState>,
    Extension(_admin): Extension<CurrentAdmin>,
    base_path_ext: Option<Extension<BasePath>>,
    jar: CookieJar,
    Path(id): Path<i64>,
) -> Response {
    let base_path = base_path_ext.map(|Extension(p)| p.0).unwrap_or_default();
    let all = list_inbounds(&state.pool).await.unwrap_or_default();
    let target = all.into_iter().find(|i| i.id == id);
    let Some(inbound) = target else {
        let target = if base_path.is_empty() {
            "/admin/inbound".to_string()
        } else {
            format!("{base_path}/admin/inbound")
        };
        return decorate(Redirect::to(&target).into_response(), Some(&jar));
    };

    let default_short_id = random_short_id();
    let default_xhttp_path = generate_xhttp_path();
    let html = InboundFormTemplate {
        csrf: csrf_from_cookies(&jar),
        inbound: Some(InboundView::from(&inbound)),
        default_short_id,
        default_xhttp_path,
        error: None,
        base_path,
    }
    .render()
    .unwrap_or_else(|e| format!("template error: {e}"));
    page(jar, StatusCode::OK, html)
}

/// `POST /admin/inbound` — create a new inbound or update an existing one.
pub async fn save(
    State(state): State<AppState>,
    Extension(_admin): Extension<CurrentAdmin>,
    base_path_ext: Option<Extension<BasePath>>,
    jar: CookieJar,
    axum::extract::Form(form): axum::extract::Form<InboundForm>,
) -> Response {
    let base_path = base_path_ext.map(|Extension(p)| p.0).unwrap_or_default();
    handle_save(state, jar, form.id, form, &base_path).await
}

/// `POST /admin/inbound/:id` — update an inbound by ID.
pub async fn update(
    State(state): State<AppState>,
    Extension(_admin): Extension<CurrentAdmin>,
    base_path_ext: Option<Extension<BasePath>>,
    jar: CookieJar,
    Path(id): Path<i64>,
    axum::extract::Form(mut form): axum::extract::Form<InboundForm>,
) -> Response {
    let base_path = base_path_ext.map(|Extension(p)| p.0).unwrap_or_default();
    form.id = Some(id);
    handle_save(state, jar, Some(id), form, &base_path).await
}

async fn handle_save(
    state: AppState,
    jar: CookieJar,
    target_id: Option<i64>,
    form: InboundForm,
    base_path: &str,
) -> Response {
    let all = list_inbounds(&state.pool).await.unwrap_or_default();
    let is_new_action = form.action.as_deref() == Some("new");

    let existing = if is_new_action {
        None
    } else if let Some(id) = target_id.or(form.id) {
        all.iter().find(|i| i.id == id).cloned()
    } else {
        // If there is only one active inbound, default to updating it; otherwise create.
        if all.len() == 1 {
            all.first().cloned()
        } else {
            None
        }
    };

    let port: i64 = match form.listen_port.trim().parse() {
        Ok(p) if (1..=65535).contains(&p) => p,
        _ => {
            return render_error(
                &jar,
                existing.as_ref(),
                "端口必须是 1-65535 之间的整数".into(),
                base_path,
            )
            .await
        }
    };

    let protocol = form.protocol.trim().to_lowercase();
    let cert_mode = form.cert_mode.trim().to_lowercase();
    let is_hy2 = protocol == "hysteria2";
    let is_mieru = protocol == "mieru";

    let port_range = form.port_range.trim().to_string();
    let transport_protocol = if form.transport_protocol.trim().is_empty() {
        "TCP".to_string()
    } else {
        form.transport_protocol.trim().to_uppercase()
    };

    let (
        dest,
        sni,
        short_id,
        xhttp_path,
        private_key,
        public_key,
        custom_cert_path,
        custom_key_path,
        masquerade_url,
    ) = if is_mieru {
        (
            "".to_string(),
            "".to_string(),
            "".to_string(),
            None,
            "".to_string(),
            "".to_string(),
            "".to_string(),
            "".to_string(),
            "".to_string(),
        )
    } else if is_hy2 {
        let sni = form.sni.trim().to_string();
        if sni.is_empty() {
            return render_error(&jar, existing.as_ref(), "SNI 不能为空".into(), base_path).await;
        }
        if sni.len() > 253 || sni.contains(char::is_whitespace) {
            return render_error(
                &jar,
                existing.as_ref(),
                "SNI 必须是合法主机名".into(),
                base_path,
            )
            .await;
        }

        let custom_cert_path = form.custom_cert_path.trim().to_string();
        let custom_key_path = form.custom_key_path.trim().to_string();
        if cert_mode == "custom" {
            if custom_cert_path.is_empty() || custom_key_path.is_empty() {
                return render_error(
                    &jar,
                    existing.as_ref(),
                    "自定义证书模式必须填写证书和私钥路径".into(),
                    base_path,
                )
                .await;
            }
            if !std::path::Path::new(&custom_cert_path).exists()
                || !std::path::Path::new(&custom_key_path).exists()
            {
                return render_error(
                    &jar,
                    existing.as_ref(),
                    "指定的证书文件或私钥文件不存在，请检查路径".into(),
                    base_path,
                )
                .await;
            }
        }

        let masquerade_url = if form.masquerade_url.trim().is_empty() {
            "https://news.ycombinator.com/".to_string()
        } else {
            let m = form.masquerade_url.trim();
            if !m.starts_with("http://") && !m.starts_with("https://") {
                format!("https://{m}")
            } else {
                m.to_string()
            }
        };

        (
            "".to_string(),
            sni,
            "".to_string(),
            None,
            "".to_string(),
            "".to_string(),
            custom_cert_path,
            custom_key_path,
            masquerade_url,
        )
    } else {
        let dest = match validate_dest(&form.dest).await {
            Ok(d) => d,
            Err(e) => {
                return render_error(&jar, existing.as_ref(), format!("{e}"), base_path).await
            }
        };

        let sni = form.sni.trim().to_string();
        if sni.is_empty() {
            return render_error(&jar, existing.as_ref(), "SNI 不能为空".into(), base_path).await;
        }
        if sni.len() > 253 || sni.contains(char::is_whitespace) {
            return render_error(
                &jar,
                existing.as_ref(),
                "SNI 必须是合法主机名".into(),
                base_path,
            )
            .await;
        }

        if let Err(msg) = validate_short_id(&form.short_id) {
            return render_error(&jar, existing.as_ref(), msg, base_path).await;
        }
        let short_id = form.short_id.trim().to_string();

        let submitted_path = form.xhttp_path.trim();
        let xhttp_path = if submitted_path.is_empty() {
            existing
                .as_ref()
                .map(config::effective_xhttp_path)
                .unwrap_or_else(generate_xhttp_path)
        } else {
            if let Err(msg) = validate_xhttp_path(submitted_path) {
                return render_error(&jar, existing.as_ref(), msg, base_path).await;
            }
            submitted_path.to_string()
        };

        let keypair = match generate_keypair() {
            Ok(k) => k,
            Err(e) => {
                tracing::error!(error = %e, "REALITY key generation failed");
                return render_error(&jar, existing.as_ref(), "密钥生成失败".into(), base_path)
                    .await;
            }
        };

        (
            dest,
            sni,
            short_id,
            Some(xhttp_path),
            keypair.private_key,
            keypair.public_key,
            "".to_string(),
            "".to_string(),
            "".to_string(),
        )
    };

    let skip_cert_verify: i64 = match form.skip_cert_verify.trim() {
        "1" | "true" | "on" => 1,
        _ => 0,
    };
    let udp: i64 = match form.udp.trim() {
        "0" | "false" | "off" => 0,
        _ => 1,
    };

    let status = if form.status.trim() == "disabled" {
        "disabled".to_string()
    } else {
        "active".to_string()
    };

    // Check port uniqueness against other active inbounds only if activating/active
    if status == "active" {
        let existing_id = existing.as_ref().map(|i| i.id).unwrap_or(0);
        let port_in_use: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM inbounds WHERE listen_port = ? AND id != ? AND status = 'active'",
        )
        .bind(port)
        .bind(existing_id)
        .fetch_one(&state.pool)
        .await
        .unwrap_or(0);

        if port_in_use > 0 {
            return render_error(
                &jar,
                existing.as_ref(),
                format!("端口 {port} 已被占用"),
                base_path,
            )
            .await;
        }
    }

    let result = match &existing {
        Some(inbound) => {
            sqlx::query(
                "UPDATE inbounds SET listen_port = ?, dest = ?, sni = ?, short_id = ?, \
                        xhttp_path = ?, private_key = ?, public_key = ?, protocol = ?, cert_mode = ?, \
                        custom_cert_path = ?, custom_key_path = ?, masquerade_url = ?, port_range = ?, transport_protocol = ?, \
                        skip_cert_verify = ?, udp = ?, status = ?, \
                        updated_at = unixepoch() \
                 WHERE id = ?",
            )
            .bind(port)
            .bind(&dest)
            .bind(&sni)
            .bind(&short_id)
            .bind(&xhttp_path)
            .bind(&private_key)
            .bind(&public_key)
            .bind(&protocol)
            .bind(&cert_mode)
            .bind(&custom_cert_path)
            .bind(&custom_key_path)
            .bind(&masquerade_url)
            .bind(&port_range)
            .bind(&transport_protocol)
            .bind(skip_cert_verify)
            .bind(udp)
            .bind(&status)
            .bind(inbound.id)
            .execute(&state.pool)
            .await
        }
        None => {
            sqlx::query(
                "INSERT INTO inbounds (listen_port, dest, sni, server_name, private_key, \
                        public_key, short_id, xhttp_path, protocol, cert_mode, \
                        custom_cert_path, custom_key_path, masquerade_url, port_range, transport_protocol, \
                        skip_cert_verify, udp, status, \
                        created_at, updated_at) \
                 VALUES (?, ?, ?, NULL, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, unixepoch(), unixepoch())",
            )
            .bind(port)
            .bind(&dest)
            .bind(&sni)
            .bind(&private_key)
            .bind(&public_key)
            .bind(&short_id)
            .bind(&xhttp_path)
            .bind(&protocol)
            .bind(&cert_mode)
            .bind(&custom_cert_path)
            .bind(&custom_key_path)
            .bind(&masquerade_url)
            .bind(&port_range)
            .bind(&transport_protocol)
            .bind(skip_cert_verify)
            .bind(udp)
            .bind(&status)
            .execute(&state.pool)
            .await
        }
    };

    if let Err(e) = result {
        let msg = if e.to_string().contains("UNIQUE") {
            format!("端口 {port} 已被占用")
        } else {
            tracing::error!(error = %e, "inbound save failed");
            "保存失败".to_string()
        };
        return render_error(&jar, existing.as_ref(), msg, base_path).await;
    }

    reload(&state).await;
    redirect_to_inbound(&jar, base_path)
}

/// `POST /admin/inbound/:id/delete` — soft-delete an inbound by ID.
pub async fn delete_by_id(
    State(state): State<AppState>,
    Extension(_admin): Extension<CurrentAdmin>,
    base_path_ext: Option<Extension<BasePath>>,
    jar: CookieJar,
    Path(id): Path<i64>,
) -> Response {
    let base_path = base_path_ext.map(|Extension(p)| p.0).unwrap_or_default();
    let _ = sqlx::query(
        "UPDATE inbounds SET status = 'deleted', updated_at = unixepoch() WHERE id = ?",
    )
    .bind(id)
    .execute(&state.pool)
    .await;

    reload(&state).await;
    redirect_to_inbound(&jar, &base_path)
}

/// `POST /admin/inbound/delete` — soft-delete the inbound (by ID from form, or first active).
pub async fn delete(
    State(state): State<AppState>,
    Extension(_admin): Extension<CurrentAdmin>,
    base_path_ext: Option<Extension<BasePath>>,
    jar: CookieJar,
    axum::extract::Form(form): axum::extract::Form<DeleteInboundForm>,
) -> Response {
    let base_path = base_path_ext.map(|Extension(p)| p.0).unwrap_or_default();
    if let Some(id) = form.id {
        let _ = sqlx::query(
            "UPDATE inbounds SET status = 'deleted', updated_at = unixepoch() WHERE id = ?",
        )
        .bind(id)
        .execute(&state.pool)
        .await;
    } else {
        let _ = sqlx::query(
            "UPDATE inbounds SET status = 'deleted', updated_at = unixepoch() \
             WHERE id = (SELECT id FROM inbounds WHERE status = 'active' ORDER BY id LIMIT 1)",
        )
        .execute(&state.pool)
        .await;
    }
    reload(&state).await;
    redirect_to_inbound(&jar, &base_path)
}

/// `POST /admin/inbound/:id/toggle` — toggle an inbound between active and disabled.
pub async fn toggle(
    State(state): State<AppState>,
    Extension(_admin): Extension<CurrentAdmin>,
    base_path_ext: Option<Extension<BasePath>>,
    jar: CookieJar,
    Path(id): Path<i64>,
) -> Response {
    let base_path = base_path_ext.map(|Extension(p)| p.0).unwrap_or_default();
    let inbounds = list_inbounds(&state.pool).await.unwrap_or_default();
    let Some(ib) = inbounds.into_iter().find(|i| i.id == id) else {
        return redirect_to_inbound(&jar, &base_path);
    };

    if ib.status == "active" {
        let _ = sqlx::query(
            "UPDATE inbounds SET status = 'disabled', updated_at = unixepoch() WHERE id = ?",
        )
        .bind(id)
        .execute(&state.pool)
        .await;
    } else {
        // Check if port is in use by another active inbound
        let port_in_use: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM inbounds WHERE listen_port = ? AND id != ? AND status = 'active'",
        )
        .bind(ib.listen_port)
        .bind(id)
        .fetch_one(&state.pool)
        .await
        .unwrap_or(0);

        if port_in_use > 0 {
            tracing::warn!(
                port = ib.listen_port,
                "cannot activate inbound: port in use"
            );
            return redirect_to_inbound(&jar, &base_path);
        }

        let _ = sqlx::query(
            "UPDATE inbounds SET status = 'active', updated_at = unixepoch() WHERE id = ?",
        )
        .bind(id)
        .execute(&state.pool)
        .await;
    }

    reload(&state).await;
    redirect_to_inbound(&jar, &base_path)
}

async fn list_inbounds(pool: &sqlx::SqlitePool) -> crate::error::AppResult<Vec<Inbound>> {
    sqlx::query_as::<_, Inbound>(
        "SELECT id, listen_port, dest, sni, server_name, private_key, public_key, short_id, \
                xhttp_path, protocol, cert_mode, custom_cert_path, custom_key_path, masquerade_url, \
                port_range, transport_protocol, skip_cert_verify, udp, status, created_at, updated_at \
          FROM inbounds WHERE status != 'deleted' ORDER BY id ASC",
    )
    .fetch_all(pool)
    .await
    .map_err(crate::error::AppError::Database)
}

#[allow(dead_code)]
async fn list_active_inbounds(pool: &sqlx::SqlitePool) -> crate::error::AppResult<Vec<Inbound>> {
    sqlx::query_as::<_, Inbound>(
        "SELECT id, listen_port, dest, sni, server_name, private_key, public_key, short_id, \
                xhttp_path, protocol, cert_mode, custom_cert_path, custom_key_path, masquerade_url, \
                port_range, transport_protocol, skip_cert_verify, udp, status, created_at, updated_at \
          FROM inbounds WHERE status = 'active' ORDER BY id ASC",
    )
    .fetch_all(pool)
    .await
    .map_err(crate::error::AppError::Database)
}

/// Re-render the xray config and ask the supervisor to reload.
async fn reload(state: &AppState) {
    let config_path = state.config.data_dir.join("xray").join("config.json");
    match config::render(&state.pool, &config_path).await {
        Ok(n) => {
            tracing::info!(inbounds = n, "xray config re-rendered");
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

async fn render_error(
    jar: &CookieJar,
    inbound: Option<&Inbound>,
    error: String,
    base_path: &str,
) -> Response {
    let default_short_id = inbound
        .map(|i| i.short_id.clone())
        .unwrap_or_else(random_short_id);
    let default_xhttp_path = inbound
        .map(config::effective_xhttp_path)
        .unwrap_or_else(generate_xhttp_path);
    let html = InboundFormTemplate {
        csrf: csrf_from_cookies(jar),
        inbound: inbound.map(InboundView::from),
        default_short_id,
        default_xhttp_path,
        error: Some(error),
        base_path: base_path.to_string(),
    }
    .render()
    .unwrap_or_else(|e| format!("template error: {e}"));
    page(jar.clone(), StatusCode::BAD_REQUEST, html)
}

/// Generate a 16-hex-char (8-byte) random shortId.
fn random_short_id() -> String {
    let mut bytes = [0u8; 8];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    let mut s = String::with_capacity(16);
    for b in bytes {
        use std::fmt::Write;
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// 303 back to the inbound page, carrying `jar`'s Set-Cookie values.
fn redirect_to_inbound(jar: &CookieJar, base_path: &str) -> Response {
    let target = if base_path.is_empty() {
        "/admin/inbound".to_string()
    } else {
        format!("{base_path}/admin/inbound")
    };
    decorate(Redirect::to(&target).into_response(), Some(jar))
}

/// Render a page with the standard security headers and any Set-Cookie
/// values from `jar`.
fn page(jar: CookieJar, status: StatusCode, body: String) -> Response {
    decorate((status, Html(body)).into_response(), Some(&jar))
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
