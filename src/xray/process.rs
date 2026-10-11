//! xray-core subprocess lifecycle management.
//!
//! `XraySupervisor` owns the xray child process: spawns it, supervises its
//! stderr, detects crashes via exit status, applies an exponential backoff
//! before respawning (reset after 5 minutes of stable running), and accepts
//! reload commands (Unix: SIGUSR1; Windows / SIGUSR1 unavailable: gRPC
//! `ConfigService` — stub for U3, wired in U7 once tonic is in place).
//!
//! Before each spawn the supervisor reaps any stale xray process whose PID
//! matches `${data_dir}/xray.pid`, so a `kill -9` of the panel cannot leave
//! a zombie xray holding the inbound port and the stats port.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, Mutex};
use tokio::time::sleep;

use crate::error::{AppError, AppResult};

/// Signal helpers backed by `libc` directly — spawning a shell for every
/// signal is slow and, on macOS, `kill -0` against a process we do not own
/// can report EPERM, which reads as "not alive" and silently skips the reap.
#[cfg(unix)]
mod signals {
    use std::io;

    pub fn is_alive(pid: i32) -> bool {
        // Signal 0 performs the permission and existence checks only.
        // EPERM means the process exists but belongs to another user, which
        // for our purposes still counts as alive.
        if unsafe { libc::kill(pid, 0) } == 0 {
            true
        } else {
            io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
        }
    }

    pub fn send(pid: i32, signal: i32) -> io::Result<()> {
        if unsafe { libc::kill(pid, signal) } == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    pub const SIGTERM: i32 = libc::SIGTERM;
    pub const SIGKILL: i32 = libc::SIGKILL;
    pub const SIGUSR1: i32 = libc::SIGUSR1;
}

#[cfg(unix)]
use signals::{is_alive, send, SIGKILL, SIGTERM, SIGUSR1};

const BACKOFF: [Duration; 7] = [
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(4),
    Duration::from_secs(8),
    Duration::from_secs(16),
    Duration::from_secs(32),
    Duration::from_secs(60),
];
const STABLE_RESET_AFTER: Duration = Duration::from_secs(300);

#[derive(Debug, Clone)]
pub enum SupervisorCommand {
    Reload,
    Stop,
}

#[derive(Debug, Default)]
struct SupervisorState {
    backoff_idx: usize,
    last_started: Option<Instant>,
}

pub struct XraySupervisor {
    xray_bin: PathBuf,
    config_path: PathBuf,
    pid_file: PathBuf,
    log_file: PathBuf,
    /// 可选的 stderr 行通道：面板（lib.rs）把 `Some(tx)` 传进来，让
    /// `spawn` 把 xray 的每一行 stderr 同时送进 `src/xray/logs.rs` 做
    /// access-log 解析与落库。测试场景传 `None` 只写 tracing / 文件。
    log_tx: Option<mpsc::Sender<String>>,
    state: Arc<Mutex<SupervisorState>>,
}

impl XraySupervisor {
    pub fn new(
        xray_bin: PathBuf,
        config_path: PathBuf,
        data_dir: PathBuf,
        log_tx: Option<mpsc::Sender<String>>,
    ) -> Self {
        Self {
            xray_bin,
            config_path,
            pid_file: data_dir.join("xray.pid"),
            log_file: data_dir.join("xray.log"),
            log_tx,
            state: Arc::new(Mutex::new(SupervisorState::default())),
        }
    }

    /// Reap any xray instance whose PID is recorded in the pid file, so a
    /// prior panel crash that left xray alive cannot block the new bind on
    /// the inbound / stats ports.
    ///
    /// Called exactly once, before the first spawn. Later respawns must NOT
    /// reap: the pid file then describes *this* supervisor's own live child,
    /// and reaping it would kill the process we just started.
    async fn reap_stale(&self) {
        let Ok(pid_str) = std::fs::read_to_string(&self.pid_file) else {
            return;
        };
        let Ok(pid) = pid_str.trim().parse::<i32>() else {
            let _ = std::fs::remove_file(&self.pid_file);
            return;
        };

        #[cfg(unix)]
        {
            if is_alive(pid) {
                tracing::warn!(pid, "reaping stale xray from previous panel run");
                let _ = send(pid, SIGTERM);
                // Give it a grace period to shut down, then force it.
                for _ in 0..20 {
                    if !is_alive(pid) {
                        break;
                    }
                    sleep(Duration::from_millis(250)).await;
                }
                if is_alive(pid) {
                    let _ = send(pid, SIGKILL);
                }
            }
        }
        let _ = std::fs::remove_file(&self.pid_file);
    }

