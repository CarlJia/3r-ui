use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, error, info, warn};

use crate::node::protocol::{compute_signature, ClientMessage, ServerMessage};
use crate::node::supervisor::AgentSupervisor;
use crate::node::telemetry::collect_system_telemetry;

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentConfig {
    pub server_url: String,
    pub node_id: Option<i64>,
    pub secret_key: Option<String>,
    pub token: Option<String>,
}

pub struct AgentDaemon {
    pub server_url: String,
    pub token: Option<String>,
    pub data_dir: PathBuf,
    pub supervisor: Arc<AgentSupervisor>,
    node_id: Arc<AtomicI64>,
    clock_skew: Arc<AtomicI64>,
}

impl AgentDaemon {
    pub fn new(server_url: String, token: Option<String>, data_dir: PathBuf) -> Self {
        let supervisor = Arc::new(AgentSupervisor::new(data_dir.clone()));
        Self {
            server_url,
            token,
            data_dir,
            supervisor,
            node_id: Arc::new(AtomicI64::new(0)),
            clock_skew: Arc::new(AtomicI64::new(0)),
        }
    }

    fn config_path(&self) -> PathBuf {
        self.data_dir.join("agent.json")
    }

    fn load_credentials(&self) -> Option<(i64, String)> {
        let path = self.config_path();
        if path.exists() {
            if let Ok(bytes) = std::fs::read(&path) {
                if let Ok(cfg) = serde_json::from_slice::<AgentConfig>(&bytes) {
                    if let (Some(id), Some(secret)) = (cfg.node_id, cfg.secret_key) {
                        return Some((id, secret));
                    }
                }
            }
        }
        None
    }

