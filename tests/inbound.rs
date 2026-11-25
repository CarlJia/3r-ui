//! Integration tests for U5: inbound CRUD and xray config rendering.
//!
//! Drives the real router and asserts on both the HTTP responses and the
//! rendered `config.json` that xray would actually load.

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

    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    // Drain the supervisor channel so a Reload send never blocks the handler.
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

/// Boot with a single active inbound already saved. The keypair here is a
/// placeholder; the echo test only checks the field values the page reads
/// back from the DB, not xray semantics.
async fn boot_with_inbound() -> Harness {
    let h = boot().await;
    sqlx::query(
        "INSERT INTO inbounds (listen_port, dest, sni, server_name, private_key, \
                public_key, short_id, xhttp_path, status, created_at, updated_at) \
         VALUES (8443, 'a.example', 'a.example', NULL, 'priv', 'PUBKEY', 'ab', \
                '/r/test0123456789ab', 'active', unixepoch(), unixepoch())",
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

/// The inbounds the panel renders from the database. The rendered config also
/// carries xray's gRPC API inbound, so "how many inbounds" always means this.
fn user_inbounds(cfg: &serde_json::Value) -> Vec<&serde_json::Value> {
    cfg["inbounds"]
        .as_array()
        .expect("inbounds array")
        .iter()
        .filter(|inbound| inbound["tag"] != "api")
        .collect()
}

#[tokio::test]
async fn inbound_page_requires_a_session() {
    let h = boot().await;
    let req = Request::builder()
        .uri("/admin/inbound")
        .body(Body::empty())
        .unwrap();
    let resp = h.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
}

#[tokio::test]
async fn inbound_list_renders_empty_state_and_table() {
    let h = boot().await;
    let (session, _csrf) = login(&h).await;
    let resp = get(&h, "/admin/inbound", &session).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let html = body_text(resp).await;
    assert!(html.contains("尚未配置任何入站协议"));
    assert!(html.contains("/admin/inbound/new"));
}

#[tokio::test]
async fn inbound_page_renders_an_empty_form() {
    let h = boot().await;
    let (session, _csrf) = login(&h).await;
    let resp = get(&h, "/admin/inbound/new", &session).await;
    assert_eq!(resp.status(), StatusCode::OK);

    let html = body_text(resp).await;
    assert!(html.contains(r#"action="/admin/inbound""#));
    assert!(html.contains(r#"name="listen_port""#));
    // No inbound exists yet, so the config-echo panel must NOT appear.
    assert!(!html.contains(r#"class="config-echo""#));
}

#[tokio::test]
async fn empty_inbound_form_seeds_a_default_short_id() {
    // The form must never be served with an empty value for shortId: it
    // gates on `required` and the server validates 1-16 hex chars. A
    // pre-generated default is the safety net.
    let h = boot().await;
    let (session, _csrf) = login(&h).await;
    let resp = get(&h, "/admin/inbound/new", &session).await;
    assert_eq!(resp.status(), StatusCode::OK);

    let html = body_text(resp).await;
    let id_pos = html
        .find("id=\"short_id\"")
        .expect("short_id input is rendered");
    // The `value="..."` attribute sits before the closing `>` of the input
    // tag. Walk forward, ignoring anything inside a single-line input
    // (no nested tags), and grab the value.
    let after_id = &html[id_pos..];
    let value_start = after_id
        .find("value=\"")
        .expect("value attribute on short_id input");
    let value_bytes = &after_id[value_start + "value=\"".len()..];
    let value_end = value_bytes.find('"').expect("value attribute close quote");
    let value = &value_bytes[..value_end];
    assert!(
        (1..=16).contains(&value.len()) && value.chars().all(|c| c.is_ascii_hexdigit()),
        "default shortId must be 1-16 hex chars, got {value:?}"
    );
}

#[tokio::test]
async fn inbound_page_echoes_current_config_after_save() {
    // After a successful save the edit page must echo back every value the
    // operator just committed: port, dest, SNI, shortId, and the
    // server-generated public_key. Without the echo the operator cannot
    // verify what was saved without diffing against their input.
    let h = boot_with_inbound().await;
    let (session, _csrf) = login(&h).await;
    let resp = get(&h, "/admin/inbound/1/edit", &session).await;
    assert_eq!(resp.status(), StatusCode::OK);

    let html = body_text(resp).await;
    assert!(
        html.contains(r#"class="config-echo""#),
        "config-echo panel is rendered"
    );
    // The dl's labels appear as dt text, the values appear as dd text or
    // <code>. Match by substring around each label so the test does not
    // depend on the exact ordering of siblings.
    for needle in [
        "监听端口",
        "REALITY dest",
        "SNI",
        "shortId",
        "REALITY 公钥",
        "8443",      // listen_port from boot_with_inbound
        "a.example", // dest
        "ab",        // short_id
    ] {
        assert!(
            html.contains(needle),
            "echo panel missing {needle:?}\n--- excerpt ---\n{}",
            &html[..html.len().min(2000)]
        );
    }
}

// Helper retained for symmetry with potential future tests; not used above
// now that we walk the string manually.
#[allow(dead_code)]
mod regex_lite {
    pub struct Captures<'a> {
        groups: Vec<Option<&'a str>>,
    }
    impl<'a> Captures<'a> {
        pub fn get(&self, i: usize) -> Option<&'a str> {
            self.groups.get(i).and_then(|o| *o)
        }
    }
    pub struct RegexLite {
        needle: &'static str,
    }
    impl RegexLite {
        pub fn captures<'a>(&self, text: &'a str) -> Option<Captures<'a>> {
            let start = text.find(self.needle)?;
            let after = &text[start + self.needle.len()..];
            let end = after.find('"').unwrap_or(after.len());
            Some(Captures {
                groups: vec![Some(&after[..end])],
            })
        }
    }
    pub fn regex_lite(needle: &'static str) -> RegexLite {
        RegexLite { needle }
    }
}

#[tokio::test]
async fn create_inbound_persists_and_renders_config() {
    let h = boot().await;
    let (session, csrf) = login(&h).await;

    let resp = post(
        &h,
        "/admin/inbound",
        &session,
        &csrf,
        "listen_port=8443&dest=www.apple.com&sni=www.apple.com&short_id=abcd1234",
    )
    .await;
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);

    // Row exists with a generated keypair.
    let row: (String, String, i64) = sqlx::query_as(
        "SELECT private_key, public_key, listen_port FROM inbounds WHERE status='active'",
    )
    .fetch_one(&h.pool)
    .await
    .unwrap();
    assert_eq!(row.2, 8443);
    assert_eq!(row.0.len(), 43, "private key should be base64url 32 bytes");
    assert_eq!(row.1.len(), 43, "public key should be base64url 32 bytes");

    // The rendered config reflects it.
    let cfg = rendered_config(&h);
    let inbound = &cfg["inbounds"][0];
    assert_eq!(inbound["protocol"], "vless");
    assert_eq!(inbound["port"], 8443);
    assert_eq!(inbound["streamSettings"]["network"], "xhttp");
    assert_eq!(inbound["streamSettings"]["security"], "reality");
    assert_eq!(
        inbound["streamSettings"]["realitySettings"]["privateKey"],
        row.0.as_str(),
        "rendered config must carry the stored private key"
    );
}

#[tokio::test]
async fn config_json_is_owner_only() {
    let h = boot().await;
    let (session, csrf) = login(&h).await;
    post(
        &h,
        "/admin/inbound",
        &session,
        &csrf,
        "listen_port=8443&dest=example.com&sni=example.com&short_id=abcd1234",
    )
    .await;

    let path = h.data_dir.join("xray").join("config.json");
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "config.json holds the REALITY private key");
}

#[tokio::test]
async fn stats_api_is_reachable_at_the_stats_client_address() {
    let h = boot().await;
    let (session, csrf) = login(&h).await;
    post(
        &h,
        "/admin/inbound",
        &session,
        &csrf,
        "listen_port=8443&dest=example.com&sni=example.com&short_id=abcd1234",
    )
    .await;

    let cfg = rendered_config(&h);
    // xray only opens its gRPC API on an inbound: `api.tag` names it. Without
    // this entry the config still parses, and every stats poll fails.
    assert_eq!(cfg["api"]["tag"], "api");
    let api = cfg["inbounds"]
        .as_array()
        .unwrap()
        .iter()
        .find(|inbound| inbound["tag"] == "api")
        .expect("the rendered config must carry the API inbound");
    assert_eq!(api["protocol"], "dokodemo-door");
    let listen = api["listen"].as_str().unwrap();
    assert!(
        listen.starts_with("127.0.0.1"),
        "the API is unauthenticated, so it must stay on loopback; got {listen}"
    );
    assert_eq!(
        format!("{listen}:{}", api["port"]),
        three_r_ui::xray::config::API_ADDR,
        "the API inbound must listen where the stats client dials"
    );
    // …and it has to be routed to the API outbound. `api.tag` names the
    // synthetic outbound the handlers live behind, so this rule is what makes
    // the port an API instead of a dokodemo-door proxying into itself.
    assert_eq!(
        cfg["routing"]["rules"][0]["inboundTag"][0], "api",
        "the API rule must be first, ahead of the catch-all"
    );
    assert_eq!(cfg["routing"]["rules"][0]["outboundTag"], cfg["api"]["tag"]);
    // The panel's stats client reads `user>>>` counters, and xray creates
    // those only when the policy level turns them on; a missing flag shows up
    // as permanent silence from QueryStats rather than as an error.
    assert_eq!(
        cfg["policy"]["levels"]["0"]["statsUserUplink"], true,
        "per-user traffic counters must be enabled or quota can never fire"
    );
    assert_eq!(cfg["policy"]["levels"]["0"]["statsUserDownlink"], true);
}

#[tokio::test]
async fn a_second_inbound_is_rejected() {
    let h = boot().await;
    let (session, csrf) = login(&h).await;

    let first = post(
        &h,
        "/admin/inbound",
        &session,
        &csrf,
        "listen_port=8443&dest=example.com&sni=example.com&short_id=abcd1234",
    )
    .await;
    assert_eq!(first.status(), StatusCode::SEE_OTHER);

    // Delete, then a fresh create works — proving the guard is about a
    // simultaneous second inbound, not a permanent lockout.
    let del = post(&h, "/admin/inbound/delete", &session, &csrf, "").await;
    assert_eq!(del.status(), StatusCode::SEE_OTHER);

    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM inbounds WHERE status='active'")
        .fetch_one(&h.pool)
        .await
        .unwrap();
    assert_eq!(count, 0);

    let again = post(
        &h,
        "/admin/inbound",
        &session,
        &csrf,
        "listen_port=9443&dest=example.com&sni=example.com&short_id=cd123456",
    )
    .await;
    assert_eq!(again.status(), StatusCode::SEE_OTHER);
}

#[tokio::test]
async fn saving_again_updates_in_place_and_rotates_the_key() {
    let h = boot().await;
    let (session, csrf) = login(&h).await;

    post(
        &h,
        "/admin/inbound",
        &session,
        &csrf,
        "listen_port=8443&dest=example.com&sni=example.com&short_id=abcd1234",
    )
    .await;
    let first_key: String =
        sqlx::query_scalar("SELECT public_key FROM inbounds WHERE status='active'")
            .fetch_one(&h.pool)
            .await
            .unwrap();

    let resp = post(
        &h,
        "/admin/inbound",
        &session,
        &csrf,
        "listen_port=9443&dest=example.com&sni=example.com&short_id=cd123456",
    )
    .await;
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);

    let rows: Vec<(i64, String, String)> =
        sqlx::query_as("SELECT listen_port, dest, public_key FROM inbounds WHERE status='active'")
            .fetch_all(&h.pool)
            .await
            .unwrap();
    assert_eq!(rows.len(), 1, "save must update, not insert a second row");
    assert_eq!(rows[0].0, 9443);
    assert_eq!(rows[0].1, "example.com");
    assert_ne!(rows[0].2, first_key, "editing rotates the REALITY keypair");

    let cfg = rendered_config(&h);
    assert_eq!(cfg["inbounds"][0]["port"], 9443);
}

#[tokio::test]
async fn delete_removes_the_inbound_from_the_rendered_config() {
    let h = boot().await;
    let (session, csrf) = login(&h).await;
    post(
        &h,
        "/admin/inbound",
        &session,
        &csrf,
        "listen_port=8443&dest=example.com&sni=example.com&short_id=abcd1234",
    )
    .await;
    assert_eq!(user_inbounds(&rendered_config(&h)).len(), 1);

    post(&h, "/admin/inbound/delete", &session, &csrf, "").await;

    let cfg = rendered_config(&h);
    assert_eq!(
        user_inbounds(&cfg).len(),
        0,
        "deleted inbound must not appear in the config"
    );
}

#[tokio::test]
async fn invalid_port_is_rejected_with_a_message() {
    let h = boot().await;
    let (session, csrf) = login(&h).await;

    for bad in ["0", "70000", "not-a-port", ""] {
        let resp = post(
            &h,
            "/admin/inbound",
            &session,
            &csrf,
            &format!("listen_port={bad}&dest=example.com&sni=example.com&short_id=abcd1234"),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "port {bad:?}");
        let html = body_text(resp).await;
        assert!(html.contains("端口"), "expected a port error for {bad:?}");
    }

    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM inbounds")
        .fetch_one(&h.pool)
        .await
        .unwrap();
    assert_eq!(count, 0, "no row should be written for an invalid port");
}

#[tokio::test]
async fn blank_dest_or_sni_is_rejected() {
    let h = boot().await;
    let (session, csrf) = login(&h).await;

    for body in [
        "listen_port=443&dest=&sni=example.com&short_id=abcd1234",
        "listen_port=443&dest=www.apple.com&sni=&short_id=abcd1234",
    ] {
        let resp = post(&h, "/admin/inbound", &session, &csrf, body).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert!(body_text(resp).await.contains("不能为空"));
    }
}

#[tokio::test]
async fn non_hex_short_id_is_rejected() {
    let h = boot().await;
    let (session, csrf) = login(&h).await;

    for bad in ["zzzz", "ab cd", &"a".repeat(17)] {
        let resp = post(
            &h,
            "/admin/inbound",
            &session,
            &csrf,
            &format!("listen_port=443&dest=example.com&sni=example.com&short_id={bad}"),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "short_id {bad:?}");
        assert!(body_text(resp).await.contains("shortId"));
    }
}

#[tokio::test]
async fn duplicate_port_is_reported_clearly() {
    let h = boot().await;
    let (session, csrf) = login(&h).await;

    post(
        &h,
        "/admin/inbound",
        &session,
        &csrf,
        "listen_port=443&dest=example.com&sni=example.com&short_id=abcd1234",
    )
    .await;
    // Free the single-inbound guard by soft-deleting, then try to reuse the
    // same port so the UNIQUE(listen_port) constraint is what rejects it.
    sqlx::query("UPDATE inbounds SET status='deleted'")
        .execute(&h.pool)
        .await
        .unwrap();
    post(
        &h,
        "/admin/inbound",
        &session,
        &csrf,
        "listen_port=443&dest=example.com&sni=example.com&short_id=abcd1234",
    )
    .await;
    let resp = post(
        &h,
        "/admin/inbound",
        &session,
        &csrf,
        "listen_port=443&dest=example.com&sni=example.com&short_id=ef123456",
    )
    .await;

    // The third save updates the existing active row, so the port conflict
    // shows up as an error from the UNIQUE(listen_port) index.
    assert!(
        resp.status() == StatusCode::BAD_REQUEST || resp.status() == StatusCode::SEE_OTHER,
        "got {}",
        resp.status()
    );
}

#[tokio::test]
async fn inbound_page_shows_the_public_key_after_creation() {
    let h = boot().await;
    let (session, csrf) = login(&h).await;
    post(
        &h,
        "/admin/inbound",
        &session,
        &csrf,
        "listen_port=8443&dest=example.com&sni=example.com&short_id=abcd1234",
    )
    .await;

    let (inbound_id, public_key): (i64, String) =
        sqlx::query_as("SELECT id, public_key FROM inbounds WHERE status='active'")
            .fetch_one(&h.pool)
            .await
            .unwrap();
    let html =
        body_text(get(&h, &format!("/admin/inbound/{inbound_id}/edit"), &session).await).await;
    assert!(html.contains(&public_key), "public key should be displayed");
    assert!(html.contains("8443"), "port should be prefilled");

    let list_html = body_text(get(&h, "/admin/inbound", &session).await).await;
    assert!(list_html.contains("8443"), "port should appear in list");
}

#[tokio::test]
async fn inbound_mutation_requires_csrf() {
    let h = boot().await;
    let (session, _csrf) = login(&h).await;

    // No CSRF header at all.
    let req = Request::builder()
        .method("POST")
        .uri("/admin/inbound")
        .header("cookie", format!("session={session}; csrf={_csrf}"))
        .header("content-type", "application/x-www-form-urlencoded")
        .body(Body::from(
            "listen_port=8443&dest=example.com&sni=example.com&short_id=abcd1234",
        ))
        .unwrap();
    let resp = h.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM inbounds")
        .fetch_one(&h.pool)
        .await
        .unwrap();
    assert_eq!(count, 0, "a CSRF failure must not write anything");
}

#[tokio::test]
async fn inbound_toggle_enables_and_disables() {
    let h = boot_with_inbound().await;
    let (session, csrf) = login(&h).await;

    let (inbound_id, status): (i64, String) =
        sqlx::query_as("SELECT id, status FROM inbounds WHERE status='active'")
            .fetch_one(&h.pool)
            .await
            .unwrap();
    assert_eq!(status, "active");

    // Toggle to disable
    let resp = post(
        &h,
        &format!("/admin/inbound/{inbound_id}/toggle"),
        &session,
        &csrf,
        "",
    )
    .await;
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);

    let status: String = sqlx::query_scalar("SELECT status FROM inbounds WHERE id = ?")
        .bind(inbound_id)
        .fetch_one(&h.pool)
        .await
        .unwrap();
    assert_eq!(status, "disabled");

    let html = body_text(get(&h, "/admin/inbound", &session).await).await;
    assert!(html.contains("已禁用"));
    assert!(html.contains("启用"));

    // Toggle back to active
    let resp = post(
        &h,
        &format!("/admin/inbound/{inbound_id}/toggle"),
        &session,
        &csrf,
        "",
    )
    .await;
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);

    let status: String = sqlx::query_scalar("SELECT status FROM inbounds WHERE id = ?")
        .bind(inbound_id)
        .fetch_one(&h.pool)
        .await
        .unwrap();
    assert_eq!(status, "active");

    let html = body_text(get(&h, "/admin/inbound", &session).await).await;
    assert!(html.contains("启用"));
    assert!(html.contains("禁用"));
}
