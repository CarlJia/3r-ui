use axum::body::Body;
use axum::http::{Request, StatusCode};
use base64::Engine;
use http_body_util::BodyExt;
use tempfile::tempdir;
use three_r_ui::config::AppConfig;
use three_r_ui::web::{self, AppState};
use tower::ServiceExt;

struct Harness {
    app: axum::Router,
    _dir: tempfile::TempDir,
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
        _dir: dir,
        pool,
    }
}

async fn body_text(resp: axum::response::Response) -> String {
    let bytes = resp
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec();
    String::from_utf8_lossy(&bytes).to_string()
}

#[tokio::test]
async fn sub_endpoint_serves_base64_nodes_and_userinfo() {
    let h = boot().await;

    // Insert 2 inbounds: 1 VLESS, 1 Hysteria2
    sqlx::query(
        "INSERT INTO inbounds (id, listen_port, dest, sni, server_name, private_key, \
                public_key, short_id, xhttp_path, protocol, cert_mode, status, created_at, updated_at) \
         VALUES (1, 443, 'www.apple.com', 'www.apple.com', NULL, 'PRIV', 'PUBKEY', 'abcd', \
                '/r/vlesspath', 'vless', 'reality', 'active', unixepoch(), unixepoch()), \
                (2, 8443, '', 'hy2.example.com', NULL, '', '', '', \
                NULL, 'hysteria2', 'auto', 'active', unixepoch(), unixepoch())",
    )
    .execute(&h.pool)
    .await
    .unwrap();

    let sub_token = "valid_sub_token_1234567890";
    sqlx::query(
        "INSERT INTO clients (id, inbound_id, uuid, name, quota_bytes, expiry_at, status, sub_token, created_at) \
         VALUES (1, 1, '11111111-2222-3333-4444-555555555555', 'testuser', 10737418240, NULL, 'active', ?, unixepoch())",
    )
    .bind(sub_token)
    .execute(&h.pool)
    .await
    .unwrap();

    // Map client to both inbounds
    sqlx::query("INSERT INTO client_inbounds (client_id, inbound_id) VALUES (1, 1), (1, 2)")
        .execute(&h.pool)
        .await
        .unwrap();

    // Add traffic snapshot
    sqlx::query(
        "INSERT INTO traffic_snapshots (client_id, uplink, downlink, xray_uplink, xray_downlink, recorded_at) \
         VALUES (1, 1024, 2048, 1024, 2048, unixepoch())",
    )
    .execute(&h.pool)
    .await
    .unwrap();

    // Send public unauthenticated request
    let req = Request::builder()
        .uri(format!("/sub/{sub_token}"))
        .body(Body::empty())
        .unwrap();
    let resp = h.app.clone().oneshot(req).await.unwrap();

    assert_eq!(resp.status(), StatusCode::OK);

    // Headers check
    let userinfo = resp
        .headers()
        .get("subscription-userinfo")
        .and_then(|v| v.to_str().ok())
        .unwrap();
    assert!(userinfo.contains("upload=1024"));
    assert!(userinfo.contains("download=2048"));
    assert!(userinfo.contains("total=10737418240"));

    let body = body_text(resp).await;
    let decoded = String::from_utf8(
        base64::engine::general_purpose::STANDARD
            .decode(body.trim())
            .unwrap(),
    )
    .unwrap();

    assert!(decoded.contains("vless://11111111-2222-3333-4444-555555555555@sub.example.com:443?"));
    assert!(
        decoded.contains("hysteria2://11111111-2222-3333-4444-555555555555@sub.example.com:8443/?")
    );
    assert!(decoded.contains("#testuser%20-%20VLESS"));
    assert!(decoded.contains("#testuser%20-%20Hy2"));
}

#[tokio::test]
async fn sub_endpoint_returns_404_for_expired_client() {
    let h = boot().await;

    sqlx::query(
        "INSERT INTO inbounds (id, listen_port, dest, sni, server_name, private_key, \
                public_key, short_id, xhttp_path, protocol, cert_mode, status, created_at, updated_at) \
         VALUES (1, 443, 'www.apple.com', 'www.apple.com', NULL, 'PRIV', 'PUBKEY', 'abcd', \
                '/r/vlesspath', 'vless', 'reality', 'active', unixepoch(), unixepoch())",
    )
    .execute(&h.pool)
    .await
    .unwrap();

    let sub_token = "expired_token";
    sqlx::query(
        "INSERT INTO clients (id, inbound_id, uuid, name, quota_bytes, expiry_at, status, sub_token, created_at) \
         VALUES (2, 1, '22222222-2222-3333-4444-555555555555', 'expired', 0, unixepoch() - 100, 'active', ?, unixepoch())",
    )
    .bind(sub_token)
    .execute(&h.pool)
    .await
    .unwrap();

    let req = Request::builder()
        .uri(format!("/sub/{sub_token}"))
        .body(Body::empty())
        .unwrap();
    let resp = h.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn sub_endpoint_returns_404_for_unknown_token() {
    let h = boot().await;
    let req = Request::builder()
        .uri("/sub/non-existent-token")
        .body(Body::empty())
        .unwrap();
    let resp = h.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}
