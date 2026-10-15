//! Tests for xray-core auto-download and install.
//!
//! Uses `wiremock` to stand in for the GitHub releases API and a fixture ZIP
//! built in-memory to verify extraction. No network or filesystem xray
//! binary is required.

use tempfile::tempdir;
use three_r_ui::xray::{DownloadProgress, Installer};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

// The installer talks to the GitHub **API** (api.github.com); the web host
// serves HTML at the equivalent path. These literals pin that contract.
const REPO_PATH: &str = "/repos/XTLS/Xray-core/releases/latest";
const ASSET_NAME_LINUX_64: &str = "Xray-linux-64.zip";

/// Build an in-memory release archive.
///
/// The entry set mirrors what XTLS/Xray-core publishes: the binary plus the geo
/// data files xray loads from the binary's directory. Carrying them is the point
/// — a fixture with only `xray` cannot see a promotion that drops them.
/// `top_dir` nests every entry under one leading directory, the layout some
/// mirrors repackage the release with.
fn build_minimal_xray_zip(top_dir: Option<&str>) -> Vec<u8> {
    let entries = [
        ("xray", b"#!/bin/sh\necho xray\n".as_slice()),
        ("geoip.dat", b"geoip".as_slice()),
        ("geosite.dat", b"geosite".as_slice()),
    ];
    let entries: Vec<(String, &[u8])> = entries
        .iter()
        .map(|(name, payload)| {
            let name = match top_dir {
                Some(dir) => format!("{dir}/{name}"),
                None => (*name).to_string(),
            };
            (name, *payload)
        })
        .collect();
    build_zip(&entries)
}

/// Serialise `entries` as a stored (uncompressed) ZIP archive.
///
/// zip 2.x writes are buffered; we construct the archive by hand in
/// central-directory + local-header form to avoid pulling in extra deps.
fn build_zip(entries: &[(String, &[u8])]) -> Vec<u8> {
    let mut buf: Vec<u8> = Vec::new();
    let mut central: Vec<u8> = Vec::new();

    for (name, payload) in entries {
        let crc = crc32(payload);

        // Local file header
        let local_offset = buf.len() as u32;
        buf.extend_from_slice(b"PK\x03\x04");
        buf.extend_from_slice(&20u16.to_le_bytes()); // version needed
        buf.extend_from_slice(&0u16.to_le_bytes()); // flags
        buf.extend_from_slice(&0u16.to_le_bytes()); // compression (stored)
        buf.extend_from_slice(&0u16.to_le_bytes()); // mod time
        buf.extend_from_slice(&0u16.to_le_bytes()); // mod date
        buf.extend_from_slice(&crc.to_le_bytes());
        buf.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        buf.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        buf.extend_from_slice(&(name.len() as u16).to_le_bytes());
        buf.extend_from_slice(&0u16.to_le_bytes()); // extra
        buf.extend_from_slice(name.as_bytes());
        buf.extend_from_slice(payload);

        // Central directory entry
        central.extend_from_slice(b"PK\x01\x02");
        central.extend_from_slice(&20u16.to_le_bytes()); // version made by
        central.extend_from_slice(&20u16.to_le_bytes()); // version needed
        central.extend_from_slice(&0u16.to_le_bytes()); // flags
        central.extend_from_slice(&0u16.to_le_bytes()); // compression
        central.extend_from_slice(&0u16.to_le_bytes()); // mod time
        central.extend_from_slice(&0u16.to_le_bytes()); // mod date
        central.extend_from_slice(&crc.to_le_bytes());
        central.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        central.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        central.extend_from_slice(&(name.len() as u16).to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes()); // extra
        central.extend_from_slice(&0u16.to_le_bytes()); // comment
        central.extend_from_slice(&0u16.to_le_bytes()); // disk number
        central.extend_from_slice(&0u16.to_le_bytes()); // internal attrs
        central.extend_from_slice(&0u32.to_le_bytes()); // external attrs
        central.extend_from_slice(&local_offset.to_le_bytes());
        central.extend_from_slice(name.as_bytes());
    }

    let cd_offset = buf.len() as u32;
    let cd_size = central.len() as u32;
    buf.extend_from_slice(&central);

    // End of central directory
    buf.extend_from_slice(b"PK\x05\x06");
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&(entries.len() as u16).to_le_bytes()); // entries on this disk
    buf.extend_from_slice(&(entries.len() as u16).to_le_bytes()); // total entries
    buf.extend_from_slice(&cd_size.to_le_bytes());
    buf.extend_from_slice(&cd_offset.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes()); // comment length
    buf
}

