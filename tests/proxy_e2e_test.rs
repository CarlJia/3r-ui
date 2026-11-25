//! End-to-end integration tests for Pingora reverse proxy with HTTPS.

use axum::routing::get;
use axum::Router;
use pingora_core::server::{RunArgs, Server};
use rcgen::generate_simple_self_signed;
use std::net::SocketAddr;
use std::time::Duration;
use tempfile::tempdir;
use three_r_ui::proxy::server::PanelProxy;
use three_r_ui::proxy::supervisor::WatchShutdownSignal;
use tokio::sync::watch;

#[tokio::test]
async fn pingora_proxy_https_e2e() {
    // 1. Start a mock upstream Axum HTTP server on an ephemeral port
    let app = Router::new().route("/hello", get(|| async { "hello from upstream" }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_addr = listener.local_addr().unwrap();

    let (upstream_shutdown_tx, upstream_shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = upstream_shutdown_rx.await;
            })
            .await
            .unwrap();
    });

    // 2. Generate a self-signed cert for panel.test
    let subject_alt_names = vec!["panel.test".to_string(), "localhost".to_string()];
    let cert = generate_simple_self_signed(subject_alt_names).unwrap();
    let cert_pem = cert.cert.pem();
    let key_pem = cert.key_pair.serialize_pem();

    let tmp = tempdir().unwrap();
    let cert_file = tmp.path().join("cert.pem");
    let key_file = tmp.path().join("key.pem");
    std::fs::write(&cert_file, cert_pem).unwrap();
    std::fs::write(&key_file, key_pem).unwrap();

    // 3. Find an available port for Pingora
    let proxy_port_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let proxy_port = proxy_port_listener.local_addr().unwrap().port();
    drop(proxy_port_listener);

    // 4. Start Pingora server in background thread
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let cert_path_clone = cert_file.clone();
    let key_path_clone = key_file.clone();

    let thread_handle = std::thread::spawn(move || {
        let mut server = Server::new(None).unwrap();
        server.bootstrap();

        let panel_proxy = PanelProxy::new(
            upstream_addr.to_string(),
            "panel.test".to_string(),
            "".to_string(),
            "/sub".to_string(),
        );

        let mut proxy_service =
            pingora_proxy::http_proxy_service(&server.configuration, panel_proxy);
        proxy_service
            .add_tls(
                &format!("127.0.0.1:{proxy_port}"),
                cert_path_clone.to_str().unwrap(),
                key_path_clone.to_str().unwrap(),
            )
            .unwrap();

        server.add_service(proxy_service);

        let run_args = RunArgs {
            shutdown_signal: Box::new(WatchShutdownSignal(shutdown_rx)),
        };
        server.run(run_args);
    });

    // Give Pingora a moment to start
    tokio::time::sleep(Duration::from_millis(500)).await;

    // 5. Test client requests via reqwest with self-signed TLS support
    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .resolve("panel.test", SocketAddr::from(([127, 0, 0, 1], proxy_port)))
        .resolve("evil.com", SocketAddr::from(([127, 0, 0, 1], proxy_port)))
        .build()
        .unwrap();

    // 5a. Request with matching domain "panel.test" -> Expect 200 OK and upstream response
    let url = format!("https://panel.test:{proxy_port}/hello");
    let resp = client
        .get(&url)
        .send()
        .await
        .expect("Failed to send request to proxy");
    assert_eq!(resp.status(), 200);
    let text = resp.text().await.unwrap();
    assert_eq!(text, "hello from upstream");

    // 5b. Request with mismatching domain "evil.com" -> Expect 403 Forbidden
    let url_evil = format!("https://evil.com:{proxy_port}/hello");
    let resp_evil = client
        .get(&url_evil)
        .send()
        .await
        .expect("Failed to send request to evil domain");
    assert_eq!(resp_evil.status(), 403);

    // 6. Shutdown Pingora and upstream cleanly
    let _ = shutdown_tx.send(true);
    let _ = thread_handle.join();
    let _ = upstream_shutdown_tx.send(());
}

