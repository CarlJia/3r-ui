//! Mieru server core (`mita`) auto-download and install.
//!
//! The upstream Mieru project ships two binaries: `mita` (server) and `mieru`
//! (client). The panel only needs the server side, published as
//! `mita_<version>_<platform>.tar.gz` on GitHub releases.
//!
//! The panel guarantees a real `mita` at `${DATA_DIR}/mieru/mita`. It used to
//! fall back to writing a shell stub that slept forever, which made the
//! supervisor believe the daemon was running while no port was ever bound —
//! clients saw a plain timeout with nothing in the logs. A missing binary is
//! now an error, never a silent stub.

use serde::Deserialize;
use std::path::{Path, PathBuf};

use crate::error::{AppError, AppResult};

/// Upstream repository that publishes the server binaries.
const MIERU_REPO: &str = "enfein/mieru";

/// GitHub API root used to resolve the latest release.
const GITHUB_API_BASE: &str = "https://api.github.com";

/// Runtime directory holding the daemon's unix domain socket.
pub const MITA_RUNTIME_DIR: &str = "/var/run/mita";

/// Unprivileged account owning the daemon's unix domain socket.
pub const MITA_SYSTEM_USER: &str = "mita";

/// A real `mita` is a ~13 MB Go binary; the legacy stub was 90 bytes. Anything
/// smaller than this is treated as a leftover stub and replaced.
const MIN_VALID_BIN_SIZE: u64 = 1024 * 1024;

/// Timeout for release metadata requests.
const METADATA_TIMEOUT_SECS: u64 = 30;

/// Timeout for the binary download itself.
const DOWNLOAD_TIMEOUT_SECS: u64 = 300;

/// Map the running platform onto the release asset suffix.
///
/// The server is published for Linux only; other platforms fall through to
/// the error arm so the caller can tell the operator to install `mita`
/// manually.
pub fn asset_suffix_for(os: &str, arch: &str) -> AppResult<&'static str> {
    let suffix = match (os, arch) {
        ("linux", "x86_64") => "linux_amd64",
        ("linux", "aarch64") => "linux_arm64",
        ("linux", "arm") => "linux_armv7",
        ("macos", "x86_64") => "darwin_amd64",
        ("macos", "aarch64") => "darwin_arm64",
        ("windows", "x86_64") => "windows_amd64",
        ("windows", "aarch64") => "windows_arm64",
        (os, arch) => {
            return Err(AppError::Mieru(format!(
                "unsupported target {os}/{arch} for mieru; please install mita manually"
            )));
        }
    };
    Ok(suffix)
}

/// Build the release asset name for a given version and platform suffix.
///
/// Upstream tags releases as `v3.40.0` but names the assets `3.40.0`.
pub fn mita_asset_name(version: &str, suffix: &str) -> String {
    format!("mita_{}_{}.tar.gz", version.trim_start_matches('v'), suffix)
}

/// Whether a file at `path` is a leftover stub rather than a real binary.
pub fn looks_like_stub(path: &Path) -> bool {
    match std::fs::metadata(path) {
        Ok(meta) => meta.len() < MIN_VALID_BIN_SIZE,
        Err(_) => false,
    }
}

#[derive(Debug, Deserialize)]
struct Release {
    tag_name: String,
    assets: Vec<Asset>,
}

#[derive(Debug, Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
}

