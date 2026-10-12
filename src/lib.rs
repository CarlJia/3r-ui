pub mod config;
pub mod crypto;
pub mod db;
pub mod error;
pub mod quota;
pub mod share;
pub mod web;
pub mod xray;

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use sqlx::SqlitePool;
use tokio::sync::{mpsc, Mutex};

pub use config::AppConfig;
pub use error::{AppError, AppResult};

/// Handle to everything a running panel owns. Returned by [`start_services`]
/// so tests can drive the lifecycle without going through signal handling.
pub struct Services {
    pub pool: SqlitePool,
    pub supervisor_tx: mpsc::Sender<xray::SupervisorCommand>,
    shutdown_initiated: Arc<std::sync::atomic::AtomicBool>,
    supervisor_handle: tokio::task::JoinHandle<()>,
    http_handle: tokio::task::JoinHandle<()>,
    stats_handle: tokio::task::JoinHandle<()>,
    logs_handle: tokio::task::JoinHandle<()>,
    prune_handle: tokio::task::JoinHandle<()>,
}

impl Services {
    /// Stop the HTTP server, the xray supervisor, the stats poll loop, the
    /// access-log receiver, and the retention pruner, then wait.
    pub async fn shutdown(self) {
        // Signal background update tasks to abort without consuming supervisor.
        self.shutdown_initiated
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let _ = self.supervisor_tx.send(xray::SupervisorCommand::Stop).await;
        let _ =
            tokio::time::timeout(std::time::Duration::from_secs(10), self.supervisor_handle).await;
        self.stats_handle.abort();
        self.logs_handle.abort();
        self.prune_handle.abort();
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

    // Single shared Installer instance for both bootstrap and runtime updates,
    // so its reqwest::Client connection pool is not duplicated.
    let installer = std::sync::Arc::new(xray::Installer::new(
        config.xray_download_base.clone(),
    )?);
    let xray_bin = ensure_xray_binary(&config, installer.clone()).await;
    let config_path = config.data_dir.join("xray").join("config.json");

    // xray_log_rx 由访问日志接收 worker 消费（见下方 spawn）；通道满时
    // supervisor 丢行而不是反压 xray，见 XraySupervisor::new 的字段契约。
    let (xray_log_tx, xray_log_rx) = mpsc::channel::<String>(256);

    let supervisor = xray::XraySupervisor::new(
        xray_bin,
        config_path,
        config.data_dir.clone(),
        Some(xray_log_tx),
    );
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
        update_jobs: Arc::new(Mutex::new(HashMap::new())),
        installer: installer.clone(),
        shutdown_initiated: Arc::new(std::sync::atomic::AtomicBool::new(false)),
    };
    // Clone the Arc handle for Services::shutdown before app_state is moved into the async block.
    let shutdown_initiated = app_state.shutdown_initiated.clone();
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

    // 访问日志接收：supervisor 把 xray stderr 每行送进本 worker；worker 解析
    // 访问日志行并落库。通道关闭（面板 shutdown）时循环自然退出。
    let logs_handle = tokio::spawn(xray::logs::run_log_line_loop(pool.clone(), xray_log_rx));

    // 保留清理：每小时删除 30 天前的 connection_logs 行。
    let prune_handle = tokio::spawn(xray::logs::prune_loop(pool.clone()));

    Ok(Services {
        pool,
        supervisor_tx,
        shutdown_initiated,
        supervisor_handle,
        http_handle,
        stats_handle,
        logs_handle,
        prune_handle,
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

async fn ensure_xray_binary(
    config: &AppConfig,
    installer: std::sync::Arc<xray::Installer>,
) -> std::path::PathBuf {
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
