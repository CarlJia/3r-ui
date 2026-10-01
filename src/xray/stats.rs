//! xray StatsService client and the per-poll traffic accumulator.
//!
//! Every `STATS_INTERVAL_SECS` the panel:
//!
//! 1. Opens (or reuses) a gRPC channel to `${stats_addr}` — xray's StatsService.
//! 2. Calls `QueryStats { pattern: "user>>>", reset: false }` to read every
//!    per-user counter at once.
//! 3. Splits the response by `email` (which the inbound config sets equal to
//!    the VLESS UUID), then maps each email to its DB client row.
//! 4. Computes `delta = current - last_xray` and writes a new
//!    `traffic_snapshots` row whose `uplink/downlink` are the panel-tracked
//!    cumulative total. The row also stores the raw xray counters so the
//!    panel can rebuild its in-memory snapshot after a restart without
//!    inventing a phantom delta.
//! 5. Calls [`crate::quota::check_and_disable`] so any client that crossed
//!    its quota is set to `disabled` and a config reload is requested.
//!
//! xray-core restarts reset its counters to 0, which surfaces as a negative
//! delta; we clamp those to 0 so a counter reset never decreases the panel's
//! cumulative total.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use sqlx::SqlitePool;
use tokio::sync::mpsc;
use tonic::transport::Channel;
use tonic::Request;

use crate::db::models::Client;
use crate::error::{AppError, AppResult};
use crate::quota;

pub mod proto {
    tonic::include_proto!("xray.app.stats.command");
}

use proto::stats_service_client::StatsServiceClient;

/// xray's gRPC StatsService address. Mirrors [`crate::xray::config::STATS_ADDR`].
pub const STATS_ADDR: &str = "127.0.0.1:10085";

/// xray names its per-user counters `user>>><email>>>traffic>>><direction>`.
const USER_PREFIX: &str = "user>>>";
const TRAFFIC_INFIX: &str = ">>>traffic>>>";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Uplink,
    Downlink,
}

/// Parse an xray stat name into `(email, direction)` if it matches the
/// `user>>><email>>>traffic>>><uplink|downlink>` shape; returns `None`
/// otherwise. Pure function — exported for unit testing.
pub fn parse_stat_name(name: &str) -> Option<(&str, Direction)> {
    let rest = name.strip_prefix(USER_PREFIX)?;
    let (email, rest) = rest.split_once(TRAFFIC_INFIX)?;
    if email.is_empty() {
        return None;
    }
    let dir = match rest {
        "uplink" => Direction::Uplink,
        "downlink" => Direction::Downlink,
        _ => return None,
    };
    Some((email, dir))
}

/// Last seen raw xray counter values per client id. Empty at startup.
#[derive(Debug, Default)]
pub struct Snapshot {
    last_xray: HashMap<i64, (i64, i64)>,
}

impl Snapshot {
    pub fn new() -> Self {
        Self::default()
    }

    /// Seed the in-memory last-xray values from the most recent snapshot row
    /// per client. Called once at panel startup.
    pub async fn seed_from_db(&mut self, pool: &SqlitePool) -> AppResult<()> {
        let rows: Vec<(i64, i64, i64, i64, i64)> = sqlx::query_as(
            "SELECT client_id, uplink, downlink, xray_uplink, xray_downlink \
             FROM traffic_snapshots \
             WHERE recorded_at IN (SELECT MAX(recorded_at) FROM traffic_snapshots GROUP BY client_id)",
        )
        .fetch_all(pool)
        .await
        .map_err(AppError::Database)?;
        for (client_id, _uplink, _downlink, xray_uplink, xray_downlink) in rows {
            self.last_xray
                .insert(client_id, (xray_uplink, xray_downlink));
        }
        Ok(())
    }

    fn last(&self, client_id: i64) -> Option<(i64, i64)> {
        self.last_xray.get(&client_id).copied()
    }

    fn remember(&mut self, client_id: i64, xray_u: i64, xray_d: i64) {
        self.last_xray.insert(client_id, (xray_u, xray_d));
    }
}

