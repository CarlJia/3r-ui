//! xray-core subprocess lifecycle management.
//!
//! `XraySupervisor` owns the xray child process: spawns it, supervises its
//! output, detects crashes via exit status, applies an exponential backoff
//! before respawning (reset after 5 minutes of stable running), and applies
//! config changes on request.
//!
//! There is no config-reload signal to send: xray ignores SIGUSR1 and dies on
//! SIGHUP (measured on 26.3.27). A `Reload` therefore compares the rendered
//! `config.json` against what the running child was started from and either
//! edits the user lists over the gRPC API — which leaves existing connections
//! alone — or restarts the child when the change is one xray cannot take at
//! runtime (a new inbound, a port, REALITY keys).
//!
//! Before each spawn the supervisor reaps any stale xray process whose PID
//! matches `${data_dir}/xray.pid`, so a `kill -9` of the panel cannot leave
//! a zombie xray holding the inbound port and the stats port.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, oneshot, watch, Mutex};
use tokio::time::{sleep, sleep_until};

use crate::error::{AppError, AppResult};
use crate::xray::config::API_ADDR;

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
}

#[cfg(unix)]
use signals::{is_alive, send, SIGKILL, SIGTERM};

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

// Stop, StopForReplace and Start have distinct semantics:
// - Stop: panel shutdown; kills xray and terminates the supervisor loop.
// - StopForReplace: xray binary replacement. Kills xray, keeps the loop alive
//   but holds xray *down* (no respawn) until Start, so the caller can swap the
//   file while nothing is executing it. This is the only ordering that works
//   on Windows, where the image of a running process cannot be overwritten at
//   all. The ack fires once the child is gone.
// - Start: spawn xray — after a StopForReplace, or as a restart on its own —
//   and report the outcome once the child has survived START_GRACE, so a
//   replacement that cannot even start is reported instead of being assumed
//   successful.
#[derive(Debug)]
pub enum SupervisorCommand {
    Reload,
    Stop,
    StopForReplace {
        ack: oneshot::Sender<()>,
    },
    Start {
        ack: oneshot::Sender<Result<(), String>>,
    },
}

/// How long a freshly spawned xray may stay silent before `Start` reports a
/// failure. xray prints its banner (and then its config line) to stdout within
/// milliseconds of starting, so silence means it did not really come up — a
/// hung binary, or one the scheduler has not run yet. Waiting longer than this
/// only delays the verdict the caller is waiting for.
const START_OUTPUT_TIMEOUT: Duration = Duration::from_secs(10);

/// Once it has produced output, xray must also stay alive this long before
/// `Start` is reported as successful. A broken config or missing assets make it
/// abort a few milliseconds after the banner (`Failed to start: …`, exit 23),
/// which is what this window catches. A process that is up but not serving is
/// the stats poll loop's business, not this check's.
const START_CONFIRM_AFTER_OUTPUT: Duration = Duration::from_millis(500);

#[derive(Debug, Default)]
struct SupervisorState {
    backoff_idx: usize,
    last_started: Option<Instant>,
    /// What the running child was started from — see [`SpawnedConfig`]. `None`
    /// when nothing is running, or when the config file could not be read at
    /// spawn time.
    spawned: Option<SpawnedConfig>,
}

/// The config a child was started from, reduced to what a live edit needs.
///
/// Two questions have to be answered when a `Reload` arrives: can this change
/// be applied to a running xray at all, and if so, which users have to be added
/// or removed. The first is a comparison of everything *except* the user lists
/// ([`structure`](Self::structure), with the client arrays emptied out); the
/// second is a comparison of those lists ([`users`](Self::users)).
#[derive(Clone, Debug, PartialEq)]
struct SpawnedConfig {
    /// The rendered config with every inbound's client list emptied.
    structure: Value,
    /// Inbound tag → (client email → client UUID).
    users: BTreeMap<String, BTreeMap<String, String>>,
}

impl SpawnedConfig {
    /// Read and reduce the config file, or `None` if it is missing or invalid.
    fn read(path: &Path) -> Option<Self> {
        let raw = std::fs::read_to_string(path).ok()?;
        let config: Value = serde_json::from_str(&raw).ok()?;
        Some(Self::of(&config))
    }

