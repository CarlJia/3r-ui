//! Integration tests for U8: VLESS share URI + QR PNG endpoint.
//!
//! Drives the full router and asserts on the rendered HTML / PNG bytes. We
//! don't decode the QR — we only confirm the endpoint streams a valid PNG
//! (magic bytes + sane header) and that the URI it embeds has the right
//! shape.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tempfile::tempdir;
use three_r_ui::config::AppConfig;
use three_r_ui::crypto::password;
use three_r_ui::web::{self, AppState};
use tower::ServiceExt;

const ADMIN_PW: &str = "s3cret-admin-pw";
const PNG_MAGIC: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];

struct Harness {
    app: axum::Router,
    _dir: tempfile::TempDir,
    pool: sqlx::SqlitePool,
}

async fn boot() -> Harness {
    let dir = tempdir().unwrap();
    let mut config = AppConfig::from_env().unwrap();
    config.data_dir = dir.path().to_path_buf();
    config.public_host = "panel.example.com".into();

    let pool = three_r_ui::init_storage(&config).await.unwrap();
    sqlx::query(
        "INSERT INTO admins (username, password_hash, created_at) VALUES (?, ?, unixepoch())",
    )
    .bind("admin")
    .bind(password::hash(ADMIN_PW).unwrap())
    .execute(&pool)
    .await
    .unwrap();

    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let (proxy_tx, mut proxy_rx) = tokio::sync::mpsc::channel(8);
    tokio::spawn(async move { while proxy_rx.recv().await.is_some() {} });
    let (mieru_tx, mut mieru_rx) = tokio::sync::mpsc::channel(8);
    tokio::spawn(async move { while mieru_rx.recv().await.is_some() {} });

    let state = AppState {
        config,
        pool: pool.clone(),
        supervisor_tx: tx,
        proxy_tx,
        mieru_tx,
        update_jobs: std::sync::Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
        installer: std::sync::Arc::new(
            three_r_ui::xray::Installer::new("https://github.com/3r-ui/xray/releases")
                .expect("installer must be constructible"),
        ),
        cert_tracker: std::sync::Arc::new(three_r_ui::proxy::CertProgressTracker::new()),
        system_tracker: std::sync::Arc::new(three_r_ui::web::system::SystemTracker::new()),
        shutdown_initiated: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
    };
    Harness {
        app: web::router(state),
        _dir: dir,
        pool,
    }
}