/// Fold a `QueryStats` response into a `client_id → (xray_uplink, xray_downlink)`
/// map. Clients whose email the inbound config did not set (no `email` field)
/// are absent — xray does not count traffic for them either.
fn fold_query_response(
    response: &[proto::Stat],
    email_to_client: &HashMap<String, i64>,
) -> HashMap<i64, (i64, i64)> {
    let mut out: HashMap<i64, (i64, i64)> = HashMap::new();
    for stat in response {
        let Some((email, dir)) = parse_stat_name(&stat.name) else {
            continue;
        };
        let Some(&client_id) = email_to_client.get(email) else {
            continue;
        };
        let entry = out.entry(client_id).or_insert((0, 0));
        match dir {
            Direction::Uplink => entry.0 = stat.value,
            Direction::Downlink => entry.1 = stat.value,
        }
    }
    out
}

/// Build the email→client_id lookup from the active clients. Only active
/// (non-disabled, non-deleted) clients are tracked so quota-disabled clients
/// stop consuming a slot in the snapshot map.
async fn active_clients_by_uuid(pool: &SqlitePool) -> AppResult<HashMap<String, i64>> {
    let rows: Vec<(i64, String)> =
        sqlx::query_as("SELECT id, uuid FROM clients WHERE status = 'active'")
            .fetch_all(pool)
            .await
            .map_err(AppError::Database)?;
    Ok(rows.into_iter().map(|(id, uuid)| (uuid, id)).collect())
}

/// One round of polling: query xray, write a snapshot row per active client,
/// then ask quota enforcement to disable anyone who exceeded their cap.
pub async fn poll_once(
    pool: SqlitePool,
    snapshot: &mut Snapshot,
    client: &mut StatsServiceClient<Channel>,
) -> AppResult<u64> {
    let email_to_client = active_clients_by_uuid(&pool).await?;

    let req = Request::new(proto::QueryStatsRequest {
        pattern: "user>>>".to_string(),
        reset: false,
    });
    let response = client
        .query_stats(req)
        .await
        .map_err(|e| AppError::Xray(format!("stats QueryStats failed: {e}")))?
        .into_inner();
    let by_client = fold_query_response(&response.stat, &email_to_client);

    let now = crate::crypto::session::now_unix();
    let mut rows_written = 0u64;
    for (client_id, (xray_u, xray_d)) in by_client {
        let (prev_u, prev_d) = match snapshot.last(client_id) {
            Some(v) => v,
            None => {
                // First poll since (re)start — establish baseline, write nothing.
                snapshot.remember(client_id, xray_u, xray_d);
                continue;
            }
        };

        // xray counter went backwards (xray restart, stats reset, etc.).
        // Clamp to 0 rather than letting the cumulative total regress.
        let delta_u = (xray_u - prev_u).max(0);
        let delta_d = (xray_d - prev_d).max(0);

        let baseline: (i64, i64) = sqlx::query_as(
            "SELECT uplink, downlink FROM traffic_snapshots \
             WHERE client_id = ? ORDER BY recorded_at DESC LIMIT 1",
        )
        .bind(client_id)
        .fetch_optional(&pool)
        .await
        .map_err(AppError::Database)?
        .unwrap_or((0, 0));

        let new_u = baseline.0.saturating_add(delta_u);
        let new_d = baseline.1.saturating_add(delta_d);

        sqlx::query(
            "INSERT INTO traffic_snapshots \
                (client_id, uplink, downlink, xray_uplink, xray_downlink, recorded_at) \
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(client_id)
        .bind(new_u)
        .bind(new_d)
        .bind(xray_u)
        .bind(xray_d)
        .bind(now)
        .execute(&pool)
        .await
        .map_err(AppError::Database)?;
        snapshot.remember(client_id, xray_u, xray_d);
        rows_written += 1;
    }

    Ok(rows_written)
}

/// Open a gRPC channel to xray. We do not connect eagerly — the URL may be
/// unreachable at boot (xray still starting, supervisor backing off, etc.) and
/// we want the poll loop to keep trying rather than failing fast.
pub async fn connect() -> AppResult<StatsServiceClient<Channel>> {
    let endpoint = tonic::transport::Endpoint::from_shared(format!("http://{STATS_ADDR}"))
        .map_err(|e| AppError::Xray(format!("bad stats endpoint {STATS_ADDR}: {e}")))?;
    Ok(StatsServiceClient::new(endpoint.connect_lazy()))
}

/// Sum uplink + downlink of the most-recent snapshot row for one client.
/// Returns 0 if no snapshot has been recorded yet.
pub async fn latest_total(pool: &SqlitePool, client_id: i64) -> AppResult<i64> {
    let row: Option<(i64, i64)> = sqlx::query_as(
        "SELECT uplink, downlink FROM traffic_snapshots \
         WHERE client_id = ? ORDER BY recorded_at DESC LIMIT 1",
    )
    .bind(client_id)
    .fetch_optional(pool)
    .await
    .map_err(AppError::Database)?;
    Ok(row.map(|(u, d)| u.saturating_add(d)).unwrap_or(0))
}

/// Long-running poll loop. Reconnects on transport failure and keeps
/// running as long as the panel is up; cancellation happens at the task
/// level when the panel shuts down.
pub async fn run_loop(
    pool: SqlitePool,
    data_dir: PathBuf,
    interval_secs: u64,
    supervisor_tx: mpsc::Sender<crate::xray::SupervisorCommand>,
) {
    let config_path = data_dir.join("xray").join("config.json");
    let interval = Duration::from_secs(interval_secs.max(1));
    let mut snapshot = Snapshot::new();
    if let Err(e) = snapshot.seed_from_db(&pool).await {
        tracing::warn!(error = %e, "stats snapshot seed from db failed");
    }

    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        ticker.tick().await;

        // Quota is enforced against the cumulative total in SQLite, which
        // survives an xray outage. So the check runs even when the poll
        // itself failed — otherwise a client that already crossed its cap
        // would stay active indefinitely while the stats API is down.
        if let Err(e) = quota::on_quota_or_disable(&pool, &config_path, &supervisor_tx).await {
            tracing::warn!(error = %e, "quota enforcement failed");
        }

        let mut client = match connect().await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, "stats gRPC connect failed; will retry");
                continue;
            }
        };

        if let Err(e) = poll_once(pool.clone(), &mut snapshot, &mut client).await {
            tracing::warn!(error = %e, "stats poll failed; will retry next tick");
        }
    }
}