    fn save_credentials(&self, id: i64, secret: &str) {
        let path = self.config_path();
        let cfg = AgentConfig {
            server_url: self.server_url.clone(),
            node_id: Some(id),
            secret_key: Some(secret.to_string()),
            token: None,
        };
        if let Ok(json) = serde_json::to_string_pretty(&cfg) {
            let _ = std::fs::create_dir_all(&self.data_dir);
            let _ = std::fs::write(&path, json);
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
            }
            info!(path = ?path, "saved node credentials with restrictive permissions");
        }
    }

    pub async fn run(self: Arc<Self>) {
        info!(server = %self.server_url, "starting 3r-ui agent daemon loop");
        let mut backoff = Duration::from_secs(1);
        let max_backoff = Duration::from_secs(30);

        loop {
            match self.connect_and_serve().await {
                Ok(()) => {
                    info!("agent connection closed normally, reconnecting...");
                    backoff = Duration::from_secs(1);
                }
                Err(e) => {
                    warn!(error = %e, backoff_secs = %backoff.as_secs(), "agent connection error, retrying...");
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(max_backoff);
                }
            }
        }
    }

    async fn connect_and_serve(&self) -> Result<(), String> {
        let ws_url = if self.server_url.starts_with("https://") {
            self.server_url.replacen("https://", "wss://", 1)
        } else if self.server_url.starts_with("http://") {
            self.server_url.replacen("http://", "ws://", 1)
        } else {
            self.server_url.clone()
        };

        let target_url = if ws_url.ends_with("/api/node/ws") {
            ws_url
        } else {
            format!("{}/api/node/ws", ws_url.trim_end_matches('/'))
        };

        info!(url = %target_url, "connecting to master websocket endpoint");
        let (ws_stream, _) = connect_async(&target_url)
            .await
            .map_err(|e| format!("failed to connect to {target_url}: {e}"))?;

        let (mut write, mut read) = ws_stream.split();

        // 1. Initial Handshake or Authentication
        let credentials = self.load_credentials();
        match credentials {
            Some((node_id, ref secret_key)) => {
                let local_now = now_secs();
                let effective_time = local_now + self.clock_skew.load(Ordering::Relaxed);
                let sig = compute_signature(secret_key, node_id, effective_time);
                let auth_msg = ClientMessage::Authenticate {
                    node_id,
                    timestamp: effective_time,
                    signature: sig,
                };
                let json = serde_json::to_string(&auth_msg).unwrap();
                write
                    .send(Message::Text(json))
                    .await
                    .map_err(|e| format!("failed to send auth: {e}"))?;
                self.node_id.store(node_id, Ordering::Relaxed);
            }
            None => {
                let token = self
                    .token
                    .clone()
                    .ok_or_else(|| "no token provided for initial node provisioning".to_string())?;
                let handshake = ClientMessage::Handshake { token };
                let json = serde_json::to_string(&handshake).unwrap();
                write
                    .send(Message::Text(json))
                    .await
                    .map_err(|e| format!("failed to send handshake: {e}"))?;
            }
        }

        // 2. Channel for outbound messages (heartbeat, acks)
        let (tx, mut rx) = tokio::sync::mpsc::channel::<ClientMessage>(16);

        // Task: forward ClientMessage to write stream
        let forward_task = tokio::spawn(async move {
            while let Some(msg) = rx.recv().await {
                if let Ok(json) = serde_json::to_string(&msg) {
                    if let Err(e) = write.send(Message::Text(json)).await {
                        debug!(error = %e, "outbound write failed");
                        break;
                    }
                }
            }
        });

        // Task: periodic heartbeats
        let tx_heartbeat = tx.clone();
        let supervisor_clone = Arc::clone(&self.supervisor);
        let heartbeat_task = tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(10));
            loop {
                interval.tick().await;
                let services = supervisor_clone.get_service_statuses().await;
                let telemetry = collect_system_telemetry(services);
                if tx_heartbeat
                    .send(ClientMessage::Heartbeat { telemetry })
                    .await
                    .is_err()
                {
                    break;
                }
            }
        });

        // 3. Process incoming server messages
        while let Some(msg_result) = read.next().await {
            let msg = match msg_result {
                Ok(Message::Text(t)) => t,
                Ok(Message::Binary(b)) => match String::from_utf8(b) {
                    Ok(s) => s,
                    Err(_) => continue,
                },
                Ok(Message::Ping(_)) | Ok(Message::Pong(_)) => continue,
                Ok(Message::Frame(_)) => continue,
                Ok(Message::Close(_)) | Err(_) => break,
            };

            let server_msg: ServerMessage = match serde_json::from_str(&msg) {
                Ok(m) => m,
                Err(e) => {
                    warn!(error = %e, raw = %msg, "unknown server message frame");
                    continue;
                }
            };

            match server_msg {
                ServerMessage::HandshakeAck {
                    node_id,
                    secret_key,
                    server_time,
                } => {
                    info!(node_id = %node_id, "received handshake ack from master");
                    self.save_credentials(node_id, &secret_key);
                    self.node_id.store(node_id, Ordering::Relaxed);
                    let skew = server_time - now_secs();
                    self.clock_skew.store(skew, Ordering::Relaxed);
                }
                ServerMessage::AuthAck {
                    success,
                    server_time,
                    error,
                } => {
                    if success {
                        info!("authenticated with master successfully");
                        let skew = server_time - now_secs();
                        self.clock_skew.store(skew, Ordering::Relaxed);
                    } else {
                        error!(error = ?error, "master rejected authentication");
                        return Err(format!("authentication rejected: {error:?}"));
                    }
                }
                ServerMessage::HeartbeatAck { server_time } => {
                    let skew = server_time - now_secs();
                    self.clock_skew.store(skew, Ordering::Relaxed);
                }
                ServerMessage::SyncServicesConfig {
                    xray_config_json,
                    mieru_config,
                    tunnel_token,
                    fail2ban_enabled,
                } => {
                    info!("received services sync payload from master");
                    match self
                        .supervisor
                        .apply_sync_config(
                            xray_config_json,
                            mieru_config,
                            tunnel_token,
                            fail2ban_enabled,
                        )
                        .await
                    {
                        Ok(()) => {
                            let _ = tx
                                .send(ClientMessage::ConfigAck {
                                    success: true,
                                    error: None,
                                })
                                .await;
                        }
                        Err(e) => {
                            error!(error = %e, "failed to apply sync config");
                            let _ = tx
                                .send(ClientMessage::ConfigAck {
                                    success: false,
                                    error: Some(e),
                                })
                                .await;
                        }
                    }
                }
                ServerMessage::RestartService { service_name } => {
                    info!(service = %service_name, "received restart service command");
                    let _ = self.supervisor.restart_service(&service_name).await;
                }
                ServerMessage::Error { message } => {
                    warn!(error = %message, "server reported error");
                }
            }
        }

        forward_task.abort();
        heartbeat_task.abort();
        Ok(())
    }
}