async fn boot_with_inbound_and_client() -> (Harness, i64) {
    let h = boot().await;
    sqlx::query(
        "INSERT INTO inbounds (listen_port, dest, sni, server_name, private_key, \
                public_key, short_id, xhttp_path, status, created_at, updated_at) \
         VALUES (443, 'www.apple.com', 'www.apple.com', NULL, 'PRIV', 'PUBKEY', 'abcd', \
                '/r/test0123456789ab', 'active', unixepoch(), unixepoch())",
    )
    .execute(&h.pool)
    .await
    .unwrap();
    let inbound_id: i64 = sqlx::query_scalar("SELECT id FROM inbounds")
        .fetch_one(&h.pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO clients (inbound_id, uuid, name, quota_bytes, expiry_at, status, created_at) \
         VALUES (?, '7c90d0c0-1234-4abc-9def-111122223333', 'alice', 0, NULL, 'active', unixepoch())",
    )
    .bind(inbound_id)
    .execute(&h.pool)
    .await
    .unwrap();
    let id: i64 = sqlx::query_scalar("SELECT id FROM clients")
        .fetch_one(&h.pool)
        .await
        .unwrap();
    (h, id)
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

async fn body_bytes(resp: axum::response::Response) -> Vec<u8> {
    resp.into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec()
}

async fn body_text(resp: axum::response::Response) -> String {
    let bytes = body_bytes(resp).await;
    String::from_utf8_lossy(&bytes).to_string()
}

#[tokio::test]
async fn detail_page_renders_the_vless_uri() {
    let (h, id) = boot_with_inbound_and_client().await;
    let (session, _csrf) = login(&h).await;
    let resp = get(&h, &format!("/admin/clients/{id}"), &session).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let html = body_text(resp).await;
    assert!(html.contains("alice"));
    assert!(html.contains("7c90d0c0-1234-4abc-9def-111122223333"));
    assert!(html.contains("vless://"));
    assert!(html.contains("@panel.example.com:443?"));
    for needle in [
        "type=xhttp",
        "security=reality",
        "pbk=PUBKEY",
        "sni=www.apple.com",
        "sid=abcd",
        "fp=chrome",
        "#alice",
    ] {
        assert!(html.contains(needle), "expected {needle} in detail HTML");
    }
}

#[tokio::test]
async fn detail_page_warns_when_public_host_is_unset() {
    let (h, id) = boot_with_inbound_and_client().await;
    // Override the harness's config to blank out public_host.
    // Simpler: spin up a second harness with empty PUBLIC_HOST.
    drop(h);
    let dir = tempdir().unwrap();
    let mut config = AppConfig::from_env().unwrap();
    config.data_dir = dir.path().to_path_buf();
    config.public_host = String::new();
    let pool = three_r_ui::init_storage(&config).await.unwrap();
    sqlx::query(
        "INSERT INTO admins (username, password_hash, created_at) VALUES (?, ?, unixepoch())",
    )
    .bind("admin")
    .bind(password::hash(ADMIN_PW).unwrap())
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO inbounds (listen_port, dest, sni, server_name, private_key, \
                public_key, short_id, xhttp_path, status, created_at, updated_at) \
         VALUES (443, 'a.example', 'a.example', NULL, 'P', 'PUBKEY', 'ab', \
                '/r/test0123456789ab', 'active', unixepoch(), unixepoch())",
    )
    .execute(&pool)
    .await
    .unwrap();
    let inbound_id: i64 = sqlx::query_scalar("SELECT id FROM inbounds")
        .fetch_one(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO clients (inbound_id, uuid, name, quota_bytes, expiry_at, status, created_at) \
         VALUES (?, 'abc', 'bob', 0, NULL, 'active', unixepoch())",
    )
    .bind(inbound_id)
    .execute(&pool)
    .await
    .unwrap();
    let new_id: i64 = sqlx::query_scalar("SELECT id FROM clients")
        .fetch_one(&pool)
        .await
        .unwrap();

    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let (proxy_tx, mut proxy_rx) = tokio::sync::mpsc::channel(8);
    tokio::spawn(async move { while proxy_rx.recv().await.is_some() {} });
    let (mieru_tx, mut mieru_rx) = tokio::sync::mpsc::channel(8);
    tokio::spawn(async move { while mieru_rx.recv().await.is_some() {} });
    let state = AppState {
        config,
        pool: pool.clone(),
        supervisor_tx: tx,
        proxy_tx,
        mieru_tx,
        update_jobs: std::sync::Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
        installer: std::sync::Arc::new(
            three_r_ui::xray::Installer::new("https://github.com/3r-ui/xray/releases")
                .expect("installer must be constructible"),
        ),
        cert_tracker: std::sync::Arc::new(three_r_ui::proxy::CertProgressTracker::new()),
        system_tracker: std::sync::Arc::new(three_r_ui::web::system::SystemTracker::new()),
        shutdown_initiated: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
    };
    let app = web::router(state);
    let h = Harness {
        app,
        _dir: dir,
        pool,
    };

    let (session, _csrf) = login(&h).await;
    let resp = get(&h, &format!("/admin/clients/{new_id}"), &session).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let html = body_text(resp).await;
    assert!(html.contains("PUBLIC_HOST"));
    assert!(html.contains("your-server.example.com"));
    let _ = id;
}

#[tokio::test]
async fn qr_png_endpoint_streams_a_real_png() {
    let (h, id) = boot_with_inbound_and_client().await;
    let (session, _csrf) = login(&h).await;
    let resp = get(&h, &format!("/admin/clients/{id}/qr.png"), &session).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok()),
        Some("image/png")
    );
    let bytes = body_bytes(resp).await;
    assert!(bytes.len() > 16);
    assert_eq!(&bytes[..8], &PNG_MAGIC, "PNG magic bytes");
    // IHDR chunk reports width / height as big-endian u32 starting at byte 16.
    let w = u32::from_be_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]);
    let h_pix = u32::from_be_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]);
    assert!(w >= 200 && h_pix >= 200, "min_dimensions respected");
}