    fn of(config: &Value) -> Self {
        let mut structure = config.clone();
        let mut users = BTreeMap::new();

        if let Some(inbounds) = structure
            .get_mut("inbounds")
            .and_then(|inbounds| inbounds.as_array_mut())
        {
            for inbound in inbounds {
                let tag = inbound
                    .get("tag")
                    .and_then(|tag| tag.as_str())
                    .unwrap_or_default()
                    .to_string();
                let mut emails = BTreeMap::new();
                if let Some(clients) = inbound
                    .pointer_mut("/settings/clients")
                    .and_then(|clients| clients.as_array_mut())
                {
                    // Draining leaves an empty array behind, which is what
                    // keeps `structure` blind to the user list.
                    for client in clients.drain(..) {
                        let email = client
                            .get("email")
                            .and_then(|email| email.as_str())
                            .unwrap_or_default();
                        let uuid = client
                            .get("id")
                            .and_then(|uuid| uuid.as_str())
                            .unwrap_or_default();
                        if !email.is_empty() {
                            emails.insert(email.to_string(), uuid.to_string());
                        }
                    }
                }
                users.insert(tag, emails);
            }
        }

        Self { structure, users }
    }

    /// The user edits that turn what xray is running (`self`) into `next`.
    ///
    /// Removals come first: an email that changed UUID has to lose its old
    /// account before the new one can claim it, because xray rejects a second
    /// user with an email that is already in use. Inbounds present on only one
    /// side are a structural change — the caller restarts for those — so they
    /// are skipped here.
    fn user_changes(&self, next: &SpawnedConfig) -> Vec<UserChange> {
        let mut changes = Vec::new();
        for (tag, users) in &next.users {
            let Some(was) = self.users.get(tag) else {
                continue;
            };
            for (email, uuid) in was {
                if users.get(email) == Some(uuid) {
                    continue;
                }
                changes.push(UserChange::Remove {
                    tag: tag.clone(),
                    email: email.clone(),
                });
            }
            for (email, uuid) in users {
                if was.get(email) == Some(uuid) {
                    continue;
                }
                changes.push(UserChange::Add {
                    tag: tag.clone(),
                    email: email.clone(),
                    uuid: uuid.clone(),
                });
            }
        }
        changes
    }
}

/// One user edit to apply to a running xray.
#[derive(Clone, Debug, PartialEq, Eq)]
enum UserChange {
    Add {
        tag: String,
        email: String,
        uuid: String,
    },
    Remove {
        tag: String,
        email: String,
    },
}

/// What the supervisor is doing while no xray child is running.
#[derive(Debug, Clone, Copy)]
enum Idle {
    /// xray exited on its own: wait out the crash backoff, then spawn again.
    Backoff(Instant),
    /// xray was stopped so its binary could be replaced; nothing may bring it
    /// back up but a `Start` command.
    HeldForReplace,
}

