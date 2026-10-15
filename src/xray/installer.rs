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
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use rand::Rng;
use serde::Deserialize;

use crate::error::{AppError, AppResult};

const XRAY_REPO: &str = "XTLS/Xray-core";
const USER_AGENT: &str = concat!("3r-ui/", env!("CARGO_PKG_VERSION"));

/// Data files the release archive ships next to the binary and xray loads at
/// startup.
///
/// xray looks them up in the binary's directory (or `$XRAY_LOCATION_ASSET`), and
/// the rendered config always contains a `geosite:category-ads-all` rule, so an
/// install that promotes only the binary dies at config load with
/// `failed to open file: geosite.dat` (exit 23) instead of serving traffic.
const DATA_FILES: [&str; 2] = ["geoip.dat", "geosite.dat"];

/// Asset filename suffix for the running target triple.
pub(crate) fn asset_suffix() -> AppResult<&'static str> {
    asset_suffix_for(std::env::consts::OS, std::env::consts::ARCH)
}

/// Asset filename suffix for an explicit `(os, arch)` pair.
///
/// These names must match the assets published by XTLS/Xray-core **exactly**.
/// They are the `assets[].name` values of
/// `https://api.github.com/repos/XTLS/Xray-core/releases/latest`; the arm64
/// and arm32 builds carry a `-v8a` / `-v7a` suffix, so `macos-arm64` alone
/// matches nothing. The table is pinned by `asset_suffix_matches_published_assets`
/// below — do not derive an expectation from this function in tests.
fn asset_suffix_for(os: &str, arch: &str) -> AppResult<&'static str> {
    let suffix = match (os, arch) {
        ("linux", "x86_64") => "linux-64",
        ("linux", "aarch64") => "linux-arm64-v8a",
        ("linux", "arm") => "linux-arm32-v7a",
        ("macos", "x86_64") => "macos-64",
        ("macos", "aarch64") => "macos-arm64-v8a",
        ("windows", "x86_64") => "windows-64",
        ("windows", "aarch64") => "windows-arm64-v8a",
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

/// Drop one leading path component, falling back to the original path when
/// nothing would be left.
///
/// XTLS/Xray-core publishes flat archives (`xray`, `geoip.dat`, ...), so
/// stripping unconditionally would empty every entry; a mirror may repackage
/// the same files under a single top-level directory, which does need stripping.
fn strip_leading_component(rel: PathBuf) -> PathBuf {
    let stripped = rel.components().skip(1).collect::<PathBuf>();
    if stripped.as_os_str().is_empty() {
        rel
    } else {
        stripped
    }
}

/// Move a staged binary, and the [`DATA_FILES`] sitting beside it, into
/// `dest_dir`; returns the promoted binary's path.
///
/// Promotion is all-or-nothing: an install is only usable with the data files
/// beside an executable binary (the rendered config always references
/// `geosite:category-ads-all`), so an archive that would leave one of them
/// missing fails here — before anything is moved — instead of producing a
/// directory the supervisor crash-loops on while the update job reports
/// success. Data files already present in `dest_dir` satisfy the requirement,
/// so an update may ship the binary alone against a previous install's assets.
///
/// The data files move first and the binary last: the binary appearing in its
/// final location is the single commit step, so an interrupted promotion leaves
/// the previous install intact.
pub(crate) fn promote_staged_xray(staged_bin: &Path, dest_dir: &Path) -> AppResult<PathBuf> {
    if !staged_bin.is_file() {
        return Err(AppError::Xray(format!(
            "extracted archive does not contain {}",
            staged_bin.display()
        )));
    }
    let asset_dir = staged_bin.parent().unwrap_or_else(|| Path::new("."));
    let file_name = staged_bin.file_name().ok_or_else(|| {
        AppError::Xray(format!(
            "staged path has no file name: {}",
            staged_bin.display()
        ))
    })?;

    let mut assets_to_move = Vec::new();
    for name in DATA_FILES {
        let staged = asset_dir.join(name);
        if staged.is_file() {
            assets_to_move.push((staged, dest_dir.join(name)));
        } else if !dest_dir.join(name).is_file() {
            return Err(AppError::Xray(format!(
                "release archive carries no {name} and none is installed beside the binary; \
                 xray cannot start without it"
            )));
        }
    }

    // The exec bit belongs to the commit, not after it: extraction writes regular
    // files (0o644), and a binary promoted without this is present yet
    // unspawnable (`EACCES`). Setting it on the staged copy keeps dest_dir
    // untouched until every step that can fail has succeeded.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(staged_bin, std::fs::Permissions::from_mode(0o755))
            .map_err(AppError::Io)?;
    }

    std::fs::create_dir_all(dest_dir).map_err(AppError::Io)?;
    for (from, to) in assets_to_move {
        std::fs::rename(&from, &to).map_err(AppError::Io)?;
    }

    let dest_bin = dest_dir.join(file_name);
    std::fs::rename(staged_bin, &dest_bin).map_err(AppError::Io)?;

    Ok(dest_bin)
}

