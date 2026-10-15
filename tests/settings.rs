//! Integration tests for U3: settings page, xray update trigger, and status endpoint.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use std::sync::Arc;
use tempfile::tempdir;
use three_r_ui::config::AppConfig;
use three_r_ui::crypto::password;
use three_r_ui::web::settings::{JobStage, JobStatus};
use three_r_ui::web::{self, AppState};
use three_r_ui::xray::DownloadProgress;
use tower::ServiceExt;

const ADMIN_PW: &str = "s3cret-admin-pw";

struct Harness {
    #[allow(dead_code)]
    app: axum::Router,
    #[allow(dead_code)]
    state: AppState,
    _dir: tempfile::TempDir,
    #[allow(dead_code)]
    pool: sqlx::SqlitePool,
    #[allow(dead_code)]
    data_dir: std::path::PathBuf,
}

async fn boot() -> Harness {
    let dir = tempdir().unwrap();
    let mut config = AppConfig::from_env().unwrap();
    config.data_dir = dir.path().to_path_buf();
    // Use a guaranteed-fast-fail URL so background run_update tasks exit
    // promptly with a Failed state (the test process does not need to
    // reach the real GitHub; download correctness is covered by
    // tests/installer.rs).
    config.xray_download_base = "http://127.0.0.1:1".to_string();

    let pool = three_r_ui::init_storage(&config).await.unwrap();
    sqlx::query(
        "INSERT INTO admins (username, password_hash, created_at) VALUES (?, ?, unixepoch())",
    )
    .bind("admin")
    .bind(password::hash(ADMIN_PW).unwrap())
    .execute(&pool)
    .await
    .unwrap();

    // The supervisor channel drains so RestartNow sends never block the handler.
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    tokio::spawn(async move { while rx.recv().await.is_some() {} });

    let state = AppState {
        config: config.clone(),
        pool: pool.clone(),
        supervisor_tx: tx,
        update_jobs: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
        installer: Arc::new(
            three_r_ui::xray::Installer::new(config.xray_download_base.clone())
                .expect("installer must be constructible"),
        ),
        shutdown_initiated: Arc::new(std::sync::atomic::AtomicBool::new(false)),
    };

    Harness {
        app: web::router(state.clone()),
        state,
        _dir: dir,
        pool,
        data_dir: config.data_dir.clone(),
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

/// Insert a job record directly, bypassing the update flow.
async fn seed_job(h: &Harness, id: &str, stage: JobStage, progress: Option<Arc<DownloadProgress>>) {
    let mut guard = h.state.update_jobs.lock().await;
    guard.insert(
        id.to_string(),
        JobStatus {
            id: id.to_string(),
            stage,
            message: "测试任务".to_string(),
            started_at: 2000,
            finished_at: None,
            installed_tag: None,
            error_message: None,
            progress,
        },
    );
}

// ─── Auth ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn settings_page_requires_session() {
    let h = boot().await;
    let req = Request::builder()
        .uri("/admin/settings")
        .body(Body::empty())
        .unwrap();
    let resp = h.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
}

// ─── GET /admin/settings ────────────────────────────────────────────────────

#[tokio::test]
async fn settings_page_renders_with_session() {
    let h = boot().await;
    let (session, _csrf) = login(&h).await;
    let resp = get(&h, "/admin/settings", &session).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let html = body_text(resp).await;
    assert!(html.contains("系统设置"), "should contain heading");
}

// ─── CSRF ───────────────────────────────────────────────────────────────────

#[tokio::test]
async fn trigger_update_requires_csrf() {
    let h = boot().await;
    let (session, csrf) = login(&h).await;
    // POST with valid session + real csrf cookie, but the form submits a
    // different csrf value. Middleware must reject with 403.
    let req = Request::builder()
        .method("POST")
        .uri("/admin/settings/xray/update")
        .header("cookie", format!("session={session}; csrf={csrf}"))
        .header("content-type", "application/x-www-form-urlencoded")
        .body(Body::from("csrf=definitely-not-the-real-token"))
        .unwrap();
    let resp = h.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

// ─── Concurrent guard ───────────────────────────────────────────────────────

#[tokio::test]
async fn second_trigger_while_active_fails_immediately() {
    let h = boot().await;
    let (session, csrf) = login(&h).await;

    // Insert a fake active job directly into update_jobs.
    {
        let mut guard = h.state.update_jobs.lock().await;
        guard.insert(
            "active-job-0000".to_string(),
            JobStatus {
                id: "active-job-0000".to_string(),
                stage: JobStage::Downloading,
                message: "下载中".to_string(),
                started_at: 0,
                finished_at: None,
                installed_tag: None,
                error_message: None,
                progress: None,
            },
        );
    }

    // Triggering a new update should immediately return a Failed job.
    let resp = post(
        &h,
        "/admin/settings/xray/update",
        &session,
        &csrf,
        &("csrf=".to_string() + &csrf),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);

    // Verify a new Failed job was added.
    let guard = h.state.update_jobs.lock().await;
    let failed_count = guard
        .values()
        .filter(|j| j.stage == JobStage::Failed && j.message == "已有更新任务进行中")
        .count();
    assert_eq!(
        failed_count, 1,
        "concurrent guard should add one Failed job"
    );
}

// ─── Status endpoint ────────────────────────────────────────────────────────

#[tokio::test]
async fn status_unknown_job_returns_synthesized_failed() {
    let h = boot().await;
    let (session, _csrf) = login(&h).await;
    let resp = get(&h, "/admin/settings/xray/status?job=nonexistent", &session).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_text(resp).await;
    assert!(
        body.contains("\"stage\":\"Failed\""),
        "should return Failed stage"
    );
    assert!(
        body.contains("任务不存在或已清理"),
        "should have the not-found message"
    );
}

#[tokio::test]
async fn status_known_job_returns_its_status() {
    let h = boot().await;
    let (session, _csrf) = login(&h).await;

    // Insert a Succeeded job.
    {
        let mut guard = h.state.update_jobs.lock().await;
        guard.insert(
            "done-job-0001".to_string(),
            JobStatus {
                id: "done-job-0001".to_string(),
                stage: JobStage::Succeeded,
                message: "已替换并已重启".to_string(),
                started_at: 1000,
                finished_at: Some(1010),
                installed_tag: Some("v1.8.10".to_string()),
                error_message: None,
                progress: None,
            },
        );
    }

    let resp = get(
        &h,
        "/admin/settings/xray/status?job=done-job-0001",
        &session,
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_text(resp).await;
    assert!(
        body.contains("\"stage\":\"Succeeded\""),
        "should return Succeeded stage"
    );
    assert!(body.contains("v1.8.10"), "should include installed_tag");
    // The polling page replaces the badge text with this label, so the endpoint
    // has to send the translated stage rather than only the enum name.
    assert!(
        body.contains("\"stage_label\":\"成功\""),
        "should include the Chinese stage label: {body}"
    );
}

/// The progress bar is drawn from the byte counters in the status payload, so
/// a download in flight has to report them.
#[tokio::test]
async fn status_reports_download_progress_bytes() {
    let h = boot().await;
    let (session, _csrf) = login(&h).await;

    let progress = Arc::new(DownloadProgress::default());
    progress.begin(Some(19_600_000));
    progress.set_downloaded(4_900_000);
    seed_job(
        &h,
        "downloading-job",
        JobStage::Downloading,
        Some(Arc::clone(&progress)),
    )
    .await;

    let resp = get(
        &h,
        "/admin/settings/xray/status?job=downloading-job",
        &session,
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_text(resp).await;
    assert!(
        body.contains("\"downloaded_bytes\":4900000"),
        "should report bytes received: {body}"
    );
    assert!(
        body.contains("\"total_bytes\":19600000"),
        "should report the announced size: {body}"
    );

    // A later poll must see the transfer's new position, not a stale copy.
    progress.set_downloaded(19_600_000);
    let resp = get(
        &h,
        "/admin/settings/xray/status?job=downloading-job",
        &session,
    )
    .await;
    let body = body_text(resp).await;
    assert!(
        body.contains("\"downloaded_bytes\":19600000"),
        "should report the latest bytes written: {body}"
    );
}

/// The page paints the bar from the row's data attributes before the first poll
/// lands, so an in-flight job has to render them.
#[tokio::test]
async fn settings_page_renders_progress_attributes_for_an_active_job() {
    let h = boot().await;
    let (session, _csrf) = login(&h).await;

    let progress = Arc::new(DownloadProgress::default());
    progress.begin(Some(19_600_000));
    progress.set_downloaded(4_900_000);
    seed_job(
        &h,
        "rendered-job",
        JobStage::Downloading,
        Some(Arc::clone(&progress)),
    )
    .await;

    let resp = get(&h, "/admin/settings", &session).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let html = body_text(resp).await;
    for expected in [
        r#"data-job-id="rendered-job""#,
        r#"data-stage-active="true""#,
        r#"data-stage="downloading""#,
        r#"data-downloaded="4900000""#,
        r#"data-total="19600000""#,
        r#"class="progress-bar""#,
    ] {
        assert!(html.contains(expected), "missing {expected} in page");
    }
}

// ─── Template rendering assertions ─────────────────────────────────────────────

#[tokio::test]
async fn settings_sidebar_active_highlight() {
    let h = boot().await;
    let (session, _csrf) = login(&h).await;
    let resp = get(&h, "/admin/settings", &session).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let html = body_text(resp).await;
    // The settings nav link must be active.
    assert!(
        html.contains("nav-link active") && html.contains("href=\"/admin/settings\""),
        "sidebar should have active link for /admin/settings"
    );
    // All four original links are still present.
    assert!(html.contains("href=\"/admin\""), "dashboard link missing");
    assert!(
        html.contains("href=\"/admin/inbound\""),
        "inbound link missing"
    );
    assert!(
        html.contains("href=\"/admin/clients\""),
        "clients link missing"
    );
    assert!(html.contains("href=\"/admin/logs\""), "logs link missing");
}

#[tokio::test]
async fn settings_form_has_csrf_field() {
    let h = boot().await;
    let (session, _csrf) = login(&h).await;
    let resp = get(&h, "/admin/settings", &session).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let html = body_text(resp).await;
    assert!(
        html.contains("type=\"hidden\" name=\"csrf\""),
        "form must contain hidden CSRF field"
    );
}

#[tokio::test]
async fn version_input_has_pattern_attribute() {
    let h = boot().await;
    let (session, _csrf) = login(&h).await;
    let resp = get(&h, "/admin/settings", &session).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let html = body_text(resp).await;
    assert!(
        html.contains(r#"pattern="^v?[0-9]+(\.[0-9]+){1,2}(-[\w.-]+)?$""#),
        "version input must have the specified pattern attribute"
    );
}

// ─── Background task lifecycle ────────────────────────────────────────────

/// Drive `trigger_update` and wait until the job reaches a terminal stage
/// (Succeeded or Failed) or the deadline passes. Returns the final JobStatus
/// snapshot from the map (cloned under the lock).
async fn wait_for_terminal(
    state: &AppState,
    job_id: &str,
    deadline: std::time::Duration,
) -> Option<JobStatus> {
    let start = std::time::Instant::now();
    loop {
        {
            let guard = state.update_jobs.lock().await;
            if let Some(s) = guard.get(job_id) {
                if matches!(s.stage, JobStage::Succeeded | JobStage::Failed) {
                    return Some(s.clone());
                }
            }
        }
        if start.elapsed() > deadline {
            return None;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn run_update_advances_to_failed_when_installer_unreachable() {
    // The harness's `xray_download_base` is set to http://127.0.0.1:1 (a
    // guaranteed-fast-fail address) so install_version returns Err inside
    // run_update. We assert the state machine reaches Failed through
    // Downloading — the same path a real network failure would take.
    let h = boot().await;
    let (session, csrf) = login(&h).await;

    let req = Request::builder()
        .method("POST")
        .uri("/admin/settings/xray/update")
        .header("cookie", format!("session={session}; csrf={csrf}"))
        .header("content-type", "application/x-www-form-urlencoded")
        .body(Body::from(format!("csrf={csrf}&version=")))
        .unwrap();
    let resp = h.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);

    // Find the job that was just inserted.
    let job_id = {
        let guard = h.state.update_jobs.lock().await;
        guard
            .values()
            .find(|j| {
                j.stage == JobStage::Queued
                    || j.stage == JobStage::Downloading
                    || j.stage == JobStage::Failed
            })
            .map(|j| j.id.clone())
            .expect("expected a freshly inserted job")
    };

    let terminal = wait_for_terminal(&h.state, &job_id, std::time::Duration::from_secs(5))
        .await
        .expect("background task did not reach terminal stage within 5s");
    assert_eq!(terminal.stage, JobStage::Failed);
    assert!(
        terminal.finished_at.is_some(),
        "finished_at must be set on Failed"
    );
    assert!(
        terminal.error_message.is_some(),
        "error_message must be set on Failed"
    );
}

#[tokio::test]
async fn run_update_pushes_failed_when_shutdown_initiated_before_download() {
    // Pre-set the shutdown flag so the run_update's pre-download check fires.
    let h = boot().await;
    h.state
        .shutdown_initiated
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let (session, csrf) = login(&h).await;

    let req = Request::builder()
        .method("POST")
        .uri("/admin/settings/xray/update")
        .header("cookie", format!("session={session}; csrf={csrf}"))
        .header("content-type", "application/x-www-form-urlencoded")
        .body(Body::from(format!("csrf={csrf}")))
        .unwrap();
    let resp = h.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);

    let job_id = {
        let guard = h.state.update_jobs.lock().await;
        guard
            .values()
            .find(|j| matches!(j.stage, JobStage::Queued | JobStage::Failed))
            .map(|j| j.id.clone())
            .expect("expected a freshly inserted job")
    };

    let terminal = wait_for_terminal(&h.state, &job_id, std::time::Duration::from_secs(3))
        .await
        .expect("background task did not reach terminal stage within 3s");
    assert_eq!(terminal.stage, JobStage::Failed);
    assert_eq!(terminal.message, "panel shutdown");
}
