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

    /// Push the current configuration into the running daemon.
    ///
    /// `mita replace config <file>` talks to the daemon over its unix domain
    /// socket, so it only works while the daemon is up. The old implementation
    /// called `mita apply config` *before* starting the daemon and waited for
    /// the command to exit — with no daemon running that call blocks forever
    /// (and the legacy stub never exits at all), which wedged the supervisor
    /// before `mita run` was ever reached.
    ///
    /// Returns `false` when the daemon is unreachable or the call fails, so the
    /// caller can fall back to restarting the process.
    async fn replace_config(&self) -> bool {
        if !self.config_path.exists() {
            return false;
        }
        let mut cmd = Command::new(&self.mieru_bin);
        cmd.arg("replace")
            .arg("config")
            .arg(&self.config_path)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);

        match tokio::time::timeout(Duration::from_secs(15), cmd.status()).await {
            Ok(Ok(status)) if status.success() => {
                tracing::info!("mieru configuration replaced via RPC");
                true
            }
            Ok(Ok(status)) => {
                tracing::warn!(code = status.code(), "mieru replace config failed");
                false
            }
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "mieru replace config could not be executed");
                false
            }
            Err(_) => {
                tracing::warn!("mieru replace config timed out");
                false
            }
        }
    }

    /// Build the command that starts the daemon in the foreground.
    ///
    /// `mita run` loads its configuration **only** from the
    /// `MITA_CONFIG_JSON_FILE` environment variable — without it the daemon
    /// comes up with an empty configuration and binds no port, which looks
    /// exactly like a network problem to the client.
    fn daemon_command(&self) -> Command {
        let mut cmd = Command::new(&self.mieru_bin);
        cmd.arg("run")
            .env("MITA_CONFIG_JSON_FILE", &self.config_path)
            .kill_on_drop(true);
        cmd
    }

    /// Spawn the Mieru server daemon loop.
    pub async fn spawn(&self) -> AppResult<()> {
        // The daemon needs its socket directory and system account; without
        // them it exits immediately with an opaque bind/permission error.
        crate::mieru::installer::ensure_runtime_env()?;

        // If config doesn't exist, don't spawn
        if !self.config_path.exists() {
            return Ok(());
        }
        if !self.mieru_bin.is_file() {
            tracing::warn!(
                path = %self.mieru_bin.display(),
                "mieru server binary missing; mieru inbounds will not listen"
            );
            return Ok(());
        }

        let mut cmd = self.daemon_command();

        // Keep the daemon's own diagnostics: it explains bind failures, config
        // problems and unsupported fields far better than a silent exit.
        if let Ok(file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log_file)
        {
            cmd.stdout(std::process::Stdio::from(
                file.try_clone().map_err(AppError::Io)?,
            ));
            cmd.stderr(std::process::Stdio::from(file));
        } else {
            cmd.stdout(std::process::Stdio::null());
            cmd.stderr(std::process::Stdio::null());
        }

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

        // Preferred path: hand the new configuration to the running daemon.
        if self.replace_config().await {
            return Ok(());
        }

        // Fallback: the daemon is down or refused the update, so restart it.
        self.stop().await;
        self.spawn().await?;

        Ok(())
    }

    /// Actor runner handling supervisor commands.
    pub async fn run(self: Arc<Self>, mut rx: mpsc::Receiver<MieruCommand>) {
        self.reap_stale().await;
        if let Err(e) = self.spawn().await {
            tracing::error!(error = %e, "failed to start the mieru server");
        }

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

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_supervisor(dir: &Path) -> MieruSupervisor {
        let bin = dir.join("mita");
        std::fs::write(&bin, "#!/bin/sh\n").unwrap();
        let config = dir.join("config.json");
        std::fs::write(&config, "{}").unwrap();
        MieruSupervisor::new(bin, config, dir)
    }

    #[test]
    fn test_daemon_command_points_at_the_config_file() {
        let temp = tempfile::tempdir().unwrap();
        let sup = sample_supervisor(temp.path());
        let cmd = sup.daemon_command();
        let std_cmd = cmd.as_std();

        let envs: Vec<(String, Option<String>)> = std_cmd
            .get_envs()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.map(|v| v.to_string_lossy().into_owned()),
                )
            })
            .collect();

        let config_path = sup.config_path.to_string_lossy().into_owned();
        assert!(
            envs.iter()
                .any(|(k, v)| k == "MITA_CONFIG_JSON_FILE" && v.as_deref() == Some(&config_path)),
            "the daemon must be told which configuration file to load: {envs:?}"
        );

        let args: Vec<String> = std_cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args, vec!["run".to_string()]);
    }
}
