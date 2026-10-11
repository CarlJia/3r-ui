//! xray-core auto-download and install.
//!
//! On first start the panel downloads the latest stable xray-core release from
//! GitHub (or a configured mirror) and unpacks it under `${data_dir}/xray/`.
//! Subsequent starts reuse the existing binary.
//!
//! `Installer::ensure_xray` is idempotent: if `${data_dir}/xray/xray` already
//! exists and is executable, it returns immediately without making any HTTP
//! request.

use std::path::{Path, PathBuf};
use std::time::Duration;

use rand::Rng;
use serde::Deserialize;

use crate::error::{AppError, AppResult};

const XRAY_REPO: &str = "XTLS/Xray-core";
const USER_AGENT: &str = concat!("3r-ui/", env!("CARGO_PKG_VERSION"));

/// Asset filename suffix for the running target triple.
fn asset_suffix() -> AppResult<&'static str> {
    let suffix = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => "linux-64",
        ("linux", "aarch64") => "linux-arm64",
        ("linux", "arm") => "linux-armv7",
        ("macos", "x86_64") => "macos-64",
        ("macos", "aarch64") => "macos-arm64",
        ("windows", "x86_64") => "windows-64",
        ("windows", "aarch64") => "windows-arm64",
        (os, arch) => {
            return Err(AppError::Xray(format!(
                "unsupported target {os}/{arch}; install xray-core manually and place it at $DATA_DIR/xray/"
            )));
        }
    };
    Ok(suffix)
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

pub struct Installer {
    http: reqwest::Client,
    download_base: String,
}

impl Installer {
    pub fn new(download_base: impl Into<String>) -> AppResult<Self> {
        let http = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .timeout(Duration::from_secs(120))
            .connect_timeout(Duration::from_secs(15))
            .build()
            .map_err(|e| AppError::Xray(format!("http client init failed: {e}")))?;
        Ok(Self {
            http,
            download_base: download_base.into(),
        })
    }

