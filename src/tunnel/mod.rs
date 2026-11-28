//! Cloudflare Tunnels (cloudflared) process management and supervision.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, Mutex, RwLock};
use tracing::{error, info, warn};

use crate::db::models::TunnelSettings;
use crate::error::{AppError, AppResult};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TunnelCommand {
    Reload,
    Restart,
    Stop,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TunnelStatus {
    pub installed: bool,
    pub bin_path: String,
    pub running: bool,
    pub is_external: bool,
    pub pid: Option<u32>,
    pub last_status: String,
    pub logs: Vec<String>,
}

/// 查找系统中可用的 cloudflared 二进制路径
pub fn find_cloudflared_bin(custom_path: &str) -> Option<PathBuf> {
    if !custom_path.trim().is_empty() {
        let p = PathBuf::from(custom_path.trim());
        if p.is_file() {
            return Some(p);
        }
    }

    let default_paths = [
        "/usr/local/bin/cloudflared",
        "/usr/bin/cloudflared",
        "/opt/homebrew/bin/cloudflared",
    ];

    for path in &default_paths {
        let p = Path::new(path);
        if p.is_file() {
            return Some(p.to_path_buf());
        }
    }

    // 尝试从 PATH 中寻找
    if let Ok(path_var) = std::env::var("PATH") {
        for dir in std::env::split_paths(&path_var) {
            let candidate = dir.join("cloudflared");
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }

    None
}

/// 查找宿主机中正在运行的独立或系统级 cloudflared 进程（非当前实例子进程）
pub fn find_system_cloudflared_pid(exclude_pid: Option<u32>) -> Option<u32> {
    // 1. 优先使用 pgrep 探测宿主机进程
    if let Ok(output) = std::process::Command::new("pgrep")
        .arg("-x")
        .arg("cloudflared")
        .output()
    {
        if output.status.success() {
            let s = String::from_utf8_lossy(&output.stdout);
            for line in s.lines() {
                if let Ok(pid) = line.trim().parse::<u32>() {
                    if Some(pid) == exclude_pid {
                        continue;
                    }
                    #[cfg(unix)]
                    if unsafe { libc::kill(pid as i32, 0) } == 0 {
                        return Some(pid);
                    }
                    #[cfg(not(unix))]
                    return Some(pid);
                }
            }
        }
    }

    // 2. Linux 下遍历 /proc 作为兜底探测（适用于精简系统无 pgrep 的场景）
    #[cfg(target_os = "linux")]
    {
        if let Ok(entries) = std::fs::read_dir("/proc") {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name_str = name.to_string_lossy();
                if let Ok(pid) = name_str.parse::<u32>() {
                    if Some(pid) == exclude_pid {
                        continue;
                    }
                    let comm_path = entry.path().join("comm");
                    if let Ok(comm) = std::fs::read_to_string(comm_path) {
                        if comm.trim() == "cloudflared" {
                            if unsafe { libc::kill(pid as i32, 0) } == 0 {
                                return Some(pid);
                            }
                        }
                    }
                }
            }
        }
    }

    None
}

pub struct CloudflaredSupervisor {
    pool: sqlx::SqlitePool,
    pid_file: PathBuf,
    current_child: Arc<Mutex<Option<Child>>>,
    recent_logs: Arc<RwLock<VecDeque<String>>>,
}

impl CloudflaredSupervisor {
    pub fn new(pool: sqlx::SqlitePool, data_dir: &Path) -> Self {
        Self {
            pool,
            pid_file: data_dir.join("cloudflared.pid"),
            current_child: Arc::new(Mutex::new(None)),
            recent_logs: Arc::new(RwLock::new(VecDeque::with_capacity(100))),
        }
    }

    pub async fn append_log(&self, line: String) {
        let mut logs = self.recent_logs.write().await;
        if logs.len() >= 100 {
            logs.pop_front();
        }
        logs.push_back(line);
    }

    pub async fn get_logs(&self) -> Vec<String> {
        let logs = self.recent_logs.read().await;
        logs.iter().cloned().collect()
    }

    pub async fn is_running(&self) -> bool {
        let mut guard = self.current_child.lock().await;
        if let Some(child) = guard.as_mut() {
            matches!(child.try_wait(), Ok(None))
        } else {
            false
        }
    }

    pub async fn get_pid(&self) -> Option<u32> {
        let mut guard = self.current_child.lock().await;
        if let Some(child) = guard.as_mut() {
            match child.try_wait() {
                Ok(None) => child.id(),
                _ => None,
            }
        } else {
            None
        }
    }

    /// 查询当前整体状态
    pub async fn status(&self) -> AppResult<TunnelStatus> {
        let settings = self.get_settings().await?;
        let bin = find_cloudflared_bin(&settings.custom_path);
        let installed = bin.is_some();
        let bin_path = bin
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();

        let (running, is_external, pid) = if self.is_running().await {
            (true, false, self.get_pid().await)
        } else if let Some(sys_pid) = find_system_cloudflared_pid(None) {
            (true, true, Some(sys_pid))
        } else {
            (false, false, None)
        };

        let mut logs = self.get_logs().await;
        if is_external && logs.is_empty() {
            logs.push(format!(
                "[3r-ui] 提示: 宿主机检测到正在运行的独立 cloudflared 系统服务 (PID: {})。如需查看日志，可在终端执行: journalctl -u cloudflared -f",
                pid.unwrap_or(0)
            ));
        }

        let last_status = if is_external && settings.last_status.is_empty() {
            "System Service Running".to_string()
        } else {
            settings.last_status
        };

        Ok(TunnelStatus {
            installed,
            bin_path,
            running,
            is_external,
            pid,
            last_status,
            logs,
        })
    }

    pub async fn get_settings(&self) -> AppResult<TunnelSettings> {
        let row = sqlx::query_as::<_, TunnelSettings>(
            "SELECT id, enabled, tunnel_token, custom_path, last_status, updated_at FROM tunnel_settings WHERE id = 1"
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(AppError::Database)?;

        Ok(row.unwrap_or_else(|| TunnelSettings {
            id: 1,
            enabled: 0,
            tunnel_token: String::new(),
            custom_path: String::new(),
            last_status: String::new(),
            updated_at: 0,
        }))
    }

    pub async fn update_last_status(&self, status: &str) {
        let _ = sqlx::query(
            "UPDATE tunnel_settings SET last_status = $1, updated_at = unixepoch() WHERE id = 1",
        )
        .bind(status)
        .execute(&self.pool)
        .await;
    }

    /// 清理残留的孤儿进程
    pub async fn reap_stale(&self) {
        let Ok(pid_str) = std::fs::read_to_string(&self.pid_file) else {
            return;
        };
        let Ok(pid) = pid_str.trim().parse::<i32>() else {
            let _ = std::fs::remove_file(&self.pid_file);
            return;
        };

        #[cfg(unix)]
        {
            if unsafe { libc::kill(pid, 0) } == 0 {
                info!(pid, "reaping stale cloudflared daemon");
                let _ = unsafe { libc::kill(pid, libc::SIGTERM) };
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                if unsafe { libc::kill(pid, 0) } == 0 {
                    let _ = unsafe { libc::kill(pid, libc::SIGKILL) };
                }
            }
        }
        let _ = std::fs::remove_file(&self.pid_file);
    }

    /// 启动 cloudflared 子进程
    pub async fn spawn(&self) -> AppResult<()> {
        let settings = self.get_settings().await?;
        if !settings.is_enabled() {
            info!("Cloudflare Tunnel is disabled in settings; not starting");
            return Ok(());
        }

        let token = settings.tunnel_token.trim();
        if token.is_empty() {
            warn!("Cloudflare Tunnel is enabled but token is empty; skipping start");
            self.update_last_status("Error: Tunnel Token is empty")
                .await;
            return Ok(());
        }

        let Some(bin_path) = find_cloudflared_bin(&settings.custom_path) else {
            let msg = "Error: cloudflared binary not found on host";
            warn!(msg);
            self.append_log(format!("[3r-ui] {msg}")).await;
            self.update_last_status(msg).await;
            return Ok(());
        };

        // 检查宿主机是否已有外部运行中的 cloudflared，防止双实例启动冲突
        let my_pid = self.get_pid().await;
        if let Some(sys_pid) = find_system_cloudflared_pid(my_pid) {
            warn!(
                sys_pid,
                "an external cloudflared process is already running on host; avoiding duplicate spawn"
            );
            self.append_log(format!(
                "[3r-ui] 警告: 宿主机已有 cloudflared 实例正在运行 (PID: {sys_pid})。为避免双实例冲突，未启动新进程。如需由面板接管，请先停止系统服务 (sudo systemctl disable --now cloudflared)。"
            ))
            .await;
            self.update_last_status("External Service Running").await;
            return Ok(());
        }

        info!(
            bin = %bin_path.display(),
            "spawning cloudflared tunnel daemon"
        );
        self.append_log(format!(
            "[3r-ui] Starting cloudflared tunnel daemon (bin: {})",
            bin_path.display()
        ))
        .await;

        let mut cmd = Command::new(&bin_path);
        cmd.arg("tunnel")
            .arg("--no-autoupdate")
            .arg("run")
            .arg("--token")
            .arg(token)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);

        let mut child = cmd.spawn().map_err(AppError::Io)?;
        let child_pid = child.id();

        if let Some(pid) = child_pid {
            let _ = std::fs::write(&self.pid_file, pid.to_string());
        }

        // 异步消费 stderr 日志行
        if let Some(stderr) = child.stderr.take() {
            let supervisor_logs = self.recent_logs.clone();
            tokio::spawn(async move {
                let reader = BufReader::new(stderr);
                let mut lines = reader.lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let mut logs = supervisor_logs.write().await;
                    if logs.len() >= 100 {
                        logs.pop_front();
                    }
                    logs.push_back(line);
                }
            });
        }

        let mut guard = self.current_child.lock().await;
        *guard = Some(child);
        self.update_last_status("Running").await;
        info!(pid = ?child_pid, "cloudflared tunnel daemon spawned successfully");

        Ok(())
    }

    /// 停止运行中的进程
    pub async fn stop(&self) {
        let mut guard = self.current_child.lock().await;
        if let Some(mut child) = guard.take() {
            info!("stopping cloudflared tunnel daemon");
            let _ = child.kill().await;
            self.append_log("[3r-ui] Cloudflare tunnel daemon stopped".to_string())
                .await;
        } else if let Some(sys_pid) = find_system_cloudflared_pid(None) {
            warn!(
                sys_pid,
                "attempted to stop external system cloudflared service from panel"
            );
            self.append_log(format!(
                "[3r-ui] 提示: 当前 cloudflared 由宿主机系统服务运行 (PID: {sys_pid})，面板未直接接管其进程生命周期。如需停止该服务，请在终端执行: sudo systemctl stop cloudflared"
            ))
            .await;
        }
        let _ = std::fs::remove_file(&self.pid_file);
        self.update_last_status("Stopped").await;
    }

    /// 重新载入配置并重启
    pub async fn reload(&self) -> AppResult<()> {
        self.stop().await;
        let settings = self.get_settings().await?;
        if settings.is_enabled() {
            self.spawn().await?;
        }
        Ok(())
    }

    /// 运行守护 actor 循环
    pub async fn run(self: Arc<Self>, mut rx: mpsc::Receiver<TunnelCommand>) {
        self.reap_stale().await;
        if let Err(e) = self.spawn().await {
            error!(error = %e, "failed to start cloudflared tunnel daemon");
        }

        while let Some(cmd) = rx.recv().await {
            match cmd {
                TunnelCommand::Reload => {
                    let _ = self.reload().await;
                }
                TunnelCommand::Restart => {
                    self.stop().await;
                    let _ = self.spawn().await;
                }
                TunnelCommand::Stop => {
                    self.stop().await;
                }
            }
        }

        self.stop().await;
        info!("cloudflared tunnel supervisor loop exited");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_find_cloudflared_bin_custom() {
        let custom = "/nonexistent/cloudflared";
        assert!(find_cloudflared_bin(custom).is_none());
    }

    #[tokio::test]
    async fn test_supervisor_logs_ring_buffer() {
        let dir = tempfile::tempdir().unwrap();
        // create in-memory sqlite pool
        let pool = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::query(
            "CREATE TABLE tunnel_settings (id INTEGER PRIMARY KEY, enabled INTEGER, tunnel_token TEXT, custom_path TEXT, last_status TEXT, updated_at INTEGER);"
        ).execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO tunnel_settings VALUES (1, 0, '', '', '', 0);")
            .execute(&pool)
            .await
            .unwrap();

        let sup = CloudflaredSupervisor::new(pool, dir.path());
        assert!(!sup.is_running().await);

        for i in 0..120 {
            sup.append_log(format!("line {i}")).await;
        }

        let logs = sup.get_logs().await;
        assert_eq!(logs.len(), 100);
        assert_eq!(logs[0], "line 20");
        assert_eq!(logs[99], "line 119");
    }

    #[tokio::test]
    async fn test_supervisor_status_structure() {
        let dir = tempfile::tempdir().unwrap();
        let pool = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::query(
            "CREATE TABLE tunnel_settings (id INTEGER PRIMARY KEY, enabled INTEGER, tunnel_token TEXT, custom_path TEXT, last_status TEXT, updated_at INTEGER);"
        ).execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO tunnel_settings VALUES (1, 0, '', '', '', 0);")
            .execute(&pool)
            .await
            .unwrap();

        let sup = CloudflaredSupervisor::new(pool, dir.path());
        let status = sup.status().await.unwrap();
        // Initially no child spawned
        assert!(!sup.is_running().await);
        // Status structure has is_external field
        assert_eq!(status.running, status.is_external);
    }
}