    fn build_command(&self) -> Command {
        let mut cmd = Command::new(&self.xray_bin);
        cmd.arg("run")
            .arg("-c")
            .arg(&self.config_path)
            // xray-core 的 stderr logger 写 `time.Now()`（本地时间）且不开
            // `log.LUTC`。面板把日志时间戳按 UTC 解析，所以必须把 xray
            // 的 `TZ` 强制锁定为 UTC，否则 `parse_xray_timestamp` 会把
            // 接入主机时区偏移塞进 `logged_at`。
            .env("TZ", "UTC")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        cmd
    }

    async fn spawn(&self) -> AppResult<Child> {
        let mut child = self.build_command().spawn().map_err(AppError::Io)?;

        // Record PID for orphan reaping. On Unix tokio fills the pid at spawn
        // time, so this is available without an intervening await.
        match child.id() {
            Some(pid) => {
                if let Err(e) = std::fs::write(&self.pid_file, pid.to_string()) {
                    tracing::warn!(error = %e, "failed to write xray pid file");
                }
            }
            None => {
                // Fallback: let the process table tell us. `id()` is None only
                // when the child already exited; log loudly so the crash is
                // visible rather than silently disabling orphan reaping.
                tracing::error!(
                    xray_bin = %self.xray_bin.display(),
                    "xray exited before its pid could be recorded; orphan reaping disabled for this spawn"
                );
            }
        }

        // Forward stderr to the rolling log file + tracing, and (when the
        // panel wired a receiver) to the access-log parse-and-persist worker.
        if let Some(stderr) = child.stderr.take() {
            let log_path = self.log_file.clone();
            let log_tx = self.log_tx.clone();
            tokio::spawn(async move {
                // Open the log file once, up front; the handle lives as long
                // as this task, so every read-line iteration can append.
                let mut file = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&log_path)
                    .ok();
                let mut reader = BufReader::new(stderr);
                let mut buf = String::new();
                let mut dropped: u64 = 0;
                loop {
                    buf.clear();
                    match reader.read_line(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {}
                    }
                    let line = buf.trim_end();
                    let access = crate::xray::logs::is_access_log_line(line);
                    // access 行（连接请求日志）已由 logs worker 落库到
                    // SQLite；写入 xray.log 会在代理流量大时无界增长——跳过。
                    // 面板日志里 access 行降级为 debug，error/warning 行
                    // 保持 warn。
                    if access {
                        tracing::debug!(target: "xray", "{}", line);
                    } else {
                        tracing::warn!(target: "xray", "{}", line);
                        if let Some(f) = file.as_mut() {
                            use std::io::Write;
                            let _ = writeln!(f, "{}", line);
                        }
                    }
                    if let Some(tx) = &log_tx {
                        // 访问日志是监控数据，可容忍丢失：通道满时丢行计数
                        // 打点，绝不反压到 xray 的 stderr 管道拖住代理流量；
                        // 接收端关闭（面板 shutdown）才退出循环。
                        match tx.try_send(line.to_string()) {
                            Ok(()) => {}
                            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                                dropped += 1;
                                if dropped % 1024 == 1 {
                                    tracing::warn!(
                                        dropped,
                                        "log receiver full; dropping xray access lines"
                                    );
                                }
                            }
                            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => break,
                        }
                    }
                }
            });
        }