fn crc32(buf: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &b in buf {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

async fn stand_up_release_endpoint(archive: Vec<u8>, status: u16) -> MockServer {
    let server = MockServer::start().await;
    let asset_name = current_asset_name();
    let body = serde_json::json!({
        "tag_name": "v1.8.5",
        "assets": [{
            "name": asset_name,
            "browser_download_url": format!("{}/xray.zip", server.uri()),
            "size": archive.len(),
        }]
    });
    Mock::given(method("GET"))
        .and(path(REPO_PATH))
        .respond_with(ResponseTemplate::new(status).set_body_json(body))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/xray.zip"))
        .respond_with(ResponseTemplate::new(status).set_body_bytes(archive))
        .mount(&server)
        .await;
    server
}

/// Asset name the installer should look for on this platform.
///
/// These literals are the oracle: they must match the names XTLS/Xray-core
/// actually publishes (the arm64/arm32 builds carry `-v8a`/`-v7a`). Deriving
/// them from the implementation would make the test pass even when both sides
/// are wrong.
fn current_asset_name() -> &'static str {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => ASSET_NAME_LINUX_64,
        ("linux", "aarch64") => "Xray-linux-arm64-v8a.zip",
        ("linux", "arm") => "Xray-linux-arm32-v7a.zip",
        ("macos", "x86_64") => "Xray-macos-64.zip",
        ("macos", "aarch64") => "Xray-macos-arm64-v8a.zip",
        ("windows", "x86_64") => "Xray-windows-64.zip",
        ("windows", "aarch64") => "Xray-windows-arm64-v8a.zip",
        _ => panic!("unsupported target triple for test"),
    }
}

#[tokio::test]
async fn ensure_xray_downloads_extracts_and_promotes_binary() {
    let server = stand_up_release_endpoint(build_minimal_xray_zip(None), 200).await;
    let dir = tempdir().unwrap();
    let installer = Installer::new(server.uri()).unwrap();

    let xray_path = installer
        .ensure_xray(dir.path(), None)
        .await
        .expect("install");

    assert!(
        xray_path.is_file(),
        "xray binary not extracted at {}",
        xray_path.display()
    );
    assert_eq!(
        xray_path.file_name().unwrap(),
        if cfg!(windows) { "xray.exe" } else { "xray" }
    );

    // xray resolves geosite.dat/geoip.dat from the binary's own directory, and
    // the rendered config always references `geosite:category-ads-all`, so an
    // install that promotes the binary alone dies at config load (exit 23).
    let install_dir = dir.path().join("xray");
    for name in ["geoip.dat", "geosite.dat"] {
        let path = install_dir.join(name);
        assert!(
            path.is_file(),
            "{name} must be promoted beside the binary, expected {}",
            path.display()
        );
    }

    // Archive + versioned dir should be cleaned up.
    let downloads_dir = dir.path().join("downloads");
    assert!(
        !downloads_dir.join(current_asset_name()).exists(),
        "archive should be cleaned up after install"
    );

    // Second call should be a no-op (no extra HTTP requests).
    let path2 = installer
        .ensure_xray(dir.path(), None)
        .await
        .expect("second call");
    assert_eq!(path2, xray_path);
}

#[tokio::test]
async fn ensure_xray_extracts_archive_nested_under_a_top_directory() {
    // Mirrors that repackage the release under a single top-level directory
    // must keep working alongside the flat layout the official zips use.
    let server =
        stand_up_release_endpoint(build_minimal_xray_zip(Some("xray-core-v1.8.5")), 200).await;
    let dir = tempdir().unwrap();
    let installer = Installer::new(server.uri()).unwrap();

    let xray_path = installer
        .ensure_xray(dir.path(), None)
        .await
        .expect("install");

    assert!(
        xray_path.is_file(),
        "xray binary not extracted at {}",
        xray_path.display()
    );
    // The data files are nested with it, so stripping the top directory must
    // leave them next to the binary too — not only the binary.
    for name in ["geoip.dat", "geosite.dat"] {
        assert!(
            dir.path().join("xray").join(name).is_file(),
            "{name} must be promoted beside the binary"
        );
    }
}

