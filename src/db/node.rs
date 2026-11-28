use rand::Rng;
use sqlx::{FromRow, SqlitePool};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::error::{AppError, AppResult};

#[derive(Debug, Clone, FromRow, serde::Serialize, serde::Deserialize)]
pub struct NodeGroup {
    pub id: i64,
    pub name: String,
    pub tag: String,
    pub description: Option<String>,
    pub created_at: i64,
}

#[derive(Debug, Clone, FromRow, serde::Serialize, serde::Deserialize)]
pub struct Node {
    pub id: i64,
    pub name: String,
    pub secret_key: String,
    pub is_local: i64,
    pub status: String,
    pub server_address: Option<String>,
    pub group_id: Option<i64>,
    pub last_heartbeat_at: Option<i64>,
    pub cpu_percent: Option<f64>,
    pub memory_used_bytes: Option<i64>,
    pub memory_total_bytes: Option<i64>,
    pub disk_used_bytes: Option<i64>,
    pub disk_total_bytes: Option<i64>,
    pub services_status: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, FromRow, serde::Serialize, serde::Deserialize)]
pub struct NodeProvisioningToken {
    pub id: i64,
    pub token: String,
    pub group_id: Option<i64>,
    pub node_name: Option<String>,
    pub expires_at: i64,
    pub used_at: Option<i64>,
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

pub async fn list_groups(pool: &SqlitePool) -> AppResult<Vec<NodeGroup>> {
    let groups = sqlx::query_as::<_, NodeGroup>(
        "SELECT id, name, tag, description, created_at FROM node_groups ORDER BY id ASC",
    )
    .fetch_all(pool)
    .await?;
    Ok(groups)
}

pub async fn create_group(
    pool: &SqlitePool,
    name: &str,
    tag: &str,
    description: Option<&str>,
) -> AppResult<NodeGroup> {
    let now = now_secs();
    let id = sqlx::query(
        "INSERT INTO node_groups (name, tag, description, created_at) VALUES (?, ?, ?, ?)",
    )
    .bind(name)
    .bind(tag)
    .bind(description)
    .bind(now)
    .execute(pool)
    .await?
    .last_insert_rowid();

    Ok(NodeGroup {
        id,
        name: name.to_string(),
        tag: tag.to_string(),
        description: description.map(|s| s.to_string()),
        created_at: now,
    })
}

pub async fn delete_group(pool: &SqlitePool, id: i64) -> AppResult<()> {
    sqlx::query("DELETE FROM node_groups WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn list_nodes(pool: &SqlitePool) -> AppResult<Vec<Node>> {
    let nodes = sqlx::query_as::<_, Node>(
        r#"SELECT id, name, secret_key, is_local, status, server_address, group_id,
                 last_heartbeat_at, cpu_percent, memory_used_bytes, memory_total_bytes,
                 disk_used_bytes, disk_total_bytes, services_status, created_at, updated_at
          FROM nodes ORDER BY is_local DESC, id ASC"#,
    )
    .fetch_all(pool)
    .await?;
    Ok(nodes)
}

pub async fn get_node_by_id(pool: &SqlitePool, id: i64) -> AppResult<Option<Node>> {
    let node = sqlx::query_as::<_, Node>(
        r#"SELECT id, name, secret_key, is_local, status, server_address, group_id,
                 last_heartbeat_at, cpu_percent, memory_used_bytes, memory_total_bytes,
                 disk_used_bytes, disk_total_bytes, services_status, created_at, updated_at
          FROM nodes WHERE id = ?"#,
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;
    Ok(node)
}

pub async fn get_node_by_secret(pool: &SqlitePool, secret_key: &str) -> AppResult<Option<Node>> {
    let node = sqlx::query_as::<_, Node>(
        r#"SELECT id, name, secret_key, is_local, status, server_address, group_id,
                 last_heartbeat_at, cpu_percent, memory_used_bytes, memory_total_bytes,
                 disk_used_bytes, disk_total_bytes, services_status, created_at, updated_at
          FROM nodes WHERE secret_key = ?"#,
    )
    .bind(secret_key)
    .fetch_optional(pool)
    .await?;
    Ok(node)
}

pub async fn create_node(
    pool: &SqlitePool,
    name: &str,
    secret_key: &str,
    server_address: Option<&str>,
    group_id: Option<i64>,
) -> AppResult<Node> {
    let now = now_secs();
    let id = sqlx::query(
        r#"INSERT INTO nodes (name, secret_key, is_local, status, server_address, group_id, created_at, updated_at)
           VALUES (?, ?, 0, 'offline', ?, ?, ?, ?)"#,
    )
    .bind(name)
    .bind(secret_key)
    .bind(server_address)
    .bind(group_id)
    .bind(now)
    .bind(now)
    .execute(pool)
    .await?
    .last_insert_rowid();

    Ok(Node {
        id,
        name: name.to_string(),
        secret_key: secret_key.to_string(),
        is_local: 0,
        status: "offline".to_string(),
        server_address: server_address.map(|s| s.to_string()),
        group_id,
        last_heartbeat_at: None,
        cpu_percent: None,
        memory_used_bytes: None,
        memory_total_bytes: None,
        disk_used_bytes: None,
        disk_total_bytes: None,
        services_status: None,
        created_at: now,
        updated_at: now,
    })
}

pub async fn update_node_status(pool: &SqlitePool, id: i64, status: &str) -> AppResult<()> {
    let now = now_secs();
    sqlx::query("UPDATE nodes SET status = ?, updated_at = ? WHERE id = ?")
        .bind(status)
        .bind(now)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

#[derive(Debug, Clone, Default)]
pub struct NodeTelemetryUpdate<'a> {
    pub cpu: Option<f64>,
    pub mem_used: Option<i64>,
    pub mem_total: Option<i64>,
    pub disk_used: Option<i64>,
    pub disk_total: Option<i64>,
    pub services_status: Option<&'a str>,
}

pub async fn update_node_telemetry(
    pool: &SqlitePool,
    id: i64,
    telemetry: NodeTelemetryUpdate<'_>,
) -> AppResult<()> {
    let now = now_secs();
    sqlx::query(
        r#"UPDATE nodes SET
             status = 'online',
             last_heartbeat_at = ?,
             cpu_percent = ?,
             memory_used_bytes = ?,
             memory_total_bytes = ?,
             disk_used_bytes = ?,
             disk_total_bytes = ?,
             services_status = ?,
             updated_at = ?
           WHERE id = ?"#,
    )
    .bind(now)
    .bind(telemetry.cpu)
    .bind(telemetry.mem_used)
    .bind(telemetry.mem_total)
    .bind(telemetry.disk_used)
    .bind(telemetry.disk_total)
    .bind(telemetry.services_status)
    .bind(now)
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn delete_node(pool: &SqlitePool, id: i64) -> AppResult<()> {
    // Cannot delete Master Local Node (id 1 or is_local = 1)
    let node = get_node_by_id(pool, id).await?;
    if let Some(n) = node {
        if n.is_local == 1 {
            return Err(AppError::Node(
                "Cannot delete Master local node".to_string(),
            ));
        }
    }
    sqlx::query("DELETE FROM nodes WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn create_provisioning_token(
    pool: &SqlitePool,
    group_id: Option<i64>,
    node_name: Option<&str>,
    duration_secs: i64,
) -> AppResult<String> {
    let random_bytes: [u8; 16] = rand::thread_rng().gen();
    let hex_str: String = random_bytes.iter().map(|b| format!("{b:02x}")).collect();
    let token = format!("provision_{hex_str}");
    let now = now_secs();
    let expires_at = now + duration_secs;

    sqlx::query(
        "INSERT INTO node_provisioning_tokens (token, group_id, node_name, expires_at) VALUES (?, ?, ?, ?)",
    )
    .bind(&token)
    .bind(group_id)
    .bind(node_name)
    .bind(expires_at)
    .execute(pool)
    .await?;

    Ok(token)
}

pub async fn validate_and_consume_token(
    pool: &SqlitePool,
    token: &str,
) -> AppResult<Option<NodeProvisioningToken>> {
    let now = now_secs();
    let record = sqlx::query_as::<_, NodeProvisioningToken>(
        "SELECT id, token, group_id, node_name, expires_at, used_at FROM node_provisioning_tokens WHERE token = ? AND used_at IS NULL AND expires_at > ?",
    )
    .bind(token)
    .bind(now)
    .fetch_optional(pool)
    .await?;

    if let Some(ref rec) = record {
        sqlx::query("UPDATE node_provisioning_tokens SET used_at = ? WHERE id = ?")
            .bind(now)
            .bind(rec.id)
            .execute(pool)
            .await?;
    }

    Ok(record)
}

pub async fn set_client_node_groups(
    pool: &SqlitePool,
    client_id: i64,
    group_ids: &[i64],
) -> AppResult<()> {
    let mut tx = pool.begin().await?;
    sqlx::query("DELETE FROM client_node_groups WHERE client_id = ?")
        .bind(client_id)
        .execute(&mut *tx)
        .await?;

    for &gid in group_ids {
        sqlx::query("INSERT INTO client_node_groups (client_id, group_id) VALUES (?, ?)")
            .bind(client_id)
            .bind(gid)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(())
}

pub async fn get_client_node_groups(pool: &SqlitePool, client_id: i64) -> AppResult<Vec<i64>> {
    let gids = sqlx::query_scalar::<_, i64>(
        "SELECT group_id FROM client_node_groups WHERE client_id = ? ORDER BY group_id ASC",
    )
    .bind(client_id)
    .fetch_all(pool)
    .await?;
    Ok(gids)
}
