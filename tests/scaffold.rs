//! Tests for U1: project scaffold, migrations, single-instance lock.

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use fs2::FileExt;
use sqlx::SqlitePool;
use tempfile::tempdir;
use three_r_ui::config::AppConfig;
use three_r_ui::error::AppError;
use three_r_ui::start_services;

#[tokio::test]
async fn migrations_create_all_business_tables() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("test.db");
    let pool = SqlitePool::connect(&format!("sqlite://{}?mode=rwc", db_path.display()))
        .await
        .unwrap();

    sqlx::migrate!("./migrations").run(&pool).await.unwrap();

    let table_names: Vec<String> =
        sqlx::query_scalar("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
            .fetch_all(&pool)
            .await
            .unwrap();

    for required in [
        "admins",
        "clients",
        "inbounds",
        "sessions",
        "traffic_snapshots",
    ] {
        assert!(
            table_names.iter().any(|n| n == required),
            "missing table {required}; got {table_names:?}"
        );
    }
}

#[tokio::test]
async fn migrations_create_partial_unique_index() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("test.db");
    let pool = SqlitePool::connect(&format!("sqlite://{}?mode=rwc", db_path.display()))
        .await
        .unwrap();

    sqlx::migrate!("./migrations").run(&pool).await.unwrap();

    let idx_sql: String = sqlx::query_scalar(
        "SELECT sql FROM sqlite_master WHERE type='index' AND name='idx_inbounds_port_active'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();

    assert!(
        idx_sql.contains("WHERE status = 'active'"),
        "partial index not enforced: {idx_sql}"
    );
}

#[tokio::test]
async fn migrations_are_idempotent() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("test.db");
    let pool = SqlitePool::connect(&format!("sqlite://{}?mode=rwc", db_path.display()))
        .await
        .unwrap();

    sqlx::migrate!("./migrations").run(&pool).await.unwrap();
    sqlx::migrate!("./migrations").run(&pool).await.unwrap(); // second run must be no-op
}

#[tokio::test]
async fn run_acquires_lock_and_creates_data_dir() {
    let dir = tempdir().unwrap();
    let data_dir = dir.path().join("data");

    // Pre-place a stub xray binary so ensure_xray() takes its idempotent
    // short-circuit path. Without this the test would hit the real GitHub
    // releases API (slow, and wrong for a unit test). The stub exits
    // immediately so the supervisor's respawn backoff loop does not slow the
    // test down.
    let xray_dir = data_dir.join("xray");
    std::fs::create_dir_all(&xray_dir).unwrap();
    let stub = xray_dir.join("xray");
    std::fs::write(&stub, b"#!/bin/sh\nexit 0\n").unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    let mut config = AppConfig::from_env().unwrap();
    config.data_dir = data_dir.clone();
    config.bind_addr = "127.0.0.1:0".to_string(); // unused — no server yet

    // start_services() returns once every subsystem is live; shutdown() then
    // stops the supervisor. Driving run() directly would block on Ctrl-C,
    // which an in-process test cannot deliver.
    let services = start_services(config).await.expect("services should start");
    services.shutdown().await;

    // Data dir exists with mode 0700.
    let metadata = std::fs::metadata(&data_dir).expect("data dir not created");
    assert_eq!(
        metadata.permissions().mode() & 0o777,
        0o700,
        "data dir should be 0700"
    );

    // Lock file exists.
    assert!(data_dir.join("3r-ui.lock").exists(), "lock file missing");

    // SQLite db file exists.
    assert!(data_dir.join("3r-ui.db").exists(), "db file missing");

    // Stub xray untouched — ensure_xray did not overwrite or delete it.
    assert!(stub.is_file(), "pre-placed stub xray should survive");

    // Initial admin password file (if generated) is mode 0600.
    let pw_path = data_dir.join("initial_password");
    if pw_path.exists() {
        let md = std::fs::metadata(&pw_path).unwrap();
        assert_eq!(md.permissions().mode() & 0o777, 0o600);
        let pw = std::fs::read_to_string(&pw_path).unwrap();
        assert!(!pw.is_empty(), "initial password file empty");
    }
}

#[tokio::test]
async fn lock_error_is_reported_with_path() {
    let dir = tempdir().unwrap();
    let data_dir: PathBuf = dir.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();

    // Pre-create the lock file with exclusive lock held to force a conflict
    // when the second `run` invocation tries to acquire it.
    let lock_path = data_dir.join("3r-ui.lock");
    let first = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .unwrap();
    first.lock_exclusive().unwrap();
    std::mem::forget(first);

    let mut config = AppConfig::from_env().unwrap();
    config.data_dir = data_dir.clone();
    config.bind_addr = "127.0.0.1:0".to_string();

    let result =
        tokio::time::timeout(std::time::Duration::from_secs(2), start_services(config)).await;
    let err = match result {
        Err(_) => panic!("run hung — second instance should have failed immediately on lock"),
        Ok(Err(e)) => e,
        Ok(Ok(_)) => panic!("run unexpectedly succeeded with held lock"),
    };

    match err {
        AppError::Lock(path, _) => assert!(path.contains("3r-ui.lock")),
        other => panic!("expected Lock error, got {other:?}"),
    }
}