fn find_in_path(name: &str) -> Option<PathBuf> {
    if let Some(paths) = std::env::var_os("PATH") {
        for path in std::env::split_paths(&paths) {
            let candidate = path.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// Downloader / installer for the Mieru server binary.
pub struct MitaInstaller {
    http: reqwest::Client,
}

impl MitaInstaller {
    pub fn new() -> AppResult<Self> {
        let http = reqwest::Client::builder()
            .user_agent(concat!("3r-ui/", env!("CARGO_PKG_VERSION")))
            .timeout(std::time::Duration::from_secs(DOWNLOAD_TIMEOUT_SECS))
            .build()
            .map_err(|e| AppError::Mieru(format!("http client init failed: {e}")))?;
        Ok(Self { http })
    }

    /// Ensure a usable `mita` exists and return its path.
    ///
    /// Order of preference:
    /// 1. `${DATA_DIR}/mieru/mita` — a real binary wins; a leftover stub is
    ///    deleted and replaced by a fresh download.
    /// 2. `mita` found on `PATH` (e.g. installed from the upstream `.deb`).
    /// 3. Download from the latest GitHub release.
    pub async fn ensure_mita(&self, data_dir: &Path) -> AppResult<PathBuf> {
        let mieru_dir = data_dir.join("mieru");
        let local_bin = mieru_dir.join("mita");

        if local_bin.is_file() {
            if !looks_like_stub(&local_bin) {
                tracing::debug!(path = %local_bin.display(), "mita already present");
                return Ok(local_bin);
            }
            tracing::warn!(
                path = %local_bin.display(),
                "legacy stub detected; replacing it with a real mita binary"
            );
            let _ = std::fs::remove_file(&local_bin);
        }

        if let Some(path) = find_in_path("mita") {
            tracing::info!(path = %path.display(), "using mita from PATH");
            return Ok(path);
        }

        self.download_and_install(&mieru_dir, &local_bin).await
    }

    /// Fetch the latest release, extract `mita` and install it at `dest`.
    async fn download_and_install(&self, dir: &Path, dest: &Path) -> AppResult<PathBuf> {
        let os = std::env::consts::OS;
        let arch = std::env::consts::ARCH;
        let suffix = asset_suffix_for(os, arch)?;
        if !suffix.starts_with("linux_") {
            return Err(AppError::Mieru(format!(
                "the mieru server is published for Linux only (this host is {os}/{arch}); \
                 install mita manually and place it at {}",
                dest.display()
            )));
        }

        std::fs::create_dir_all(dir).map_err(AppError::Io)?;

        let release = self.fetch_latest_release().await?;
        let asset_name = mita_asset_name(&release.tag_name, suffix);
        let asset = release
            .assets
            .iter()
            .find(|a| a.name == asset_name)
            .ok_or_else(|| {
                AppError::Mieru(format!(
                    "mieru release {} has no asset {} for this platform",
                    release.tag_name, asset_name
                ))
            })?;

        let archive_path = dir.join(&asset_name);
        tracing::info!(
            url = %asset.browser_download_url,
            dest = %archive_path.display(),
            "downloading mieru server release"
        );
        self.download_to(&asset.browser_download_url, &archive_path)
            .await
            .map_err(|e| {
                let _ = std::fs::remove_file(&archive_path);
                e
            })?;

        let staging = dir.join(format!("mita-staging-{:016x}", rand::random::<u64>()));
        std::fs::create_dir_all(&staging).map_err(AppError::Io)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&staging, std::fs::Permissions::from_mode(0o700))
                .map_err(AppError::Io)?;
        }

        let result = (|| -> AppResult<()> {
            extract_tar_gz(&archive_path, &staging)?;
            let extracted = staging.join("mita");
            if !extracted.is_file() {
                return Err(AppError::Mieru(format!(
                    "release archive does not contain a mita binary ({})",
                    extracted.display()
                )));
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&extracted, std::fs::Permissions::from_mode(0o755))
                    .map_err(AppError::Io)?;
            }
            // The binary is the commit step: it appears at its final path only
            // after every fallible step has succeeded.
            std::fs::rename(&extracted, dest).map_err(AppError::Io)?;
            Ok(())
        })();

        let _ = std::fs::remove_dir_all(&staging);
        let _ = std::fs::remove_file(&archive_path);
        result?;

        tracing::info!(path = %dest.display(), version = %release.tag_name, "mieru server installed");
        Ok(dest.to_path_buf())
    }

    async fn fetch_latest_release(&self) -> AppResult<Release> {
        let url = format!("{}/repos/{}/releases/latest", GITHUB_API_BASE, MIERU_REPO);
        let resp = self
            .http
            .get(&url)
            .timeout(std::time::Duration::from_secs(METADATA_TIMEOUT_SECS))
            .send()
            .await
            .map_err(|e| {
                AppError::Mieru(format!(
                    "mieru release metadata request failed: {e}; check connectivity to {GITHUB_API_BASE}"
                ))
            })?;
        let status = resp.status();
        if !status.is_success() {
            return Err(AppError::Mieru(format!(
                "mieru release metadata HTTP {status} from {url}"
            )));
        }
        resp.json::<Release>()
            .await
            .map_err(|e| AppError::Mieru(format!("mieru release metadata parse failed: {e}")))
    }

    async fn download_to(&self, url: &str, dest: &Path) -> AppResult<()> {
        use tokio::io::AsyncWriteExt;
        let mut resp = self
            .http
            .get(url)
            .send()
            .await
            .map_err(|e| AppError::Mieru(format!("mieru download request failed: {e}")))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(AppError::Mieru(format!(
                "mieru download HTTP {status} from {url}"
            )));
        }
        let mut file = tokio::fs::File::create(dest).await.map_err(AppError::Io)?;
        while let Some(chunk) = resp
            .chunk()
            .await
            .map_err(|e| AppError::Mieru(format!("mieru download body read failed: {e}")))?
        {
            file.write_all(&chunk).await.map_err(AppError::Io)?;
        }
        file.flush().await.map_err(AppError::Io)?;
        Ok(())
    }
}