/// Byte counters of an archive download, published as the bytes arrive.
///
/// The download writes, the settings page reads: the page polls the job status
/// endpoint, and the endpoint snapshots whatever the transfer has reached when
/// asked. Counters rather than formatted text because a progress bar needs the
/// numbers to compute a percentage — MiB and `%` belong to the UI.
///
/// `total` is `None` until the server announces a `Content-Length`: a chunked
/// or proxied response may never send one, and an invented denominator would
/// show a percentage that means nothing.
#[derive(Debug, Default)]
pub struct DownloadProgress {
    downloaded: AtomicU64,
    total: AtomicU64,
}

impl DownloadProgress {
    /// Start a new transfer of `total` bytes (`None` when the server did not
    /// announce a length), discarding a previous transfer's counters.
    pub fn begin(&self, total: Option<u64>) {
        self.downloaded.store(0, Ordering::Relaxed);
        self.total.store(total.unwrap_or(0), Ordering::Relaxed);
    }

    /// Record how many bytes of the archive have been written so far.
    pub fn set_downloaded(&self, bytes: u64) {
        self.downloaded.store(bytes, Ordering::Relaxed);
    }

    /// Bytes written so far, and the announced total (`None` when unknown).
    pub fn snapshot(&self) -> (u64, Option<u64>) {
        let total = self.total.load(Ordering::Relaxed);
        (
            self.downloaded.load(Ordering::Relaxed),
            (total > 0).then_some(total),
        )
    }
}

pub struct Installer {
    http: reqwest::Client,
    download_base: String,
}

/// How long to wait for the connection to the download host to come up.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// Maximum gap between two body reads. A slow-but-progressing transfer keeps
/// resetting it; a stalled one fails instead of hanging forever.
const READ_TIMEOUT: Duration = Duration::from_secs(60);

/// Total budget for the small release-metadata JSON.
const METADATA_TIMEOUT: Duration = Duration::from_secs(30);

/// Last-resort ceiling on the archive transfer, sized so even a very slow link
/// finishes: reading a ~20 MB release at ~15 KB/s takes about 22 minutes.
/// `scripts/dev.sh`'s readiness budget is deliberately larger than this.
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// Log download progress once per this many bytes, so a slow cold start is
/// visible to the operator (and to `scripts/dev.sh`) instead of looking hung.
const DOWNLOAD_PROGRESS_STEP: u64 = 2 * 1024 * 1024;

/// Whether an install already on disk can actually start: an executable binary
/// (unix) with the [`DATA_FILES`] beside it.
///
/// Anything else must be re-fetched. Builds before the promotion fix left
/// binaries that are present but unusable — no `geosite.dat`/`geoip.dat`
/// (xray exits 23 at config load) or no exec bit (the supervisor fails to spawn
/// with `EACCES`) — and treating those as installed keeps the panel reporting
/// ready while the supervisor crash-loops behind its backoff.
fn install_is_startable(xray_dir: &Path, xray_bin: &Path) -> bool {
    if !DATA_FILES.iter().all(|name| xray_dir.join(name).is_file()) {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        match std::fs::metadata(xray_bin) {
            Ok(meta) => meta.permissions().mode() & 0o111 != 0,
            Err(_) => false,
        }
    }
    #[cfg(not(unix))]
    {
        let _ = xray_bin;
        true
    }
}

