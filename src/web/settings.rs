//! `/admin/settings` — system settings page and xray update handlers.
//!
//! The settings page is read-only and shows current xray binary path, download
//! base URL, and version. The update form triggers a background task that
//! downloads a new xray binary, stops xray, swaps the binary in, and starts it
//! again — reporting success only once the supervisor confirms the new binary
//! stayed up.

use askama::Template;
use axum::extract::{Extension, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Redirect, Response};
use axum_extra::extract::cookie::CookieJar;
use rand::Rng;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::oneshot;

use crate::crypto::session::now_unix;
use crate::web::middleware::{csrf_from_cookies, render_page, CurrentAdmin};
use crate::web::AppState;
use crate::xray::DownloadProgress;

/// Job stage in the update state machine.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub enum JobStage {
    Queued,
    Downloading,
    Downloaded,
    Restarting,
    Succeeded,
    Failed,
}

/// Status record for an in-flight or completed xray-update job.
#[derive(Clone, Debug)]
pub struct JobStatus {
    pub id: String,
    pub stage: JobStage,
    pub message: String,
    pub started_at: i64,
    pub finished_at: Option<i64>,
    pub installed_tag: Option<String>,
    pub error_message: Option<String>,
    /// Byte counters of the archive download, or `None` before it starts.
    ///
    /// Held rather than copied: the page polls the status endpoint, so reading
    /// the counters when the record is serialized always reports the bytes the
    /// transfer has actually reached. Copying them into the record on a timer
    /// instead would show whatever the last tick saw — including a bar that
    /// stops short of 100% when a fast download outruns the tick. The transfer
    /// stops writing when it returns, so finished rows keep their final size.
    pub progress: Option<Arc<DownloadProgress>>,
}

impl JobStatus {
    fn new(id: String, stage: JobStage, message: String) -> Self {
        Self {
            id,
            stage,
            message,
            started_at: now_unix(),
            finished_at: None,
            installed_tag: None,
            error_message: None,
            progress: None,
        }
    }
}

/// Whether a job stage is "active" (not a terminal state).
pub fn is_active(stage: &JobStage) -> bool {
    matches!(
        stage,
        JobStage::Queued | JobStage::Downloading | JobStage::Downloaded | JobStage::Restarting
    )
}

/// Generate a random 16-hex job ID.
fn new_job_id() -> String {
    format!("{:016x}", rand::thread_rng().gen::<u64>())
}

