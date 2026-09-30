//! Integration tests for U6: client CRUD, quota / expiry semantics, and the
//! resulting xray config (clients whose status is not `'active'` must fall
//! out of the rendered JSON).

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use std::os::unix::fs::PermissionsExt;
use tempfile::tempdir;
use three_r_ui::config::AppConfig;
use three_r_ui::crypto::password;
use three_r_ui::web::{self, AppState};
use tower::ServiceExt;

const ADMIN_PW: &str = "s3cret-admin-pw";

struct Harness {
    app: axum::Router,
    _dir: tempfile::TempDir,
    pool: sqlx::SqlitePool,
    data_dir: std::path::PathBuf,
}

async fn boot() -> Harness {
    let dir = tempdir().unwrap();
    let mut config = AppConfig::from_env().unwrap();
    config.data_dir = dir.path().to_path_buf();

    let pool = three_r_ui::init_storage(&config).await.unwrap();
    sqlx::query(
        "INSERT INTO admins (username, password_hash, created_at) VALUES (?, ?, unixepoch())",
    )
    .bind("admin")
    .bind(password::hash(ADMIN_PW).unwrap())
    .execute(&pool)
    .await
    .unwrap();

    // The supervisor channel drains so Reload sends never block the handler.
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    tokio::spawn(async move { while rx.recv().await.is_some() {} });

    let data_dir = config.data_dir.clone();
    let state = AppState {
        config,
        pool: pool.clone(),
        supervisor_tx: tx,
    };
    Harness {
        app: web::router(state),
        _dir: dir,
        pool,
        data_dir,
    }
}

async fn boot_with_inbound() -> Harness {
    let h = boot().await;
    sqlx::query(
        "INSERT INTO inbounds (listen_port, dest, sni, server_name, private_key, \
                public_key, short_id, status, created_at, updated_at) \
         VALUES (8443, 'a.example', 'a.example', NULL, 'priv', 'pub', 'ab', 'active', \
                unixepoch(), unixepoch())",
    )
    .execute(&h.pool)
    .await
    .unwrap();
    h
}

fn set_cookie(resp: &axum::response::Response, name: &str) -> Option<String> {
    let prefix = format!("{name}=");
    resp.headers()
        .get_all("set-cookie")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find_map(|v| {
            let after = v.strip_prefix(&prefix)?;
            Some(after.split(';').next().unwrap_or("").to_string())
        })
}

async fn login(h: &Harness) -> (String, String) {
    let req = Request::builder()
        .method("POST")
        .uri("/login")
        .header("content-type", "application/x-www-form-urlencoded")
        .body(Body::from(format!("username=admin&password={ADMIN_PW}")))
        .unwrap();
    let resp = h.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    (
        set_cookie(&resp, "session").expect("session"),
        set_cookie(&resp, "csrf").expect("csrf"),
    )
}

async fn get(h: &Harness, uri: &str, session: &str) -> axum::response::Response {
    let req = Request::builder()
        .uri(uri)
        .header("cookie", format!("session={session}"))
        .body(Body::empty())
        .unwrap();
    h.app.clone().oneshot(req).await.unwrap()
}

async fn post(
    h: &Harness,
    uri: &str,
    session: &str,
    csrf: &str,
    body: &str,
) -> axum::response::Response {
    let req = Request::builder()
        .method("POST")
        .uri(uri)
        .header("cookie", format!("session={session}; csrf={csrf}"))
        .header("x-csrf-token", csrf)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(Body::from(body.to_string()))
        .unwrap();
    h.app.clone().oneshot(req).await.unwrap()
}

async fn body_text(resp: axum::response::Response) -> String {
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    String::from_utf8_lossy(&bytes).to_string()
}

fn rendered_config(h: &Harness) -> serde_json::Value {
    let path = h.data_dir.join("xray").join("config.json");
    let raw = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("config.json should exist at {path:?}: {e}"));
    serde_json::from_str(&raw).expect("config.json should be valid JSON")
}

#[tokio::test]
async fn clients_list_requires_a_session() {
    let h = boot().await;
    let req = Request::builder()
        .uri("/admin/clients")
        .body(Body::empty())
        .unwrap();
    let resp = h.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
}