#[tokio::test]
async fn qr_png_query_param_overrides_public_host() {
    let (h, id) = boot_with_inbound_and_client().await;
    let (session, _csrf) = login(&h).await;
    let resp = get(
        &h,
        &format!("/admin/clients/{id}/qr.png?host=custom.example.org"),
        &session,
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    // The image is opaque PNG, but the response was successful — the actual
    // host override is exercised by `detail_page_renders_the_vless_uri` +
    // the vless.rs unit tests. Here we only assert the endpoint accepts the
    // override parameter without erroring out.
    let bytes = body_bytes(resp).await;
    assert_eq!(&bytes[..8], &PNG_MAGIC);
}

#[tokio::test]
async fn detail_page_404_for_unknown_client() {
    let (h, _id) = boot_with_inbound_and_client().await;
    let (session, _csrf) = login(&h).await;
    let resp = get(&h, "/admin/clients/9999", &session).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn detail_page_404_without_inbound() {
    let h = boot().await;
    sqlx::query(
        "INSERT INTO clients (inbound_id, uuid, name, quota_bytes, expiry_at, status, created_at) \
         VALUES (NULL, 'orphan-uuid', 'ghost', 0, NULL, 'active', unixepoch())",
    )
    .execute(&h.pool)
    .await
    .unwrap();
    let (session, _csrf) = login(&h).await;
    let resp = get(&h, "/admin/clients/1", &session).await;
    // No inbound → we render a friendly message, not a 404. Just verify
    // 200 with a hint pointing at the inbound page.
    assert_eq!(resp.status(), StatusCode::OK);
    let html = body_text(resp).await;
    assert!(html.contains("入站"));
}

#[tokio::test]
async fn detail_and_qr_require_a_session() {
    let (h, id) = boot_with_inbound_and_client().await;
    let req = Request::builder()
        .uri(format!("/admin/clients/{id}"))
        .body(Body::empty())
        .unwrap();
    let resp = h.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);

    let req = Request::builder()
        .uri(format!("/admin/clients/{id}/qr.png"))
        .body(Body::empty())
        .unwrap();
    let resp = h.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
}

#[tokio::test]
async fn hy2_share_uri_generation() {
    let client = three_r_ui::db::models::Client {
        id: 1,
        inbound_id: 1,
        uuid: "7c90d0c0-1234-4abc-9def-111122223333".into(),
        name: Some("bob-node".into()),
        quota_bytes: 0,
        expiry_at: None,
        status: "active".into(),
        sub_token: Some("abc".into()),
        mieru_username: Some("bob".into()),
        mieru_password: Some("pwd123".into()),
        created_at: None,
    };
    let inbound = three_r_ui::db::models::Inbound {
        id: 1,
        listen_port: 8443,
        dest: "".into(),
        sni: "hy2.example.com".into(),
        server_name: None,
        private_key: "".into(),
        public_key: "".into(),
        short_id: "".into(),
        xhttp_path: None,
        protocol: "hysteria2".into(),
        cert_mode: "auto".into(),
        custom_cert_path: "".into(),
        custom_key_path: "".into(),
        masquerade_url: "".into(),
        port_range: "".into(),
        transport_protocol: "TCP".into(),
        status: "active".into(),
        created_at: None,
        updated_at: None,
    };

    let uri = three_r_ui::share::hy2::build_uri(&client, &inbound, "panel.example.com");
    assert!(uri
        .starts_with("hysteria2://7c90d0c0-1234-4abc-9def-111122223333@panel.example.com:8443/?"));
    assert!(uri.contains("sni=hy2.example.com"));
    assert!(uri.contains("insecure=0"));
    assert!(uri.ends_with("#bob-node"));
}
