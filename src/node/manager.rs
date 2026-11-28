use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, RwLock};
use tracing::{info, warn};

use crate::db::node::Node;
use crate::error::{AppError, AppResult};
use crate::node::protocol::{NodeTelemetry, ServerMessage};

#[derive(Debug, Clone)]
pub struct NodeSession {
    pub node_id: i64,
    pub tx: mpsc::Sender<ServerMessage>,
    pub connected_at: Instant,
    pub last_heartbeat: Instant,
    pub telemetry: Option<NodeTelemetry>,
}

#[derive(Clone)]
pub struct NodeManager {
    pool: sqlx::SqlitePool,
    sessions: Arc<RwLock<HashMap<i64, NodeSession>>>,
    shutdown: Arc<AtomicBool>,
}

impl NodeManager {
    pub fn new(pool: sqlx::SqlitePool) -> Self {
        Self {
            pool,
            sessions: Arc::new(RwLock::new(HashMap::new())),
            shutdown: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Start background watchdog loop checking heartbeats and updating circuit breaker state.
    pub fn start_watchdog(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let manager = Arc::clone(self);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(5));
            while !manager.shutdown.load(Ordering::Relaxed) {
                interval.tick().await;
                manager.check_heartbeats().await;
            }
        })
    }

    pub fn shutdown(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
    }

    /// Check heartbeats of all active sessions. If > 30s since last heartbeat, mark offline.
    pub async fn check_heartbeats(&self) {
        let now = Instant::now();
        let mut to_drop = Vec::new();

        {
            let sessions = self.sessions.read().await;
            for (node_id, session) in sessions.iter() {
                // Node 1 (Local Master Node) is always online unless explicitly disabled
                if *node_id == 1 {
                    continue;
                }
                if now.duration_since(session.last_heartbeat) > Duration::from_secs(30) {
                    warn!(node_id = %node_id, "node heartbeat timed out (>30s), evicting to offline");
                    to_drop.push(*node_id);
                }
            }
        }

        if !to_drop.is_empty() {
            let mut sessions = self.sessions.write().await;
            for id in to_drop {
                sessions.remove(&id);
                let _ = crate::db::node::update_node_status(&self.pool, id, "offline").await;
            }
        }
    }

    pub async fn register_session(&self, node_id: i64, tx: mpsc::Sender<ServerMessage>) {
        let mut sessions = self.sessions.write().await;
        sessions.insert(
            node_id,
            NodeSession {
                node_id,
                tx,
                connected_at: Instant::now(),
                last_heartbeat: Instant::now(),
                telemetry: None,
            },
        );
        let _ = crate::db::node::update_node_status(&self.pool, node_id, "online").await;
        info!(node_id = %node_id, "node session registered and online");
    }

    pub async fn unregister_session(&self, node_id: i64) {
        let mut sessions = self.sessions.write().await;
        sessions.remove(&node_id);
        let _ = crate::db::node::update_node_status(&self.pool, node_id, "offline").await;
        info!(node_id = %node_id, "node session unregistered and offline");
    }

    pub async fn update_heartbeat(&self, node_id: i64, telemetry: NodeTelemetry) {
        {
            let mut sessions = self.sessions.write().await;
            if let Some(session) = sessions.get_mut(&node_id) {
                session.last_heartbeat = Instant::now();
                session.telemetry = Some(telemetry.clone());
            }
        }

        // Async write telemetry to SQLite
        let services_json = serde_json::to_string(&telemetry.services).ok();
        let _ = crate::db::node::update_node_telemetry(
            &self.pool,
            node_id,
            crate::db::node::NodeTelemetryUpdate {
                cpu: Some(telemetry.cpu_percent),
                mem_used: Some(telemetry.memory_used_bytes),
                mem_total: Some(telemetry.memory_total_bytes),
                disk_used: Some(telemetry.disk_used_bytes),
                disk_total: Some(telemetry.disk_total_bytes),
                services_status: services_json.as_deref(),
            },
        )
        .await;
    }

    pub async fn is_node_online(&self, node_id: i64) -> bool {
        // Local Node 1 is always online by default
        if node_id == 1 {
            return true;
        }
        let sessions = self.sessions.read().await;
        sessions.contains_key(&node_id)
    }

    /// Return all node IDs that are currently healthy and online.
    pub async fn get_healthy_node_ids(&self) -> Vec<i64> {
        let mut healthy = Vec::new();
        // Node 1 (Local Node) is always included
        healthy.push(1);

        let sessions = self.sessions.read().await;
        for &id in sessions.keys() {
            if id != 1 {
                healthy.push(id);
            }
        }
        healthy
    }

    pub async fn send_to_node(&self, node_id: i64, msg: ServerMessage) -> AppResult<()> {
        let sessions = self.sessions.read().await;
        if let Some(session) = sessions.get(&node_id) {
            session
                .tx
                .send(msg)
                .await
                .map_err(|e| AppError::Node(format!("failed to send to node {node_id}: {e}")))?;
            Ok(())
        } else {
            Err(AppError::Node(format!("node {node_id} is offline")))
        }
    }

    pub async fn get_all_nodes(&self) -> AppResult<Vec<Node>> {
        crate::db::node::list_nodes(&self.pool).await
    }
}
