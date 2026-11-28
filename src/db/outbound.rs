use sqlx::{FromRow, SqlitePool};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::error::{AppError, AppResult};

#[derive(Debug, Clone, FromRow, serde::Serialize, serde::Deserialize)]
pub struct Outbound {
    pub id: i64,
    pub tag: String,
    pub name: String,
    pub protocol: String,
    pub server_addr: String,
    pub server_port: i64,
    pub settings_json: String,
    pub status: String,
    pub created_at: i64,
    pub updated_at: i64,
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

pub async fn list_outbounds(pool: &SqlitePool) -> AppResult<Vec<Outbound>> {
    sqlx::query_as::<_, Outbound>(
        "SELECT id, tag, name, protocol, server_addr, server_port, settings_json, status, created_at, updated_at \
         FROM outbounds ORDER BY id ASC",
    )
    .fetch_all(pool)
    .await
    .map_err(AppError::Database)
}

pub async fn list_active_outbounds(pool: &SqlitePool) -> AppResult<Vec<Outbound>> {
    sqlx::query_as::<_, Outbound>(
        "SELECT id, tag, name, protocol, server_addr, server_port, settings_json, status, created_at, updated_at \
         FROM outbounds WHERE status = 'active' ORDER BY id ASC",
    )
    .fetch_all(pool)
    .await
    .map_err(AppError::Database)
}

pub async fn get_outbound(pool: &SqlitePool, id: i64) -> AppResult<Option<Outbound>> {
    sqlx::query_as::<_, Outbound>(
        "SELECT id, tag, name, protocol, server_addr, server_port, settings_json, status, created_at, updated_at \
         FROM outbounds WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(pool)
    .await
    .map_err(AppError::Database)
}

pub async fn create_outbound(
    pool: &SqlitePool,
    tag: &str,
    name: &str,
    protocol: &str,
    server_addr: &str,
    server_port: i64,
    settings_json: &str,
) -> AppResult<i64> {
    let now = now_secs();
    let res = sqlx::query(
        "INSERT INTO outbounds (tag, name, protocol, server_addr, server_port, settings_json, status, created_at, updated_at) \
         VALUES (?, ?, ?, ?, ?, ?, 'active', ?, ?)",
    )
    .bind(tag.trim())
    .bind(name.trim())
    .bind(protocol.trim())
    .bind(server_addr.trim())
    .bind(server_port)
    .bind(settings_json.trim())
    .bind(now)
    .bind(now)
    .execute(pool)
    .await
    .map_err(AppError::Database)?;

    Ok(res.last_insert_rowid())
}

#[allow(clippy::too_many_arguments)]
pub async fn update_outbound(
    pool: &SqlitePool,
    id: i64,
    tag: &str,
    name: &str,
    protocol: &str,
    server_addr: &str,
    server_port: i64,
    settings_json: &str,
    status: &str,
) -> AppResult<()> {
    let now = now_secs();
    sqlx::query(
        "UPDATE outbounds \
         SET tag = ?, name = ?, protocol = ?, server_addr = ?, server_port = ?, settings_json = ?, status = ?, updated_at = ? \
         WHERE id = ?",
    )
    .bind(tag.trim())
    .bind(name.trim())
    .bind(protocol.trim())
    .bind(server_addr.trim())
    .bind(server_port)
    .bind(settings_json.trim())
    .bind(status.trim())
    .bind(now)
    .bind(id)
    .execute(pool)
    .await
    .map_err(AppError::Database)?;

    Ok(())
}

pub async fn delete_outbound(pool: &SqlitePool, id: i64) -> AppResult<()> {
    sqlx::query("DELETE FROM outbounds WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await
        .map_err(AppError::Database)?;

    Ok(())
}