/// Extract a `.tar.gz` archive into `dest`.
fn extract_tar_gz(archive: &Path, dest: &Path) -> AppResult<()> {
    let file = std::fs::File::open(archive).map_err(AppError::Io)?;
    let decoder = flate2::read::GzDecoder::new(file);
    let mut tar = tar::Archive::new(decoder);
    tar.unpack(dest)
        .map_err(|e| AppError::Mieru(format!("failed to extract mieru archive: {e}")))
}

/// Prepare the host for the daemon: the socket directory and the unprivileged
/// account that owns the socket.
///
/// `mita` refuses to start without both — it fails with
/// `update server unix domain socket permission failed: getUid("mita") failed`
/// or `listen on RPC address ... bind: no such file or directory` — so the
/// panel creates them instead of leaving the operator with a dead daemon.
///
/// The server is published for Linux only; on other platforms the Mieru
/// daemon cannot run at all, so nothing is touched here.
pub fn ensure_runtime_env() -> AppResult<()> {
    #[cfg(target_os = "linux")]
    {
        ensure_system_user(MITA_SYSTEM_USER)?;
        std::fs::create_dir_all(MITA_RUNTIME_DIR).map_err(AppError::Io)?;
        ensure_tmpfiles_rule();
    }
    Ok(())
}

/// Whether the named account exists on this host.
#[cfg(unix)]
pub fn system_user_exists(name: &str) -> bool {
    std::process::Command::new("id")
        .arg("-u")
        .arg(name)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Create the unprivileged daemon account when it is missing.
#[cfg(target_os = "linux")]
fn ensure_system_user(name: &str) -> AppResult<()> {
    if system_user_exists(name) {
        return Ok(());
    }
    let status = std::process::Command::new("useradd")
        .args([
            "--system",
            "--no-create-home",
            "--shell",
            "/usr/sbin/nologin",
        ])
        .arg(name)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map_err(|e| AppError::Mieru(format!("failed to run useradd for {name}: {e}")))?;
    if !status.success() {
        return Err(AppError::Mieru(format!(
            "failed to create the {name} system account (useradd exited with {status}); \
             create it manually and restart the panel"
        )));
    }
    tracing::info!(user = %name, "created mieru system account");
    Ok(())
}

/// `/var/run` is a tmpfs on most distributions, so the socket directory has to
/// be recreated on every boot. A tmpfiles rule makes that automatic.
#[cfg(target_os = "linux")]
fn ensure_tmpfiles_rule() {
    let path = Path::new("/etc/tmpfiles.d/3r-ui-mita.conf");
    if path.exists() {
        return;
    }
    let rule = format!("d {MITA_RUNTIME_DIR} 0755 root root -\n");
    if std::fs::write(path, rule).is_ok() {
        tracing::info!(path = %path.display(), "installed tmpfiles rule for the mieru socket dir");
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_asset_suffix_mapping() {
        assert_eq!(asset_suffix_for("linux", "x86_64").unwrap(), "linux_amd64");
        assert_eq!(asset_suffix_for("linux", "aarch64").unwrap(), "linux_arm64");
        assert_eq!(
            asset_suffix_for("macos", "aarch64").unwrap(),
            "darwin_arm64"
        );
        assert!(asset_suffix_for("freebsd", "mips").is_err());
    }

    #[test]
    fn test_mita_asset_name_drops_tag_prefix() {
        assert_eq!(
            mita_asset_name("v3.40.0", "linux_amd64"),
            "mita_3.40.0_linux_amd64.tar.gz"
        );
        assert_eq!(
            mita_asset_name("3.40.0", "linux_arm64"),
            "mita_3.40.0_linux_arm64.tar.gz"
        );
    }

    #[test]
    fn test_looks_like_stub_detects_small_files() {
        let temp = tempfile::tempdir().unwrap();
        let stub = temp.path().join("mita");
        std::fs::write(&stub, "#!/bin/sh\nwhile true; do sleep 3600; done\n").unwrap();
        assert!(looks_like_stub(&stub));

        let missing = temp.path().join("nope");
        assert!(!looks_like_stub(&missing));
    }
}