    pub async fn ensure_xray(&self, data_dir: &Path) -> AppResult<PathBuf> {
        let xray_dir = data_dir.join("xray");
        let xray_bin = xray_dir.join(if cfg!(windows) { "xray.exe" } else { "xray" });

        if xray_bin.is_file() {
            // Cheap existence check; executability is verified by the OS at spawn time.
            tracing::debug!(path = %xray_bin.display(), "xray already present; skipping download");
            return Ok(xray_bin);
        }

        // fetch_and_extract does NOT rename or chmod — we do that here.
        let (staging_xray, tag) = self.fetch_and_extract(data_dir, None).await?;

        std::fs::create_dir_all(&xray_dir).map_err(AppError::Io)?;
        std::fs::rename(&staging_xray, &xray_bin).map_err(AppError::Io)?;

        // chmod 0755 on the binary (no-op on Windows).
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&xray_bin, std::fs::Permissions::from_mode(0o755))
                .map_err(AppError::Io)?;
        }

        // Best-effort cleanup of the staging directory.
        let staging_dir = staging_xray
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| data_dir.join(format!("xray-{}-staging", tag)));
        let _ = std::fs::remove_dir_all(&staging_dir);

        tracing::info!(path = %xray_bin.display(), version = %tag, "xray-core installed");
        Ok(xray_bin)
    }

    /// Download a specific xray-core version (or latest if `version` is `None`) and
    /// return the path to the extracted binary inside a staging directory along
    /// with the release tag.
    ///
    /// Unlike `ensure_xray`, this method **always downloads**, never checks for an
    /// existing canonical binary, and does **not** rename the binary into place.
    /// The caller is responsible for promotion and cleanup.
    ///
    /// Staging directory is `${data_dir}/xray-<tag>-staging-<rand16hex>` with mode 0o700.
    pub async fn install_version(
        &self,
        data_dir: &Path,
        version: Option<&str>,
    ) -> AppResult<(PathBuf, String)> {
        self.fetch_and_extract(data_dir, version).await
    }

    async fn fetch_release(&self, version: Option<&str>) -> AppResult<Release> {
        let url = if let Some(v) = version {
            let encoded = urlencoding::encode(v);
            format!(
                "{}/{}/releases/tags/{}",
                self.download_base.trim_end_matches('/'),
                XRAY_REPO,
                encoded
            )
        } else {
            format!(
                "{}/{}/releases/latest",
                self.download_base.trim_end_matches('/'),
                XRAY_REPO
            )
        };
        let resp = self.http.get(&url).send().await.map_err(|e| {
            AppError::Xray(format!(
                "xray release metadata request failed: {e}; check connectivity to {}",
                self.download_base
            ))
        })?;
        let status = resp.status();
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(AppError::Xray(
                "GitHub rate-limited (HTTP 429); set XRAY_DOWNLOAD_BASE to a mirror or retry later"
                    .to_string(),
            ));
        }
        if !status.is_success() {
            return Err(AppError::Xray(format!(
                "xray release metadata HTTP {status} from {url}"
            )));
        }
        resp.json::<Release>()
            .await
            .map_err(|e| AppError::Xray(format!("xray release metadata parse failed: {e}")))
    }

    /// Fetch a release manifest and download + extract the matching asset into a
    /// staging directory.  Returns the path to the extracted `xray` binary and
    /// the release tag.
    ///
    /// The staging directory is placed at `${data_dir}/xray-<tag>-staging-<16-hex rand>`
    /// and is created with mode 0o700 (unix only; no-op on Windows).
    async fn fetch_and_extract(
        &self,
        data_dir: &Path,
        version: Option<&str>,
    ) -> AppResult<(PathBuf, String)> {
        use std::os::unix::fs::PermissionsExt;

        let suffix = asset_suffix()?;
        let archive_name = format!("Xray-{suffix}.zip");

        tracing::info!(
            base = %self.download_base,
            version = version.unwrap_or("latest"),
            asset = %archive_name,
            "fetching xray-core release metadata"
        );
        let release = self.fetch_release(version).await?;

        let asset = release
            .assets
            .iter()
            .find(|a| a.name == archive_name)
            .ok_or_else(|| {
                AppError::Xray(format!(
                    "xray release {} has no asset {} for this platform",
                    release.tag_name, archive_name
                ))
            })?;

        let downloads_dir = data_dir.join("downloads");
        std::fs::create_dir_all(&downloads_dir).map_err(AppError::Io)?;
        let archive_path = downloads_dir.join(&archive_name);

        tracing::info!(url = %asset.browser_download_url, dest = %archive_path.display(), "downloading xray-core");
        self.download_to(&asset.browser_download_url, &archive_path)
            .await
            .map_err(|e| {
                let _ = std::fs::remove_file(&archive_path);
                e
            })?;

        // Build the staging directory path: ${data_dir}/xray-<tag>-staging-<rand16hex>
        let rand_hex: u64 = rand::thread_rng().gen();
        let staging_dir = data_dir.join(format!(
            "xray-{}-staging-{:016x}",
            release.tag_name, rand_hex
        ));
        std::fs::create_dir_all(&staging_dir).map_err(AppError::Io)?;

        // Apply 0o700 permissions on unix (staging contains an incomplete binary).
        #[cfg(unix)]
        {
            std::fs::set_permissions(&staging_dir, std::fs::Permissions::from_mode(0o700))
                .map_err(AppError::Io)?;
        }

        tracing::info!(archive = %archive_path.display(), dest = %staging_dir.display(), "extracting");
        self.extract_zip(&archive_path, &staging_dir)?;

        let inner_bin = staging_dir.join(if cfg!(windows) { "xray.exe" } else { "xray" });
        if !inner_bin.is_file() {
            return Err(AppError::Xray(format!(
                "extracted archive does not contain {}",
                inner_bin.display()
            )));
        }

        // Best-effort cleanup of archive.
        let _ = std::fs::remove_file(&archive_path);

        Ok((inner_bin, release.tag_name))
    }

    async fn download_to(&self, url: &str, dest: &Path) -> AppResult<()> {
        let resp = self
            .http
            .get(url)
            .send()
            .await
            .map_err(|e| AppError::Xray(format!("xray download request failed: {e}")))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(AppError::Xray(format!("xray download HTTP {status}")));
        }
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| AppError::Xray(format!("xray download body read failed: {e}")))?;
        std::fs::write(dest, &bytes).map_err(AppError::Io)?;
        Ok(())
    }

    fn extract_zip(&self, archive: &Path, dest_dir: &Path) -> AppResult<()> {
        std::fs::create_dir_all(dest_dir).map_err(AppError::Io)?;
        let file = std::fs::File::open(archive).map_err(AppError::Io)?;
        let mut zip = zip::ZipArchive::new(file)
            .map_err(|e| AppError::Xray(format!("open zip failed: {e}")))?;
        for i in 0..zip.len() {
            let mut entry = zip
                .by_index(i)
                .map_err(|e| AppError::Xray(format!("zip entry {i}: {e}")))?;
            let Some(rel) = entry.enclosed_name().map(|p| p.to_path_buf()) else {
                continue;
            };
            // Strip top-level directory prefix (xray-core archives all nest under a single top dir).
            let stripped = rel.components().skip(1).collect::<std::path::PathBuf>();
            if stripped.as_os_str().is_empty() {
                continue;
            }
            let out_path = dest_dir.join(&stripped);
            if entry.is_dir() {
                std::fs::create_dir_all(&out_path).map_err(AppError::Io)?;
            } else {
                if let Some(parent) = out_path.parent() {
                    std::fs::create_dir_all(parent).map_err(AppError::Io)?;
                }
                let mut out = std::fs::File::create(&out_path).map_err(AppError::Io)?;
                std::io::copy(&mut entry, &mut out).map_err(AppError::Io)?;
            }
        }
        Ok(())
    }
}
