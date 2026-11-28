use axum::body::Body;
use axum::http::{Request, StatusCode};
use base64::Engine;
use std::sync::Arc;
use tempfile::TempDir;
use tower::ServiceExt;

use three_r_ui::config::AppConfig;
use three_r_ui::db::node;
use three_r_ui::node::protocol::{
    compute_signature, verify_signature, NodeTelemetry, ServiceStatusItem,
};
use three_r_ui::node::supervisor::AgentSupervisor;
use three_r_ui::node::NodeManager;
use three_r_ui::web::{self, AppState};

async fn setup_test_app() -> (axum::Router, sqlx::SqlitePool, TempDir, Arc<NodeManager>) {
    let dir = tempfile::tempdir().expect("tempdir failed");
    let mut config = AppConfig::from_env().unwrap_or_else(|_| AppConfig {
        bind_addr: "127.0.0.1:0".to_string(),
        data_dir: dir.path().to_path_buf(),
        stats_interval_secs: 60,
        xray_download_base: "https://github.com/3r-ui/xray/releases".to_string(),
        public_host: "127.0.0.1".to_string(),
    });
    config.data_dir = dir.path().to_path_buf();

    let pool = three_r_ui::init_storage(&config)
        .await
        .expect("init storage failed");
    let hash = three_r_ui::crypto::password::hash("s3cret-admin-pw").unwrap();
    sqlx::query(
        "INSERT INTO admins (username, password_hash, created_at) VALUES (?, ?, unixepoch())",
    )
    .bind("admin")
    .bind(&hash)
    .execute(&pool)
    .await
    .unwrap();

    let (tx, _rx) = tokio::sync::mpsc::channel(16);
    let (proxy_tx, _proxy_rx) = tokio::sync::mpsc::channel(16);
    let (mieru_tx, _mieru_rx) = tokio::sync::mpsc::channel(16);
    let (tunnel_tx, _tunnel_rx) = tokio::sync::mpsc::channel(16);
    let tunnel_supervisor = Arc::new(three_r_ui::tunnel::CloudflaredSupervisor::new(
        pool.clone(),
        dir.path(),
    ));

    let node_manager = Arc::new(NodeManager::new(pool.clone()));

    let state = AppState {
        config: config.clone(),
        pool: pool.clone(),
        supervisor_tx: tx,
        proxy_tx,
        mieru_tx,
        tunnel_tx,
        tunnel_supervisor,
        update_jobs: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
        installer: Arc::new(
            three_r_ui::xray::Installer::new("https://github.com/3r-ui/xray/releases")
                .expect("installer must be constructible"),
        ),
        cert_tracker: Arc::new(three_r_ui::proxy::CertProgressTracker::new()),
        system_tracker: Arc::new(three_r_ui::web::system::SystemTracker::new()),
        ddns_notifier: three_r_ui::ddns::DdnsNotifier::new(),
        node_manager: node_manager.clone(),
        shutdown_initiated: Arc::new(std::sync::atomic::AtomicBool::new(false)),
    };

    let router = web::router(state);
    (router, pool, dir, node_manager)
}

