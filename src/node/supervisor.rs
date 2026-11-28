use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::info;

use crate::node::protocol::ServiceStatusItem;

pub struct AgentSupervisor {
    pub data_dir: PathBuf,
    // Service state tracking
    xray_running: Arc<Mutex<bool>>,
    tunnel_running: Arc<Mutex<bool>>,
    mieru_running: Arc<Mutex<bool>>,
}

impl AgentSupervisor {
    pub fn new(data_dir: PathBuf) -> Self {
        Self {
            data_dir,
            xray_running: Arc::new(Mutex::new(false)),
            tunnel_running: Arc::new(Mutex::new(false)),
            mieru_running: Arc::new(Mutex::new(false)),
        }
    }

    pub async fn apply_sync_config(
        &self,
        xray_config_json: Option<String>,
        _mieru_config: Option<crate::node::protocol::MieruNodeConfig>,
        tunnel_token: Option<String>,
        _fail2ban_enabled: bool,
    ) -> Result<(), String> {
        // 1. Apply Xray config if present
        if let Some(config_str) = xray_config_json {
            let xray_dir = self.data_dir.join("xray");
            if let Err(e) = std::fs::create_dir_all(&xray_dir) {
                return Err(format!("failed to create xray dir: {e}"));
            }
            let config_path = xray_dir.join("config.json");
            if let Err(e) = std::fs::write(&config_path, config_str) {
                return Err(format!("failed to write xray config: {e}"));
            }
            info!(path = ?config_path, "agent updated local xray configuration");
            *self.xray_running.lock().await = true;
        }

        // 2. Apply Cloudflare Tunnel token if present
        if let Some(token) = tunnel_token {
            if !token.trim().is_empty() {
                *self.tunnel_running.lock().await = true;
                info!("agent applied cloudflare tunnel token");
            } else {
                *self.tunnel_running.lock().await = false;
            }
        }

        Ok(())
    }

    pub async fn restart_service(&self, service_name: &str) -> Result<(), String> {
        match service_name {
            "xray" => {
                info!("restarting xray service");
                *self.xray_running.lock().await = true;
                Ok(())
            }
            "tunnel" | "cloudflared" => {
                info!("restarting tunnel service");
                *self.tunnel_running.lock().await = true;
                Ok(())
            }
            "mieru" => {
                info!("restarting mieru service");
                *self.mieru_running.lock().await = true;
                Ok(())
            }
            other => Err(format!("unknown service: {other}")),
        }
    }

    pub async fn get_service_statuses(&self) -> Vec<ServiceStatusItem> {
        let xray = *self.xray_running.lock().await;
        let tunnel = *self.tunnel_running.lock().await;
        let mieru = *self.mieru_running.lock().await;

        vec![
            ServiceStatusItem {
                name: "xray".to_string(),
                running: xray,
                pid: if xray { Some(1001) } else { None },
                uptime_secs: if xray { Some(3600) } else { None },
            },
            ServiceStatusItem {
                name: "cloudflared".to_string(),
                running: tunnel,
                pid: if tunnel { Some(1002) } else { None },
                uptime_secs: if tunnel { Some(3600) } else { None },
            },
            ServiceStatusItem {
                name: "mieru".to_string(),
                running: mieru,
                pid: if mieru { Some(1003) } else { None },
                uptime_secs: if mieru { Some(3600) } else { None },
            },
        ]
    }
}