#[tokio::test]
async fn ensure_xray_is_idempotent_when_binary_present() {
    let dir = tempdir().unwrap();
    let xray_dir = dir.path().join("xray");
    std::fs::create_dir_all(&xray_dir).unwrap();
    let placeholder = xray_dir.join(if cfg!(windows) { "xray.exe" } else { "xray" });
    std::fs::write(&placeholder, b"#!/bin/sh\necho pre-existing\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&placeholder, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    // A complete install also carries the assets xray loads at startup; a binary
    // on its own is not an install (see `ensure_xray_reinstalls_an_unstartable_install`).
    for name in ["geoip.dat", "geosite.dat"] {
        std::fs::write(xray_dir.join(name), b"data").unwrap();
    }

    // Use a never-reached mock; the call should never hit it.
    let server = MockServer::start().await;
    let installer = Installer::new(server.uri()).unwrap();
    let path = installer
        .ensure_xray(dir.path(), None)
        .await
        .expect("no-op install");
    assert_eq!(path, placeholder);
    // No requests received (no mock set).
    assert_eq!(
        server
            .received_requests()
            .await
            .map(|r| r.len())
            .unwrap_or(0),
        0
    );
}

/// Builds left by earlier versions promote the binary without the assets (xray
/// exits 23 at config load) or without the exec bit (spawn fails with EACCES).
/// Seeing one of those at startup must re-install, not report ready.
#[tokio::test]
async fn ensure_xray_reinstalls_an_unstartable_install() {
    let server = stand_up_release_endpoint(build_minimal_xray_zip(None), 200).await;
    let dir = tempdir().unwrap();
    let xray_dir = dir.path().join("xray");
    std::fs::create_dir_all(&xray_dir).unwrap();
    let stale = xray_dir.join(if cfg!(windows) { "xray.exe" } else { "xray" });
    std::fs::write(&stale, b"stale binary without assets").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&stale, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    let installer = Installer::new(server.uri()).unwrap();
    let xray_path = installer
        .ensure_xray(dir.path(), None)
        .await
        .expect("should re-install over the unusable install");

    assert_eq!(xray_path, stale);
    assert_ne!(
        std::fs::read(&xray_path).unwrap(),
        b"stale binary without assets",
        "the stale binary must have been replaced"
    );
    for name in ["geoip.dat", "geosite.dat"] {
        assert!(xray_dir.join(name).is_file(), "{name} must be installed");
    }
}

#[tokio::test]
async fn ensure_xray_returns_readable_error_on_http_429() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(REPO_PATH))
        .respond_with(ResponseTemplate::new(429).set_body_string("rate limited"))
        .mount(&server)
        .await;

    let dir = tempdir().unwrap();
    let installer = Installer::new(server.uri()).unwrap();
    let err = installer
        .ensure_xray(dir.path(), None)
        .await
        .expect_err("should fail");
    let msg = format!("{err}");
    assert!(
        msg.contains("rate-limited") || msg.contains("429"),
        "msg: {msg}"
    );
}

#[tokio::test]
async fn ensure_xray_returns_readable_error_on_http_403_rate_limit() {
    // GitHub reports an exhausted token-less quota as 403 plus
    // `x-ratelimit-remaining: 0`, which the installer has to recognize as
    // throttling rather than surfacing as a bare "HTTP 403".
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(REPO_PATH))
        .respond_with(
            ResponseTemplate::new(403)
                .insert_header("x-ratelimit-remaining", "0")
                .set_body_string("API rate limit exceeded"),
        )
        .mount(&server)
        .await;

    let dir = tempdir().unwrap();
    let installer = Installer::new(server.uri()).unwrap();
    let err = installer
        .ensure_xray(dir.path(), None)
        .await
        .expect_err("should fail");
    let msg = format!("{err}");
    assert!(msg.contains("rate-limited"), "msg: {msg}");
    assert!(
        msg.contains("mirror"),
        "msg should point at a way out: {msg}"
    );
}

#[tokio::test]
async fn ensure_xray_returns_readable_error_on_connection_failure() {
    let dir = tempdir().unwrap();
    // Point at a port nothing is listening on.
    let installer = Installer::new("http://127.0.0.1:1").unwrap();
    let err = installer
        .ensure_xray(dir.path(), None)
        .await
        .expect_err("should fail");
    let msg = format!("{err}");
    assert!(!msg.is_empty(), "error message should not be empty");
}