/// `GET /admin/settings` — renders the settings page.
pub async fn settings_page(
    State(state): State<AppState>,
    Extension(CurrentAdmin(admin)): Extension<CurrentAdmin>,
    jar: CookieJar,
) -> Response {
    let xray_bin =
        state
            .config
            .data_dir
            .join("xray")
            .join(if cfg!(windows) { "xray.exe" } else { "xray" });
    let download_base = state.config.xray_download_base.clone();
    let xray_log = state.config.data_dir.join("xray.log");
    let data_dir = state.config.data_dir.clone();

    // Run `xray --version` in a blocking task to get the installed version.
    let xray_bin_for_version = xray_bin.clone();
    let version_output = tokio::task::spawn_blocking(move || {
        let output = std::process::Command::new(&xray_bin_for_version)
            .arg("--version")
            .output();
        match output {
            Ok(o) if o.status.success() => {
                let first_line = String::from_utf8_lossy(&o.stdout)
                    .lines()
                    .next()
                    .unwrap_or("")
                    .trim()
                    .to_string();
                if first_line.is_empty() {
                    "未安装".to_string()
                } else {
                    first_line
                }
            }
            _ => "未安装".to_string(),
        }
    })
    .await
    .unwrap_or_else(|_| "未安装".to_string());

    // Collect job history sorted by started_at DESC (most recent first).
    let jobs: Vec<JobRowView> = {
        let guard = state.update_jobs.lock().await;
        let mut jobs: Vec<_> = guard.values().map(JobRowView::from_job).collect();
        jobs.sort_by_key(|b| std::cmp::Reverse(b.started_at));
        jobs
    };

    let csrf = csrf_from_cookies(&jar);

    let proxy_raw = crate::proxy::config::get_proxy_settings(&state.pool)
        .await
        .unwrap_or_else(|_| crate::db::models::ProxySettings {
            id: 1,
            enabled: 0,
            listen_port: 8443,
            domain: String::new(),
            cert_mode: "cf_dns".to_string(),
            cf_api_token: String::new(),
            cf_zone_id: String::new(),
            custom_cert_path: String::new(),
            custom_key_path: String::new(),
            cert_expires_at: None,
            last_renewed_at: None,
            last_error: None,
            updated_at: 0,
        });

    let cert_status_text = match proxy_raw.cert_expires_at {
        Some(exp) => {
            let now = now_unix();
            let remaining_days = (exp - now) / 86400;
            let iso = format_unix_time(exp);
            if remaining_days < 0 {
                format!("已过期 ({iso})")
            } else {
                format!("有效至 {iso} (剩余 {remaining_days} 天)")
            }
        }
        None => "未签发或未安装".to_string(),
    };

    let masked_token = proxy_raw.masked_cf_token();
    let proxy = ProxySettingsView {
        enabled: proxy_raw.is_enabled(),
        listen_port: proxy_raw.listen_port,
        domain: proxy_raw.domain,
        cert_mode: proxy_raw.cert_mode,
        masked_token,
        cf_zone_id: proxy_raw.cf_zone_id,
        custom_cert_path: proxy_raw.custom_cert_path,
        custom_key_path: proxy_raw.custom_key_path,
        cert_status_text,
        last_error: proxy_raw.last_error,
    };

    let certs_view = crate::proxy::cert::scan_certs_dir(&data_dir);

    let html = SettingsTemplate {
        csrf,
        username: admin.username.clone(),
        xray_bin: xray_bin.display().to_string(),
        download_base,
        xray_log: xray_log.display().to_string(),
        data_dir: data_dir.display().to_string(),
        xray_version: version_output,
        jobs,
        proxy,
        certs_view,
        form_action: "/admin/settings/xray/update".to_string(),
    }
    .render()
    .unwrap_or_else(|e| format!("template error: {e}"));

    render_page(jar, StatusCode::OK, html)
}

/// Format a unix timestamp as an ISO datetime string in UTC.
fn format_unix_time(ts: i64) -> String {
    crate::web::logs::timestamp_to_iso(ts)
}

