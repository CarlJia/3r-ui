use axum::body::Body;
use axum::http::{Request, StatusCode};
use base64::Engine;
use http_body_util::BodyExt;
use serde_json::Value;
use tempfile::tempdir;
use three_r_ui::config::AppConfig;
use three_r_ui::mieru::config::render_to_file;
use three_r_ui::web::{self, AppState};
use tower::ServiceExt;

struct Harness {
    app: axum::Router,
    dir: tempfile::TempDir,
    pool: sqlx::SqlitePool,
}

async fn boot() -> Harness {
    let dir = tempdir().unwrap();
    let mut config = AppConfig::from_env().unwrap();
    config.data_dir = dir.path().to_path_buf();
    config.public_host = "sub.example.com".into();

    let pool = three_r_ui::init_storage(&config).await.unwrap();

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
        dir,
        pool,
    }
}

#[tokio::test]
async fn test_mieru_config_rendering_and_subscription() {
    let h = boot().await;

    // 1. Insert Mieru inbound with port range
    sqlx::query(
        "INSERT INTO inbounds (id, listen_port, dest, sni, server_name, private_key, \
                public_key, short_id, xhttp_path, protocol, cert_mode, port_range, \
                transport_protocol, status, created_at, updated_at) \
         VALUES (1, 2012, '', '', NULL, '', '', '', \
                NULL, 'mieru', 'none', '2012-2022', 'TCP,UDP', 'active', unixepoch(), unixepoch())",
    )
    .execute(&h.pool)
    .await
    .expect("insert mieru inbound");

    // 2. Insert client with Mieru credentials
    sqlx::query(
        "INSERT INTO clients (id, inbound_id, uuid, name, quota_bytes, expiry_at, \
                status, sub_token, mieru_username, mieru_password, created_at) \
         VALUES (1, 1, '11111111-2222-3333-4444-555555555555', 'alice', 10737418240, NULL, \
                'active', 'subtoken-mieru-123', 'alice', 'pass123456', unixepoch())",
    )
    .execute(&h.pool)
    .await
    .expect("insert mieru client");

    sqlx::query("INSERT INTO client_inbounds (client_id, inbound_id) VALUES (1, 1)")
        .execute(&h.pool)
        .await
        .expect("insert client inbound relation");

    // 3. Test config rendering
    let mieru_config_path = h.dir.path().join("mieru.json");
    let has_active = render_to_file(&h.pool, &mieru_config_path)
        .await
        .expect("render config");
    assert!(has_active);

    let content = std::fs::read_to_string(&mieru_config_path).expect("read mieru.json");
    let json: Value = serde_json::from_str(&content).expect("parse mieru.json");

    assert_eq!(json["portBindings"].as_array().unwrap().len(), 2);
    assert_eq!(json["portBindings"][0]["portRange"], "2012-2022");
    assert_eq!(json["portBindings"][0]["protocol"], "TCP");
    assert_eq!(json["portBindings"][1]["portRange"], "2012-2022");
    assert_eq!(json["portBindings"][1]["protocol"], "UDP");
    assert_eq!(json["users"][0]["name"], "alice");
    assert_eq!(json["users"][0]["password"], "pass123456");

    // 4. Test subscription endpoint
    let req = Request::builder()
        .uri("/sub/subtoken-mieru-123")
        .body(Body::empty())
        .unwrap();

    let res = h.app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    let body = String::from_utf8(bytes.to_vec()).unwrap();

    let decoded = base64::engine::general_purpose::STANDARD
        .decode(body.trim())
        .unwrap();
    let decoded_str = String::from_utf8(decoded).unwrap();

    assert!(decoded_str.starts_with("mierus://"));
    assert!(decoded_str.contains("alice:pass123456@sub.example.com"));
    assert!(decoded_str.contains("port=2012-2022"));
    assert!(decoded_str.contains("protocol=TCP%2CUDP") || decoded_str.contains("protocol=TCP,UDP"));
    assert!(decoded_str.contains("mtu=1400"));
}
