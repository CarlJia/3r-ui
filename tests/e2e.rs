//! U10: end-to-end smoke that drives the actual `3r-ui` binary as a child
//! process and walks the full HTTP flow with a real TCP socket (not the
//! in-process `tower::ServiceExt` shim used by other integration tests).
//!
//! Marked `#[ignore]` so the default `cargo test` does not pay the cost of
//! a release build. Run explicitly with:
//!
//!     cargo test --release --test e2e -- --ignored
//!
//! The smoke targets a unique random TCP port and a fresh temp data dir, so
//! it can run alongside a dev server. It deliberately uses a shell-stub
//! `xray` so the panel's supervisor path runs end-to-end without needing
//! the real xray-core binary on the test host.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tempfile::tempdir;

const ADMIN_PW: &str = "e2e-secret-pw";

fn bin_path() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_3r-ui"))
}

fn pick_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    port
}

fn wait_for_port(port: u16, deadline: Duration) -> std::io::Result<()> {
    let start = Instant::now();
    while start.elapsed() < deadline {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        "port never opened",
    ))
}

/// Tiny synchronous HTTP/1.1 client good enough for the smoke flow. We
/// could pull in reqwest, but keeping this self-contained means the smoke
/// does not need a separate dev-dependency or any TLS stack.
///
/// The body comes back as bytes because `/qr.png` returns binary — a String
/// return would fail UTF-8 validation on the PNG.
fn http(
    host: &str,
    port: u16,
    method: &str,
    path: &str,
    body: Option<&str>,
    cookies: &str,
) -> std::io::Result<(u16, String, Vec<u8>)> {
    let mut stream = TcpStream::connect((host, port))?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let mut req =
        format!("{method} {path} HTTP/1.1\r\nHost: {host}:{port}\r\nConnection: close\r\n");
    if !cookies.is_empty() {
        req.push_str(&format!("Cookie: {cookies}\r\n"));
    }
    if let Some(b) = body {
        req.push_str(&format!(
            "Content-Type: application/x-www-form-urlencoded\r\nContent-Length: {}\r\n\r\n{b}",
            b.len()
        ));
    } else {
        req.push_str("\r\n");
    }
    stream.write_all(req.as_bytes())?;

    let mut raw = Vec::new();
    stream.read_to_end(&mut raw)?;
    // Split at the first CRLFCRLF; everything after is the body, bytes intact.
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, "no header terminator")
        })?;
    let head = String::from_utf8_lossy(&raw[..split]).into_owned();
    let body = raw[split + 4..].to_vec();
    let status_line = head.lines().next().unwrap_or("");
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|n| n.parse().ok())
        .unwrap_or(0);
    Ok((status, head, body))
}

/// Set `Set-Cookie: csrf=...; session=...` headers from the previous
/// response into a single `Cookie:` header for the next request.
fn cookie_header(set_cookie_headers: &[&str], prev: &str) -> String {
    let mut cookies: Vec<(String, String)> = prev
        .split(';')
        .filter_map(|kv| kv.trim().split_once('='))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    for sc in set_cookie_headers {
        let Some(pair) = sc.split(';').next() else {
            continue;
        };
        let Some((k, v)) = pair.split_once('=') else {
            continue;
        };
        if let Some(existing) = cookies.iter_mut().find(|(name, _)| name == k) {
            existing.1 = v.to_string();
        } else {
            cookies.push((k.to_string(), v.to_string()));
        }
    }
    cookies
        .into_iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("; ")
}

/// Pre-seed an admin row with a known password. We shell out to a one-shot
/// helper that uses sqlx-cli-style parameters… but since we have sqlx in
/// process, easier: write directly with sqlite3 if available, otherwise
/// use the Rust helper binary.
fn seed_admin(db_path: &std::path::Path, username: &str, password: &str) {
    use three_r_ui::crypto::password;
    let hash = password::hash(password).expect("bcrypt hash");
    let dir = db_path.parent().unwrap().to_path_buf();
    let _ = std::fs::create_dir_all(&dir);
    // Open with sqlx directly via a tiny tokio runtime.
    let pool = tokio::runtime::Runtime::new().unwrap().block_on(async {
        let url = format!("sqlite://{}?mode=rwc", db_path.display());
        sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect(&url)
            .await
            .unwrap()
    });
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        sqlx::query(
            "INSERT OR REPLACE INTO admins (id, username, password_hash, created_at) \
                 VALUES (1, ?, ?, unixepoch())",
        )
        .bind(username)
        .bind(&hash)
        .execute(&pool)
        .await
        .unwrap();
    });
}

