use axum::body::Body;
use axum::http::{Request, StatusCode};
use base64::Engine;
use http_body_util::BodyExt;
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
    config.public_host = "test.example.com".into();

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

    let data_dir = config.data_dir.clone();
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
        data_dir,
    }
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
async fn multi_protocol_inbound_and_client_binding() {
    let h = boot().await;
    let (session, csrf) = login(&h).await;

    // 1. Create Inbound 1: VLESS (listen_port=443, dest=example.com, sni=example.com, short_id=12345678)
    let resp = post(
        &h,
        "/admin/inbound",
        &session,
        &csrf,
        "action=new&protocol=vless&listen_port=443&dest=example.com&sni=example.com&short_id=12345678&xhttp_path=/r/vless1",
    )
    .await;
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);

    // 2. Create Inbound 2: Hysteria 2 (listen_port=8443, sni=hy2.example.com, cert_mode=auto)
    let resp = post(
        &h,
        "/admin/inbound",
        &session,
        &csrf,
        "action=new&protocol=hysteria2&listen_port=8443&sni=hy2.example.com&cert_mode=auto&masquerade_url=https://news.ycombinator.com/",
    )
    .await;
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);

    // 3. Verify inbounds in DB
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM inbounds WHERE status = 'active'")
        .fetch_one(&h.pool)
        .await
        .unwrap();
    assert_eq!(count, 2);

    let vless_id: i64 = sqlx::query_scalar("SELECT id FROM inbounds WHERE protocol = 'vless'")
        .fetch_one(&h.pool)
        .await
        .unwrap();
    let hy2_id: i64 = sqlx::query_scalar("SELECT id FROM inbounds WHERE protocol = 'hysteria2'")
        .fetch_one(&h.pool)
        .await
        .unwrap();

    // 4. Create Client bound only to Hysteria 2
    let resp = post(
        &h,
        "/admin/clients",
        &session,
        &csrf,
        &format!("name=hy2user&quota_gb=50&inbound_ids={hy2_id}"),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);

    let client_id: i64 = sqlx::query_scalar("SELECT id FROM clients WHERE name = 'hy2user'")
        .fetch_one(&h.pool)
        .await
        .unwrap();
    let sub_token: String = sqlx::query_scalar("SELECT sub_token FROM clients WHERE id = ?")
        .bind(client_id)
        .fetch_one(&h.pool)
        .await
        .unwrap();

    // 5. Verify rendered Xray config
    let cfg = rendered_config(&h);
    let inbounds = cfg["inbounds"].as_array().unwrap();
    let vless_ib = inbounds.iter().find(|i| i["protocol"] == "vless").unwrap();
    let hy2_ib = inbounds
        .iter()
        .find(|i| i["protocol"] == "hysteria")
        .unwrap();

    assert_eq!(vless_ib["port"], 443);
    assert_eq!(hy2_ib["port"], 8443);
    assert_eq!(hy2_ib["settings"]["version"], 2);

    // Hy2 user list should contain hy2user UUID, but VLESS should be empty because client only bound to hy2
    assert_eq!(vless_ib["settings"]["clients"].as_array().unwrap().len(), 0);
    assert_eq!(hy2_ib["settings"]["users"].as_array().unwrap().len(), 1);

    // 6. Test Subscription endpoint: should only output Hysteria 2 URI
    let req = Request::builder()
        .uri(format!("/sub/{sub_token}"))
        .body(Body::empty())
        .unwrap();
    let sub_resp = h.app.clone().oneshot(req).await.unwrap();
    assert_eq!(sub_resp.status(), StatusCode::OK);
    let raw_b64 = body_text(sub_resp).await;
    let decoded = String::from_utf8(
        base64::engine::general_purpose::STANDARD
            .decode(raw_b64.trim())
            .unwrap(),
    )
    .unwrap();

    assert!(decoded.contains("hysteria2://"));
    assert!(!decoded.contains("vless://"));

    // 7. Update Client to bind to BOTH inbounds
    let resp = post(
        &h,
        &format!("/admin/clients/{client_id}"),
        &session,
        &csrf,
        &format!("name=hy2user&quota_gb=50&inbound_ids={vless_id},{hy2_id}"),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);

    // Re-check subscription: now should contain BOTH
    let req = Request::builder()
        .uri(format!("/sub/{sub_token}"))
        .body(Body::empty())
        .unwrap();
    let sub_resp = h.app.clone().oneshot(req).await.unwrap();
    let raw_b64 = body_text(sub_resp).await;
    let decoded = String::from_utf8(
        base64::engine::general_purpose::STANDARD
            .decode(raw_b64.trim())
            .unwrap(),
    )
    .unwrap();

    assert!(decoded.contains("hysteria2://"));
    assert!(decoded.contains("vless://"));

    // 8. View Detail Page
    let detail_resp = get(&h, &format!("/admin/clients/{client_id}"), &session).await;
    assert_eq!(detail_resp.status(), StatusCode::OK);
    let detail_html = body_text(detail_resp).await;
    assert!(detail_html.contains("聚合订阅"));
    assert!(detail_html.contains(&format!("/sub/{sub_token}")));
    assert!(detail_html.contains("Hysteria 2"));
    assert!(detail_html.contains("VLESS"));
}
