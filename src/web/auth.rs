//! Authentication handlers: login, logout, password change, dashboard shell.
//!
//! The dashboard here is a placeholder that U5/U6 fill in. U4 owns the auth
//! flow and the fact that an authenticated request reaches a rendered page.

use askama::Template;
use axum::extract::State;
use axum::http::{header, StatusCode};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum_extra::extract::cookie::{Cookie, CookieJar, SameSite};

use crate::crypto::password;
use crate::crypto::session::{new_csrf_token, new_session_token, now_unix};
use crate::db::models::Admin;
use crate::web::middleware::{csrf_from_cookies, CurrentAdmin, SECURITY_HEADERS};
use crate::web::{AppState, CSRF_COOKIE, SESSION_COOKIE, SESSION_TTL_SECS};

#[derive(Template)]
#[template(path = "login.html", escape = "html")]
struct LoginTemplate {
    csrf: String,
    error: Option<String>,
}

#[derive(Template)]
#[template(path = "dashboard.html", escape = "html")]
struct DashboardTemplate {
    username: String,
    csrf: String,
}

/// `GET /` — the login form. An authenticated visitor is bounced to /admin.
pub async fn login_page(State(state): State<AppState>, jar: CookieJar) -> Response {
    if let Some(admin) = current_admin(&state, &jar).await {
        let _ = admin;
        return Redirect::to("/admin").into_response();
    }
    let csrf = new_csrf_token();
    let html = LoginTemplate {
        csrf: csrf.clone(),
        error: None,
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
    axum::extract::Form(form): axum::extract::Form<LoginForm>,
) -> Response {
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
        // Re-render with an error rather than redirecting, so the operator can
        // retry immediately. The generic message avoids disclosing whether the
        // username exists.
        let csrf = new_csrf_token();
        let html = LoginTemplate {
            csrf: csrf.clone(),
            error: Some("用户名或密码错误".to_string()),
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
    redirect_with(jar, "/admin")
}

/// `POST /admin/logout` — delete the session row and clear both cookies.
pub async fn logout(State(state): State<AppState>, jar: CookieJar) -> Response {
    if let Some(session_id) = jar.get(SESSION_COOKIE).map(|c| c.value().to_string()) {
        let _ = sqlx::query("DELETE FROM sessions WHERE session_id = ?")
            .bind(&session_id)
            .execute(&state.pool)
            .await;
    }
    let jar = jar
        .remove(removable(SESSION_COOKIE))
        .remove(removable(CSRF_COOKIE));
    redirect_with(jar, "/")
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
    jar: CookieJar,
    axum::extract::Form(form): axum::extract::Form<PasswordForm>,
) -> Response {
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
    redirect_with(jar, "/")
}

/// `GET /admin` — authenticated landing page. U5 replaces the body.
pub async fn dashboard(
    State(state): State<AppState>,
    axum::Extension(admin): axum::Extension<CurrentAdmin>,
    jar: CookieJar,
) -> Response {
    let _ = &state;
    let csrf = csrf_from_cookies(&jar);
    let html = DashboardTemplate {
        username: admin.0.username,
        csrf,
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
fn redirect_with(jar: CookieJar, location: &'static str) -> Response {
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