        let mut state = self.state.lock().await;
        state.last_started = Some(Instant::now());
        Ok(child)
    }

    async fn next_backoff(&self) -> Duration {
        let mut state = self.state.lock().await;
        if let Some(started) = state.last_started {
            if started.elapsed() >= STABLE_RESET_AFTER {
                state.backoff_idx = 0;
            }
        }
        let idx = state.backoff_idx.min(BACKOFF.len() - 1);
        let delay = BACKOFF[idx];
        state.backoff_idx = (state.backoff_idx + 1).min(BACKOFF.len() - 1);
        delay
    }

    async fn wait_and_backoff(&self, status: std::process::ExitStatus) {
        let delay = self.next_backoff().await;
        tracing::warn!(
            code = ?status.code(),
            ?delay,
            "xray exited unexpectedly; restarting after backoff"
        );
        sleep(delay).await;
    }

    /// Send SIGUSR1 to reload xray's config in place. Returns Ok on Unix
    /// unconditionally (best-effort); on non-Unix returns
    /// `AppError::NotImplemented` because the gRPC ConfigService fallback
    /// requires the stats client from U7.
    pub async fn reload(&self) -> AppResult<()> {
        let pid_str = std::fs::read_to_string(&self.pid_file).map_err(AppError::Io)?;
        let pid: i32 = pid_str
            .trim()
            .parse()
            .map_err(|e| AppError::Xray(format!("xray pid parse failed: {e}")))?;

        #[cfg(unix)]
        {
            send(pid, SIGUSR1).map_err(|e| {
                AppError::Xray(format!("xray SIGUSR1 (reload) failed for pid {pid}: {e}"))
            })?;
            tracing::info!(pid, "xray reload signal sent");
            Ok(())
        }

        #[cfg(not(unix))]
        {
            let _ = pid;
            Err(AppError::NotImplemented(
                "xray reload via gRPC ConfigService; wired in U7".into(),
            ))
        }
    }

    /// Main supervisor loop. Spawns xray, waits for it to exit, applies the
    /// backoff policy, respawns. Terminates cleanly on `Stop` command or
    /// graceful xray exit (admin stop).
    pub async fn run(self, mut rx: mpsc::Receiver<SupervisorCommand>) -> AppResult<()> {
        // Reap exactly once, before the first spawn. Inside the loop the pid
        // file describes our own live child, so reaping there would kill the
        // process we just started.
        self.reap_stale().await;

        // Outer loop: spawn → wait → backoff → respawn
        loop {
            let mut child = match self.spawn().await {
                Ok(c) => c,
                Err(e) => {
                    tracing::error!(error = %e, "failed to spawn xray; backing off");
                    sleep(self.next_backoff().await).await;
                    continue;
                }
            };

            tokio::select! {
                biased;
                cmd = rx.recv() => {
                    match cmd {
                        Some(SupervisorCommand::Stop) | None => {
                            stop_child(&mut child).await;
                            let _ = std::fs::remove_file(&self.pid_file);
                            return Ok(());
                        }
                        Some(SupervisorCommand::Reload) => {
                            if let Err(e) = self.reload().await {
                                tracing::warn!(error = %e, "xray reload failed");
                            }
                        }
                    }
                }
                status = child.wait() => {
                    // The pid file is left in place on an unexpected exit: the
                    // next spawn overwrites it, and keeping it lets an operator
                    // inspect the last known pid. It is only removed on a clean
                    // Stop.
                    match status {
                        Ok(s) if s.success() => {
                            let _ = std::fs::remove_file(&self.pid_file);
                            tracing::info!(code = ?s.code(), "xray exited cleanly");
                            return Ok(());
                        }
                        Ok(s) => {
                            self.wait_and_backoff(s).await;
                        }
                        Err(e) => {
                            tracing::error!(error = %e, "xray wait failed; backing off");
                            sleep(self.next_backoff().await).await;
                        }
                    }
                }
            }
        }
    }
}

async fn stop_child(child: &mut Child) {
    if let Err(e) = child.start_kill() {
        tracing::warn!(error = %e, "xray kill failed");
        let _ = child.kill().await;
    }
    let _ = child.wait().await;
}