#[derive(Template)]
#[template(path = "settings.html", escape = "html")]
struct SettingsTemplate {
    csrf: String,
    username: String,
    xray_bin: String,
    download_base: String,
    xray_log: String,
    data_dir: String,
    xray_version: String,
    jobs: Vec<JobRowView>,
    proxy: ProxySettingsView,
    certs_view: crate::proxy::cert::CertsDirView,
    form_action: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct ProxySettingsView {
    pub enabled: bool,
    pub listen_port: i64,
    pub domain: String,
    pub cert_mode: String,
    pub masked_token: String,
    pub cf_zone_id: String,
    pub custom_cert_path: String,
    pub custom_key_path: String,
    pub cert_status_text: String,
    pub last_error: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ProxySettingsForm {
    #[serde(default)]
    pub enabled: Option<String>,
    pub listen_port: u16,
    pub domain: String,
    pub cert_mode: String,
    pub cf_api_token: Option<String>,
    pub cf_zone_id: Option<String>,
    pub custom_cert_path: Option<String>,
    pub custom_key_path: Option<String>,
}

pub async fn save_proxy_settings(
    State(state): State<AppState>,
    Extension(_admin): Extension<CurrentAdmin>,
    axum::extract::Form(form): axum::extract::Form<ProxySettingsForm>,
) -> Response {
    let is_enabled = form.enabled.as_deref() == Some("on")
        || form.enabled.as_deref() == Some("true")
        || form.enabled.as_deref() == Some("1");
    let custom_cert = form.custom_cert_path.as_deref().unwrap_or_default();
    let custom_key = form.custom_key_path.as_deref().unwrap_or_default();

    if let Err(e) = crate::proxy::config::update_proxy_settings(
        &state.pool,
        is_enabled,
        form.listen_port,
        &form.domain,
        &form.cert_mode,
        form.cf_api_token.as_deref(),
        form.cf_zone_id.as_deref(),
        custom_cert,
        custom_key,
    )
    .await
    {
        tracing::error!(error = %e, "failed to update proxy settings");
    }

    let _ = state
        .proxy_tx
        .send(crate::proxy::ProxyCommand::Reload)
        .await;

    Redirect::to("/admin/settings").into_response()
}

pub async fn request_cert_now(
    State(state): State<AppState>,
    Extension(_admin): Extension<CurrentAdmin>,
    headers: axum::http::HeaderMap,
) -> Response {
    let settings = match crate::proxy::config::get_proxy_settings(&state.pool).await {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = %e, "failed to load proxy settings for cert request");
            if headers
                .get(axum::http::header::ACCEPT)
                .and_then(|v| v.to_str().ok())
                .map(|s| s.contains("application/json"))
                .unwrap_or(false)
            {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    axum::Json(serde_json::json!({ "error": "failed to load proxy settings" })),
                )
                    .into_response();
            }
            return Redirect::to("/admin/settings").into_response();
        }
    };

    if settings.domain.is_empty() || settings.cf_api_token.is_empty() {
        let _ = crate::proxy::config::update_cert_status(
            &state.pool,
            None,
            None,
            Some("申请失败：必须先配置域名和 Cloudflare API Token"),
        )
        .await;
        if headers
            .get(axum::http::header::ACCEPT)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.contains("application/json"))
            .unwrap_or(false)
        {
            return (
                StatusCode::BAD_REQUEST,
                axum::Json(serde_json::json!({ "error": "domain or cf_api_token missing" })),
            )
                .into_response();
        }
        return Redirect::to("/admin/settings").into_response();
    }

    let pool = state.pool.clone();
    let data_dir = state.config.data_dir.clone();
    let domain = settings.domain.clone();
    let cf_token = settings.cf_api_token.clone();
    let cf_zone_id = if settings.cf_zone_id.is_empty() {
        None
    } else {
        Some(settings.cf_zone_id.clone())
    };
    let proxy_tx = state.proxy_tx.clone();
    let cert_tracker = state.cert_tracker.clone();

    if cert_tracker.start(&domain).await {
        tokio::spawn(async move {
            match crate::proxy::acme::request_certificate(
                &pool,
                &data_dir,
                &domain,
                &cf_token,
                cf_zone_id.as_deref(),
                false,
                Some(&cert_tracker),
            )
            .await
            {
                Ok(_) => {
                    let _ = proxy_tx.send(crate::proxy::ProxyCommand::Reload).await;
                }
                Err(e) => {
                    let err_msg = format!("ACME 证书申请失败: {e}");
                    tracing::error!(%err_msg);
                    let _ =
                        crate::proxy::config::update_cert_status(&pool, None, None, Some(&err_msg))
                            .await;
                }
            }
        });
    }

    if headers
        .get(axum::http::header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.contains("application/json"))
        .unwrap_or(false)
    {
        return axum::Json(serde_json::json!({ "status": "ok" })).into_response();
    }

    Redirect::to("/admin/settings").into_response()
}

/// `GET /admin/settings/proxy/cert-status` — JSON polling endpoint for ACME logs & progress.
pub async fn cert_request_status(
    State(state): State<AppState>,
    Extension(_admin): Extension<CurrentAdmin>,
) -> Response {
    let status = state.cert_tracker.get_status().await;
    axum::Json(status).into_response()
}