#[tokio::test]
async fn pingora_proxy_custom_path_and_sub_path_e2e() {
    // 1. Start a mock upstream server that checks path received
    let app = axum::Router::new()
        .route(
            "/hello",
            axum::routing::get(|req: axum::extract::Request| async move {
                let prefix = req
                    .headers()
                    .get("x-forwarded-prefix")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("");
                format!("hello from upstream, prefix={prefix}")
            }),
        )
        .route(
            "/sub/token123",
            axum::routing::get(|| async { "sub content" }),
        );

    let upstream_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_addr = upstream_listener.local_addr().unwrap();

    let (upstream_shutdown_tx, upstream_shutdown_rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        axum::serve(upstream_listener, app)
            .with_graceful_shutdown(async {
                let _ = upstream_shutdown_rx.await;
            })
            .await
            .unwrap();
    });

    // 2. Generate temporary self-signed certificate for "panel.test"
    let temp_dir = tempfile::tempdir().unwrap();
    let cert_file = temp_dir.path().join("panel.crt");
    let key_file = temp_dir.path().join("panel.key");

    let subject_alt_names = vec!["panel.test".to_string()];
    let cert = rcgen::generate_simple_self_signed(subject_alt_names).unwrap();
    let cert_pem = cert.cert.pem();
    let key_pem = cert.key_pair.serialize_pem();

    std::fs::write(&cert_file, cert_pem).unwrap();
    std::fs::write(&key_file, key_pem).unwrap();

    // 3. Find an available port for Pingora
    let proxy_port_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let proxy_port = proxy_port_listener.local_addr().unwrap().port();
    drop(proxy_port_listener);

    // 4. Start Pingora server configured with panel_path "/xxjjye" and sub_path "/mysub"
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let cert_path_clone = cert_file.clone();
    let key_path_clone = key_file.clone();

    let thread_handle = std::thread::spawn(move || {
        let mut server = Server::new(None).unwrap();
        server.bootstrap();

        let panel_proxy = PanelProxy::new(
            upstream_addr.to_string(),
            "panel.test".to_string(),
            "/xxjjye".to_string(),
            "/mysub".to_string(),
        );

        let mut proxy_service =
            pingora_proxy::http_proxy_service(&server.configuration, panel_proxy);
        proxy_service
            .add_tls(
                &format!("127.0.0.1:{proxy_port}"),
                cert_path_clone.to_str().unwrap(),
                key_path_clone.to_str().unwrap(),
            )
            .unwrap();

        server.add_service(proxy_service);

        let run_args = RunArgs {
            shutdown_signal: Box::new(WatchShutdownSignal(shutdown_rx)),
        };
        server.run(run_args);
    });

    tokio::time::sleep(Duration::from_millis(500)).await;

    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .resolve("panel.test", SocketAddr::from(([127, 0, 0, 1], proxy_port)))
        .resolve("evil.com", SocketAddr::from(([127, 0, 0, 1], proxy_port)))
        .build()
        .unwrap();

    // 5a. Root / or /hello without path prefix -> Expect 404 (panel is hidden)
    let resp = client
        .get(&format!("https://panel.test:{proxy_port}/"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);

    let resp = client
        .get(&format!("https://panel.test:{proxy_port}/hello"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);

    // 5b. Request with custom path /xxjjye/hello -> 200 OK, stripped prefix and X-Forwarded-Prefix injected
    let resp = client
        .get(&format!("https://panel.test:{proxy_port}/xxjjye/hello"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let text = resp.text().await.unwrap();
    assert_eq!(text, "hello from upstream, prefix=/xxjjye");

    // 5c. Request with custom subscription path /mysub/token123 -> maps to upstream /sub/token123
    let resp = client
        .get(&format!("https://panel.test:{proxy_port}/mysub/token123"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let text = resp.text().await.unwrap();
    assert_eq!(text, "sub content");

    // 5d. Mismatched domain -> 403 Forbidden
    let resp = client
        .get(&format!("https://evil.com:{proxy_port}/xxjjye/hello"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 403);

    // 6. Cleanup
    let _ = shutdown_tx.send(true);
    let _ = thread_handle.join();
    let _ = upstream_shutdown_tx.send(());
}
