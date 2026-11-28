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
    let (tunnel_tx, mut tunnel_rx) = tokio::sync::mpsc::channel(8);
    tokio::spawn(async move { while tunnel_rx.recv().await.is_some() {} });

    let tunnel_supervisor = std::sync::Arc::new(three_r_ui::tunnel::CloudflaredSupervisor::new(
        pool.clone(),
        &config.data_dir,
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

#[tokio::test]
async fn sub_endpoint_decouples_cdn_domain_and_uses_inbound_listen_ip_and_server_name() {
    let h = boot().await;

    // Inbound 1: VLESS on specific IP (203.0.113.10) without server_name -> should connect to 203.0.113.10
    // Inbound 2: Mieru on IP (203.0.113.20) with custom direct server_name (direct2.example.com) -> should connect to direct2.example.com
    sqlx::query(
        "INSERT INTO inbounds (id, listen_ip, listen_port, dest, sni, server_name, private_key, \
                public_key, short_id, xhttp_path, protocol, cert_mode, status, created_at, updated_at) \
         VALUES (1, '203.0.113.10', 443, 'www.apple.com', 'www.apple.com', NULL, 'PRIV', 'PUBKEY', 'abcd', \
                '/r/vlesspath', 'vless', 'reality', 'active', unixepoch(), unixepoch()), \
                (2, '203.0.113.20', 8443, '', '', 'direct2.example.com', '', '', '', \
                NULL, 'mieru', 'auto', 'active', unixepoch(), unixepoch())",
    )
    .execute(&h.pool)
    .await
    .unwrap();

    let sub_token = "multi_nic_sub_token";
    sqlx::query(
        "INSERT INTO clients (id, inbound_id, uuid, name, quota_bytes, expiry_at, status, sub_token, created_at) \
         VALUES (1, 1, '11111111-2222-3333-4444-555555555555', 'nicuser', 10737418240, NULL, 'active', ?, unixepoch())",
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

    // Send public unauthenticated request
    let req = Request::builder()
        .uri(format!("/sub/{sub_token}"))
        .body(Body::empty())
        .unwrap();
    let resp = h.app.clone().oneshot(req).await.unwrap();

    assert_eq!(resp.status(), StatusCode::OK);

    let body = body_text(resp).await;
    let decoded = String::from_utf8(
        base64::engine::general_purpose::STANDARD
            .decode(body.trim())
            .unwrap(),
    )
    .unwrap();

    // Inbound 1 should use listen_ip 203.0.113.10 instead of public_host sub.example.com
    assert!(
        decoded.contains("vless://11111111-2222-3333-4444-555555555555@203.0.113.10:443?"),
        "Decoded subscription links should contain node connecting to 203.0.113.10: {}",
        decoded
    );

    // Inbound 2 should use server_name direct2.example.com
    assert!(
        decoded.contains("mieru"),
        "Decoded subscription links should contain mieru: {}",
        decoded
    );
    assert!(
        decoded.contains("@direct2.example.com"),
        "Mieru link should connect to direct2.example.com: {}",
        decoded
    );
}

#[tokio::test]
async fn sub_endpoint_serves_html_preview_to_browsers() {
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

    let sub_token = "browser_sub_token_abc";
    sqlx::query(
        "INSERT INTO clients (id, inbound_id, uuid, name, quota_bytes, expiry_at, status, sub_token, created_at) \
         VALUES (1, 1, '11111111-2222-3333-4444-555555555555', 'alice', 10737418240, unixepoch() + 86400 * 30, 'active', ?, unixepoch())",
    )
    .bind(sub_token)
    .execute(&h.pool)
    .await
    .unwrap();

    // Traffic snapshot: 50MB up, 150MB down
    sqlx::query(
        "INSERT INTO traffic_snapshots (client_id, uplink, downlink, xray_uplink, xray_downlink, recorded_at) \
         VALUES (1, 52428800, 157286400, 52428800, 157286400, unixepoch())",
    )
    .execute(&h.pool)
    .await
    .unwrap();

    let req = Request::builder()
        .uri(format!("/sub/{sub_token}"))
        .header(
            "Accept",
            "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
        )
        .header(
            "User-Agent",
            "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36",
        )
        .body(Body::empty())
        .unwrap();

    let resp = h.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Content-Type should be HTML, without attachment disposition
    let content_type = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap();
    assert!(content_type.contains("text/html"));
    assert!(resp.headers().get("content-disposition").is_none());

    let html = body_text(resp).await;
    assert!(html.contains("alice"));
    assert!(html.contains("11111111-2222-3333-4444-555555555555"));
    assert!(html.contains("VLESS"));
    assert!(html.contains("data:image/png;base64,"));
    assert!(html.contains("由 3R-UI 高性能代理面板驱动"));
}

#[tokio::test]
async fn sub_endpoint_preview_respects_forwarded_proto_https() {
    let h = boot().await;

    let sub_token = "proto_sub_token_test";
    sqlx::query(
        "INSERT INTO inbounds (id, listen_port, dest, sni, server_name, private_key, \
                public_key, short_id, xhttp_path, protocol, cert_mode, status, created_at, updated_at) \
         VALUES (1, 443, 'www.apple.com', 'www.apple.com', NULL, 'PRIV', 'PUBKEY', 'abcd', \
                '/r/vlesspath', 'vless', 'reality', 'active', unixepoch(), unixepoch())",
    )
    .execute(&h.pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO clients (id, inbound_id, uuid, name, quota_bytes, expiry_at, status, sub_token, created_at) \
         VALUES (1, 1, '11111111-2222-3333-4444-555555555555', 'proto_user', 10737418240, NULL, 'active', ?, unixepoch())",
    )
    .bind(sub_token)
    .execute(&h.pool)
    .await
    .unwrap();

    // Set public host without scheme
    sqlx::query("UPDATE system_settings SET public_host = 'tw.856668.xyz' WHERE id = 1")
        .execute(&h.pool)
        .await
        .unwrap();

    // Request with x-forwarded-proto: https
    let req = Request::builder()
        .uri(format!("/sub/{sub_token}?preview=1"))
        .header("x-forwarded-proto", "https")
        .header("host", "tw.856668.xyz")
        .body(Body::empty())
        .unwrap();

    let resp = h.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let html = body_text(resp).await;
    assert!(html.contains("https://tw.856668.xyz/sub/proto_sub_token_test"));
}

#[tokio::test]
async fn sub_endpoint_serves_html_preview_with_query_param() {
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

    let sub_token = "preview_param_token_xyz";
    sqlx::query(
        "INSERT INTO clients (id, inbound_id, uuid, name, quota_bytes, expiry_at, status, sub_token, created_at) \
         VALUES (1, 1, '33333333-4444-5555-6666-777777777777', 'bob', 0, NULL, 'active', ?, unixepoch())",
    )
    .bind(sub_token)
    .execute(&h.pool)
    .await
    .unwrap();

    let req = Request::builder()
        .uri(format!("/sub/{sub_token}?preview=1"))
        .header("Accept", "*/*")
        .body(Body::empty())
        .unwrap();

    let resp = h.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let content_type = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap();
    assert!(content_type.contains("text/html"));

    let html = body_text(resp).await;
    assert!(html.contains("bob"));
    assert!(html.contains("不限流量"));
}

#[tokio::test]
async fn sub_endpoint_preserves_base64_for_proxy_clients_with_html_accept() {
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

    let sub_token = "clash_client_token";
    sqlx::query(
        "INSERT INTO clients (id, inbound_id, uuid, name, quota_bytes, expiry_at, status, sub_token, created_at) \
         VALUES (1, 1, '11111111-2222-3333-4444-555555555555', 'clashuser', 10737418240, NULL, 'active', ?, unixepoch())",
    )
    .bind(sub_token)
    .execute(&h.pool)
    .await
    .unwrap();

    // Client sends Accept: text/html but User-Agent is Clash
    let req = Request::builder()
        .uri(format!("/sub/{sub_token}"))
        .header("Accept", "text/html,*/*")
        .header("User-Agent", "ClashforWindows/0.20.39")
        .body(Body::empty())
        .unwrap();

    let resp = h.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let content_type = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap();
    assert_eq!(content_type, "text/plain; charset=utf-8");

    let disp = resp
        .headers()
        .get("content-disposition")
        .and_then(|v| v.to_str().ok())
        .unwrap();
    assert!(disp.contains("attachment"));

    assert!(resp.headers().contains_key("subscription-userinfo"));

    let body = body_text(resp).await;
    let decoded = String::from_utf8(
        base64::engine::general_purpose::STANDARD
            .decode(body.trim())
            .unwrap(),
    )
    .unwrap();
    assert!(decoded.contains("vless://"));
}
