//! Authentication handlers: login, logout, password change, dashboard shell.
//!
//! The dashboard here is a placeholder that U5/U6 fill in. U4 owns the auth
//! flow and the fact that an authenticated request reaches a rendered page.

use askama::Template;
use axum::extract::{Extension, State};
use axum::http::{header, StatusCode};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum_extra::extract::cookie::{Cookie, CookieJar, SameSite};

use crate::crypto::password;
use crate::crypto::session::{new_csrf_token, new_session_token, now_unix};
use crate::db::models::{Admin, Inbound};
use crate::web::middleware::{
    csrf_from_cookies, BasePath, ClientIp, CurrentAdmin, SECURITY_HEADERS,
};
use crate::web::{AppState, CSRF_COOKIE, SESSION_COOKIE, SESSION_TTL_SECS};

#[derive(Template)]
#[template(path = "login.html", escape = "html")]
struct LoginTemplate {
    csrf: String,
    error: Option<String>,
    base_path: String,
}

#[derive(Template)]
#[template(path = "dashboard.html", escape = "html")]
struct DashboardTemplate {
    username: String,
    csrf: String,
    xray_version: String,
    xray_running: bool,
    client_count: i64,
    active_client_count: i64,
    inbound: Option<DashboardInboundView>,
    proxy: Option<DashboardProxyView>,
    system: crate::web::system::SystemMetrics,
    base_path: String,
}

struct DashboardInboundView {
    listen_port: i64,
    dest: String,
    sni: String,
    xhttp_path: String,
}

struct DashboardProxyView {
    enabled: bool,
    listen_port: i64,
    domain: String,
    path: String,
    full_url: String,
    cert_status: String,
}

/// `GET /` — the login form. An authenticated visitor is bounced to /admin.
pub async fn login_page(
    State(state): State<AppState>,
    jar: CookieJar,
    base_path_ext: Option<Extension<BasePath>>,
) -> Response {
    let base_path = base_path_ext.map(|Extension(p)| p.0).unwrap_or_default();
    if let Some(_admin) = current_admin(&state, &jar).await {
        let target = format!("{base_path}/admin");
        return Redirect::to(&target).into_response();
    }
    let csrf = new_csrf_token();
    let html = LoginTemplate {
        csrf: csrf.clone(),
        error: None,
        base_path,
    }
    .render()
    .unwrap_or_else(|e| format!("template error: {e}"));
    page(
        jar.add(build_cookie(CSRF_COOKIE, &csrf, false)),
        StatusCode::OK,
        html,
    )
}

#[derive(serde::Deserialize)]
pub struct LoginForm {
    pub username: String,
    pub password: String,
}

/// `POST /login` — verify credentials, then mint a session + CSRF pair.
pub async fn login_submit(
    State(state): State<AppState>,
    jar: CookieJar,
    base_path_ext: Option<Extension<BasePath>>,
    client_ip_ext: Option<Extension<ClientIp>>,
    axum::extract::Form(form): axum::extract::Form<LoginForm>,
) -> Response {
    let base_path = base_path_ext.map(|Extension(p)| p.0).unwrap_or_default();
    let client_ip = client_ip_ext
        .map(|Extension(ip)| ip.0)
        .unwrap_or(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST));
    let admin = sqlx::query_as::<_, Admin>(
        "SELECT id, username, password_hash, created_at FROM admins WHERE username = ?",
    )
    .bind(&form.username)
    .fetch_optional(&state.pool)
    .await
    .ok()
    .flatten();

    let valid = admin
        .as_ref()
        .is_some_and(|a| password::verify(&form.password, &a.password_hash));

    if !valid {
        // [SECURITY] 审计日志：输出结构化安全登录失败日志，供 Fail2ban filter.d/3r-ui.conf 精准匹配与封禁
        tracing::warn!(
            target: "security",
            "[SECURITY] Web login failed for user='{}' from ip='{}'",
            form.username,
            client_ip
        );

        // Re-render with an error rather than redirecting, so the operator can
        // retry immediately. The generic message avoids disclosing whether the
        // username exists.
        let csrf = new_csrf_token();
        let html = LoginTemplate {
            csrf: csrf.clone(),
            error: Some("用户名或密码错误".to_string()),
            base_path: base_path.clone(),
        }
        .render()
        .unwrap_or_else(|e| format!("template error: {e}"));
        return page(
            jar.add(build_cookie(CSRF_COOKIE, &csrf, false)),
            StatusCode::UNAUTHORIZED,
            html,
        );
    }

    let admin = admin.expect("checked above");
    let session_id = new_session_token();
    let csrf = new_csrf_token();
    let expires_at = now_unix() + SESSION_TTL_SECS;

    if let Err(e) = sqlx::query(
        "INSERT INTO sessions (session_id, admin_id, expires_at, created_at) VALUES (?, ?, ?, ?)",
    )
    .bind(&session_id)
    .bind(admin.id)
    .bind(expires_at)
    .bind(now_unix())
    .execute(&state.pool)
    .await
    {
        tracing::error!(error = %e, "failed to persist session");
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            "failed to create session",
        )
            .into_response();
    }

    let jar = jar
        .add(build_cookie(SESSION_COOKIE, &session_id, true))
        .add(build_cookie(CSRF_COOKIE, &csrf, true));
    let target = format!("{base_path}/admin");
    redirect_with(jar, &target)
}

