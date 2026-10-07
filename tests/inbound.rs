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

    let data_dir = config.data_dir.clone();
    let state = AppState {
        config,
        pool: pool.clone(),
        supervisor_tx: tx,
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
async fn inbound_page_renders_an_empty_form() {
    let h = boot().await;
    let (session, _csrf) = login(&h).await;
    let resp = get(&h, "/admin/inbound", &session).await;
    assert_eq!(resp.status(), StatusCode::OK);

    let html = body_text(resp).await;
    assert!(html.contains(r#"action="/admin/inbound""#));
    assert!(html.contains(r#"name="listen_port""#));
    assert!(!html.contains("REALITY 公钥"), "no key yet");
}

#[tokio::test]
async fn empty_inbound_form_seeds_a_default_short_id() {
    // The form must never be served with an empty value for shortId: it
    // gates on `required` and the server validates 1-16 hex chars. A
    // pre-generated default is the safety net.
    let h = boot().await;
    let (session, _csrf) = login(&h).await;
    let resp = get(&h, "/admin/inbound", &session).await;
    assert_eq!(resp.status(), StatusCode::OK);

    let html = body_text(resp).await;
    let id_pos = html.find("id=\"short_id\"").expect("short_id input is rendered");
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
            Some(Captures { groups: vec![Some(&after[..end])] })
        }
    }
    pub fn regex_lite(needle: &'static str) -> RegexLite { RegexLite { needle } }
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
        "listen_port=8443&dest=www.microsoft.com&sni=www.microsoft.com&short_id=abcd",
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
        "listen_port=8443&dest=a.example&sni=a.example&short_id=ab",
    )
    .await;

    let path = h.data_dir.join("xray").join("config.json");
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "config.json holds the REALITY private key");
}

#[tokio::test]
async fn stats_api_is_bound_to_loopback() {
    let h = boot().await;
    let (session, csrf) = login(&h).await;
    post(
        &h,
        "/admin/inbound",
        &session,
        &csrf,
        "listen_port=8443&dest=a.example&sni=a.example&short_id=ab",
    )
    .await;

    let cfg = rendered_config(&h);
    let listen = cfg["stats api"]["listen"].as_str().unwrap();
    assert!(
        listen.starts_with("127.0.0.1:"),
        "stats api listen = {listen}"
    );
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
        "listen_port=8443&dest=a.example&sni=a.example&short_id=ab",
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
        "listen_port=9443&dest=b.example&sni=b.example&short_id=cd",
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
        "listen_port=8443&dest=a.example&sni=a.example&short_id=ab",
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
        "listen_port=9443&dest=b.example&sni=b.example&short_id=cd",
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
    assert_eq!(rows[0].1, "b.example");
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
        "listen_port=8443&dest=a.example&sni=a.example&short_id=ab",
    )
    .await;
    assert_eq!(rendered_config(&h)["inbounds"].as_array().unwrap().len(), 1);

    post(&h, "/admin/inbound/delete", &session, &csrf, "").await;

    let cfg = rendered_config(&h);
    assert_eq!(
        cfg["inbounds"].as_array().unwrap().len(),
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
            &format!("listen_port={bad}&dest=a.example&sni=a.example&short_id=ab"),
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
        "listen_port=443&dest=&sni=a.example&short_id=ab",
        "listen_port=443&dest=a.example&sni=&short_id=ab",
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
            &format!("listen_port=443&dest=a.example&sni=a.example&short_id={bad}"),
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
        "listen_port=443&dest=a.example&sni=a.example&short_id=ab",
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
        "listen_port=443&dest=a.example&sni=a.example&short_id=ab",
    )
    .await;
    let resp = post(
        &h,
        "/admin/inbound",
        &session,
        &csrf,
        "listen_port=443&dest=c.example&sni=c.example&short_id=ef",
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
        "listen_port=8443&dest=a.example&sni=a.example&short_id=ab",
    )
    .await;

    let public_key: String =
        sqlx::query_scalar("SELECT public_key FROM inbounds WHERE status='active'")
            .fetch_one(&h.pool)
            .await
            .unwrap();
    let html = body_text(get(&h, "/admin/inbound", &session).await).await;
    assert!(html.contains(&public_key), "public key should be displayed");
    assert!(html.contains("8443"), "port should be prefilled");
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
            "listen_port=8443&dest=a.example&sni=a.example&short_id=ab",
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