/// A job row prepared for template rendering, with Chinese stage label.
///
/// This is also the shape the status endpoint returns: the polling page and the
/// server-rendered table then agree on every field, including the label it
/// renders in the badge.
#[derive(Clone, Serialize)]
pub struct JobRowView {
    pub id: String,
    pub stage: JobStage,
    pub stage_label: String,
    pub stage_key: String,
    pub message: String,
    pub started_at: i64,
    pub started_at_iso: String,
    pub finished_at: Option<i64>,
    pub is_active: bool,
    pub installed_tag: Option<String>,
    pub error_message: Option<String>,
    /// Bytes of the archive written so far.
    pub downloaded_bytes: u64,
    /// Size the server announced for the archive, or `None` while it has not
    /// been announced — the bar is indeterminate until then.
    pub total_bytes: Option<u64>,
}

impl JobRowView {
    pub fn from_job(job: &JobStatus) -> Self {
        let stage_key = match job.stage {
            JobStage::Queued => "queued",
            JobStage::Downloading => "downloading",
            JobStage::Downloaded => "downloaded",
            JobStage::Restarting => "restarting",
            JobStage::Succeeded => "succeeded",
            JobStage::Failed => "failed",
        };
        // Snapshot at render time, so a poll always sees the bytes the
        // download has reached (see `JobStatus::progress`).
        let (downloaded_bytes, total_bytes) = job
            .progress
            .as_ref()
            .map(|p| p.snapshot())
            .unwrap_or((0, None));
        Self {
            id: job.id.clone(),
            stage: job.stage.clone(),
            stage_label: stage_label(&job.stage),
            stage_key: stage_key.to_string(),
            message: job.message.clone(),
            started_at: job.started_at,
            started_at_iso: format_unix_time(job.started_at),
            finished_at: job.finished_at,
            is_active: is_active(&job.stage),
            installed_tag: job.installed_tag.clone(),
            error_message: job.error_message.clone(),
            downloaded_bytes,
            total_bytes,
        }
    }
}

/// Translate a JobStage to a Chinese human-readable label.
pub fn stage_label(stage: &JobStage) -> String {
    match stage {
        JobStage::Queued => "排队中".to_string(),
        JobStage::Downloading => "下载中".to_string(),
        JobStage::Downloaded => "已下载".to_string(),
        JobStage::Restarting => "重启中".to_string(),
        JobStage::Succeeded => "成功".to_string(),
        JobStage::Failed => "失败".to_string(),
    }
}

/// Form fields for the update trigger.
#[derive(Deserialize)]
pub struct UpdateForm {
    pub csrf: String,
    pub version: Option<String>,
}

/// `POST /admin/settings/xray/update` — start an xray update background task.
pub async fn trigger_update(
    State(state): State<AppState>,
    Extension(_admin): Extension<CurrentAdmin>,
    jar: CookieJar,
    axum::extract::Form(form): axum::extract::Form<UpdateForm>,
) -> Response {
    // CSRF check (same pattern as auth::change_password).
    let expected = csrf_from_cookies(&jar);
    if !crate::crypto::session::constant_time_eq(&expected, &form.csrf) {
        return (StatusCode::FORBIDDEN, "missing or invalid CSRF token").into_response();
    }

    let version = form.version.map(|v| v.trim().to_string());
    let version_opt = version
        .as_ref()
        .filter(|v| !v.is_empty())
        .map(|v| v.as_str());

    // Concurrent guard + insert under a single lock acquisition. Splitting
    // these into two locks opened a TOCTOU race where two near-simultaneous
    // POSTs could both pass the active-job check before either inserted.
    let job_id_for_spawn = {
        let mut guard = state.update_jobs.lock().await;
        let has_active = guard.values().any(|j| is_active(&j.stage));
        if has_active {
            let job_id = new_job_id();
            let mut job = JobStatus::new(
                job_id.clone(),
                JobStage::Failed,
                "已有更新任务进行中".to_string(),
            );
            job.finished_at = Some(now_unix());
            trim_and_insert(&mut guard, job_id, job);
            return Redirect::to("/admin/settings").into_response();
        }
        let job_id = new_job_id();
        let initial = JobStatus::new(job_id.clone(), JobStage::Queued, "排队中".to_string());
        trim_and_insert(&mut guard, job_id.clone(), initial);
        job_id
    };

    spawn_update(state, job_id_for_spawn, version_opt);
    Redirect::to("/admin/settings").into_response()
}

