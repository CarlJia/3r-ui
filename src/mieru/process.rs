//! Mieru daemon supervisor.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, Mutex};
use tokio::time::sleep;

use crate::error::{AppError, AppResult};

#[cfg(unix)]
mod signals {
    use std::io;

    pub fn is_alive(pid: i32) -> bool {
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

#[cfg(not(unix))]
fn is_alive(_pid: i32) -> bool {
    false
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MieruCommand {
    Reload,
    Restart,
    Stop,
}

pub struct MieruSupervisor {
    pub mieru_bin: PathBuf,
    pub config_path: PathBuf,
    pub pid_file: PathBuf,
    pub log_file: PathBuf,
    current_child: Arc<Mutex<Option<Child>>>,
}

impl MieruSupervisor {
    pub fn new(mieru_bin: PathBuf, config_path: PathBuf, data_dir: &Path) -> Self {
        Self {
            mieru_bin,
            config_path,
            pid_file: data_dir.join("mieru.pid"),
            log_file: data_dir.join("mieru.log"),
            current_child: Arc::new(Mutex::new(None)),
        }
    }

    /// Reap stale Mieru / Mita process recorded in pid_file.
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
            if is_alive(pid) {
                tracing::warn!(pid, "reaping stale mieru process from previous run");
                let _ = send(pid, SIGTERM);
                for _ in 0..15 {
                    if !is_alive(pid) {
                        break;
                    }
                    sleep(Duration::from_millis(150)).await;
                }
                if is_alive(pid) {
                    let _ = send(pid, SIGKILL);
                }
            }
        }
        let _ = std::fs::remove_file(&self.pid_file);
    }

    /// Apply configuration to Mieru / Mita CLI before starting daemon.
    pub async fn apply_config(&self) -> AppResult<()> {
        if !self.config_path.exists() {
            return Ok(());
        }

        let mut cmd = Command::new(&self.mieru_bin);
        cmd.arg("apply")
            .arg("config")
            .arg(&self.config_path)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());

        let _ = cmd.status().await;
        Ok(())
    }

    /// Spawn the Mieru server daemon loop.
    pub async fn spawn(&self) -> AppResult<()> {
        self.apply_config().await?;

        // If config doesn't exist, don't spawn
        if !self.config_path.exists() {
            return Ok(());
        }

        let mut cmd = Command::new(&self.mieru_bin);
        cmd.arg("run")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);

        let child = cmd.spawn().map_err(AppError::Io)?;
        if let Some(pid) = child.id() {
            let _ = std::fs::write(&self.pid_file, pid.to_string());
        }

        let mut guard = self.current_child.lock().await;
        *guard = Some(child);
        Ok(())
    }

    /// Stop the active Mieru process.
    pub async fn stop(&self) {
        let mut guard = self.current_child.lock().await;
        if let Some(mut child) = guard.take() {
            let _ = child.kill().await;
        }
        let _ = std::fs::remove_file(&self.pid_file);
    }

    /// Reload or restart Mieru with updated configuration.
    pub async fn reload(&self) -> AppResult<()> {
        if !self.config_path.exists() {
            self.stop().await;
            return Ok(());
        }

        // Try applying config dynamically first
        let _ = self.apply_config().await;

        // Check if alive, else spawn
        let mut needs_spawn = false;
        {
            let mut guard = self.current_child.lock().await;
            if let Some(ref mut child) = *guard {
                if let Ok(Some(_)) = child.try_wait() {
                    needs_spawn = true;
                }
            } else {
                needs_spawn = true;
            }
        }

        if needs_spawn {
            self.stop().await;
            self.spawn().await?;
        }

        Ok(())
    }

    /// Actor runner handling supervisor commands.
    pub async fn run(self: Arc<Self>, mut rx: mpsc::Receiver<MieruCommand>) {
        self.reap_stale().await;
        let _ = self.spawn().await;

        while let Some(cmd) = rx.recv().await {
            match cmd {
                MieruCommand::Reload => {
                    let _ = self.reload().await;
                }
                MieruCommand::Restart => {
                    self.stop().await;
                    let _ = self.spawn().await;
                }
                MieruCommand::Stop => {
                    self.stop().await;
                }
            }
        }

        self.stop().await;
    }
}