pub struct XraySupervisor {
    xray_bin: PathBuf,
    config_path: PathBuf,
    pid_file: PathBuf,
    log_file: PathBuf,
    /// Where xray serves its gRPC API — the address live user edits go to.
    api_addr: String,
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
            api_addr: API_ADDR.to_string(),
            log_tx,
            state: Arc::new(Mutex::new(SupervisorState::default())),
        }
    }

    /// Dial a different gRPC API address. Tests point it at a mock
    /// HandlerService; production always uses [`API_ADDR`].
    pub fn with_api_addr(mut self, api_addr: impl Into<String>) -> Self {
        self.api_addr = api_addr.into();
        self
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
            // xray-core 的 logger 写 `time.Now()`（本地时间）且不开
            // `log.LUTC`。面板把日志时间戳按 UTC 解析，所以必须把 xray
            // 的 `TZ` 强制锁定为 UTC，否则 `parse_xray_timestamp` 会把
            // 接入主机时区偏移塞进 `logged_at`。
            .env("TZ", "UTC")
            // 内存调优：限制 Go 运行时内存上限并强制积极归还闲置堆内存给操作系统内核 (MADV_DONTNEED)
            .env("GOMEMLIMIT", "128MiB")
            .env("GODEBUG", "madvdontneed=1")
            // 两条流都要接管：xray 的 access/error 日志按配置写 **stdout**
            // （见 `xray::config` 的 `log` 段），而 Go 运行时的 panic 与
            // `log.Fatal` 走 **stderr**。只接管其中一条都会丢诊断信息。
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        cmd
    }

    async fn spawn(&self, first_output: watch::Sender<bool>) -> AppResult<Child> {
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

        // Forward both of xray's output streams to the rolling log file +
        // tracing, and (when the panel wired a receiver) to the access-log
        // parse-and-persist worker. Each stream also reports its first line on
        // `first_output`, which is how `Start` learns the child really ran.
        if let Some(stdout) = child.stdout.take() {
            forward_output(
                stdout,
                self.log_file.clone(),
                self.log_tx.clone(),
                first_output.clone(),
            );
        }
        if let Some(stderr) = child.stderr.take() {
            forward_output(
                stderr,
                self.log_file.clone(),
                self.log_tx.clone(),
                first_output,
            );
        }

        let mut state = self.state.lock().await;
        state.last_started = Some(Instant::now());
        // What this child is being started from: a later `Reload` diffs the
        // config file against it to decide between a live edit and a restart.
        state.spawned = SpawnedConfig::read(&self.config_path);
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

    /// Log an unexpected exit and report how long to wait before the next
    /// spawn.
    async fn backoff_after_crash(&self, status: std::process::ExitStatus) -> Duration {
        let delay = self.next_backoff().await;
        tracing::warn!(
            code = ?status.code(),
            ?delay,
            "xray exited unexpectedly; restarting after backoff"
        );
        delay
    }

    /// Apply the config on disk to the running child in place.
    ///
    /// User-list changes go through the gRPC API, which leaves every live
    /// connection alone; anything else — and any API failure — reports `false`,
    /// and the caller restarts, which applies the config as well.
    async fn apply_live_config(&self) -> bool {
        let Some(next) = SpawnedConfig::read(&self.config_path) else {
            tracing::warn!("xray config.json is unreadable; restarting to apply it");
            return false;
        };
        let previous = self.state.lock().await.spawned.clone();
        let Some(previous) = previous else {
            tracing::info!("no recorded xray config to diff against; restarting");
            return false;
        };
        if previous.structure != next.structure {
            tracing::info!("xray config changed structurally; restarting to apply it");
            return false;
        }

        let changes = previous.user_changes(&next);
        if changes.is_empty() {
            tracing::debug!("xray config reloaded with no user-list change");
            return true;
        }
        for change in &changes {
            let applied = match change {
                UserChange::Add { tag, email, uuid } => {
                    crate::xray::api::add_user(&self.api_addr, tag, uuid, email).await
                }
                UserChange::Remove { tag, email } => {
                    crate::xray::api::remove_user(&self.api_addr, tag, email).await
                }
            };
            if let Err(e) = applied {
                tracing::warn!(
                    error = %e,
                    "live xray user edit failed; restarting to apply the config"
                );
                return false;
            }
            tracing::info!(?change, "applied a user edit to the running xray");
        }

        self.state.lock().await.spawned = Some(next);
        true
    }

    /// Main supervisor loop. Spawns xray, waits for it to exit, applies the
    /// backoff policy, respawns. Terminates cleanly on `Stop` command or
    /// graceful xray exit (admin stop).
    ///
    /// While no child is running the loop still answers commands — see [`Idle`]
    /// — so a replacement or a stop never has to wait out a backoff timer.
    pub async fn run(self, mut rx: mpsc::Receiver<SupervisorCommand>) -> AppResult<()> {
        // Reap exactly once, before the first spawn. Inside the loop the pid
        // file describes our own live child, so reaping there would kill the
        // process we just started.
        self.reap_stale().await;

        // Set while no child is running — see [`Idle`].
        let mut idle: Option<Idle> = None;
        // Verdict owed to the most recent `Start`, resolved when the child
        // either survives START_GRACE or exits first.
        let mut start_ack: Option<oneshot::Sender<Result<(), String>>> = None;

        // Outer loop: spawn → wait → backoff → respawn
        'respawn: loop {
            if let Some(current) = idle {
                // Nothing is running. Commands are still answered here, and not
                // only after the timer: a backoff reaches a minute, which would
                // outlast a `StopForReplace` ack (15s in the update job) while
                // there was nothing running to stop in the first place. That is
                // precisely the crash-looping xray an operator reaches for
                // "update now" to repair.
                let wait = async {
                    match current {
                        Idle::Backoff(deadline) => sleep_until(deadline.into()).await,
                        // Held down for a replacement: only a command (or the
                        // panel shutting down) ends this state.
                        Idle::HeldForReplace => std::future::pending::<()>().await,
                    }
                };
                tokio::select! {
                    cmd = rx.recv() => {
                        match cmd {
                            None | Some(SupervisorCommand::Stop) => return Ok(()),
                            Some(SupervisorCommand::Start { ack }) => {
                                if let Some(superseded) = start_ack.replace(ack) {
                                    let _ = superseded
                                        .send(Err("superseded by a newer start request".into()));
                                }
                                idle = None;
                            }
                            Some(SupervisorCommand::StopForReplace { ack }) => {
                                // Already down: a second replacement request is
                                // a no-op but still has to ack, or its caller
                                // waits forever.
                                fail_pending_start(
                                    start_ack.take(),
                                    "xray stopped before the restart could be confirmed",
                                );
                                // The child crashed with the pid file left in
                                // place (see the exit path); a stale pid could
                                // name an unrelated process by the time the next
                                // panel run reaps it.
                                let _ = std::fs::remove_file(&self.pid_file);
                                idle = Some(Idle::HeldForReplace);
                                let _ = ack.send(());
                            }
                            Some(SupervisorCommand::Reload) => {
                                // Nothing is running to signal, and the next
                                // spawn reads the config file as it is by then.
                                tracing::debug!(
                                    "reload requested while xray is down; the next spawn reads the config"
                                );
                            }
                        }
                    }
                    _ = wait => {
                        idle = None;
                    }
                }
                continue;
            }

            let (output_tx, mut output_rx) = watch::channel(false);
            let mut child = match self.spawn(output_tx).await {
                Ok(c) => c,
                Err(e) => {
                    fail_pending_start(start_ack.take(), &format!("failed to spawn xray: {e}"));
                    tracing::error!(error = %e, "failed to spawn xray; backing off");
                    idle = Some(Idle::Backoff(Instant::now() + self.next_backoff().await));
                    continue;
                }
            };
            // Verdict deadline for a pending `Start`: it has to show output
            // before this fires, and then survive long enough afterwards to
            // clear `START_CONFIRM_AFTER_OUTPUT`, which is why the deadline is
            // pushed forward when the first line arrives.
            let mut verdict_deadline = Instant::now() + START_OUTPUT_TIMEOUT;
            let mut output_seen = false;
            // The output watch is one-shot: it must be taken out of the select
            // once it fires, because a closed channel reports Ready(Err) on
            // every poll and would otherwise spin the loop.
            let mut output_watch_armed = true;

            // Inner loop: keep this child handle alive across commands that do
            // not end its life — dropping it would trigger `kill_on_drop` and
            // take xray down. Only the terminal paths leave for `'respawn`.
            loop {
                tokio::select! {
                    biased;
                    cmd = rx.recv() => {
                        match cmd {
                            Some(SupervisorCommand::Stop) | None => {
                                fail_pending_start(
                                    start_ack.take(),
                                    "xray stopped before the restart could be confirmed",
                                );
                                stop_child(&mut child).await;
                                let _ = std::fs::remove_file(&self.pid_file);
                                return Ok(());
                            }
                            Some(SupervisorCommand::Start { ack }) => {
                                // Restart: exec the binary on disk now and give
                                // the caller a verdict on it.
                                if let Some(superseded) = start_ack.replace(ack) {
                                    let _ = superseded
                                        .send(Err("superseded by a newer start request".into()));
                                }
                                stop_child(&mut child).await;
                                continue 'respawn;
                            }
                            Some(SupervisorCommand::StopForReplace { ack }) => {
                                fail_pending_start(
                                    start_ack.take(),
                                    "xray stopped before the restart could be confirmed",
                                );
                                stop_child(&mut child).await;
                                // Drop the pid file: this child is gone on
                                // purpose, and a stale pid could name an
                                // unrelated process by the time the next panel
                                // run reaps it.
                                let _ = std::fs::remove_file(&self.pid_file);
                                idle = Some(Idle::HeldForReplace);
                                let _ = ack.send(());
                                continue 'respawn;
                            }
                            Some(SupervisorCommand::Reload) => {
                                if self.apply_live_config().await {
                                    continue;
                                }
                                tracing::info!("restarting xray to apply the new config");
                                stop_child(&mut child).await;
                                continue 'respawn;
                            }
                        }
                    }
                    status = child.wait() => {
                        let owed = start_ack.take();
                        // The pid file is left in place on an unexpected exit: the
                        // next spawn overwrites it, and keeping it lets an operator
                        // inspect the last known pid. It is only removed on a clean
                        // Stop.
                        match status {
                            Ok(s) if s.success() => {
                                fail_pending_start(
                                    owed,
                                    "xray exited cleanly before the restart could be confirmed",
                                );
                                let _ = std::fs::remove_file(&self.pid_file);
                                tracing::info!(code = ?s.code(), "xray exited cleanly");
                                return Ok(());
                            }
                            Ok(s) => {
                                fail_pending_start(
                                    owed,
                                    &format!("xray exited immediately (status {:?})", s.code()),
                                );
                                let delay = self.backoff_after_crash(s).await;
                                idle = Some(Idle::Backoff(Instant::now() + delay));
                            }
                            Err(e) => {
                                fail_pending_start(owed, &format!("xray wait failed: {e}"));
                                tracing::error!(error = %e, "xray wait failed; backing off");
                                let delay = self.next_backoff().await;
                                idle = Some(Idle::Backoff(Instant::now() + delay));
                            }
                        }
                        continue 'respawn;
                    }
                    // Reached only while the child is still running — an exit
                    // is observed by the branch above, which is polled first.
                    _ = sleep_until(verdict_deadline.into()), if start_ack.is_some() => {
                        if let Some(ack) = start_ack.take() {
                            let _ = ack.send(if output_seen {
                                Ok(())
                            } else {
                                Err(format!(
                                    "xray produced no output within {}s; it never came up",
                                    START_OUTPUT_TIMEOUT.as_secs()
                                ))
                            });
                        }
                    }
                    // The child wrote something, so it is really running; from
                    // here on it only has to stay alive briefly.
                    changed = output_rx.changed(), if output_watch_armed && start_ack.is_some() => {
                        output_watch_armed = false;
                        if changed.is_ok() {
                            output_seen = true;
                            verdict_deadline = Instant::now() + START_CONFIRM_AFTER_OUTPUT;
                        }
                    }
                }
            }
        }
    }
}