/// Trim history to 10 entries and insert a new job.
fn trim_and_insert(
    guard: &mut std::collections::HashMap<String, JobStatus>,
    id: String,
    job: JobStatus,
) {
    if guard.len() >= 10 {
        // Remove the oldest entry (lowest started_at).
        if let Some(oldest) = guard
            .values()
            .min_by_key(|j| j.started_at)
            .map(|j| j.id.clone())
        {
            guard.remove(&oldest);
        }
    }
    guard.insert(id, job);
}

/// Spawn a background update task.
pub fn spawn_update(state: AppState, job_id: String, version: Option<&str>) {
    let version_owned = version.map(|s| s.to_string());
    tokio::spawn(async move {
        run_update(state, job_id, version_owned).await;
    });
}

/// Background task that drives the update state machine.
async fn run_update(state: AppState, job_id: String, version: Option<String>) {
    let version_ref = version.as_deref();

    // The download publishes its byte counters here; the job record carries the
    // handle so the settings page can render a progress bar from it (see
    // `JobStatus::progress`).
    let progress = Arc::new(DownloadProgress::default());

    // Transition to Downloading.
    {
        let mut guard = state.update_jobs.lock().await;
        if let Some(job) = guard.get_mut(&job_id) {
            job.stage = JobStage::Downloading;
            job.message = "正在从下载源获取 xray-core".to_string();
            job.progress = Some(Arc::clone(&progress));
        }
    }

    // Check shutdown before downloading.
    if state
        .shutdown_initiated
        .load(std::sync::atomic::Ordering::SeqCst)
    {
        push_failed(&state, &job_id, "panel shutdown".to_string()).await;
        return;
    }

    // Download.
    let (staging_xray, tag) = match state
        .installer
        .install_version(&state.config.data_dir, version_ref, Some(&progress))
        .await
    {
        Ok((path, tag)) => (path, tag),
        Err(e) => {
            push_failed(&state, &job_id, e.to_string()).await;
            return;
        }
    };

    // Check shutdown before rename.
    if state
        .shutdown_initiated
        .load(std::sync::atomic::Ordering::SeqCst)
    {
        push_failed(&state, &job_id, "panel shutdown".to_string()).await;
        return;
    }

    // Transition to Downloaded.
    {
        let mut guard = state.update_jobs.lock().await;
        if let Some(job) = guard.get_mut(&job_id) {
            job.stage = JobStage::Downloaded;
            job.installed_tag = Some(tag.clone());
            job.message = format!("已下载 {}", tag);
        }
    }

    // Replace binary (and the assets xray loads from its own directory).
    let xray_dir = state.config.data_dir.join("xray");

    // Transition to Restarting.
    {
        let mut guard = state.update_jobs.lock().await;
        if let Some(job) = guard.get_mut(&job_id) {
            job.stage = JobStage::Restarting;
            job.message = "正在替换二进制并重启 xray".to_string();
        }
    }

    // Replace the binary with xray stopped: a running image cannot be
    // overwritten on Windows, and on every platform stopping first removes the
    // window in which the supervisor could respawn the install being replaced.
    if let Err(msg) = stop_xray_for_replace(&state).await {
        push_failed(&state, &job_id, msg).await;
        return;
    }

    if let Err(e) = crate::xray::installer::promote_staged_xray(&staging_xray, &xray_dir) {
        // Promote failed: clean up the staging dir we no longer intend to keep,
        // then report Failed. Without this, repeated failed renames leak
        // extracted xray binaries + the downloaded zip on disk.
        if let Some(parent) = staging_xray.parent() {
            let _ = std::fs::remove_dir_all(parent);
        }
        if let Ok(suffix) = crate::xray::installer::asset_suffix() {
            let _ = std::fs::remove_file(
                state
                    .config
                    .data_dir
                    .join("downloads")
                    .join(format!("Xray-{suffix}.zip")),
            );
        }
        // xray is down because of this replacement, so put the install back up
        // rather than leaving the panel without a proxy. The job still failed.
        if let Err(start_err) = start_xray(&state).await {
            tracing::error!(error = %start_err, "xray did not come back after a failed promote");
        }
        push_failed(&state, &job_id, format!("替换 xray 失败: {}", e)).await;
        return;
    }

    // Report success only after the supervisor confirms the new binary is up:
    // a replacement that exits at once (broken config, missing assets) has to
    // surface as a failure instead of a green "已替换并已重启".
    match start_xray(&state).await {
        Ok(()) => {
            let mut guard = state.update_jobs.lock().await;
            if let Some(job) = guard.get_mut(&job_id) {
                job.stage = JobStage::Succeeded;
                job.message = "已替换并已重启".to_string();
                job.finished_at = Some(now_unix());
            }
        }
        Err(msg) => {
            push_failed(&state, &job_id, format!("xray 已替换但未能启动: {msg}")).await;
        }
    }

    // Best-effort cleanup of staging dir and downloaded zip.
    let staging_dir = staging_xray.parent().map(|p| p.to_path_buf());
    let data_dir = state.config.data_dir.clone();
    let _ = tokio::task::spawn_blocking(move || {
        if let Some(dir) = staging_dir {
            if let Err(e) = std::fs::remove_dir_all(&dir) {
                tracing::warn!(
                    error = %e,
                    "failed to cleanup staging dir {}",
                    dir.display()
                );
            }
        }
        // Cleanup downloaded zip. Use the installer's own platform-suffix helper
        // so the same path matches across download + cleanup on every OS/arch.
        if let Ok(suffix) = crate::xray::installer::asset_suffix() {
            let zip_name = format!("Xray-{suffix}.zip");
            let zip_path = data_dir.join("downloads").join(&zip_name);
            let _ = std::fs::remove_file(&zip_path);
        }
    })
    .await;
}

