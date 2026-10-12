//! Tests for xray-core auto-download and install.
//!
//! Uses `wiremock` to stand in for the GitHub releases API and a fixture ZIP
//! built in-memory to verify extraction. No network or filesystem xray
//! binary is required.

use tempfile::tempdir;
use three_r_ui::xray::Installer;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const REPO_PATH: &str = "/XTLS/Xray-core/releases/latest";
const ASSET_NAME_LINUX_64: &str = "Xray-linux-64.zip";

fn build_minimal_xray_zip(version: &str) -> Vec<u8> {
    // zip 2.x writes are buffered; we construct the archive by hand in
    // central-directory + local-header form to avoid pulling in extra deps.
    // The contents are an empty executable file named "xray" inside a
    // top-level "xray-core-${version}/" directory.
    let mut buf: Vec<u8> = Vec::new();
    let payload = b"#!/bin/sh\necho xray\n";
    let top = format!("xray-core-{version}/");
    let file_name = format!("{top}xray");

    // Local file header for `xray`
    buf.extend_from_slice(b"PK\x03\x04");
    buf.extend_from_slice(&20u16.to_le_bytes()); // version needed
    buf.extend_from_slice(&0u16.to_le_bytes()); // flags
    buf.extend_from_slice(&0u16.to_le_bytes()); // compression (stored)
    buf.extend_from_slice(&0u16.to_le_bytes()); // mod time
    buf.extend_from_slice(&0u16.to_le_bytes()); // mod date
    let crc = crc32(payload);
    buf.extend_from_slice(&crc.to_le_bytes());
    buf.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    buf.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    buf.extend_from_slice(&(file_name.len() as u16).to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(file_name.as_bytes());
    buf.extend_from_slice(payload);

    // Central directory entry for `xray`
    let cd_offset = buf.len() as u32;
    buf.extend_from_slice(b"PK\x01\x02");
    buf.extend_from_slice(&20u16.to_le_bytes()); // version made by
    buf.extend_from_slice(&20u16.to_le_bytes()); // version needed
    buf.extend_from_slice(&0u16.to_le_bytes()); // flags
    buf.extend_from_slice(&0u16.to_le_bytes()); // compression
    buf.extend_from_slice(&0u16.to_le_bytes()); // mod time
    buf.extend_from_slice(&0u16.to_le_bytes()); // mod date
    buf.extend_from_slice(&crc.to_le_bytes());
    buf.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    buf.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    buf.extend_from_slice(&(file_name.len() as u16).to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes()); // extra
    buf.extend_from_slice(&0u16.to_le_bytes()); // comment
    buf.extend_from_slice(&0u16.to_le_bytes()); // disk number
    buf.extend_from_slice(&0u16.to_le_bytes()); // internal attrs
    buf.extend_from_slice(&0u32.to_le_bytes()); // external attrs
    buf.extend_from_slice(&0u32.to_le_bytes()); // local header offset
    buf.extend_from_slice(file_name.as_bytes());

    let cd_size = buf.len() as u32 - cd_offset;

    // End of central directory
    buf.extend_from_slice(b"PK\x05\x06");
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&1u16.to_le_bytes()); // entries on this disk
    buf.extend_from_slice(&1u16.to_le_bytes()); // total entries
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

fn current_asset_name() -> &'static str {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => ASSET_NAME_LINUX_64,
        ("linux", "aarch64") => "Xray-linux-arm64.zip",
        ("linux", "arm") => "Xray-linux-armv7.zip",
        ("macos", "x86_64") => "Xray-macos-64.zip",
        ("macos", "aarch64") => "Xray-macos-arm64.zip",
        ("windows", "x86_64") => "Xray-windows-64.zip",
        ("windows", "aarch64") => "Xray-windows-arm64.zip",
        _ => panic!("unsupported target triple for test"),
    }
}

#[tokio::test]
async fn ensure_xray_downloads_extracts_and_promotes_binary() {
    let server = stand_up_release_endpoint(build_minimal_xray_zip("v1.8.5"), 200).await;
    let dir = tempdir().unwrap();
    let installer = Installer::new(server.uri()).unwrap();

    let xray_path = installer.ensure_xray(dir.path()).await.expect("install");

    assert!(
        xray_path.is_file(),
        "xray binary not extracted at {}",
        xray_path.display()
    );
    assert_eq!(
        xray_path.file_name().unwrap(),
        if cfg!(windows) { "xray.exe" } else { "xray" }
    );

    // Archive + versioned dir should be cleaned up.
    let downloads_dir = dir.path().join("downloads");
    assert!(
        !downloads_dir.join(current_asset_name()).exists(),
        "archive should be cleaned up after install"
    );

    // Second call should be a no-op (no extra HTTP requests).
    let path2 = installer
        .ensure_xray(dir.path())
        .await
        .expect("second call");
    assert_eq!(path2, xray_path);
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

    // Use a never-reached mock; the call should never hit it.
    let server = MockServer::start().await;
    let installer = Installer::new(server.uri()).unwrap();
    let path = installer
        .ensure_xray(dir.path())
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
        .ensure_xray(dir.path())
        .await
        .expect_err("should fail");
    let msg = format!("{err}");
    assert!(
        msg.contains("rate-limited") || msg.contains("429"),
        "msg: {msg}"
    );
}

#[tokio::test]
async fn ensure_xray_returns_readable_error_on_connection_failure() {
    let dir = tempdir().unwrap();
    // Point at a port nothing is listening on.
    let installer = Installer::new("http://127.0.0.1:1").unwrap();
    let err = installer
        .ensure_xray(dir.path())
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
        .ensure_xray(dir.path())
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
    let server = stand_up_release_endpoint(build_minimal_xray_zip("v1.8.5"), 200).await;
    let dir = tempdir().unwrap();
    let installer = Installer::new(server.uri()).unwrap();

    let (xray_path, tag) = installer
        .install_version(dir.path(), None)
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
        .and(path("/XTLS/Xray-core/releases/tags/v1.8.10"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&body))
        .mount(&server)
        .await;

    let archive = build_minimal_xray_zip("v1.8.10");
    Mock::given(method("GET"))
        .and(path("/xray.zip"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(archive))
        .mount(&server)
        .await;

    let dir = tempdir().unwrap();
    let installer = Installer::new(server.uri()).unwrap();

    let (xray_path, tag) = installer
        .install_version(dir.path(), Some("v1.8.10"))
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
        .and(path("/XTLS/Xray-core/releases/latest"))
        .respond_with(ResponseTemplate::new(429).set_body_string("rate limited"))
        .mount(&server)
        .await;

    let dir = tempdir().unwrap();
    let installer = Installer::new(server.uri()).unwrap();

    let err = installer
        .install_version(dir.path(), None)
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
        .and(path("/XTLS/Xray-core/releases/latest"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(&server)
        .await;

    let dir = tempdir().unwrap();
    let installer = Installer::new(server.uri()).unwrap();

    let err = installer
        .install_version(dir.path(), None)
        .await
        .expect_err("install_version should error on missing asset");

    let msg = format!("{err}");
    assert!(
        msg.contains("no asset") || msg.contains("for this platform"),
        "msg: {msg}"
    );
}
