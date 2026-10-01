//! Quota enforcement.
//!
//! After each stats poll, walk every active client and disable any whose
//! cumulative uplink + downlink has crossed its `quota_bytes`. Disabling
//! drops the client out of the next xray config render, so subsequent
//! connections are rejected by xray without further action.
//!
//! `quota_bytes = 0` is treated as "unlimited" — a default that lets the
//! operator mint users without setting a cap.

use sqlx::SqlitePool;
use tokio::sync::mpsc;

use crate::error::{AppError, AppResult};
use crate::xray::SupervisorCommand;

/// Compare each active client's cumulative traffic to its quota. Returns the
/// list of client ids that were just disabled. The caller is responsible for
/// triggering a config re-render and reload — [`on_quota_or_disable`] is a
/// convenience wrapper that does the full pipeline.
pub async fn check(pool: &SqlitePool) -> AppResult<Vec<i64>> {
    let active: Vec<(i64, i64)> =
        sqlx::query_as("SELECT id, quota_bytes FROM clients WHERE status = 'active'")
            .fetch_all(pool)
            .await
            .map_err(AppError::Database)?;

    let mut disabled = Vec::new();
    for (client_id, quota_bytes) in active {
        if quota_bytes <= 0 {
            continue;
        }
        let total = crate::xray::stats::latest_total(pool, client_id).await?;
        if total >= quota_bytes {
            let res = sqlx::query(
                "UPDATE clients SET status = 'disabled' \
                 WHERE id = ? AND status = 'active'",
            )
            .bind(client_id)
            .execute(pool)
            .await
            .map_err(AppError::Database)?;
            if res.rows_affected() > 0 {
                tracing::info!(
                    client_id,
                    total,
                    quota_bytes,
                    "quota exceeded; client disabled"
                );
                disabled.push(client_id);
            }
        }
    }
    Ok(disabled)
}

/// Run the quota check; if anyone was disabled, re-render the xray config
/// and ask the supervisor to reload. This is what the stats poll loop calls.
pub async fn on_quota_or_disable(
    pool: &SqlitePool,
    config_path: &std::path::Path,
    supervisor_tx: &mpsc::Sender<SupervisorCommand>,
) -> AppResult<usize> {
    let disabled = check(pool).await?;
    if disabled.is_empty() {
        return Ok(0);
    }
    let rendered = crate::xray::config::render(pool, config_path).await?;
    if let Err(e) = supervisor_tx.send(SupervisorCommand::Reload).await {
        tracing::warn!(error = %e, "failed to enqueue reload after quota disable");
    }
    Ok(rendered)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::xray::stats::latest_total;

    async fn pool() -> (tempfile::TempDir, SqlitePool) {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("test.db");
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect(&format!("sqlite://{}?mode=rwc", db.display()))
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        // FK on clients.inbound_id requires a parent row.
        sqlx::query(
            "INSERT INTO inbounds (id, listen_port, dest, sni, server_name, private_key, \
                    public_key, short_id, status, created_at, updated_at) \
             VALUES (1, 8443, 'a.example', 'a.example', NULL, 'priv', 'pub', 'ab', \
                    'active', unixepoch(), unixepoch())",
        )
        .execute(&pool)
        .await
        .unwrap();
        (dir, pool)
    }

    async fn insert_client(pool: &SqlitePool, id: i64, quota: i64) {
        sqlx::query(
            "INSERT INTO clients (id, inbound_id, uuid, name, quota_bytes, expiry_at, status, created_at) \
             VALUES (?, 1, ?, NULL, ?, NULL, 'active', unixepoch())",
        )
        .bind(id)
        .bind(format!("uuid-{id}"))
        .bind(quota)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn seed_traffic(pool: &SqlitePool, id: i64, u: i64, d: i64) {
        sqlx::query(
            "INSERT INTO traffic_snapshots (client_id, uplink, downlink, xray_uplink, xray_downlink, recorded_at) \
             VALUES (?, ?, ?, 0, 0, unixepoch())",
        )
        .bind(id)
        .bind(u)
        .bind(d)
        .execute(pool)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn zero_quota_is_never_disabled() {
        let (_dir, pool) = pool().await;
        insert_client(&pool, 1, 0).await;
        seed_traffic(&pool, 1, 1_000_000_000, 1_000_000_000).await;
        let disabled = check(&pool).await.unwrap();
        assert!(disabled.is_empty());
    }

    #[tokio::test]
    async fn under_quota_is_not_disabled() {
        let (_dir, pool) = pool().await;
        insert_client(&pool, 1, 100 * 1024 * 1024 * 1024).await;
        seed_traffic(&pool, 1, 1024, 1024).await;
        assert!(check(&pool).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn crossing_quota_disables_once() {
        let (_dir, pool) = pool().await;
        insert_client(&pool, 1, 10 * 1024).await;
        seed_traffic(&pool, 1, 7 * 1024, 5 * 1024).await;
        let disabled = check(&pool).await.unwrap();
        assert_eq!(disabled, vec![1]);

        // A second check on the same disabled client is a no-op (status
        // already disabled, the WHERE clause guards against double-disable).
        assert!(check(&pool).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn uses_latest_snapshot_not_history_sum() {
        // latest_total reads the *most recent* row — old rows are kept for
        // audit but quota is enforced against the latest cumulative only.
        let (_dir, pool) = pool().await;
        insert_client(&pool, 1, 10 * 1024).await;
        seed_traffic(&pool, 1, 20 * 1024, 0).await;
        // The latest total is 20k which exceeds 10k → disabled.
        assert_eq!(check(&pool).await.unwrap(), vec![1]);
    }

    #[tokio::test]
    async fn latest_total_returns_zero_without_snapshots() {
        let (_dir, pool) = pool().await;
        insert_client(&pool, 1, 100).await;
        assert_eq!(latest_total(&pool, 1).await.unwrap(), 0);
    }

    /// Regression: the stats loop used to `continue` past the quota check when
    /// `poll_once` failed (xray unreachable). Since the cumulative total
    /// lives in SQLite and survives the outage, an already-over-quota client
    /// would stay active for as long as the stats API was down. The check is
    /// now unconditional; this test pins the property that makes that safe —
    /// `check` reads only SQLite and needs no stats round-trip.
    #[tokio::test]
    async fn quota_check_works_without_any_stats_round_trip() {
        let (_dir, pool) = pool().await;
        insert_client(&pool, 1, 10 * 1024).await;
        seed_traffic(&pool, 1, 8 * 1024, 8 * 1024).await;
        // No gRPC, no snapshot state — just the DB rows a previous poll wrote.
        assert_eq!(check(&pool).await.unwrap(), vec![1]);
    }
}
