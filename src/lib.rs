pub mod config;
pub mod crypto;
pub mod db;
pub mod error;
pub mod quota;
pub mod share;
pub mod web;
pub mod xray;

use std::path::Path;

use sqlx::SqlitePool;
use tokio::sync::mpsc;

pub use config::AppConfig;
pub use error::{AppError, AppResult};

/// Handle to everything a running panel owns. Returned by [`start_services`]
/// so tests can drive the lifecycle without going through signal handling.
pub struct Services {
    pub pool: SqlitePool,
    pub supervisor_tx: mpsc::Sender<xray::SupervisorCommand>,
    supervisor_handle: tokio::task::JoinHandle<()>,
    http_handle: tokio::task::JoinHandle<()>,
    stats_handle: tokio::task::JoinHandle<()>,
}

impl Services {
    /// Stop the HTTP server, the xray supervisor, and the stats poll loop,
    /// then wait for all three.
    pub async fn shutdown(self) {
        let _ = self.supervisor_tx.send(xray::SupervisorCommand::Stop).await;
        let _ =
            tokio::time::timeout(std::time::Duration::from_secs(10), self.supervisor_handle).await;
        self.stats_handle.abort();
        self.http_handle.abort();
    }
}

/// Boot every long-running subsystem the panel owns: SQLite (migrated),
/// the initial admin account, the xray-core binary, and the xray supervisor.
///
/// Returns once all subsystems are live. The caller decides how long to stay
/// up (see [`run`], which waits for Ctrl-C).
pub async fn start_services(config: AppConfig) -> AppResult<Services> {
    let pool = init_storage(&config).await?;
    ensure_initial_admin(&pool, &config.data_dir).await?;

    let xray_bin = ensure_xray_binary(&config).await;
    let config_path = config.data_dir.join("xray").join("config.json");

    let supervisor = xray::XraySupervisor::new(xray_bin, config_path, config.data_dir.clone());
    let (supervisor_tx, supervisor_rx) = mpsc::channel::<xray::SupervisorCommand>(16);
    let supervisor_handle = tokio::spawn(async move {
        if let Err(e) = supervisor.run(supervisor_rx).await {
            tracing::error!(error = %e, "xray supervisor exited with error");
        }
    });

    // U4: HTTP server.
    let bind_addr: std::net::SocketAddr = config.bind_addr.parse().map_err(|e| {
        AppError::Config(format!(
            "BIND_ADDR {:?} is not a valid socket address: {e}",
            config.bind_addr
        ))
    })?;
    let app_state = web::AppState {
        config: config.clone(),
        pool: pool.clone(),
        supervisor_tx: supervisor_tx.clone(),
    };
    let http_handle = tokio::spawn(async move {
        let listener = match tokio::net::TcpListener::bind(bind_addr).await {
            Ok(l) => l,
            Err(e) => {
                tracing::error!(error = %e, %bind_addr, "failed to bind HTTP listener");
                return;
            }
        };
        tracing::info!(addr = %bind_addr, "HTTP server listening");
        if let Err(e) = axum::serve(listener, web::router(app_state)).await {
            tracing::error!(error = %e, "HTTP server exited with error");
        }
    });

    // Periodically drop expired sessions so the table does not grow without bound.
    {
        let pool = pool.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(std::time::Duration::from_secs(3600));
            loop {
                ticker.tick().await;
                if let Err(e) = sqlx::query("DELETE FROM sessions WHERE expires_at < ?")
                    .bind(crypto::session::now_unix())
                    .execute(&pool)
                    .await
                {
                    tracing::warn!(error = %e, "expired session cleanup failed");
                }
            }
        });
    }

    // U7: stats poll loop. Owns its own gRPC client and snapshot state; runs
    // until the panel shuts down.
    let stats_handle = tokio::spawn(xray::stats::run_loop(
        pool.clone(),
        config.data_dir.clone(),
        config.stats_interval_secs,
        supervisor_tx.clone(),
    ));

    Ok(Services {
        pool,
        supervisor_tx,
        supervisor_handle,
        http_handle,
        stats_handle,
    })
}

/// Acquire the single-instance lock, create the data directory, and open a
/// migrated SQLite pool.
///
/// Public so integration tests can boot against the same migration and
/// locking path the binary uses, instead of a parallel setup that could drift.
pub async fn init_storage(config: &AppConfig) -> AppResult<SqlitePool> {
    use fs2::FileExt;
    use std::fs::OpenOptions;
    use std::os::unix::fs::PermissionsExt;

    std::fs::create_dir_all(&config.data_dir).map_err(AppError::Io)?;
    std::fs::set_permissions(&config.data_dir, std::fs::Permissions::from_mode(0o700))
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
    Ok(pool)
}

async fn ensure_xray_binary(config: &AppConfig) -> std::path::PathBuf {
    let installer = match xray::Installer::new(config.xray_download_base.clone()) {
        Ok(i) => i,
        Err(e) => {
            tracing::warn!(error = %e, "xray installer init failed");
            return default_xray_path(config);
        }
    };
    match installer.ensure_xray(&config.data_dir).await {
        Ok(path) => {
            tracing::info!(path = %path.display(), "xray-core ready");
            path
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                "xray-core not installed; xray will not start until a binary is placed at $DATA_DIR/xray/"
            );
            default_xray_path(config)
        }
    }
}

fn default_xray_path(config: &AppConfig) -> std::path::PathBuf {
    config
        .data_dir
        .join("xray")
        .join(if cfg!(windows) { "xray.exe" } else { "xray" })
}

/// Start the panel and block until Ctrl-C, then shut the supervisor down.
pub async fn run(config: AppConfig) -> AppResult<()> {
    let services = start_services(config.clone()).await?;

    tracing::info!(
        addr = %config.bind_addr,
        data_dir = %config.data_dir.display(),
        "3r-ui ready (HTTP server + xray supervisor + stats poll)"
    );

    match tokio::signal::ctrl_c().await {
        Ok(()) => tracing::info!("shutdown signal received; stopping xray"),
        Err(e) => tracing::warn!(error = %e, "signal handler failed; stopping on channel close"),
    }
    services.shutdown().await;
    Ok(())
}

async fn ensure_initial_admin(pool: &SqlitePool, data_dir: &Path) -> AppResult<()> {
    use rand::{distributions::Alphanumeric, Rng};
    use std::os::unix::fs::PermissionsExt;

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
        let hash = crypto::password::hash(&password)?;
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
        eprintln!(
            "3r-ui initial admin password written to: {}",
            pw_path.display()
        );
        eprintln!("Username: admin");
        eprintln!("Password: {password}");
        eprintln!("Change it after your first login at /admin.");
        eprintln!("======================================================================");
    }
    Ok(())
}