/// Bytes rendered as MiB, for download progress lines.
fn human_bytes(bytes: u64) -> String {
    format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0))
}

impl Installer {
    pub fn new(download_base: impl Into<String>) -> AppResult<Self> {
        // No blanket request timeout: the old 120s cap aborted the ~20 MB
        // archive on a slow-but-healthy link (at 56 KB/s — measured on a
        // domestic line — a full transfer needs ~6 minutes), and the panel
        // reported itself ready anyway, so a cold start looked like a silent
        // failure. Instead the client bounds connecting and stalling per
        // request:
        // - `connect_timeout` bounds reaching the host,
        // - `read_timeout` bounds each gap between body reads and resets after
        //   every successful one, so progress forgives slowness but a dead
        //   connection still fails promptly with an actionable error,
        // - the metadata call keeps an explicit total timeout, and the archive
        //   keeps a generous one as a last resort (see `download_to`).
        let http = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .connect_timeout(CONNECT_TIMEOUT)
            .read_timeout(READ_TIMEOUT)
            .build()
            .map_err(|e| AppError::Xray(format!("http client init failed: {e}")))?;
        Ok(Self {
            http,
            download_base: download_base.into(),
        })
    }

    /// Install xray into `${data_dir}/xray/` if it is not already startable.
    ///
    /// `progress` publishes the archive's byte counters for the caller's UI; the
    /// startup path has none and passes `None`.
    pub async fn ensure_xray(
        &self,
        data_dir: &Path,
        progress: Option<&DownloadProgress>,
    ) -> AppResult<PathBuf> {
        let xray_dir = data_dir.join("xray");
        let xray_bin = xray_dir.join(if cfg!(windows) { "xray.exe" } else { "xray" });

        if xray_bin.is_file() && install_is_startable(&xray_dir, &xray_bin) {
            // Cheap existence check; executability is verified by the OS at spawn time.
            tracing::debug!(path = %xray_bin.display(), "xray already present; skipping download");
            return Ok(xray_bin);
        }

        // fetch_and_extract does NOT rename or chmod — we do that here.
        let (staging_xray, tag) = self.fetch_and_extract(data_dir, None, progress).await?;

        // Promotion carries the binary AND the assets xray loads from its own
        // directory, and leaves the binary executable; see `promote_staged_xray`.
        let xray_bin = promote_staged_xray(&staging_xray, &xray_dir)?;

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
    /// The caller is responsible for promotion — via [`promote_staged_xray`], so
    /// the assets beside the binary come along — and cleanup.
    ///
    /// Staging directory is `${data_dir}/xray-<tag>-staging-<rand16hex>` with mode 0o700.
    ///
    /// `progress` publishes the archive's byte counters for the caller's UI.
    pub async fn install_version(
        &self,
        data_dir: &Path,
        version: Option<&str>,
        progress: Option<&DownloadProgress>,
    ) -> AppResult<(PathBuf, String)> {
        self.fetch_and_extract(data_dir, version, progress).await
    }

    async fn fetch_release(&self, version: Option<&str>) -> AppResult<Release> {
        // `download_base` is the GitHub **API** root (`https://api.github.com`),
        // not the web host: the release JSON only exists under `/repos/<owner>/<repo>/`.
        // Requesting the web host instead yields an HTML page that fails to parse.
        let url = if let Some(v) = version {
            let encoded = crate::web::middleware::url_encode(v);
            format!(
                "{}/repos/{}/releases/tags/{}",
                self.download_base.trim_end_matches('/'),
                XRAY_REPO,
                encoded
            )
        } else {
            format!(
                "{}/repos/{}/releases/latest",
                self.download_base.trim_end_matches('/'),
                XRAY_REPO
            )
        };
        let resp = self
            .http
            .get(&url)
            .timeout(METADATA_TIMEOUT)
            .send()
            .await
            .map_err(|e| {
                AppError::Xray(format!(
                    "xray release metadata request failed: {e}; check connectivity to {}",
                    self.download_base
                ))
            })?;
        let status = resp.status();
        // GitHub throttles a token-less client with 429, and answers 403 with
        // `x-ratelimit-remaining: 0` once the hourly quota is spent. Both mean
        // "come back later or point XRAY_DOWNLOAD_BASE at a mirror", which is
        // worth saying out loud instead of reporting a bare HTTP 403.
        let rate_limited = status == reqwest::StatusCode::TOO_MANY_REQUESTS
            || (status == reqwest::StatusCode::FORBIDDEN
                && resp
                    .headers()
                    .get("x-ratelimit-remaining")
                    .map(|v| v.as_bytes() == b"0")
                    .unwrap_or(false));
        if rate_limited {
            return Err(AppError::Xray(format!(
                "GitHub rate-limited (HTTP {status}); set XRAY_DOWNLOAD_BASE to a mirror or retry later"
            )));
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
        progress: Option<&DownloadProgress>,
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
        self.download_to(&asset.browser_download_url, &archive_path, progress)
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

    /// Stream the archive straight to `dest`, chunk by chunk.
    ///
    /// The body is written as it arrives instead of being buffered whole (the
    /// archive is ~20 MB), and the client's read timeout — not a total request
    /// timeout — is what fails a stalled transfer, so a slow link still
    /// completes. Progress is published to `progress` on every chunk and logged
    /// every [`DOWNLOAD_PROGRESS_STEP`] bytes: a cold start over a slow link
    /// otherwise looks hung to the operator.
    ///
    /// A short body is rejected here rather than left on disk as a corrupt
    /// archive that fails later inside zip extraction.
    async fn download_to(
        &self,
        url: &str,
        dest: &Path,
        progress: Option<&DownloadProgress>,
    ) -> AppResult<()> {
        use tokio::io::AsyncWriteExt;
        let mut resp = self
            .http
            .get(url)
            .timeout(DOWNLOAD_TIMEOUT)
            .send()
            .await
            .map_err(|e| AppError::Xray(format!("xray download request failed: {e}")))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(AppError::Xray(format!(
                "xray download HTTP {status} from {url}"
            )));
        }
        let expected = resp.content_length();
        if let Some(progress) = progress {
            progress.begin(expected);
        }
        let mut file = tokio::fs::File::create(dest).await.map_err(AppError::Io)?;
        let mut written: u64 = 0;
        let mut next_report = DOWNLOAD_PROGRESS_STEP;
        while let Some(chunk) = resp
            .chunk()
            .await
            .map_err(|e| AppError::Xray(format!("xray download body read failed: {e}")))?
        {
            file.write_all(&chunk).await.map_err(AppError::Io)?;
            written += chunk.len() as u64;
            if let Some(progress) = progress {
                progress.set_downloaded(written);
            }
            if written >= next_report {
                next_report = written + DOWNLOAD_PROGRESS_STEP;
                tracing::info!(
                    downloaded = %human_bytes(written),
                    total = expected.map(human_bytes).unwrap_or_else(|| "?".to_string()),
                    "downloading xray-core"
                );
            }
        }
        file.flush().await.map_err(AppError::Io)?;

        if let Some(expected) = expected {
            if written != expected {
                return Err(AppError::Xray(format!(
                    "xray download truncated: got {written} bytes of {expected}"
                )));
            }
        }
        tracing::info!(bytes = written, "xray archive downloaded");
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
            let out_path = dest_dir.join(strip_leading_component(rel));
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Every suffix must match an asset XTLS/Xray-core actually publishes.
    ///
    /// The expectations are literal copies of the real `assets[].name` list from
    /// `https://api.github.com/repos/XTLS/Xray-core/releases/latest`, never
    /// derived from `asset_suffix_for` itself: an expectation that mirrors the
    /// implementation passes even when both are wrong.
    #[test]
    fn asset_suffix_matches_published_assets() {
        let cases = [
            ("linux", "x86_64", "linux-64"),
            ("linux", "aarch64", "linux-arm64-v8a"),
            ("linux", "arm", "linux-arm32-v7a"),
            ("macos", "x86_64", "macos-64"),
            ("macos", "aarch64", "macos-arm64-v8a"),
            ("windows", "x86_64", "windows-64"),
            ("windows", "aarch64", "windows-arm64-v8a"),
        ];
        for (os, arch, expected) in cases {
            assert_eq!(
                asset_suffix_for(os, arch).unwrap(),
                expected,
                "Xray-{expected}.zip must be the published asset for {os}/{arch}"
            );
        }
    }

    #[test]
    fn asset_suffix_rejects_unsupported_target() {
        assert!(asset_suffix_for("freebsd", "x86_64").is_err());
    }

    /// Stage a directory that looks like an extracted archive.
    fn stage(files: &[(&str, &[u8])]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (name, payload) in files {
            std::fs::write(dir.path().join(name), payload).unwrap();
        }
        dir
    }

    #[test]
    fn promote_staged_xray_moves_data_files_beside_the_binary() {
        let staging = stage(&[
            ("xray", b"binary"),
            ("geoip.dat", b"geoip"),
            ("geosite.dat", b"geosite"),
        ]);
        let root = tempfile::tempdir().unwrap();
        let dest = root.path().join("xray");

        let promoted = promote_staged_xray(&staging.path().join("xray"), &dest).unwrap();

        assert_eq!(promoted, dest.join("xray"));
        assert_eq!(std::fs::read(&promoted).unwrap(), b"binary");
        // xray resolves these from the directory holding the binary; dropping
        // them made the promoted install exit 23 at config load.
        assert_eq!(std::fs::read(dest.join("geoip.dat")).unwrap(), b"geoip");
        assert_eq!(std::fs::read(dest.join("geosite.dat")).unwrap(), b"geosite");
        assert!(
            !staging.path().join("geosite.dat").exists(),
            "moved, not copied"
        );
    }

    #[test]
    fn promote_staged_xray_refuses_a_binary_only_archive_on_a_cold_install() {
        // No assets in dest_dir and none in the archive: xray would exit 23 at
        // config load, so the promotion must fail rather than report success.
        let staging = stage(&[("xray", b"binary")]);
        let root = tempfile::tempdir().unwrap();
        let dest = root.path().join("xray");

        let err = promote_staged_xray(&staging.path().join("xray"), &dest).unwrap_err();

        assert!(
            format!("{err}").contains("xray cannot start without it"),
            "got: {err}"
        );
        assert!(!dest.join("xray").exists(), "nothing moved");
    }

    #[test]
    fn promote_staged_xray_accepts_a_binary_only_archive_over_existing_assets() {
        // An update may ship the binary alone; the assets already installed stay
        // valid, so this is a startable install.
        let staging = stage(&[("xray", b"binary")]);
        let root = tempfile::tempdir().unwrap();
        let dest = root.path().join("xray");
        std::fs::create_dir_all(&dest).unwrap();
        std::fs::write(dest.join("geoip.dat"), b"old geoip").unwrap();
        std::fs::write(dest.join("geosite.dat"), b"old geosite").unwrap();

        let promoted = promote_staged_xray(&staging.path().join("xray"), &dest).unwrap();

        assert_eq!(std::fs::read(&promoted).unwrap(), b"binary");
        assert_eq!(
            std::fs::read(dest.join("geosite.dat")).unwrap(),
            b"old geosite"
        );
    }

    #[test]
    fn promote_staged_xray_leaves_binary_staged_when_a_data_file_cannot_be_moved() {
        let staging = stage(&[
            ("xray", b"binary"),
            ("geoip.dat", b"geoip"),
            ("geosite.dat", b"geosite"),
        ]);
        let root = tempfile::tempdir().unwrap();
        let dest = root.path().join("xray");
        // A directory where the file must land makes that single move fail.
        std::fs::create_dir_all(dest.join("geosite.dat")).unwrap();

        let err = promote_staged_xray(&staging.path().join("xray"), &dest).unwrap_err();

        assert!(matches!(err, AppError::Io(_)), "got: {err:?}");
        // The binary is the commit step: it must not land before every asset is
        // in place, or xray starts and dies.
        assert!(!dest.join("xray").exists());
    }

    #[test]
    fn promote_staged_xray_rejects_an_archive_without_a_binary() {
        let staging = stage(&[("geoip.dat", b"geoip")]);
        let root = tempfile::tempdir().unwrap();
        let dest = root.path().join("xray");

        let err = promote_staged_xray(&staging.path().join("xray"), &dest).unwrap_err();

        assert!(format!("{err}").contains("does not contain"), "got: {err}");
        assert!(!dest.join("geoip.dat").exists(), "nothing moved");
    }

    /// An install left behind by a build before the promotion fix must not be
    /// reported as installed: the panel would say ready while the supervisor
    /// crash-loops on it.
    #[test]
    fn install_is_startable_requires_assets_and_an_executable_binary() {
        let root = tempfile::tempdir().unwrap();
        let xray_dir = root.path().join("xray");
        std::fs::create_dir_all(&xray_dir).unwrap();
        let bin = xray_dir.join("xray");
        std::fs::write(&bin, b"binary").unwrap();

        // Assets missing (the old promotion dropped them).
        assert!(!install_is_startable(&xray_dir, &bin));

        std::fs::write(xray_dir.join("geoip.dat"), b"geoip").unwrap();
        std::fs::write(xray_dir.join("geosite.dat"), b"geosite").unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            // Assets present but extraction left it 0o644 (the old update path
            // never chmod'd): unspawnable.
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o644)).unwrap();
            assert!(!install_is_startable(&xray_dir, &bin));

            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        assert!(install_is_startable(&xray_dir, &bin));
    }

    /// The settings page polls the job status while the archive downloads, so
    /// the counters have to advance as the bytes arrive: a progress value only
    /// written at the end would leave the bar empty for the whole transfer.
    #[tokio::test]
    async fn download_publishes_progress_while_the_body_streams() {
        use std::sync::Arc;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        const TOTAL: usize = 64 * 1024;

        // A body in two parts, the second sent after a pause, so a snapshot
        // taken during the transfer has a partial value to report.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 1024];
            let _ = socket.read(&mut request).await;
            socket
                .write_all(format!("HTTP/1.1 200 OK\r\ncontent-length: {TOTAL}\r\n\r\n").as_bytes())
                .await
                .unwrap();
            socket.write_all(&vec![b'a'; TOTAL / 2]).await.unwrap();
            socket.flush().await.unwrap();
            tokio::time::sleep(Duration::from_millis(300)).await;
            socket
                .write_all(&vec![b'b'; TOTAL - TOTAL / 2])
                .await
                .unwrap();
            socket.flush().await.unwrap();
        });

        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("Xray-test.zip");
        let installer = Installer::new("http://127.0.0.1:1").unwrap();
        let progress = Arc::new(DownloadProgress::default());

