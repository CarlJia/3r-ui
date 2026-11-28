//! Cloudflare Tunnel management web handlers and templates.

use askama::Template;
use axum::{
    extract::{Extension, State},
    http::StatusCode,
    response::{IntoResponse, Redirect, Response},
    Form, Json,
};
use axum_extra::extract::CookieJar;
use serde::Deserialize;
use serde_json::json;

use crate::db::models::TunnelSettings;
use crate::tunnel::TunnelStatus;
use crate::web::middleware::{csrf_from_cookies, BasePath, CurrentAdmin};
use crate::web::AppState;

#[derive(Template)]
#[template(path = "tunnel.html", escape = "html")]
pub struct TunnelTemplate {
    pub csrf: String,
    pub base_path: String,
    pub tunnel: TunnelSettings,
    pub tunnel_status: TunnelStatus,
}

pub async fn tunnel_page(
    State(state): State<AppState>,
    Extension(_admin): Extension<CurrentAdmin>,
    base_path_ext: Option<Extension<BasePath>>,
    jar: CookieJar,
) -> Response {
    let base_path = base_path_ext.map(|Extension(p)| p.0).unwrap_or_default();

    let tunnel = state
        .tunnel_supervisor
        .get_settings()
        .await
        .unwrap_or_default();

    let tunnel_status = state
        .tunnel_supervisor
        .status()
        .await
        .unwrap_or(TunnelStatus {
            installed: false,
            bin_path: "未找到".into(),
            running: false,
            is_external: false,
            pid: None,
            last_status: "未运行".into(),
            logs: vec![],
        });

    let html = TunnelTemplate {
        csrf: csrf_from_cookies(&jar),
        base_path,
        tunnel,
        tunnel_status,
    }
    .render()
    .unwrap_or_else(|e| format!("template error: {e}"));

    crate::web::middleware::render_page(jar, StatusCode::OK, html)
}

#[derive(Deserialize)]
pub struct TunnelForm {
    pub enabled: Option<String>,
    pub tunnel_token: Option<String>,
    pub custom_path: Option<String>,
}

pub async fn save_tunnel_settings(
    State(state): State<AppState>,
    Extension(_admin): Extension<CurrentAdmin>,
    base_path_ext: Option<Extension<BasePath>>,
    Form(form): Form<TunnelForm>,
) -> Response {
    let base_path = base_path_ext.map(|Extension(p)| p.0).unwrap_or_default();
    let enabled = form.enabled.as_deref() == Some("1")
        || form.enabled.as_deref() == Some("on")
        || form.enabled.as_deref() == Some("true");
    let enabled_int: i64 = if enabled { 1 } else { 0 };
    let token = form.tunnel_token.unwrap_or_default().trim().to_string();
    let custom_path = form.custom_path.unwrap_or_default().trim().to_string();

    let res = sqlx::query(
        "UPDATE tunnel_settings SET enabled = $1, tunnel_token = $2, custom_path = $3, updated_at = unixepoch() WHERE id = 1"
    )
    .bind(enabled_int)
    .bind(token)
    .bind(custom_path)
    .execute(&state.pool)
    .await;

    if let Err(e) = res {
        tracing::error!(error = %e, "failed to update tunnel settings in db");
    } else {
        let _ = state
            .tunnel_tx
            .send(crate::tunnel::TunnelCommand::Reload)
            .await;
    }

    let target = if base_path.is_empty() {
        "/admin/tunnel".to_string()
    } else {
        format!("{base_path}/admin/tunnel")
    };
    Redirect::to(&target).into_response()
}

#[derive(Deserialize)]
pub struct TunnelActionReq {
    pub action: String,
    #[allow(dead_code)]
    #[serde(default)]
    pub csrf: String,
}

pub async fn tunnel_action_api(
    State(state): State<AppState>,
    Extension(_admin): Extension<CurrentAdmin>,
    Form(form): Form<TunnelActionReq>,
) -> Response {
    let cmd = match form.action.as_str() {
        "stop" => crate::tunnel::TunnelCommand::Stop,
        _ => crate::tunnel::TunnelCommand::Restart,
    };

    let _ = state.tunnel_tx.send(cmd).await;

    tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;

    match state.tunnel_supervisor.status().await {
        Ok(status) => Json(json!({
            "status": "ok",
            "tunnel_status": status,
        }))
        .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({
                "status": "error",
                "error": e.to_string(),
            })),
        )
            .into_response(),
    }
}

pub async fn tunnel_status_api(
    State(state): State<AppState>,
    Extension(_admin): Extension<CurrentAdmin>,
) -> Response {
    match state.tunnel_supervisor.status().await {
        Ok(status) => Json(json!({
            "status": "ok",
            "tunnel_status": status,
        }))
        .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({
                "status": "error",
                "error": e.to_string(),
            })),
        )
            .into_response(),
    }
}
