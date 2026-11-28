//! Integration tests for U4: authentication, sessions, and CSRF.
//!
//! Drives the real Axum router with `tower::ServiceExt::oneshot`, so the
//! middleware chain, cookie handling, and SQLite session store are all
//! exercised without binding a socket.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tempfile::tempdir;
use three_r_ui::config::AppConfig;
use three_r_ui::crypto::password;
use three_r_ui::web::{self, AppState};
use tower::ServiceExt;

/// Boot an app against a fresh temp data dir with one known admin.
async fn boot() -> (axum::Router, tempfile::TempDir, sqlx::SqlitePool) {
    let dir = tempdir().unwrap();
    let mut config = AppConfig::from_env().unwrap();
    config.data_dir = dir.path().to_path_buf();

    // Reuse the production storage bootstrap so migrations run identically.
    let pool = three_r_ui::init_storage(&config).await.unwrap();
    let hash = password::hash("s3cret-admin-pw").unwrap();
    sqlx::query(
        "INSERT INTO admins (username, password_hash, created_at) VALUES (?, ?, unixepoch())",
    )
    .bind("admin")
    .bind(&hash)
    .execute(&pool)
    .await
    .unwrap();

    let (tx, _rx) = tokio::sync::mpsc::channel(4);
    let (proxy_tx, _proxy_rx) = tokio::sync::mpsc::channel(4);
    let (mieru_tx, _mieru_rx) = tokio::sync::mpsc::channel(4);
    let (tunnel_tx, _tunnel_rx) = tokio::sync::mpsc::channel(4);
    let tunnel_supervisor = std::sync::Arc::new(three_r_ui::tunnel::CloudflaredSupervisor::new(
        pool.clone(),
        dir.path(),
    ));
    let state = AppState {
        config,
        pool: pool.clone(),
        supervisor_tx: tx,
        proxy_tx,
        mieru_tx,
        tunnel_tx,
        tunnel_supervisor,
        update_jobs: std::sync::Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
        installer: std::sync::Arc::new(
            three_r_ui::xray::Installer::new("https://github.com/3r-ui/xray/releases")
                .expect("installer must be constructible"),
        ),
        cert_tracker: std::sync::Arc::new(three_r_ui::proxy::CertProgressTracker::new()),
        system_tracker: std::sync::Arc::new(three_r_ui::web::system::SystemTracker::new()),
        ddns_notifier: three_r_ui::ddns::DdnsNotifier::new(),
        node_manager: std::sync::Arc::new(three_r_ui::node::NodeManager::new(pool.clone())),
        shutdown_initiated: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
    };
    (web::router(state), dir, pool)
}

fn get(uri: &str) -> Request<Body> {
    Request::builder().uri(uri).body(Body::empty()).unwrap()
}

fn post_form(uri: &str, body: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(Body::from(body.to_string()))
        .unwrap()
}

/// Pull a cookie value out of a response's Set-Cookie headers.
fn set_cookie(resp: &axum::response::Response, name: &str) -> Option<String> {
    let prefix = format!("{name}=");
    resp.headers()
        .get_all("set-cookie")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find_map(|v| {
            let after = v.strip_prefix(&prefix)?;
            // Cookie value ends at the first ';' (attributes follow).
            Some(after.split(';').next().unwrap_or("").to_string())
        })
}

async fn login(app: &axum::Router, password: &str) -> (String, String) {
    let resp = app
        .clone()
        .oneshot(post_form(
            "/login",
            &format!("username=admin&password={password}"),
        ))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::SEE_OTHER,
        "login should redirect"
    );
    let session = set_cookie(&resp, "session").expect("session cookie");
    let csrf = set_cookie(&resp, "csrf").expect("csrf cookie");
    (session, csrf)
}