#[tokio::test]
async fn empty_list_shows_an_empty_state() {
    let h = boot_with_inbound().await;
    let (session, _csrf) = login(&h).await;
    let resp = get(&h, "/admin/clients", &session).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let html = body_text(resp).await;
    assert!(html.contains("添加用户"));
    assert!(html.contains("还没有用户"));
}

#[tokio::test]
async fn list_without_inbound_hides_the_add_button() {
    let h = boot().await;
    let (session, _csrf) = login(&h).await;
    let resp = get(&h, "/admin/clients", &session).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let html = body_text(resp).await;
    assert!(html.contains("尚未创建入站"));
    assert!(!html.contains(r#"href="/admin/clients/new""#));
}

#[tokio::test]
async fn create_client_mints_a_uuid_and_persists() {
    let h = boot_with_inbound().await;
    let (session, csrf) = login(&h).await;

    let resp = post(
        &h,
        "/admin/clients",
        &session,
        &csrf,
        "name=alice&quota_gb=100&expiry=2030-01-15",
    )
    .await;
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);

    let row: (String, Option<String>, i64, Option<i64>, String) =
        sqlx::query_as("SELECT uuid, name, quota_bytes, expiry_at, status FROM clients")
            .fetch_one(&h.pool)
            .await
            .unwrap();

    assert_eq!(row.0.len(), 36, "uuid must be canonical 8-4-4-4-12");
    assert_eq!(row.1.as_deref(), Some("alice"));
    assert_eq!(row.2, 100 * 1024 * 1024 * 1024);
    assert!(row.3.is_some());
    assert_eq!(row.4, "active");
}

#[tokio::test]
async fn create_without_inbound_is_rejected() {
    let h = boot().await;
    let (session, csrf) = login(&h).await;
    let resp = post(
        &h,
        "/admin/clients",
        &session,
        &csrf,
        "name=alice&quota_gb=100",
    )
    .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert!(body_text(resp).await.contains("入站"));

    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM clients")
        .fetch_one(&h.pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
}

#[tokio::test]
async fn invalid_quota_is_rejected() {
    let h = boot_with_inbound().await;
    let (session, csrf) = login(&h).await;

    for bad in ["-5", "not-a-number", "99999999"] {
        let resp = post(
            &h,
            "/admin/clients",
            &session,
            &csrf,
            &format!("name=alice&quota_gb={bad}"),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "quota {bad:?}");
        let html = body_text(resp).await;
        assert!(
            html.contains("配额") || html.contains("整数"),
            "expected quota error for {bad:?}, got: {html}"
        );
    }
}

#[tokio::test]
async fn invalid_expiry_is_rejected() {
    let h = boot_with_inbound().await;
    let (session, csrf) = login(&h).await;
    let resp = post(
        &h,
        "/admin/clients",
        &session,
        &csrf,
        "name=alice&quota_gb=10&expiry=not-a-date",
    )
    .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert!(body_text(resp).await.contains("过期"));
}

#[tokio::test]
async fn blank_fields_are_treated_as_no_quota_and_no_expiry() {
    let h = boot_with_inbound().await;
    let (session, csrf) = login(&h).await;
    let resp = post(
        &h,
        "/admin/clients",
        &session,
        &csrf,
        "name=&quota_gb=&expiry=",
    )
    .await;
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);

    let row: (Option<String>, i64, Option<i64>) =
        sqlx::query_as("SELECT name, quota_bytes, expiry_at FROM clients")
            .fetch_one(&h.pool)
            .await
            .unwrap();
    assert_eq!(row.0, None);
    assert_eq!(row.1, 0);
    assert_eq!(row.2, None);
}

#[tokio::test]
async fn active_client_appears_in_rendered_xray_config() {
    let h = boot_with_inbound().await;
    let (session, csrf) = login(&h).await;
    post(
        &h,
        "/admin/clients",
        &session,
        &csrf,
        "name=alice&quota_gb=10",
    )
    .await;

    let cfg = rendered_config(&h);
    let clients = cfg["inbounds"][0]["settings"]["clients"]
        .as_array()
        .expect("clients array");
    assert_eq!(clients.len(), 1, "active client must be in xray config");
    assert_eq!(
        clients[0]["email"], clients[0]["id"],
        "email must equal uuid"
    );
}

#[tokio::test]
async fn deleting_a_client_removes_it_from_the_rendered_config() {
    let h = boot_with_inbound().await;
    let (session, csrf) = login(&h).await;
    post(
        &h,
        "/admin/clients",
        &session,
        &csrf,
        "name=alice&quota_gb=10",
    )
    .await;
    let id: i64 = sqlx::query_scalar("SELECT id FROM clients")
        .fetch_one(&h.pool)
        .await
        .unwrap();
    assert_eq!(
        rendered_config(&h)["inbounds"][0]["settings"]["clients"]
            .as_array()
            .unwrap()
            .len(),
        1
    );

    let resp = post(
        &h,
        &format!("/admin/clients/{id}/delete"),
        &session,
        &csrf,
        "",
    )
    .await;
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);

    let cfg = rendered_config(&h);
    assert_eq!(
        cfg["inbounds"][0]["settings"]["clients"]
            .as_array()
            .unwrap()
            .len(),
        0,
        "deleted client must fall out of the rendered config"
    );

    let status: String = sqlx::query_scalar("SELECT status FROM clients WHERE id = ?")
        .bind(id)
        .fetch_one(&h.pool)
        .await
        .unwrap();
    assert_eq!(status, "deleted", "soft delete, not row removal");
}

