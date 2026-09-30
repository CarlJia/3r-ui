//! Inbound CRUD backed by SQLite, with an xray config re-render and reload
//! after every mutation.
//!
//! The panel supports exactly one active inbound (the schema enforces it with
//! a partial unique index), so create rejects a second one rather than
//! silently displacing the first.

use askama::Template;
use axum::extract::{Extension, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum_extra::extract::cookie::CookieJar;

use crate::crypto::reality::generate_keypair;
use crate::db::models::Inbound;
use crate::web::middleware::{csrf_from_cookies, CurrentAdmin, SECURITY_HEADERS};
use crate::web::AppState;
use crate::xray::config;
use crate::xray::SupervisorCommand;

#[derive(Template)]
#[template(path = "inbound_form.html", escape = "html")]
struct InboundFormTemplate {
    csrf: String,
    inbound: Option<InboundView>,
    error: Option<String>,
}

#[derive(Clone, Debug)]
pub struct InboundView {
    pub id: i64,
    pub listen_port: i64,
    pub dest: String,
    pub sni: String,
    pub public_key: String,
    pub short_id: String,
}

impl From<&Inbound> for InboundView {
    fn from(i: &Inbound) -> Self {
        Self {
            id: i.id,
            listen_port: i.listen_port,
            dest: i.dest.clone(),
            sni: i.sni.clone(),
            public_key: i.public_key.clone(),
            short_id: i.short_id.clone(),
        }
    }
}

#[derive(serde::Deserialize)]
pub struct InboundForm {
    pub listen_port: String,
    pub dest: String,
    pub sni: String,
    pub short_id: String,
}

/// `GET /admin/inbound` — show the single inbound, or an empty create form.
pub async fn show(
    State(state): State<AppState>,
    Extension(_admin): Extension<CurrentAdmin>,
    jar: CookieJar,
) -> Response {
    let existing = current_inbound(&state).await.ok().flatten();
    let html = InboundFormTemplate {
        csrf: csrf_from_cookies(&jar),
        inbound: existing.as_ref().map(InboundView::from),
        error: None,
    }
    .render()
    .unwrap_or_else(|e| format!("template error: {e}"));
    page(jar, StatusCode::OK, html)
}

/// `POST /admin/inbound` — create or update the single inbound.
pub async fn save(
    State(state): State<AppState>,
    Extension(_admin): Extension<CurrentAdmin>,
    jar: CookieJar,
    axum::extract::Form(form): axum::extract::Form<InboundForm>,
) -> Response {
    let existing = current_inbound(&state).await.ok().flatten();

    let port: i64 = match form.listen_port.trim().parse() {
        Ok(p) if (1..=65535).contains(&p) => p,
        _ => {
            return render_error(
                &jar,
                existing.as_ref(),
                "端口必须是 1-65535 之间的整数".into(),
            )
            .await
        }
    };

    let dest = form.dest.trim();
    let sni = form.sni.trim();
    if dest.is_empty() || sni.is_empty() {
        return render_error(&jar, existing.as_ref(), "dest 与 SNI 不能为空".into()).await;
    }

    let short_id = form.short_id.trim().to_string();
    if short_id.is_empty()
        || short_id.len() > 16
        || !short_id.chars().all(|c| c.is_ascii_hexdigit())
    {
        return render_error(
            &jar,
            existing.as_ref(),
            "shortId 必须是 1-16 位十六进制字符".into(),
        )
        .await;
    }

    // A second active inbound is rejected up front so the operator gets a
    // clear message rather than a UNIQUE-constraint error from the database.
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM inbounds WHERE status = 'active'")
        .fetch_one(&state.pool)
        .await
        .unwrap_or(0);
    if count > 0 && existing.is_none() {
        return render_error(&jar, None, "已存在启用的入站，请先编辑或删除它".into()).await;
    }

    let keypair = match generate_keypair() {
        Ok(k) => k,
        Err(e) => {
            tracing::error!(error = %e, "REALITY key generation failed");
            return render_error(&jar, existing.as_ref(), "密钥生成失败".into()).await;
        }
    };

    let result = match &existing {
        Some(inbound) => {
            sqlx::query(
                "UPDATE inbounds SET listen_port = ?, dest = ?, sni = ?, short_id = ?, \
                        private_key = ?, public_key = ?, updated_at = unixepoch() \
                 WHERE id = ?",
            )
            .bind(port)
            .bind(dest)
            .bind(sni)
            .bind(&short_id)
            .bind(&keypair.private_key)
            .bind(&keypair.public_key)
            .bind(inbound.id)
            .execute(&state.pool)
            .await
        }
        None => {
            sqlx::query(
                "INSERT INTO inbounds (listen_port, dest, sni, server_name, private_key, \
                        public_key, short_id, status, created_at, updated_at) \
                 VALUES (?, ?, ?, NULL, ?, ?, ?, 'active', unixepoch(), unixepoch())",
            )
            .bind(port)
            .bind(dest)
            .bind(sni)
            .bind(&keypair.private_key)
            .bind(&keypair.public_key)
            .bind(&short_id)
            .execute(&state.pool)
            .await
        }
    };

    if let Err(e) = result {
        // A port collision surfaces here as a UNIQUE violation.
        let msg = if e.to_string().contains("UNIQUE") {
            format!("端口 {port} 已被占用")
        } else {
            tracing::error!(error = %e, "inbound save failed");
            "保存失败".to_string()
        };
        return render_error(&jar, existing.as_ref(), msg).await;
    }

    reload(&state).await;
    redirect_to_inbound(&jar)
}

/// `POST /admin/inbound/delete` — soft-delete the inbound.
pub async fn delete(
    State(state): State<AppState>,
    Extension(_admin): Extension<CurrentAdmin>,
    jar: CookieJar,
) -> Response {
    let _ = sqlx::query(
        "UPDATE inbounds SET status = 'deleted', updated_at = unixepoch() \
                         WHERE status = 'active'",
    )
    .execute(&state.pool)
    .await;
    reload(&state).await;
    redirect_to_inbound(&jar)
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
}

async fn render_error(jar: &CookieJar, inbound: Option<&Inbound>, error: String) -> Response {
    let html = InboundFormTemplate {
        csrf: csrf_from_cookies(jar),
        inbound: inbound.map(InboundView::from),
        error: Some(error),
    }
    .render()
    .unwrap_or_else(|e| format!("template error: {e}"));
    page(jar.clone(), StatusCode::BAD_REQUEST, html)
}

/// 303 back to the inbound page, carrying `jar`'s Set-Cookie values.
fn redirect_to_inbound(jar: &CookieJar) -> Response {
    decorate(Redirect::to("/admin/inbound").into_response(), Some(jar))
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
