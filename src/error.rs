#[derive(thiserror::Error, Debug)]
pub enum AppError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),

    #[error("migration error: {0}")]
    Migration(#[from] sqlx::migrate::MigrateError),

    #[error("config error: {0}")]
    Config(String),

    #[error("crypto error: {0}")]
    Crypto(String),

    #[error("could not acquire lock at {0}: {1}")]
    Lock(String, String),

    #[error("xray error: {0}")]
    Xray(String),

    #[error("proxy error: {0}")]
    Proxy(String),

    #[error("mieru error: {0}")]
    Mieru(String),

    #[error("http error: {0}")]
    Http(String),
}

pub type AppResult<T> = Result<T, AppError>;