#[tokio::test]
async fn update_rotates_fields_but_preserves_uuid() {
    let h = boot_with_inbound().await;
    let (session, csrf) = login(&h).await;
    post(
        &h,
        "/admin/clients",
        &session,
        &csrf,
        "name=alice&quota_gb=10&expiry=2030-01-15",
    )
    .await;
    let (id, original_uuid): (i64, String) = sqlx::query_as("SELECT id, uuid FROM clients")
        .fetch_one(&h.pool)
        .await
        .unwrap();

    let resp = post(
        &h,
        &format!("/admin/clients/{id}"),
        &session,
        &csrf,
        "name=alice2&quota_gb=200&expiry=2031-06-01",
    )
    .await;
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);

    let row: (String, Option<String>, i64, Option<i64>) =
        sqlx::query_as("SELECT uuid, name, quota_bytes, expiry_at FROM clients WHERE id = ?")
            .bind(id)
            .fetch_one(&h.pool)
            .await
            .unwrap();
    assert_eq!(row.0, original_uuid, "UUID must never rotate");
    assert_eq!(row.1.as_deref(), Some("alice2"));
    assert_eq!(row.2, 200 * 1024 * 1024 * 1024);
    assert!(row.3.is_some());
}

#[tokio::test]
async fn expired_client_is_auto_disabled_when_rendering() {
    let h = boot_with_inbound().await;
    let (session, csrf) = login(&h).await;

    post(
        &h,
        "/admin/clients",
        &session,
        &csrf,
        "name=alice&quota_gb=10",
    )
    .await;
    let id: i64 = sqlx::query_scalar("SELECT id FROM clients")
        .fetch_one(&h.pool)
        .await
        .unwrap();

    // Backdate expiry to one second in the past; the next list / mutation
    // should sweep it.
    sqlx::query("UPDATE clients SET expiry_at = ? WHERE id = ?")
        .bind(three_r_ui::crypto::session::now_unix() - 1)
        .bind(id)
        .execute(&h.pool)
        .await
        .unwrap();

    let resp = get(&h, "/admin/clients", &session).await;
    assert_eq!(resp.status(), StatusCode::OK);

    let status: String = sqlx::query_scalar("SELECT status FROM clients WHERE id = ?")
        .bind(id)
        .fetch_one(&h.pool)
        .await
        .unwrap();
    assert_eq!(
        status, "disabled",
        "list handler must sweep expired clients on entry"
    );

    let html = body_text(resp).await;
    assert!(html.contains("disabled"));
}