/// `POST /admin/logout` — delete the session row and clear both cookies.
pub async fn logout(
    State(state): State<AppState>,
    jar: CookieJar,
    base_path_ext: Option<Extension<BasePath>>,
) -> Response {
    let base_path = base_path_ext.map(|Extension(p)| p.0).unwrap_or_default();
    if let Some(session_id) = jar.get(SESSION_COOKIE).map(|c| c.value().to_string()) {
        let _ = sqlx::query("DELETE FROM sessions WHERE session_id = ?")
            .bind(&session_id)
            .execute(&state.pool)
            .await;
    }
    let jar = jar
        .remove(removable(SESSION_COOKIE))
        .remove(removable(CSRF_COOKIE));
    let target = if base_path.is_empty() {
        "/".to_string()
    } else {
        base_path
    };
    redirect_with(jar, &target)
}

#[derive(serde::Deserialize)]
pub struct PasswordForm {
    pub current_password: String,
    pub new_password: String,
}

/// `POST /admin/password` — rotate the admin password after re-authenticating.
pub async fn change_password(
    State(state): State<AppState>,
    axum::Extension(admin): axum::Extension<CurrentAdmin>,
    base_path_ext: Option<Extension<BasePath>>,
    jar: CookieJar,
    axum::extract::Form(form): axum::extract::Form<PasswordForm>,
) -> Response {
    let base_path = base_path_ext.map(|Extension(p)| p.0).unwrap_or_default();
    if !password::verify(&form.current_password, &admin.0.password_hash) {
        return (StatusCode::FORBIDDEN, "当前密码不正确").into_response();
    }
    if form.new_password.len() < 12 {
        return (StatusCode::UNPROCESSABLE_ENTITY, "新密码至少 12 位").into_response();
    }
    let new_hash = match password::hash(&form.new_password) {
        Ok(h) => h,
        Err(e) => {
            tracing::error!(error = %e, "bcrypt hash failed");
            return (StatusCode::INTERNAL_SERVER_ERROR, "hash failed").into_response();
        }
    };
    if let Err(e) = sqlx::query("UPDATE admins SET password_hash = ? WHERE id = ?")
        .bind(&new_hash)
        .bind(admin.0.id)
        .execute(&state.pool)
        .await
    {
        tracing::error!(error = %e, "password update failed");
        return (StatusCode::INTERNAL_SERVER_ERROR, "update failed").into_response();
    }
    // Force re-login everywhere after a credential change.
    let _ = sqlx::query("DELETE FROM sessions WHERE admin_id = ?")
        .bind(admin.0.id)
        .execute(&state.pool)
        .await;

    let jar = jar
        .remove(removable(SESSION_COOKIE))
        .remove(removable(CSRF_COOKIE));
    let target = if base_path.is_empty() {
        "/".to_string()
    } else {
        base_path
    };
    redirect_with(jar, &target)
}