#[tokio::test]
async fn ensure_xray_missing_asset_returns_actionable_error() {
    let server = MockServer::start().await;
    let body = serde_json::json!({
        "tag_name": "v1.8.5",
        "assets": [{
            "name": "Xray-some-other-platform.zip",
            "browser_download_url": format!("{}/xray.zip", server.uri()),
        }]
    });
    Mock::given(method("GET"))
        .and(path(REPO_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(&server)
        .await;

    let dir = tempdir().unwrap();
    let installer = Installer::new(server.uri()).unwrap();
    let err = installer
        .ensure_xray(dir.path(), None)
        .await
        .expect_err("should fail");
    let msg = format!("{err}");
    assert!(
        msg.contains("no asset") || msg.contains("for this platform"),
        "msg: {msg}"
    );
}

// ─── install_version tests ──────────────────────────────────────────────────

#[tokio::test]
async fn install_version_downloads_latest_into_staging() {
    let server = stand_up_release_endpoint(build_minimal_xray_zip(None), 200).await;
    let dir = tempdir().unwrap();
    let installer = Installer::new(server.uri()).unwrap();

    let (xray_path, tag) = installer
        .install_version(dir.path(), None, None)
        .await
        .expect("install_version should succeed");

    assert!(
        xray_path.is_file(),
        "xray should exist at {}",
        xray_path.display()
    );
    assert_eq!(&tag, "v1.8.5");

    // Staging directory should exist (not cleaned up — caller is responsible).
    let staging_parent = xray_path.parent().unwrap();
    let staging_name = staging_parent.file_name().unwrap().to_str().unwrap();
    assert!(
        staging_name.starts_with("xray-v1.8.5-staging-"),
        "staging dir name: {staging_name}"
    );
}

#[tokio::test]
async fn install_version_explicit_tag_only_hits_tag_route() {
    // Mock the /releases/tags/v1.8.10 route (not /releases/latest).
    let server = MockServer::start().await;
    let asset_name = current_asset_name();
    let body = serde_json::json!({
        "tag_name": "v1.8.10",
        "assets": [{
            "name": asset_name,
            "browser_download_url": format!("{}/xray.zip", server.uri()),
            "size": 0,
        }]
    });

    // Only mount the tags endpoint — no /releases/latest mock.
    Mock::given(method("GET"))
        .and(path("/repos/XTLS/Xray-core/releases/tags/v1.8.10"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&body))
        .mount(&server)
        .await;

    let archive = build_minimal_xray_zip(None);
    Mock::given(method("GET"))
        .and(path("/xray.zip"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(archive))
        .mount(&server)
        .await;

    let dir = tempdir().unwrap();
    let installer = Installer::new(server.uri()).unwrap();

    let (xray_path, tag) = installer
        .install_version(dir.path(), Some("v1.8.10"), None)
        .await
        .expect("install_version with explicit tag should succeed");

    assert!(xray_path.is_file());
    assert_eq!(&tag, "v1.8.10");

    // Verify no /releases/latest was called.
    let latest_hits = server
        .received_requests()
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|r| r.url.path().contains("latest"))
        .count();
    assert_eq!(
        latest_hits, 0,
        "/releases/latest should not have been called"
    );
}

#[tokio::test]
async fn install_version_http_429_returns_error_no_staging() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(REPO_PATH))
        .respond_with(ResponseTemplate::new(429).set_body_string("rate limited"))
        .mount(&server)
        .await;

    let dir = tempdir().unwrap();
    let installer = Installer::new(server.uri()).unwrap();

    let err = installer
        .install_version(dir.path(), None, None)
        .await
        .expect_err("install_version should error on 429");

    let msg = format!("{err}");
    assert!(
        msg.contains("429") || msg.contains("rate-limited"),
        "msg: {msg}"
    );

    // No staging directory should be left behind.
    let entries = std::fs::read_dir(dir.path()).unwrap();
    let staging_dirs: Vec<_> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains("staging"))
        .collect();
    assert!(
        staging_dirs.is_empty(),
        "no staging dirs expected, got: {staging_dirs:?}"
    );
}

#[tokio::test]
async fn install_version_missing_asset_returns_error() {
    let server = MockServer::start().await;
    let body = serde_json::json!({
        "tag_name": "v99.0.0",
        "assets": [{
            // Asset name does not match the current platform.
            "name": "Xray-some-other-platform.zip",
            "browser_download_url": format!("{}/xray.zip", server.uri()),
        }]
    });
    Mock::given(method("GET"))
        .and(path(REPO_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(&server)
        .await;

    let dir = tempdir().unwrap();
    let installer = Installer::new(server.uri()).unwrap();

    let err = installer
        .install_version(dir.path(), None, None)
        .await
        .expect_err("install_version should error on missing asset");

    let msg = format!("{err}");
    assert!(
        msg.contains("no asset") || msg.contains("for this platform"),
        "msg: {msg}"
    );
}

/// The update job renders a progress bar from these counters, so an install has
/// to publish the archive's byte counts to the observer the caller hands it.
#[tokio::test]
async fn install_version_publishes_download_progress() {
    let archive = build_minimal_xray_zip(None);
    let server = stand_up_release_endpoint(archive.clone(), 200).await;
    let dir = tempdir().unwrap();
    let installer = Installer::new(server.uri()).unwrap();
    let progress = DownloadProgress::default();

    installer
        .install_version(dir.path(), None, Some(&progress))
        .await
        .expect("install_version should succeed");

    let (downloaded, total) = progress.snapshot();
    assert_eq!(
        downloaded,
        archive.len() as u64,
        "the observer must see the whole archive"
    );
    assert_eq!(
        total,
        Some(archive.len() as u64),
        "the announced size must reach the observer"
    );
}
