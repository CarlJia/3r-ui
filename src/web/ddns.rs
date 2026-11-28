//! Cloudflare DDNS management web handlers and templates.

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

use crate::db::models::DdnsSettings;
use crate::web::middleware::{csrf_from_cookies, BasePath, CurrentAdmin};
use crate::web::AppState;

#[derive(Template)]
#[template(path = "ddns.html", escape = "html")]
pub struct DdnsTemplate {
    pub csrf: String,
    pub base_path: String,
    pub ddns: DdnsSettings,
    pub ddns_effective_token_masked: String,
    pub ddns_inherited_from_proxy: bool,
    pub ddns_last_sync_time: String,
}

pub async fn ddns_page(
    State(state): State<AppState>,
    Extension(_admin): Extension<CurrentAdmin>,
    base_path_ext: Option<Extension<BasePath>>,
    jar: CookieJar,
) -> Response {
    let base_path = base_path_ext.map(|Extension(p)| p.0).unwrap_or_default();

    let ddns = crate::ddns::config::get_ddns_settings(&state.pool)
        .await
        .unwrap_or_default();

    let ddns_last_sync_time = if ddns.last_sync_at > 0 {
        format_unix_time(ddns.last_sync_at)
    } else {
        "尚未执行过同步".to_string()
    };

    let proxy_raw = crate::proxy::config::get_proxy_settings(&state.pool).await;
    let (ddns_effective_token_masked, ddns_inherited_from_proxy) = if !ddns.cf_api_token.is_empty()
    {
        (ddns.masked_token(), false)
    } else if let Ok(ref proxy) = proxy_raw {
        if !proxy.cf_api_token.is_empty() {
            (proxy.masked_cf_token(), true)
        } else {
            (String::new(), false)
        }
    } else {
        (String::new(), false)
    };

    let html = DdnsTemplate {
        csrf: csrf_from_cookies(&jar),
        base_path,
        ddns,
        ddns_effective_token_masked,
        ddns_inherited_from_proxy,
        ddns_last_sync_time,
    }
    .render()
    .unwrap_or_else(|e| format!("template error: {e}"));

    crate::web::middleware::render_page(jar, StatusCode::OK, html)
}

#[derive(Deserialize)]
pub struct DdnsForm {
    pub enabled: Option<String>,
    pub cf_api_token: Option<String>,
    pub cf_proxied: Option<String>,
    pub auto_sync_public_host: Option<String>,
    pub ipv4_enabled: Option<String>,
    pub ipv4_domains: Option<String>,
    pub ipv6_enabled: Option<String>,
    pub ipv6_domains: Option<String>,
    pub check_interval_mins: Option<i64>,
}

pub async fn save_ddns_settings(
    State(state): State<AppState>,
    Extension(_admin): Extension<CurrentAdmin>,
    base_path_ext: Option<Extension<BasePath>>,
    Form(form): Form<DdnsForm>,
) -> Response {
    let base_path = base_path_ext.map(|Extension(p)| p.0).unwrap_or_default();

    let current = crate::ddns::config::get_ddns_settings(&state.pool)
        .await
        .unwrap_or_default();

    let token_input = form.cf_api_token.unwrap_or_default().trim().to_string();
    let effective_token = if token_input.contains("••••") || token_input.is_empty() {
        current.cf_api_token
    } else {
        token_input
    };

    let enabled = form.enabled.as_deref() == Some("1");
    let ipv4_enabled = form.ipv4_enabled.as_deref() == Some("1");
    let ipv6_enabled = form.ipv6_enabled.as_deref() == Some("1");
    let cf_proxied = form.cf_proxied.as_deref() == Some("1");
    let auto_sync_public_host = form.auto_sync_public_host.as_deref() == Some("1");
    let check_interval_mins = form.check_interval_mins.unwrap_or(5).clamp(1, 1440);
    let ipv4_domains = form.ipv4_domains.unwrap_or_default().trim().to_string();
    let ipv6_domains = form.ipv6_domains.unwrap_or_default().trim().to_string();

    if let Err(e) = crate::ddns::config::update_ddns_settings(
        &state.pool,
        enabled,
        if effective_token.is_empty() {
            None
        } else {
            Some(&effective_token)
        },
        ipv4_enabled,
        &ipv4_domains,
        ipv6_enabled,
        &ipv6_domains,
        check_interval_mins,
        cf_proxied,
        auto_sync_public_host,
    )
    .await
    {
        tracing::error!(error = %e, "failed to update DDNS settings");
    } else if enabled {
        state.ddns_notifier.notify_sync();
    }

    let target = if base_path.is_empty() {
        "/admin/ddns".to_string()
    } else {
        format!("{base_path}/admin/ddns")
    };
    Redirect::to(&target).into_response()
}

pub async fn trigger_ddns_sync(
    State(state): State<AppState>,
    Extension(_admin): Extension<CurrentAdmin>,
) -> Response {
    match crate::ddns::runner::perform_ddns_sync(&state.pool).await {
        Ok(msg) => {
            let settings = crate::ddns::config::get_ddns_settings(&state.pool)
                .await
                .unwrap_or_default();
            Json(json!({
                "status": "ok",
                "message": msg,
                "last_ipv4": settings.last_ipv4,
                "last_ipv6": settings.last_ipv6,
                "last_status": settings.last_status,
                "last_sync_at": settings.last_sync_at,
            }))
            .into_response()
        }
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

pub async fn ddns_status_api(
    State(state): State<AppState>,
    Extension(_admin): Extension<CurrentAdmin>,
) -> Response {
    match crate::ddns::config::get_ddns_settings(&state.pool).await {
        Ok(s) => Json(json!({
            "status": "ok",
            "enabled": s.is_enabled(),
            "last_ipv4": s.last_ipv4,
            "last_ipv6": s.last_ipv6,
            "last_status": s.last_status,
            "last_sync_at": s.last_sync_at,
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

fn format_unix_time(ts: i64) -> String {
    if ts <= 0 {
        return "-".to_string();
    }
    match time::OffsetDateTime::from_unix_timestamp(ts) {
        Ok(dt) => {
            let fmt = time::macros::format_description!(
                "[year]-[month]-[day] [hour]:[minute]:[second] UTC"
            );
            dt.format(&fmt).unwrap_or_else(|_| "-".to_string())
        }
        Err(_) => "-".to_string(),
    }
}