#[tokio::test]
async fn mutation_after_expiry_sweeps_the_client_from_xray_config() {
    let h = boot_with_inbound().await;
    let (session, csrf) = login(&h).await;
    post(
        &h,
        "/admin/clients",
        &session,
        &csrf,
        "name=alice&quota_gb=10&expiry=2030-01-01",
    )
    .await;
    let id: i64 = sqlx::query_scalar("SELECT id FROM clients")
        .fetch_one(&h.pool)
        .await
        .unwrap();

    // Backdate so the next mutation triggers the sweep.
    sqlx::query("UPDATE clients SET expiry_at = ? WHERE id = ?")
        .bind(three_r_ui::crypto::session::now_unix() - 1)
        .bind(id)
        .execute(&h.pool)
        .await
        .unwrap();

    // A second client mutation forces a render() — the sweep runs in reload().
    post(
        &h,
        "/admin/clients",
        &session,
        &csrf,
        "name=bob&quota_gb=20",
    )
    .await;

    let cfg = rendered_config(&h);
    let client_count = cfg["inbounds"][0]["settings"]["clients"]
        .as_array()
        .unwrap()
        .len();
    assert_eq!(
        client_count, 1,
        "expired client must fall out of xray config after the next mutation"
    );
}

#[tokio::test]
async fn update_with_past_expiry_disables_the_client_immediately() {
    let h = boot_with_inbound().await;
    let (session, csrf) = login(&h).await;
    post(
        &h,
        "/admin/clients",
        &session,
        &csrf,
        "name=alice&quota_gb=10",
    )
    .await;
    let id: i64 = sqlx::query_scalar("SELECT id FROM clients")
        .fetch_one(&h.pool)
        .await
        .unwrap();

    let resp = post(
        &h,
        &format!("/admin/clients/{id}"),
        &session,
        &csrf,
        "name=alice&quota_gb=10&expiry=2020-01-01",
    )
    .await;
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);

    let status: String = sqlx::query_scalar("SELECT status FROM clients WHERE id = ?")
        .bind(id)
        .fetch_one(&h.pool)
        .await
        .unwrap();
    assert_eq!(status, "disabled");
}

#[tokio::test]
async fn edit_form_renders_with_existing_values() {
    let h = boot_with_inbound().await;
    let (session, csrf) = login(&h).await;
    post(
        &h,
        "/admin/clients",
        &session,
        &csrf,
        "name=alice&quota_gb=42&expiry=2030-01-15",
    )
    .await;
    let id: i64 = sqlx::query_scalar("SELECT id FROM clients")
        .fetch_one(&h.pool)
        .await
        .unwrap();

    let resp = get(&h, &format!("/admin/clients/{id}/edit"), &session).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let html = body_text(resp).await;
    assert!(html.contains("alice"));
    assert!(html.contains("42"));
    assert!(html.contains("2030-01-15"));
    assert!(html.contains("UUID"));
}

#[tokio::test]
async fn edit_form_for_unknown_id_returns_404() {
    let h = boot().await;
    let (session, _csrf) = login(&h).await;
    let resp = get(&h, "/admin/clients/999/edit", &session).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn new_form_hints_when_no_inbound() {
    let h = boot().await;
    let (session, _csrf) = login(&h).await;
    let resp = get(&h, "/admin/clients/new", &session).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let html = body_text(resp).await;
    assert!(html.contains("入站"));
}

#[tokio::test]
async fn name_too_long_is_rejected() {
    let h = boot_with_inbound().await;
    let (session, csrf) = login(&h).await;
    let long = "a".repeat(65);
    let resp = post(
        &h,
        "/admin/clients",
        &session,
        &csrf,
        &format!("name={long}&quota_gb=10"),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert!(body_text(resp).await.contains("64"));
}

#[tokio::test]
async fn csrf_is_required_for_create() {
    let h = boot_with_inbound().await;
    let (session, _csrf) = login(&h).await;
    let req = Request::builder()
        .method("POST")
        .uri("/admin/clients")
        .header("cookie", format!("session={session}; csrf={_csrf}"))
        .header("content-type", "application/x-www-form-urlencoded")
        .body(Body::from("name=alice&quota_gb=10"))
        .unwrap();
    let resp = h.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM clients")
        .fetch_one(&h.pool)
        .await
        .unwrap();
    assert_eq!(count, 0, "CSRF failure must not insert");
}

#[tokio::test]
async fn config_json_remains_owner_only_after_client_changes() {
    let h = boot_with_inbound().await;
    let (session, csrf) = login(&h).await;
    post(
        &h,
        "/admin/clients",
        &session,
        &csrf,
        "name=alice&quota_gb=10",
    )
    .await;
    let path = h.data_dir.join("xray").join("config.json");
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "REALITY private key lives in this file");
}