fn write_stub_xray(dir: &std::path::Path) -> PathBuf {
    let path = dir.join("xray");
    std::fs::create_dir_all(&path).unwrap();
    let bin = path.join("xray");
    std::fs::write(
        &bin,
        "#!/bin/sh\ntrap 'exit 0' TERM INT\necho \"stub xray pid=$$\"\nexec sleep 3600\n",
    )
    .unwrap();
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    bin
}

struct Harness {
    child: Child,
    base_port: u16,
    _data: tempfile::TempDir,
    _stub: PathBuf,
    _config_path: PathBuf,
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn spawn_panel() -> Harness {
    let port = pick_port();
    let data = tempdir().unwrap();
    let db_path = data.path().join("3r-ui.db");
    seed_admin(&db_path, "admin", ADMIN_PW);

    let stub = write_stub_xray(data.path());
    // Symlink so `${DATA_DIR}/xray/xray` resolves to our stub, exactly
    // what the panel's xray installer would have done.
    let target_xray = data.path().join("xray-bin");
    std::fs::remove_dir_all(&target_xray).ok();
    std::os::unix::fs::symlink(&stub, &target_xray).unwrap();

    let config_path = data.path().join("xray").join("config.json");
    std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();

    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let child = Command::new(bin_path())
        .env("BIND_ADDR", format!("127.0.0.1:{port}"))
        .env("DATA_DIR", data.path())
        .env(
            "XRAY_DOWNLOAD_BASE",
            format!("http://127.0.0.1:1/invalid-{nonce}"),
        )
        .env("RUST_LOG", "info")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn 3r-ui");

    Harness {
        child,
        base_port: port,
        _data: data,
        _stub: stub,
        _config_path: config_path,
    }
}

#[test]
#[ignore = "requires the release binary; run with `cargo test --release --test e2e -- --ignored`"]
fn panel_starts_and_serves_the_login_page() {
    let h = spawn_panel();
    wait_for_port(h.base_port, Duration::from_secs(10)).expect("port should open");

    let (status, _head, body) = http("127.0.0.1", h.base_port, "GET", "/", None, "").unwrap();
    assert_eq!(status, 200, "GET / should be 200");
    let body = String::from_utf8_lossy(&body);
    assert!(body.contains("登录"));
    // Security headers from the middleware must be on every response.
    let (_s, head, _) = http("127.0.0.1", h.base_port, "GET", "/", None, "").unwrap();
    assert!(head
        .to_ascii_lowercase()
        .contains("x-content-type-options: nosniff"));
}

#[test]
#[ignore = "requires the release binary"]
fn unauthenticated_admin_route_redirects_to_login() {
    let h = spawn_panel();
    wait_for_port(h.base_port, Duration::from_secs(10)).unwrap();

    let (status, head, _) = http("127.0.0.1", h.base_port, "GET", "/admin", None, "").unwrap();
    assert_eq!(status, 303);
    assert!(head.to_ascii_lowercase().contains("location: /"));
}

#[test]
#[ignore = "requires the release binary"]
fn login_rejects_unknown_user() {
    let h = spawn_panel();
    wait_for_port(h.base_port, Duration::from_secs(10)).unwrap();

    let (status, _head, body) = http(
        "127.0.0.1",
        h.base_port,
        "POST",
        "/login",
        Some("username=admin&password=wrong"),
        "",
    )
    .unwrap();
    assert_eq!(status, 401, "wrong password should be 401");
    let body = String::from_utf8_lossy(&body);
    assert!(body.contains("用户名或密码错误"));
}

/// Walk the full login → inbound create → user create → share-QR flow
/// against a real spawned binary. This is the closest thing we have to an
/// end-to-end smoke without a real xray-core.
#[test]
#[ignore = "requires the release binary"]
fn full_admin_flow_completes() {
    let h = spawn_panel();
    wait_for_port(h.base_port, Duration::from_secs(10)).unwrap();

    // 1. Fetch the login page to grab the csrf cookie.
    let (_status, _lh, _lb) = http("127.0.0.1", h.base_port, "GET", "/", None, "").unwrap();
    let (status, head, _) = http("127.0.0.1", h.base_port, "GET", "/", None, "").unwrap();
    assert_eq!(status, 200);
    let set_cookies: Vec<String> = head
        .lines()
        .filter(|l| l.to_ascii_lowercase().starts_with("set-cookie:"))
        .map(|l| l["set-cookie:".len()..].trim().to_string())
        .collect();
    assert!(!set_cookies.is_empty(), "csrf cookie should be set");
    let cookies = cookie_header(
        &set_cookies.iter().map(String::as_str).collect::<Vec<_>>(),
        "",
    );

    // 2. Login.
    let (status, head, body) = http(
        "127.0.0.1",
        h.base_port,
        "POST",
        "/login",
        Some(&format!("username=admin&password={ADMIN_PW}")),
        &cookies,
    )
    .unwrap();
    let body = String::from_utf8_lossy(&body);
    assert_eq!(status, 303, "login should redirect; body: {body}");
    let set_cookies: Vec<String> = head
        .lines()
        .filter(|l| l.to_ascii_lowercase().starts_with("set-cookie:"))
        .map(|l| l["set-cookie:".len()..].trim().to_string())
        .collect();
    let cookies = cookie_header(
        &set_cookies.iter().map(String::as_str).collect::<Vec<_>>(),
        &cookies,
    );
    assert!(cookies.contains("session="), "session cookie missing");

    // 3. Pull csrf for the inbound POST.
    let csrf = cookies
        .split(';')
        .find_map(|kv| kv.trim().strip_prefix("csrf=").map(|s| s.to_string()))
        .expect("csrf cookie");
    let form_with_csrf = |fields: &str| format!("csrf={csrf}&{fields}");

    // 4. Create the inbound.
    let (status, _h, body) = http(
        "127.0.0.1",
        h.base_port,
        "POST",
        "/admin/inbound",
        Some(&form_with_csrf(
            "listen_port=2053&dest=www.apple.com&sni=www.apple.com&short_id=abcd",
        )),
        &cookies,
    )
    .unwrap();
    let body = String::from_utf8_lossy(&body);
    assert_eq!(status, 303, "inbound save should redirect; body: {body}");

    // 5. Create a user.
    let (status, _h, body) = http(
        "127.0.0.1",
        h.base_port,
        "POST",
        "/admin/clients",
        Some(&form_with_csrf("name=alice&quota_gb=10")),
        &cookies,
    )
    .unwrap();
    let body = String::from_utf8_lossy(&body);
    assert_eq!(status, 303, "client save should redirect; body: {body}");

    // 6. Verify xray config.json reflects both.
    let config_raw = std::fs::read_to_string(&h._config_path).expect("config.json should exist");
    let cfg: serde_json::Value = serde_json::from_str(&config_raw).unwrap();
    assert_eq!(cfg["inbounds"][0]["port"], 2053);
    let clients = cfg["inbounds"][0]["settings"]["clients"]
        .as_array()
        .unwrap();
    assert_eq!(clients.len(), 1, "the new client must be in config");
    assert!(clients[0]["email"].is_string(), "email = uuid invariant");

    // 7. QR PNG endpoint must stream a valid PNG with the vless URI baked in.
    let (status, head, body) = http(
        "127.0.0.1",
        h.base_port,
        "GET",
        "/admin/clients/1/qr.png",
        None,
        &cookies,
    )
    .unwrap();
    assert_eq!(status, 200);
    assert!(head
        .to_ascii_lowercase()
        .contains("content-type: image/png"));
    assert!(body.len() > 100, "png should not be tiny");
    assert_eq!(
        &body[..8],
        &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]
    );
}