        let url = format!("http://{addr}/xray.zip");
        let download = {
            let dest = dest.clone();
            let progress = Arc::clone(&progress);
            tokio::spawn(async move { installer.download_to(&url, &dest, Some(&progress)).await })
        };

        // Mid-transfer: half the body is written and the total is announced.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let (downloaded, total) = progress.snapshot();
            if downloaded > 0 {
                assert_eq!(
                    downloaded,
                    (TOTAL / 2) as u64,
                    "a mid-transfer snapshot must report the bytes received so far"
                );
                assert_eq!(total, Some(TOTAL as u64));
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the download never published progress"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        download.await.unwrap().expect("download should succeed");
        assert_eq!(progress.snapshot(), (TOTAL as u64, Some(TOTAL as u64)));
        assert_eq!(std::fs::metadata(&dest).unwrap().len(), TOTAL as u64);
    }

    /// Extraction writes 0o644 files, so a promotion that does not set the mode
    /// leaves a binary the supervisor cannot spawn (EACCES).
    #[cfg(unix)]
    #[test]
    fn promote_staged_xray_marks_the_binary_executable() {
        use std::os::unix::fs::PermissionsExt;

        let staging = stage(&[
            ("xray", b"#!/bin/sh\n"),
            ("geoip.dat", b"geoip"),
            ("geosite.dat", b"geosite"),
        ]);
        let root = tempfile::tempdir().unwrap();
        let dest = root.path().join("xray");

        let promoted = promote_staged_xray(&staging.path().join("xray"), &dest).unwrap();

        let mode = std::fs::metadata(&promoted).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o755);
    }
}