/// Resolve a pending `Start` with a failure, when its child died (or is being
/// stopped) before it could be confirmed as started.
fn fail_pending_start(ack: Option<oneshot::Sender<Result<(), String>>>, reason: &str) {
    if let Some(ack) = ack {
        let _ = ack.send(Err(reason.to_string()));
    }
}

async fn stop_child(child: &mut Child) {
    if let Err(e) = child.start_kill() {
        tracing::warn!(error = %e, "xray kill failed");
        let _ = child.kill().await;
    }
    let _ = child.wait().await;
}

/// Consume one of the child's output streams line by line:每行写入
/// `$DATA_DIR/xray.log`（access 行除外——它们由 logs worker 落库 SQLite，
/// 再写文件会在代理流量大时无界增长）、打进 tracing，并送进 access-log
/// 解析 worker。
///
/// 单个任务同时服务 stdout 与 stderr，所以两条流的行都走同一条判定路径。
fn forward_output<R>(
    stream: R,
    log_path: PathBuf,
    log_tx: Option<mpsc::Sender<String>>,
    first_output: watch::Sender<bool>,
) where
    R: AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        // Open the log file once, up front; the handle lives as long as this
        // task, so every read-line iteration can append.
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .ok();
        let mut reader = BufReader::new(stream);
        let mut buf = String::new();
        let mut dropped: u64 = 0;
        loop {
            buf.clear();
            match reader.read_line(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            // Tell a pending `Start` that the child is really running. `send`
            // is a no-op for the second and later lines.
            if !*first_output.borrow() {
                let _ = first_output.send(true);
            }
            let line = buf.trim_end();
            // 面板日志里 access 行降级为 debug，error/warning 行保持 warn。
            if crate::xray::logs::is_access_log_line(line) {
                tracing::debug!(target: "xray", "{}", line);
            } else {
                tracing::warn!(target: "xray", "{}", line);
                if let Some(f) = file.as_mut() {
                    use std::io::Write;
                    let _ = writeln!(f, "{}", line);
                }
            }
            if let Some(tx) = &log_tx {
                // 访问日志是监控数据，可容忍丢失：通道满时丢行计数打点，
                // 绝不反压到 xray 的输出管道拖住代理流量；接收端关闭
                // （面板 shutdown）才退出循环。
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

#[cfg(test)]
mod tests {
    use super::*;

    fn spawn_config(config: Value) -> SpawnedConfig {
        SpawnedConfig::of(&config)
    }

    fn config(clients: &[(&str, &str)]) -> Value {
        serde_json::json!({
            "inbounds": [{
                "tag": "inbound-1",
                "port": 8443,
                "settings": {
                    "clients": clients
                        .iter()
                        .map(|(uuid, email)| serde_json::json!({
                            "id": uuid, "email": email, "level": 0
                        }))
                        .collect::<Vec<_>>(),
                    "decryption": "none",
                },
            }],
        })
    }

    /// The structure comparison is what routes a reload to the API or to a
    /// restart, so it has to be blind to the user list and sensitive to
    /// everything else.
    #[test]
    fn structure_ignores_users_but_sees_everything_else() {
        let before = spawn_config(config(&[]));
        let after_users = spawn_config(config(&[("uuid-1", "user-1")]));
        assert_eq!(before.structure, after_users.structure);

        let mut moved_port = config(&[("uuid-1", "user-1")]);
        moved_port["inbounds"][0]["port"] = serde_json::json!(9443);
        assert_ne!(before.structure, spawn_config(moved_port).structure);
    }

    #[test]
    fn user_changes_report_additions_and_removals() {
        let before = spawn_config(config(&[("uuid-1", "user-1")]));
        let after = spawn_config(config(&[("uuid-1", "user-1"), ("uuid-2", "user-2")]));
        assert_eq!(
            before.user_changes(&after),
            vec![UserChange::Add {
                tag: "inbound-1".into(),
                email: "user-2".into(),
                uuid: "uuid-2".into(),
            }]
        );
        assert_eq!(
            after.user_changes(&before),
            vec![UserChange::Remove {
                tag: "inbound-1".into(),
                email: "user-2".into(),
            }]
        );
    }

    /// A rotated UUID keeps the email, so the old account has to go first:
    /// xray rejects a second user with an email that is already taken.
    #[test]
    fn a_rotated_uuid_removes_before_it_adds() {
        let before = spawn_config(config(&[("uuid-1", "user-1")]));
        let after = spawn_config(config(&[("uuid-2", "user-1")]));
        assert_eq!(
            before.user_changes(&after),
            vec![
                UserChange::Remove {
                    tag: "inbound-1".into(),
                    email: "user-1".into(),
                },
                UserChange::Add {
                    tag: "inbound-1".into(),
                    email: "user-1".into(),
                    uuid: "uuid-2".into(),
                },
            ]
        );
    }

    /// An inbound that appears or disappears is structural, not a user edit:
    /// reporting it as one would send the API a tag that does not exist there.
    #[test]
    fn user_changes_skip_inbounds_that_only_one_side_has() {
        let one = config(&[("uuid-1", "user-1")]);
        let mut two = one.clone();
        two["inbounds"].as_array_mut().unwrap().push(
            serde_json::json!({"tag": "inbound-2", "port": 9443, "settings": {"clients": []}}),
        );

        assert!(spawn_config(one.clone())
            .user_changes(&spawn_config(two.clone()))
            .is_empty());
        assert!(spawn_config(two)
            .user_changes(&spawn_config(one))
            .is_empty());
    }

    #[test]
    fn an_unreadable_or_missing_config_has_no_snapshot() {
        assert!(SpawnedConfig::read(Path::new("/nonexistent/config.json")).is_none());
    }
}