#[tokio::test]
async fn test_node_db_crud_and_tokens() {
    let (_router, pool, _dir, _manager) = setup_test_app().await;

    // 1. Initial nodes check: Local Node 1 seeded
    let nodes = node::list_nodes(&pool).await.expect("list nodes failed");
    assert!(!nodes.is_empty());
    assert_eq!(nodes[0].is_local, 1);
    assert_eq!(nodes[0].name, "Master Local Node");

    // 2. Groups CRUD
    let group = node::create_group(&pool, "Hong Kong Tier 1", "hk-t1", Some("HK Datacenter"))
        .await
        .expect("create group failed");
    assert_eq!(group.name, "Hong Kong Tier 1");
    assert_eq!(group.tag, "hk-t1");

    let groups = node::list_groups(&pool).await.expect("list groups failed");
    assert_eq!(groups.len(), 1);

    // 3. Provisioning Tokens
    let token = node::create_provisioning_token(&pool, Some(group.id), Some("HK-Node-01"), 300)
        .await
        .expect("token create failed");
    assert!(token.starts_with("provision_"));

    // Consume token
    let validated = node::validate_and_consume_token(&pool, &token)
        .await
        .expect("token validation failed");
    assert!(validated.is_some());
    let token_data = validated.unwrap();
    assert_eq!(token_data.node_name.as_deref(), Some("HK-Node-01"));
    assert_eq!(token_data.group_id, Some(group.id));

    // Consuming second time fails
    let reused = node::validate_and_consume_token(&pool, &token)
        .await
        .expect("token re-check failed");
    assert!(reused.is_none());

    // 4. Remote Node Creation & Telemetry Update
    let new_node = node::create_node(
        &pool,
        "HK-Node-01",
        "sec_test_secret_key_12345",
        Some("1.2.3.4"),
        Some(group.id),
    )
    .await
    .expect("create node failed");
    assert_eq!(new_node.status, "offline");
    assert_eq!(new_node.is_local, 0);

    node::update_node_telemetry(
        &pool,
        new_node.id,
        node::NodeTelemetryUpdate {
            cpu: Some(25.5),
            mem_used: Some(1024 * 1024 * 500),
            mem_total: Some(1024 * 1024 * 2000),
            disk_used: Some(1024 * 1024 * 10000),
            disk_total: Some(1024 * 1024 * 50000),
            services_status: Some(r#"[{"name":"xray","running":true}]"#),
        },
    )
    .await
    .expect("telemetry update failed");

    let updated = node::get_node_by_id(&pool, new_node.id)
        .await
        .expect("get node failed")
        .unwrap();
    assert_eq!(updated.status, "online");
    assert_eq!(updated.cpu_percent, Some(25.5));
    assert!(updated.last_heartbeat_at.is_some());

    // 5. Delete remote node succeeds, delete local node fails
    node::delete_node(&pool, new_node.id)
        .await
        .expect("delete remote node failed");
    let deleted = node::get_node_by_id(&pool, new_node.id).await.unwrap();
    assert!(deleted.is_none());

    let delete_local = node::delete_node(&pool, 1).await;
    assert!(delete_local.is_err());
}

#[test]
fn test_protocol_hmac_and_signature_security() {
    let secret = "sec_very_secure_node_key_987654321";
    let node_id = 100;
    let now = 1710000000;

    let sig = compute_signature(secret, node_id, now);
    assert!(!sig.is_empty());

    // Normal matching
    assert!(verify_signature(secret, node_id, now, now, &sig, 60));
    // Acceptable clock drift
    assert!(verify_signature(
        secret,
        node_id,
        now - 45,
        now,
        &compute_signature(secret, node_id, now - 45),
        60
    ));
    // Excessive clock drift (> 60s)
    assert!(!verify_signature(
        secret,
        node_id,
        now - 65,
        now,
        &compute_signature(secret, node_id, now - 65),
        60
    ));
    // Wrong secret
    assert!(!verify_signature(
        "wrong_secret",
        node_id,
        now,
        now,
        &sig,
        60
    ));
    // Wrong node
    assert!(!verify_signature(secret, 101, now, now, &sig, 60));
}

#[tokio::test]
async fn test_node_manager_heartbeat_and_circuit_breaker() {
    let (_router, pool, _dir, manager) = setup_test_app().await;

    // Create a node record in DB
    let remote_node = node::create_node(
        &pool,
        "Tokyo-Node",
        "sec_tokyo_node_secret",
        Some("2.3.4.5"),
        None,
    )
    .await
    .unwrap();

    let (tx, _rx) = tokio::sync::mpsc::channel(8);
    manager.register_session(remote_node.id, tx).await;

    // Initially online
    assert!(manager.is_node_online(remote_node.id).await);
    let healthy = manager.get_healthy_node_ids().await;
    assert!(healthy.contains(&1)); // Local node
    assert!(healthy.contains(&remote_node.id));

    // Update heartbeat
    manager
        .update_heartbeat(
            remote_node.id,
            NodeTelemetry {
                cpu_percent: 12.0,
                memory_used_bytes: 100,
                memory_total_bytes: 1000,
                disk_used_bytes: 500,
                disk_total_bytes: 5000,
                services: vec![ServiceStatusItem {
                    name: "xray".to_string(),
                    running: true,
                    pid: Some(1234),
                    uptime_secs: Some(60),
                }],
            },
        )
        .await;

    // Simulate circuit breaker timeout check
    manager.check_heartbeats().await;
    assert!(manager.is_node_online(remote_node.id).await); // Still within 30s

    // Unregister session explicitly (e.g. socket close)
    manager.unregister_session(remote_node.id).await;
    assert!(!manager.is_node_online(remote_node.id).await);

    let healthy_after = manager.get_healthy_node_ids().await;
    assert_eq!(healthy_after, vec![1]);
}

#[tokio::test]
async fn test_agent_daemon_supervisor_lifecycle() {
    let dir = tempfile::tempdir().expect("tempdir failed");
    let supervisor = AgentSupervisor::new(dir.path().to_path_buf());

    let statuses_before = supervisor.get_service_statuses().await;
    assert_eq!(statuses_before.len(), 3);
    assert!(!statuses_before[0].running); // xray stopped

    // Apply config
    let config_json = r#"{"log":{"loglevel":"warning"}}"#;
    supervisor
        .apply_sync_config(
            Some(config_json.to_string()),
            None,
            Some("cf_tunnel_tok_123".to_string()),
            false,
        )
        .await
        .expect("apply config failed");

    let statuses_after = supervisor.get_service_statuses().await;
    assert!(statuses_after[0].running); // xray now running
    assert!(statuses_after[1].running); // cloudflared now running

    // Verify config file written
    let written = std::fs::read_to_string(dir.path().join("xray").join("config.json")).unwrap();
    assert_eq!(written, config_json);

    // Restart service
    supervisor
        .restart_service("xray")
        .await
        .expect("restart failed");
    assert!(supervisor.restart_service("invalid_service").await.is_err());
}

#[tokio::test]
async fn test_web_endpoints_nodes() {
    let (router, pool, _dir, _manager) = setup_test_app().await;

    // Create a valid session
    sqlx::query(
        "INSERT INTO sessions (session_id, admin_id, expires_at, created_at) VALUES ('test_session_id', 1, unixepoch() + 3600, unixepoch())",
    )
    .execute(&pool)
    .await
    .unwrap();

    // Test install.sh endpoint
    let req = Request::builder()
        .uri("/admin/nodes/install.sh")
        .method("GET")
        .header("cookie", "session=test_session_id")
        .body(Body::empty())
        .unwrap();

    let resp = router.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let script = String::from_utf8(bytes.to_vec()).unwrap();
    assert!(script.contains("3R-UI Sub-Agent Automated Installer"));

    // Test API nodes list
    let req = Request::builder()
        .uri("/admin/api/nodes")
        .method("GET")
        .header("cookie", "session=test_session_id")
        .body(Body::empty())
        .unwrap();

    let resp = router.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let body_str = String::from_utf8(bytes.to_vec()).unwrap();
    assert!(body_str.contains("Master Local Node"));
}

#[tokio::test]
async fn test_multi_node_subscription_aggregation_and_circuit_breaker() {
    let (router, pool, _dir, manager) = setup_test_app().await;

    // 1. Create an active inbound
    let inbound_id = sqlx::query(
        r#"INSERT INTO inbounds (listen_port, dest, sni, server_name, private_key, public_key, short_id, xhttp_path, protocol, cert_mode, custom_cert_path, custom_key_path, masquerade_url, port_range, transport_protocol, skip_cert_verify, udp, status, created_at, updated_at)
           VALUES (443, "127.0.0.1:80", "example.com", "sub.example.com", "privkey", "pubkey", "shortid", "/xhttp", "vless", "none", "", "", "", "", "tcp", 0, 0, 'active', unixepoch(), unixepoch())"#
    )
    .execute(&pool)
    .await
    .unwrap()
    .last_insert_rowid();

    // 2. Create an active client
    let sub_token = "sub_tok_multi_node_test_12345";
    let client_id = sqlx::query(
        r#"INSERT INTO clients (inbound_id, uuid, name, quota_bytes, expiry_at, status, sub_token, created_at)
           VALUES (?, "11111111-2222-3333-4444-555555555555", "testclient", 1000000, unixepoch() + 3600, 'active', ?, unixepoch())"#
    )
    .bind(inbound_id)
    .bind(sub_token)
    .execute(&pool)
    .await
    .unwrap()
    .last_insert_rowid();

    // Map client_inbounds
    sqlx::query("INSERT INTO client_inbounds (client_id, inbound_id) VALUES (?, ?)")
        .bind(client_id)
        .bind(inbound_id)
        .execute(&pool)
        .await
        .unwrap();

    // 3. Create remote Node 2 (Tokyo Node)
    let remote_node = node::create_node(
        &pool,
        "Tokyo Highspeed",
        "sec_tokyo_12345",
        Some("tokyo.node.example.com"),
        None,
    )
    .await
    .unwrap();

    // Phase A: Node 2 is offline
    let req = Request::builder()
        .uri(format!("/sub/{sub_token}"))
        .method("GET")
        .header("host", "master.example.com")
        .body(Body::empty())
        .unwrap();

    let resp = router.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body_bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let decoded = String::from_utf8(
        base64::engine::general_purpose::STANDARD
            .decode(std::str::from_utf8(&body_bytes).unwrap().trim())
            .unwrap(),
    )
    .unwrap();

    // Only master local endpoint present, remote node is absent
    assert!(decoded.contains("sub.example.com"));
    assert!(!decoded.contains("tokyo.node.example.com"));

    // Phase B: Node 2 connects and becomes online
    let (tx, _rx) = tokio::sync::mpsc::channel(8);
    manager.register_session(remote_node.id, tx).await;

    let req = Request::builder()
        .uri(format!("/sub/{sub_token}"))
        .method("GET")
        .header("host", "master.example.com")
        .body(Body::empty())
        .unwrap();

    let resp = router.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body_bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let decoded_online = String::from_utf8(
        base64::engine::general_purpose::STANDARD
            .decode(std::str::from_utf8(&body_bytes).unwrap().trim())
            .unwrap(),
    )
    .unwrap();
    eprintln!("DEBUG decoded_online: {}", decoded_online);

    // Both Master and Tokyo Node endpoints are aggregated into the subscription!
    assert!(decoded_online.contains("sub.example.com"));
    assert!(decoded_online.contains("tokyo.node.example.com"));
    assert!(decoded_online.contains("Tokyo%20Highspeed"));

    // Phase C: Node 2 disconnected (Circuit Breaker Eviction)
    manager.unregister_session(remote_node.id).await;

    let req = Request::builder()
        .uri(format!("/sub/{sub_token}"))
        .method("GET")
        .header("host", "master.example.com")
        .body(Body::empty())
        .unwrap();

    let resp = router.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body_bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let decoded_evicted = String::from_utf8(
        base64::engine::general_purpose::STANDARD
            .decode(std::str::from_utf8(&body_bytes).unwrap().trim())
            .unwrap(),
    )
    .unwrap();

    // Evicted from subscription again
    assert!(decoded_evicted.contains("sub.example.com"));
    assert!(!decoded_evicted.contains("tokyo.node.example.com"));
}
