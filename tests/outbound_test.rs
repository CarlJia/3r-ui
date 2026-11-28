use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use three_r_ui::config::AppConfig;
use three_r_ui::crypto::password;
use three_r_ui::web::{self, AppState};

const ADMIN_PW: &str = "admin_pass";

struct Harness {
    app: axum::Router,
    pool: sqlx::SqlitePool,
    _dir: tempfile::TempDir,
}

async fn boot() -> Harness {
    let dir = tempfile::tempdir().unwrap();
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

    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let (proxy_tx, mut proxy_rx) = tokio::sync::mpsc::channel(8);
    tokio::spawn(async move { while proxy_rx.recv().await.is_some() {} });
    let (mieru_tx, mut mieru_rx) = tokio::sync::mpsc::channel(8);
    tokio::spawn(async move { while mieru_rx.recv().await.is_some() {} });
    let (tunnel_tx, mut tunnel_rx) = tokio::sync::mpsc::channel(8);
    tokio::spawn(async move { while tunnel_rx.recv().await.is_some() {} });

    let data_dir = config.data_dir.clone();
    let tunnel_supervisor = std::sync::Arc::new(three_r_ui::tunnel::CloudflaredSupervisor::new(
        pool.clone(),
        &data_dir,
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

    let app = web::router(state);
    Harness {
        app,
        pool,
        _dir: dir,
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

async fn body_text(resp: axum::response::Response) -> String {
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    String::from_utf8_lossy(&bytes).to_string()
}

#[tokio::test]
async fn outbound_crud_and_inbound_chaining() {
    let h = boot().await;
    let (session, csrf) = login(&h).await;

    // 1. Visit outbounds list (initially empty)
    let req = Request::builder()
        .uri("/admin/outbounds")
        .header("cookie", format!("session={session}"))
        .body(Body::empty())
        .unwrap();
    let resp = h.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let html = body_text(resp).await;
    assert!(html.contains("出站代理节点"));
    assert!(html.contains("暂无自定义出站节点"));

    // 2. Create a new outbound
    let req = Request::builder()
        .method("POST")
        .uri("/admin/outbounds/new")
        .header("cookie", format!("session={session}; csrf={csrf}"))
        .header("x-csrf-token", &csrf)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(Body::from(
            "name=HK-Relay&tag=out-hk-relay&protocol=socks&server_addr=198.51.100.5&server_port=1080&settings_json={}&status=active",
        ))
        .unwrap();
    let resp = h.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);

    // Verify row created in DB
    let ob = three_r_ui::db::outbound::get_outbound(&h.pool, 1)
        .await
        .unwrap()
        .expect("outbound row exists");
    assert_eq!(ob.tag, "out-hk-relay");
    assert_eq!(ob.name, "HK-Relay");
    assert_eq!(ob.server_port, 1080);

    // 3. Edit outbound
    let req = Request::builder()
        .method("POST")
        .uri("/admin/outbounds/1/edit")
        .header("cookie", format!("session={session}; csrf={csrf}"))
        .header("x-csrf-token", &csrf)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(Body::from(
            "name=HK-Relay-Updated&tag=out-hk-relay&protocol=socks&server_addr=198.51.100.5&server_port=1081&settings_json={}&status=active",
        ))
        .unwrap();
    let resp = h.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);

    let ob_updated = three_r_ui::db::outbound::get_outbound(&h.pool, 1)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(ob_updated.name, "HK-Relay-Updated");
    assert_eq!(ob_updated.server_port, 1081);

    // 4. Create an inbound and bind to this outbound (Proxy Chaining)
    sqlx::query(
        "INSERT INTO inbounds (id, listen_ip, listen_port, dest, sni, server_name, private_key, \
                public_key, short_id, xhttp_path, protocol, cert_mode, custom_cert_path, custom_key_path, \
                masquerade_url, port_range, transport_protocol, skip_cert_verify, udp, status, outbound_id, created_at, updated_at) \
         VALUES (1, '0.0.0.0', 8443, 'www.apple.com', 'www.apple.com', NULL, 'PRIV', 'PUBKEY', 'abcd', \
                '/r/vlesspath', 'vless', 'reality', '', '', '', '', 'TCP', 0, 1, 'active', 1, unixepoch(), unixepoch())",
    )
    .execute(&h.pool)
    .await
    .unwrap();

    // Render Xray config and assert proxy chaining routing rule
    let config_file = h._dir.path().join("xray").join("config.json");
    three_r_ui::xray::config::render(&h.pool, &config_file)
        .await
        .unwrap();

    let config_content = std::fs::read_to_string(&config_file).unwrap();
    let cfg: serde_json::Value = serde_json::from_str(&config_content).unwrap();

    // Check outbounds contains out-hk-relay
    let outbounds_arr = cfg["outbounds"].as_array().unwrap();
    let found_ob = outbounds_arr
        .iter()
        .find(|o| o["tag"] == "out-hk-relay")
        .expect("out-hk-relay in config.json outbounds");
    assert_eq!(found_ob["protocol"], "socks");

    // Check routing rule routes inbound-1 to out-hk-relay
    let rules = cfg["routing"]["rules"].as_array().unwrap();
    let relay_rule = rules
        .iter()
        .find(|r| {
            r.get("inboundTag")
                .and_then(|ib| ib.as_array())
                .map(|arr| arr.iter().any(|v| v == "inbound-1"))
                .unwrap_or(false)
        })
        .expect("routing rule for inbound-1");
    assert_eq!(relay_rule["outboundTag"], "out-hk-relay");
}

#[tokio::test]
async fn test_parse_link_api() {
    let h = boot().await;
    let (session, csrf) = login(&h).await;

    // 1. Without session should redirect to login
    let req = Request::builder()
        .method("POST")
        .uri("/admin/outbounds/parse_link")
        .header("content-type", "application/x-www-form-urlencoded")
        .body(Body::from("link=vless://abc@example.com:443&csrf=invalid"))
        .unwrap();
    let resp = h.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);

    // 2. With session but invalid CSRF should be 403 Forbidden
    let req = Request::builder()
        .method("POST")
        .uri("/admin/outbounds/parse_link")
        .header("cookie", format!("session={session}; csrf={csrf}"))
        .header("content-type", "application/x-www-form-urlencoded")
        .body(Body::from(
            "link=vless://abc@example.com:443&csrf=wrong_csrf",
        ))
        .unwrap();
    let resp = h.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    // 3. Valid VLESS link parsing
    let vless_link = "vless://11111111-2222-3333-4444-555555555555@sg.example.com:443?type=ws&security=tls&sni=sg.example.com&path=%2Fws#SG-Node-01";
    let body = format!(
        "link={}&csrf={}",
        urlencoding_simple(vless_link),
        urlencoding_simple(&csrf)
    );
    let req = Request::builder()
        .method("POST")
        .uri("/admin/outbounds/parse_link")
        .header("cookie", format!("session={session}; csrf={csrf}"))
        .header("content-type", "application/x-www-form-urlencoded")
        .body(Body::from(body))
        .unwrap();
    let resp = h.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let res: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(res["success"], true);
    assert_eq!(res["data"]["protocol"], "vless");
    assert_eq!(res["data"]["name"], "SG-Node-01");
    assert_eq!(res["data"]["server_addr"], "sg.example.com");
    assert_eq!(res["data"]["server_port"], 443);
    assert!(res["data"]["settings_json"]
        .as_str()
        .unwrap()
        .contains("11111111-2222-3333-4444-555555555555"));

    // 4. Invalid link format should return 400 Bad Request
    let body = format!("link=ftp://invalid&csrf={}", urlencoding_simple(&csrf));
    let req = Request::builder()
        .method("POST")
        .uri("/admin/outbounds/parse_link")
        .header("cookie", format!("session={session}; csrf={csrf}"))
        .header("content-type", "application/x-www-form-urlencoded")
        .body(Body::from(body))
        .unwrap();
    let resp = h.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let res: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(res["success"], false);
    assert!(res["error"]
        .as_str()
        .unwrap()
        .contains("不支持的节点链接协议"));
}

fn urlencoding_simple(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.' || b == b'~' {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{:02X}", b));
        }
    }
    out
}
