pub mod config;
pub mod db;
pub mod error;

use std::path::Path;

use sqlx::SqlitePool;
use tokio::sync::mpsc;

pub use config::AppConfig;
pub use error::{AppError, AppResult};

#[derive(Clone)]
pub struct AppState {
    pub config: AppConfig,
    pub pool: SqlitePool,
    pub supervisor_tx: mpsc::Sender<SupervisorCommand>,
}

#[derive(Debug, Clone)]
pub enum SupervisorCommand {
    Reload,
    Stop,
}

/// U1 partial implementation: scaffolding only.
///
/// Wires up:
/// - Single-instance `flock` on `${data_dir}/3r-ui.lock`
/// - Data directory at mode 0700
/// - SQLite pool + `sqlx::migrate!()` apply
/// - Initial admin password generation on first start
/// - Holds the process alive via `ctrl_c`
///
/// Subsequent units extend `run` to start the xray supervisor (U3),
/// the Axum HTTP server (U4), the stats poller (U7), and the e2e
/// wiring (U10).
pub async fn run(config: AppConfig) -> AppResult<()> {
    use fs2::FileExt;
    use std::fs::OpenOptions;
    use std::os::unix::fs::PermissionsExt;

    std::fs::create_dir_all(&config.data_dir).map_err(AppError::Io)?;
    std::fs::set_permissions(
        &config.data_dir,
        std::fs::Permissions::from_mode(0o700),
    )
    .map_err(AppError::Io)?;

    // Single-instance lock: leaked on purpose — released on process exit.
    // Uses try_lock_exclusive so a second invocation fails fast with a clear
    // error rather than blocking forever waiting for the first to die.
    let lock_path = config.data_dir.join("3r-ui.lock");
    let lock_file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .map_err(AppError::Io)?;
    lock_file
        .try_lock_exclusive()
        .map_err(|e| AppError::Lock(lock_path.display().to_string(), e.to_string()))?;
    std::mem::forget(lock_file);

    let db_path = config.data_dir.join("3r-ui.db");
    let db_url = format!("sqlite://{}?mode=rwc", db_path.display());
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(8)
        .connect(&db_url)
        .await
        .map_err(AppError::Database)?;
    sqlx::migrate!("./migrations")
        .run(&pool)
        .await
        .map_err(AppError::Migration)?;

    ensure_initial_admin(&pool, &config.data_dir).await?;

    tracing::info!(
        addr = %config.bind_addr,
        data_dir = %config.data_dir.display(),
        "U1 scaffold ready (xray installer + axum server wire in U2/U4)"
    );

    // U1 placeholder: hold the pool until the future is dropped, so the
    // connection remains valid for the lifetime of `run`. U4 will replace
    // this with the axum serve loop and pass the pool to AppState.
    let _pool = pool;
    Ok(())
}

async fn ensure_initial_admin(pool: &SqlitePool, data_dir: &Path) -> AppResult<()> {
    use std::os::unix::fs::PermissionsExt;
    use rand::{distributions::Alphanumeric, Rng};

    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM admins")
        .fetch_one(pool)
        .await
        .map_err(AppError::Database)?;
    if count == 0 {
        let password: String = rand::thread_rng()
            .sample_iter(&Alphanumeric)
            .take(24)
            .map(char::from)
            .collect();
        // U1 placeholder hash; U4 will swap to bcrypt cost=12 via crypto::password::hash.
        let hash = format!("placeholder-u4-todo:{password}");
        sqlx::query(
            "INSERT INTO admins (username, password_hash, created_at) VALUES (?, ?, unixepoch())",
        )
        .bind("admin")
        .bind(&hash)
        .execute(pool)
        .await
        .map_err(AppError::Database)?;

        let pw_path = data_dir.join("initial_password");
        std::fs::write(&pw_path, &password).map_err(AppError::Io)?;
        std::fs::set_permissions(&pw_path, std::fs::Permissions::from_mode(0o600))
            .map_err(AppError::Io)?;

        eprintln!("======================================================================");
        eprintln!("3r-ui initial admin password written to: {}", pw_path.display());
        eprintln!("Username: admin");
        eprintln!("Password: {password}");
        eprintln!("(U1 placeholder hash; bcrypt to be wired in U4)");
        eprintln!("======================================================================");
    }
    Ok(())
}