/// `GET /admin` — authenticated landing page with system status summary.
pub async fn dashboard(
    State(state): State<AppState>,
    axum::Extension(admin): axum::Extension<CurrentAdmin>,
    base_path_ext: Option<Extension<BasePath>>,
    jar: CookieJar,
) -> Response {
    let base_path = base_path_ext.map(|Extension(p)| p.0).unwrap_or_default();
    let csrf = csrf_from_cookies(&jar);

    // 1. Get xray version and running status
    let xray_bin =
        state
            .config
            .data_dir
            .join("xray")
            .join(if cfg!(windows) { "xray.exe" } else { "xray" });

    let xray_bin_clone = xray_bin.clone();
    let xray_version = tokio::task::spawn_blocking(move || {
        let output = std::process::Command::new(&xray_bin_clone)
            .arg("--version")
            .output();
        match output {
            Ok(o) if o.status.success() => {
                let first_line = String::from_utf8_lossy(&o.stdout)
                    .lines()
                    .next()
                    .unwrap_or("")
                    .trim()
                    .to_string();
                if first_line.is_empty() {
                    "未安装".to_string()
                } else {
                    first_line
                }
            }
            _ => "未安装".to_string(),
        }
    })
    .await
    .unwrap_or_else(|_| "未安装".to_string());

    // Check if xray process is actually running via PID file
    let pid_file = state.config.data_dir.join("xray.pid");
    let xray_running = if let Ok(content) = std::fs::read_to_string(&pid_file) {
        if let Ok(pid) = content.trim().parse::<i32>() {
            #[cfg(unix)]
            {
                unsafe { libc::kill(pid, 0) == 0 }
            }
            #[cfg(not(unix))]
            {
                false
            }
        } else {
            false
        }
    } else {
        false
    };

    // 2. Client counts
    let client_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM clients")
        .fetch_one(&state.pool)
        .await
        .unwrap_or(0);
    let active_client_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM clients WHERE status = 'active'")
            .fetch_one(&state.pool)
            .await
            .unwrap_or(0);

    // 3. Inbound info
    let inbound_row = sqlx::query_as::<_, Inbound>(
        "SELECT id, listen_ip, listen_port, dest, sni, server_name, private_key, public_key, short_id, xhttp_path, protocol, cert_mode, custom_cert_path, custom_key_path, masquerade_url, port_range, transport_protocol, skip_cert_verify, udp, status, outbound_id, created_at, updated_at FROM inbounds LIMIT 1",
    )
    .fetch_optional(&state.pool)
    .await
    .unwrap_or(None);

    let inbound = inbound_row.map(|i| {
        let xhttp_path = crate::xray::config::effective_xhttp_path(&i);
        DashboardInboundView {
            listen_port: i.listen_port,
            dest: i.dest,
            sni: i.sni,
            xhttp_path,
        }
    });

    // 4. Proxy info
    let proxy_settings = crate::proxy::config::get_proxy_settings(&state.pool)
        .await
        .ok();
    let proxy = proxy_settings.map(|p| {
        let cert_status = match p.cert_expires_at {
            Some(exp) => {
                let now = now_unix();
                let remaining_days = (exp - now) / 86400;
                let iso = crate::web::logs::timestamp_to_iso(exp);
                if remaining_days < 0 {
                    format!("已过期 ({iso})")
                } else {
                    format!("有效至 {iso} (剩余 {remaining_days} 天)")
                }
            }
            None => {
                if p.cert_mode == "custom" {
                    "自定义本地证书".to_string()
                } else {
                    "未签发".to_string()
                }
            }
        };
        DashboardProxyView {
            enabled: p.is_enabled(),
            listen_port: p.listen_port,
            domain: p.domain.clone(),
            path: p.normalized_path(),
            full_url: p.full_url(),
            cert_status,
        }
    });

    let system = state.system_tracker.collect(&state.config.data_dir);

    let html = DashboardTemplate {
        username: admin.0.username,
        csrf,
        xray_version,
        xray_running,
        client_count,
        active_client_count,
        inbound,
        proxy,
        system,
        base_path,
    }
    .render()
    .unwrap_or_else(|e| format!("template error: {e}"));
    page(jar, StatusCode::OK, html)
}

pub async fn not_found() -> Response {
    (StatusCode::NOT_FOUND, "404 Not Found").into_response()
}

async fn current_admin(state: &AppState, jar: &CookieJar) -> Option<CurrentAdmin> {
    let session_id = jar.get(SESSION_COOKIE)?.value().to_string();
    let (admin_id, expires_at): (i64, i64) =
        sqlx::query_as("SELECT admin_id, expires_at FROM sessions WHERE session_id = ?")
            .bind(&session_id)
            .fetch_optional(&state.pool)
            .await
            .ok()
            .flatten()?;
    if expires_at <= now_unix() {
        return None;
    }
    let admin = sqlx::query_as::<_, Admin>(
        "SELECT id, username, password_hash, created_at FROM admins WHERE id = ?",
    )
    .bind(admin_id)
    .fetch_optional(&state.pool)
    .await
    .ok()
    .flatten()?;
    Some(CurrentAdmin(admin))
}

fn build_cookie(name: &'static str, value: &str, http_only: bool) -> Cookie<'static> {
    let mut c = Cookie::build((name, value.to_string()))
        .path("/")
        .same_site(SameSite::Strict)
        .secure(false)
        .build();
    if http_only {
        c.set_http_only(true);
    }
    c
}

fn removable(name: &'static str) -> Cookie<'static> {
    Cookie::build((name, ""))
        .path("/")
        .max_age(time::Duration::seconds(0))
        .build()
}

/// 303 to `location`, carrying `jar`'s Set-Cookie values so a login or logout
/// can set and clear cookies in the same response as the redirect.
fn redirect_with(jar: CookieJar, location: &str) -> Response {
    let mut response = Redirect::to(location).into_response();
    for cookie in jar.iter() {
        if let Ok(header_value) = cookie.encoded().to_string().parse() {
            response
                .headers_mut()
                .append(header::SET_COOKIE, header_value);
        }
    }
    for (name, value) in SECURITY_HEADERS {
        response.headers_mut().insert(
            axum::http::HeaderName::from_bytes(name.as_bytes()).expect("static header name"),
            axum::http::HeaderValue::from_static(value),
        );
    }
    response
}

/// Render a page with the standard security headers and any Set-Cookie
/// values from `jar` appended.
fn page(jar: CookieJar, status: StatusCode, body: String) -> Response {
    let mut response = (status, Html(body)).into_response();
    for (name, value) in SECURITY_HEADERS {
        response.headers_mut().insert(
            axum::http::HeaderName::from_bytes(name.as_bytes()).expect("static header name"),
            axum::http::HeaderValue::from_static(value),
        );
    }
    for cookie in jar.iter() {
        if let Ok(header_value) = cookie.encoded().to_string().parse() {
            response
                .headers_mut()
                .append(header::SET_COOKIE, header_value);
        }
    }
    response
}
