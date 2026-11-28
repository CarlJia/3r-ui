use askama::Template;
use axum::{
    extract::{Extension, Path, State},
    http::StatusCode,
    response::{IntoResponse, Redirect, Response},
    Form,
};
use axum_extra::extract::CookieJar;
use serde::Deserialize;

use crate::db::outbound::{
    create_outbound, delete_outbound, get_outbound, list_outbounds, update_outbound, Outbound,
};
use crate::web::middleware::{csrf_from_cookies, BasePath, CurrentAdmin};
use crate::web::AppState;
use crate::xray::SupervisorCommand;

#[derive(Template)]
#[template(path = "outbound_list.html", escape = "html")]
struct OutboundListTemplate {
    csrf: String,
    outbounds: Vec<Outbound>,
    base_path: String,
}

#[derive(Template)]
#[template(path = "outbound_form.html", escape = "html")]
struct OutboundFormTemplate {
    csrf: String,
    outbound: Option<Outbound>,
    error: Option<String>,
    base_path: String,
}

#[derive(Deserialize)]
pub struct OutboundFormData {
    #[serde(default)]
    pub tag: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub protocol: String,
    #[serde(default)]
    pub server_addr: String,
    #[serde(default)]
    pub server_port: String,
    #[serde(default)]
    pub settings_json: String,
    #[serde(default)]
    pub status: String,
}

pub async fn list(
    State(state): State<AppState>,
    Extension(_admin): Extension<CurrentAdmin>,
    base_path_ext: Option<Extension<BasePath>>,
    jar: CookieJar,
) -> Response {
    let base_path = base_path_ext.map(|Extension(p)| p.0).unwrap_or_default();
    let outbounds = list_outbounds(&state.pool).await.unwrap_or_default();
    let html = OutboundListTemplate {
        csrf: csrf_from_cookies(&jar),
        outbounds,
        base_path,
    }
    .render()
    .unwrap_or_else(|e| format!("template error: {e}"));
    crate::web::middleware::render_page(jar, StatusCode::OK, html)
}

pub async fn new_form(
    State(_state): State<AppState>,
    Extension(_admin): Extension<CurrentAdmin>,
    base_path_ext: Option<Extension<BasePath>>,
    jar: CookieJar,
) -> Response {
    let base_path = base_path_ext.map(|Extension(p)| p.0).unwrap_or_default();
    let html = OutboundFormTemplate {
        csrf: csrf_from_cookies(&jar),
        outbound: None,
        error: None,
        base_path,
    }
    .render()
    .unwrap_or_else(|e| format!("template error: {e}"));
    crate::web::middleware::render_page(jar, StatusCode::OK, html)
}

pub async fn edit_form(
    State(state): State<AppState>,
    Extension(_admin): Extension<CurrentAdmin>,
    Path(id): Path<i64>,
    base_path_ext: Option<Extension<BasePath>>,
    jar: CookieJar,
) -> Response {
    let base_path = base_path_ext.map(|Extension(p)| p.0).unwrap_or_default();
    let outbound = match get_outbound(&state.pool, id).await {
        Ok(Some(ob)) => ob,
        _ => {
            let target = if base_path.is_empty() {
                "/admin/outbounds".to_string()
            } else {
                format!("{base_path}/admin/outbounds")
            };
            return Redirect::to(&target).into_response();
        }
    };

    let html = OutboundFormTemplate {
        csrf: csrf_from_cookies(&jar),
        outbound: Some(outbound),
        error: None,
        base_path,
    }
    .render()
    .unwrap_or_else(|e| format!("template error: {e}"));
    crate::web::middleware::render_page(jar, StatusCode::OK, html)
}

pub async fn save_new(
    State(state): State<AppState>,
    Extension(_admin): Extension<CurrentAdmin>,
    base_path_ext: Option<Extension<BasePath>>,
    jar: CookieJar,
    Form(form): Form<OutboundFormData>,
) -> Response {
    let base_path = base_path_ext.map(|Extension(p)| p.0).unwrap_or_default();
    let tag = form.tag.trim();
    let name = form.name.trim();
    let protocol = form.protocol.trim().to_lowercase();
    let server_addr = form.server_addr.trim();
    let server_port: i64 = form.server_port.trim().parse().unwrap_or(0);
    let mut settings_json = form.settings_json.trim().to_string();
    if settings_json.is_empty() {
        settings_json = "{}".to_string();
    }

    if tag.is_empty()
        || name.is_empty()
        || server_addr.is_empty()
        || !(1..=65535).contains(&server_port)
    {
        let html = OutboundFormTemplate {
            csrf: csrf_from_cookies(&jar),
            outbound: None,
            error: Some("请完整填写 Tag、名称、服务器地址及有效端口（1-65535）".into()),
            base_path,
        }
        .render()
        .unwrap_or_else(|e| format!("template error: {e}"));
        return crate::web::middleware::render_page(jar, StatusCode::BAD_REQUEST, html);
    }

    if let Err(e) = create_outbound(
        &state.pool,
        tag,
        name,
        &protocol,
        server_addr,
        server_port,
        &settings_json,
    )
    .await
    {
        let html = OutboundFormTemplate {
            csrf: csrf_from_cookies(&jar),
            outbound: None,
            error: Some(format!("创建出站失败: {e}")),
            base_path,
        }
        .render()
        .unwrap_or_else(|e| format!("template error: {e}"));
        return crate::web::middleware::render_page(jar, StatusCode::BAD_REQUEST, html);
    }

    let config_path = state.config.data_dir.join("xray").join("config.json");
    let _ = crate::xray::config::render(&state.pool, &config_path).await;
    let _ = state.supervisor_tx.send(SupervisorCommand::Reload).await;

    let target = if base_path.is_empty() {
        "/admin/outbounds".to_string()
    } else {
        format!("{base_path}/admin/outbounds")
    };
    Redirect::to(&target).into_response()
}