/// How long the settings page waits for the supervisor to confirm that xray is
/// down, and then that it is up again. The supervisor answers within
/// milliseconds (the `Start` verdict costs one [`START_GRACE`]); the bound is
/// there so a wedged supervisor fails the job instead of hanging it.
const SUPERVISOR_ACK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// Stop xray and confirm the process is really gone, so its binary can be
/// replaced with nothing executing it.
async fn stop_xray_for_replace(state: &AppState) -> Result<(), String> {
    let (ack_tx, ack_rx) = oneshot::channel();
    if state
        .supervisor_tx
        .send(crate::xray::SupervisorCommand::StopForReplace { ack: ack_tx })
        .await
        .is_err()
    {
        return Err("supervisor gone".to_string());
    }
    match tokio::time::timeout(SUPERVISOR_ACK_TIMEOUT, ack_rx).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(_)) => Err("supervisor dropped the stop request".to_string()),
        Err(_) => Err("等待 xray 停止超时".to_string()),
    }
}

/// Spawn xray from the binary now on disk and report whether it came up. The
/// supervisor only answers `Ok` once the child has survived its start grace
/// period, so an install that exit-codes immediately is reported as a failure.
async fn start_xray(state: &AppState) -> Result<(), String> {
    let (ack_tx, ack_rx) = oneshot::channel();
    if state
        .supervisor_tx
        .send(crate::xray::SupervisorCommand::Start { ack: ack_tx })
        .await
        .is_err()
    {
        return Err("supervisor gone".to_string());
    }
    match tokio::time::timeout(SUPERVISOR_ACK_TIMEOUT, ack_rx).await {
        Ok(Ok(result)) => result,
        Ok(Err(_)) => Err("supervisor dropped the start request".to_string()),
        Err(_) => Err("等待 xray 启动超时".to_string()),
    }
}