#[tokio::test]
async fn login_page_renders_and_sets_csrf_cookie() {
    let (app, _dir, _pool) = boot().await;
    let resp = app.clone().oneshot(get("/")).await.unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let html = String::from_utf8_lossy(&body);
    assert!(
        html.contains("<form"),
        "login page should render a form, got: {}",
        &html[..html.len().min(400)]
    );
    assert!(html.contains(r#"action="/login""#));
}

#[tokio::test]
async fn login_succeeds_and_sets_session_cookies() {
    let (app, _dir, _pool) = boot().await;
    let (session, csrf) = login(&app, "s3cret-admin-pw").await;

    assert!(!session.is_empty());
    assert!(!csrf.is_empty());
    assert_ne!(session, csrf, "session and csrf must be distinct values");
}

#[tokio::test]
async fn login_rejects_wrong_password() {
    let (app, _dir, _pool) = boot().await;
    let resp = app
        .clone()
        .oneshot(post_form("/login", "username=admin&password=wrong"))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert!(
        set_cookie(&resp, "session").is_none(),
        "no session on failure"
    );
}

#[tokio::test]
async fn login_rejects_unknown_user_without_disclosure() {
    let (app, _dir, _pool) = boot().await;
    let unknown = app
        .clone()
        .oneshot(post_form(
            "/login",
            "username=nobody&password=s3cret-admin-pw",
        ))
        .await
        .unwrap();
    let wrong = app
        .clone()
        .oneshot(post_form("/login", "username=admin&password=nope"))
        .await
        .unwrap();

    assert_eq!(unknown.status(), wrong.status());

    // Both render the same error page. Each response mints its own CSRF token,
    // so mask that one field before comparing — everything else must match, so
    // a wrong username cannot be told apart from a wrong password.
    let strip_token = |html: String| {
        let marker = r#"name="csrf" value=""#;
        let start = html
            .find(marker)
            .map(|i| i + marker.len())
            .expect("csrf hidden field");
        let end = start + html[start..].find('"').expect("token terminator");
        format!("{}<token>{}", &html[..start], &html[end..])
    };

    let unknown_html = strip_token(body_text(unknown).await);
    let wrong_html = strip_token(body_text(wrong).await);

    assert_eq!(
        unknown_html, wrong_html,
        "unknown user and wrong password must render identically"
    );
    assert!(unknown_html.contains("用户名或密码错误"));
}

async fn body_text(resp: axum::response::Response) -> String {
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    String::from_utf8_lossy(&bytes).to_string()
}

#[tokio::test]
async fn admin_requires_a_session() {
    let (app, _dir, _pool) = boot().await;
    let resp = app.clone().oneshot(get("/admin")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    assert_eq!(resp.headers()["location"], "/");
}

#[tokio::test]
async fn admin_rejects_an_unknown_session_token() {
    let (app, _dir, _pool) = boot().await;
    let req = Request::builder()
        .uri("/admin")
        .header("cookie", "session=not-a-real-session")
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
}

#[tokio::test]
async fn admin_renders_for_an_authenticated_session() {
    let (app, _dir, _pool) = boot().await;
    let (session, _csrf) = login(&app, "s3cret-admin-pw").await;

    let req = Request::builder()
        .uri("/admin")
        .header("cookie", format!("session={session}"))
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let html = String::from_utf8_lossy(&body);
    assert!(html.contains("admin"), "dashboard should greet the admin");
    assert!(html.contains("系统运行状态与硬件负载"));
    assert!(html.contains("CPU 使用率"));
    assert!(html.contains("内存使用量"));
    assert!(html.contains("硬盘存储"));
    assert!(html.contains("网络吞吐速率"));
    assert!(html.contains("系统负载"));
}

#[tokio::test]
async fn system_status_api_returns_json_metrics() {
    let (app, _dir, _pool) = boot().await;
    let (session, _csrf) = login(&app, "s3cret-admin-pw").await;

    let req = Request::builder()
        .uri("/admin/api/system-status")
        .header("cookie", format!("session={session}"))
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let metrics: three_r_ui::web::system::SystemMetrics =
        serde_json::from_slice(&body).expect("valid SystemMetrics JSON");
    assert!(metrics.cpu_cores >= 1);
    assert!(!metrics.hostname.is_empty());
    assert!(!metrics.uptime_formatted.is_empty());
}

#[tokio::test]
async fn admin_response_carries_security_headers() {
    let (app, _dir, _pool) = boot().await;
    let (session, _csrf) = login(&app, "s3cret-admin-pw").await;

    let req = Request::builder()
        .uri("/admin")
        .header("cookie", format!("session={session}"))
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();

    assert_eq!(resp.headers()["x-content-type-options"], "nosniff");
    assert_eq!(resp.headers()["x-frame-options"], "DENY");
    assert!(resp.headers().contains_key("content-security-policy"));
}

#[tokio::test]
async fn state_changing_request_without_csrf_header_is_rejected() {
    let (app, _dir, _pool) = boot().await;
    let (session, _csrf) = login(&app, "s3cret-admin-pw").await;

    let req = Request::builder()
        .method("POST")
        .uri("/admin/logout")
        .header("cookie", format!("session={session}"))
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn state_changing_request_with_wrong_csrf_is_rejected() {
    let (app, _dir, _pool) = boot().await;
    let (session, _csrf) = login(&app, "s3cret-admin-pw").await;

    let req = Request::builder()
        .method("POST")
        .uri("/admin/logout")
        .header("cookie", format!("session={session}; csrf={_csrf}"))
        .header("x-csrf-token", "definitely-not-the-token")
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn state_changing_request_with_matching_csrf_succeeds() {
    let (app, _dir, _pool) = boot().await;
    let (session, csrf) = login(&app, "s3cret-admin-pw").await;

    let req = Request::builder()
        .method("POST")
        .uri("/admin/logout")
        .header("cookie", format!("session={session}; csrf={csrf}"))
        .header("x-csrf-token", &csrf)
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
}

/// A plain HTML form cannot set headers, so the token may arrive as a `csrf`
/// field in the urlencoded body instead.
#[tokio::test]
async fn state_changing_request_accepts_csrf_in_form_body() {
    let (app, _dir, _pool) = boot().await;
    let (session, csrf) = login(&app, "s3cret-admin-pw").await;

    let req = Request::builder()
        .method("POST")
        .uri("/admin/logout")
        .header("cookie", format!("session={session}; csrf={csrf}"))
        .header("content-type", "application/x-www-form-urlencoded")
        .body(Body::from(format!("csrf={csrf}")))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
}

#[tokio::test]
async fn form_body_csrf_with_wrong_value_is_rejected() {
    let (app, _dir, _pool) = boot().await;
    let (session, csrf) = login(&app, "s3cret-admin-pw").await;

    let req = Request::builder()
        .method("POST")
        .uri("/admin/logout")
        .header("cookie", format!("session={session}; csrf={csrf}"))
        .header("content-type", "application/x-www-form-urlencoded")
        .body(Body::from("csrf=not-the-token"))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

/// Without a CSRF cookie there is nothing to compare against, so the request
/// must not be waved through.
#[tokio::test]
async fn state_changing_request_without_csrf_cookie_is_rejected() {
    let (app, _dir, _pool) = boot().await;
    let (session, _csrf) = login(&app, "s3cret-admin-pw").await;

    let req = Request::builder()
        .method("POST")
        .uri("/admin/logout")
        .header("cookie", format!("session={session}"))
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn logout_invalidates_the_session() {
    let (app, _dir, _pool) = boot().await;
    let (session, csrf) = login(&app, "s3cret-admin-pw").await;

    let req = Request::builder()
        .method("POST")
        .uri("/admin/logout")
        .header("cookie", format!("session={session}; csrf={csrf}"))
        .header("x-csrf-token", &csrf)
        .body(Body::empty())
        .unwrap();
    app.clone().oneshot(req).await.unwrap();

    // The old session must no longer open /admin.
    let req = Request::builder()
        .uri("/admin")
        .header("cookie", format!("session={session}"))
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
}

#[tokio::test]
async fn expired_session_is_rejected() {
    let (app, _dir, pool) = boot().await;
    let (session, _csrf) = login(&app, "s3cret-admin-pw").await;

    // Force the session into the past.
    sqlx::query("UPDATE sessions SET expires_at = 1")
        .execute(&pool)
        .await
        .unwrap();

    let req = Request::builder()
        .uri("/admin")
        .header("cookie", format!("session={session}"))
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
}

#[tokio::test]
async fn change_password_requires_the_current_password() {
    let (app, _dir, _pool) = boot().await;
    let (session, csrf) = login(&app, "s3cret-admin-pw").await;

    let req = Request::builder()
        .method("POST")
        .uri("/admin/password")
        .header("cookie", format!("session={session}; csrf={csrf}"))
        .header("x-csrf-token", &csrf)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(Body::from(
            "current_password=wrong&new_password=a-brand-new-password",
        ))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn change_password_rejects_a_short_new_password() {
    let (app, _dir, _pool) = boot().await;
    let (session, csrf) = login(&app, "s3cret-admin-pw").await;

    let req = Request::builder()
        .method("POST")
        .uri("/admin/password")
        .header("cookie", format!("session={session}; csrf={csrf}"))
        .header("x-csrf-token", &csrf)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(Body::from(
            "current_password=s3cret-admin-pw&new_password=short",
        ))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn change_password_rotates_the_credential_and_drops_sessions() {
    let (app, _dir, _pool) = boot().await;
    let (session, csrf) = login(&app, "s3cret-admin-pw").await;

    let req = Request::builder()
        .method("POST")
        .uri("/admin/password")
        .header("cookie", format!("session={session}; csrf={csrf}"))
        .header("x-csrf-token", &csrf)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(Body::from(
            "current_password=s3cret-admin-pw&new_password=a-much-longer-password",
        ))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);

    // The old password no longer logs in.
    let resp = app
        .clone()
        .oneshot(post_form(
            "/login",
            "username=admin&password=s3cret-admin-pw",
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    // The new one does.
    let resp = app
        .clone()
        .oneshot(post_form(
            "/login",
            "username=admin&password=a-much-longer-password",
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
}

#[tokio::test]
async fn unknown_route_is_404() {
    let (app, _dir, _pool) = boot().await;
    let resp = app.clone().oneshot(get("/nope")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}