pub async fn save_edit(
    State(state): State<AppState>,
    Extension(_admin): Extension<CurrentAdmin>,
    Path(id): Path<i64>,
    base_path_ext: Option<Extension<BasePath>>,
    jar: CookieJar,
    Form(form): Form<OutboundFormData>,
) -> Response {
    let base_path = base_path_ext.map(|Extension(p)| p.0).unwrap_or_default();
    let tag = form.tag.trim();
    let name = form.name.trim();
    let protocol = form.protocol.trim().to_lowercase();
    let server_addr = form.server_addr.trim();
    let server_port: i64 = form.server_port.trim().parse().unwrap_or(0);
    let mut settings_json = form.settings_json.trim().to_string();
    if settings_json.is_empty() {
        settings_json = "{}".to_string();
    }
    let status = if form.status.trim() == "disabled" {
        "disabled"
    } else {
        "active"
    };

    if tag.is_empty()
        || name.is_empty()
        || server_addr.is_empty()
        || !(1..=65535).contains(&server_port)
    {
        let existing = get_outbound(&state.pool, id).await.ok().flatten();
        let html = OutboundFormTemplate {
            csrf: csrf_from_cookies(&jar),
            outbound: existing,
            error: Some("请完整填写 Tag、名称、服务器地址及有效端口（1-65535）".into()),
            base_path,
        }
        .render()
        .unwrap_or_else(|e| format!("template error: {e}"));
        return crate::web::middleware::render_page(jar, StatusCode::BAD_REQUEST, html);
    }

    if let Err(e) = update_outbound(
        &state.pool,
        id,
        tag,
        name,
        &protocol,
        server_addr,
        server_port,
        &settings_json,
        status,
    )
    .await
    {
        let existing = get_outbound(&state.pool, id).await.ok().flatten();
        let html = OutboundFormTemplate {
            csrf: csrf_from_cookies(&jar),
            outbound: existing,
            error: Some(format!("更新出站失败: {e}")),
            base_path,
        }
        .render()
        .unwrap_or_else(|e| format!("template error: {e}"));
        return crate::web::middleware::render_page(jar, StatusCode::BAD_REQUEST, html);
    }

    let config_path = state.config.data_dir.join("xray").join("config.json");
    let _ = crate::xray::config::render(&state.pool, &config_path).await;
    let _ = state.supervisor_tx.send(SupervisorCommand::Reload).await;

    let target = if base_path.is_empty() {
        "/admin/outbounds".to_string()
    } else {
        format!("{base_path}/admin/outbounds")
    };
    Redirect::to(&target).into_response()
}

pub async fn delete(
    State(state): State<AppState>,
    Extension(_admin): Extension<CurrentAdmin>,
    Path(id): Path<i64>,
    base_path_ext: Option<Extension<BasePath>>,
) -> Response {
    let base_path = base_path_ext.map(|Extension(p)| p.0).unwrap_or_default();
    let _ = delete_outbound(&state.pool, id).await;

    let config_path = state.config.data_dir.join("xray").join("config.json");
    let _ = crate::xray::config::render(&state.pool, &config_path).await;
    let _ = state.supervisor_tx.send(SupervisorCommand::Reload).await;

    let target = if base_path.is_empty() {
        "/admin/outbounds".to_string()
    } else {
        format!("{base_path}/admin/outbounds")
    };
    Redirect::to(&target).into_response()
}

#[derive(Deserialize)]
pub struct ParseLinkForm {
    #[serde(default)]
    pub link: String,
    #[allow(dead_code)]
    #[serde(default)]
    pub csrf: String,
}

pub async fn parse_link(
    Extension(_admin): Extension<CurrentAdmin>,
    Form(payload): Form<ParseLinkForm>,
) -> Response {
    match crate::share::parse_outbound_link(&payload.link) {
        Ok(parsed) => (
            StatusCode::OK,
            axum::Json(serde_json::json!({
                "success": true,
                "data": parsed,
            })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            axum::Json(serde_json::json!({
                "success": false,
                "error": e.to_string(),
            })),
        )
            .into_response(),
    }
}