/// Push a Failed status and return early.
async fn push_failed(state: &AppState, job_id: &str, message: String) {
    let now = now_unix();
    let mut guard = state.update_jobs.lock().await;
    if let Some(job) = guard.get_mut(job_id) {
        job.stage = JobStage::Failed;
        job.message = message.clone();
        job.error_message = Some(message);
        job.finished_at = Some(now);
    }
}

/// `GET /admin/settings/xray/status?job=<id>` — return job status as JSON.
///
/// The payload is the same [`JobRowView`] the table is rendered from, so a poll
/// can refresh every field the page shows — stage label, message, and the
/// download byte counters behind the progress bar.
pub async fn update_status(
    State(state): State<AppState>,
    Extension(_admin): Extension<CurrentAdmin>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Response {
    let job_id = match params.get("job") {
        Some(id) => id.clone(),
        None => "<unknown>".to_string(),
    };

    let view = {
        let guard = state.update_jobs.lock().await;
        guard.get(&job_id).map(JobRowView::from_job)
    };

    let view = view.unwrap_or_else(|| {
        let synth = JobStatus {
            id: job_id,
            stage: JobStage::Failed,
            message: "任务不存在或已清理".to_string(),
            started_at: now_unix(),
            finished_at: Some(now_unix()),
            installed_tag: None,
            error_message: None,
            progress: None,
        };
        JobRowView::from_job(&synth)
    });
    axum::Json(view).into_response()
}

/// `GET /admin/settings/certs` — return JSON view of certs directory
pub async fn certs_dir_json(
    State(state): State<AppState>,
    Extension(_admin): Extension<CurrentAdmin>,
) -> Response {
    let view = crate::proxy::cert::scan_certs_dir(&state.config.data_dir);
    (StatusCode::OK, axum::Json(view)).into_response()
}

/// Query parameters for downloading a public certificate
#[derive(Debug, Deserialize)]
pub struct CertDownloadQuery {
    pub domain: String,
}

/// `GET /admin/settings/certs/download` — download public certificate fullchain.pem
pub async fn cert_file_download(
    State(state): State<AppState>,
    Extension(_admin): Extension<CurrentAdmin>,
    Query(query): Query<CertDownloadQuery>,
) -> Response {
    let domain = query.domain.trim();
    // Safety check: no path traversal or empty domain
    if domain.is_empty() || domain.contains('/') || domain.contains('\\') || domain.contains("..") {
        return (StatusCode::BAD_REQUEST, "invalid domain").into_response();
    }

    let cp = crate::proxy::cert::cert_path(&state.config.data_dir, domain);
    if !cp.is_file() {
        return (StatusCode::NOT_FOUND, "certificate file not found").into_response();
    }

    match std::fs::read(&cp) {
        Ok(bytes) => {
            let filename = format!("fullchain-{domain}.pem");
            axum::response::Response::builder()
                .status(StatusCode::OK)
                .header(axum::http::header::CONTENT_TYPE, "application/x-pem-file")
                .header(
                    axum::http::header::CONTENT_DISPOSITION,
                    format!("attachment; filename=\"{filename}\""),
                )
                .body(axum::body::Body::from(bytes))
                .unwrap_or_else(|_| {
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "failed to build response",
                    )
                        .into_response()
                })
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("failed to read certificate: {e}"),
        )
            .into_response(),
    }
}