/// Look up the active client rows for tests / quota debugging.
#[allow(dead_code)]
pub async fn _active_clients(pool: &SqlitePool) -> AppResult<Vec<Client>> {
    sqlx::query_as::<_, Client>(
        "SELECT id, inbound_id, uuid, name, quota_bytes, expiry_at, status, created_at \
         FROM clients WHERE status = 'active'",
    )
    .fetch_all(pool)
    .await
    .map_err(AppError::Database)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_user_uplink_and_downlink() {
        assert_eq!(
            parse_stat_name("user>>>abc-uuid>>>traffic>>>uplink"),
            Some(("abc-uuid", Direction::Uplink))
        );
        assert_eq!(
            parse_stat_name("user>>>abc-uuid>>>traffic>>>downlink"),
            Some(("abc-uuid", Direction::Downlink))
        );
    }

    #[test]
    fn rejects_non_user_names() {
        assert_eq!(parse_stat_name("inbound>>>443>>>traffic>>>uplink"), None);
        assert_eq!(parse_stat_name("user>>>x>>>session>>>"), None);
        assert_eq!(parse_stat_name("user>>>>>>traffic>>>uplink"), None);
    }

    #[test]
    fn fold_drops_unknown_emails_and_unrelated_stats() {
        let mut map = HashMap::new();
        map.insert("known-uuid".to_string(), 7i64);
        let resp = vec![
            proto::Stat {
                name: "user>>>known-uuid>>>traffic>>>uplink".into(),
                value: 100,
            },
            proto::Stat {
                name: "user>>>known-uuid>>>traffic>>>downlink".into(),
                value: 200,
            },
            proto::Stat {
                name: "user>>>ghost>>>traffic>>>uplink".into(),
                value: 999,
            },
            proto::Stat {
                name: "inbound>>>443>>>traffic>>>uplink".into(),
                value: 5,
            },
        ];
        let out = fold_query_response(&resp, &map);
        assert_eq!(out.get(&7), Some(&(100, 200)));
        assert_eq!(out.len(), 1);
    }
}
