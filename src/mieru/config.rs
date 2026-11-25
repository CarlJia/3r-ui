//! Mieru server configuration generator.

use std::path::Path;
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

use crate::db::models::{Client, Inbound};
use crate::error::{AppError, AppResult};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PortBinding {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub port: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub port_range: Option<String>,
    pub protocol: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MieruUser {
    pub name: String,
    pub password: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct MieruServerConfig {
    pub port_bindings: Vec<PortBinding>,
    pub users: Vec<MieruUser>,
    pub logging_level: String,
    pub mtu: u32,
}

impl MieruServerConfig {
    pub fn new() -> Self {
        Self {
            port_bindings: Vec::new(),
            users: Vec::new(),
            logging_level: "INFO".to_string(),
            mtu: 1400,
        }
    }
}

impl Default for MieruServerConfig {
    fn default() -> Self {
        Self::new()
    }
}

/// Query active Mieru inbounds and mapped active clients to generate the Mieru server config.
pub async fn generate_config(pool: &SqlitePool) -> AppResult<Option<MieruServerConfig>> {
    let inbounds: Vec<Inbound> = sqlx::query_as(
        "SELECT id, listen_port, dest, sni, server_name, private_key, public_key, short_id, \
                xhttp_path, protocol, cert_mode, custom_cert_path, custom_key_path, masquerade_url, \
                port_range, transport_protocol, status, created_at, updated_at \
         FROM inbounds WHERE protocol = 'mieru' AND status = 'active' ORDER BY id ASC",
    )
    .fetch_all(pool)
    .await
    .map_err(AppError::Database)?;

    if inbounds.is_empty() {
        return Ok(None);
    }

    let mut config = MieruServerConfig::new();

    // 1. Port bindings
    for ib in &inbounds {
        let protocols: Vec<&str> = if ib.transport_protocol.contains(',') {
            ib.transport_protocol.split(',').map(|s| s.trim()).filter(|s| !s.is_empty()).collect()
        } else if !ib.transport_protocol.trim().is_empty() {
            vec![ib.transport_protocol.trim()]
        } else {
            vec!["TCP"]
        };

        for proto in protocols {
            let proto_upper = proto.to_uppercase();
            if !ib.port_range.trim().is_empty() {
                config.port_bindings.push(PortBinding {
                    port: None,
                    port_range: Some(ib.port_range.trim().to_string()),
                    protocol: proto_upper,
                });
            } else {
                config.port_bindings.push(PortBinding {
                    port: Some(ib.listen_port),
                    port_range: None,
                    protocol: proto_upper,
                });
            }
        }
    }

    // 2. Users mapped to these Mieru inbounds
    let clients: Vec<Client> = sqlx::query_as(
        "SELECT DISTINCT c.id, c.inbound_id, c.uuid, c.name, c.quota_bytes, c.expiry_at, \
                c.status, c.sub_token, c.mieru_username, c.mieru_password, c.created_at \
         FROM clients c \
         JOIN client_inbounds ci ON ci.client_id = c.id \
         WHERE c.status = 'active' AND ci.inbound_id IN ( \
             SELECT id FROM inbounds WHERE protocol = 'mieru' AND status = 'active' \
         ) \
         ORDER BY c.id ASC",
    )
    .fetch_all(pool)
    .await
    .map_err(AppError::Database)?;

    for c in clients {
        let username = c.mieru_username
            .filter(|s| !s.trim().is_empty())
            .or_else(|| c.name.clone().filter(|s| !s.trim().is_empty()))
            .unwrap_or_else(|| format!("user_{}", &c.uuid[..8.min(c.uuid.len())]));
        let password = c.mieru_password
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| c.uuid.clone());

        config.users.push(MieruUser {
            name: username,
            password,
        });
    }

    // If client_inbounds is empty, check legacy single inbound_id
    if config.users.is_empty() {
        let legacy_clients: Vec<Client> = sqlx::query_as(
            "SELECT c.id, c.inbound_id, c.uuid, c.name, c.quota_bytes, c.expiry_at, \
                    c.status, c.sub_token, c.mieru_username, c.mieru_password, c.created_at \
             FROM clients c \
             WHERE c.status = 'active' AND c.inbound_id IN ( \
                 SELECT id FROM inbounds WHERE protocol = 'mieru' AND status = 'active' \
             ) \
             ORDER BY c.id ASC",
        )
        .fetch_all(pool)
        .await
        .map_err(AppError::Database)?;

        for c in legacy_clients {
            let username = c.mieru_username
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| format!("user_{}", &c.uuid[..8.min(c.uuid.len())]));
            let password = c.mieru_password
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| c.uuid.clone());
            config.users.push(MieruUser {
                name: username,
                password,
            });
        }
    }

    Ok(Some(config))
}

/// Render the Mieru configuration to disk.
pub async fn render_to_file(pool: &SqlitePool, dest_file: &Path) -> AppResult<bool> {
    if let Some(config) = generate_config(pool).await? {
        if let Some(parent) = dest_file.parent() {
            std::fs::create_dir_all(parent).map_err(AppError::Io)?;
        }
        let json_data = serde_json::to_string_pretty(&config)
            .map_err(|e| AppError::Mieru(format!("failed to serialize mieru config: {e}")))?;
        std::fs::write(dest_file, json_data).map_err(AppError::Io)?;
        Ok(true)
    } else {
        // No active mieru inbounds
        if dest_file.exists() {
            let _ = std::fs::remove_file(dest_file);
        }
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mieru_config_serialization() {
        let mut config = MieruServerConfig::new();
        config.port_bindings.push(PortBinding {
            port: Some(2027),
            port_range: None,
            protocol: "TCP".to_string(),
        });
        config.port_bindings.push(PortBinding {
            port: None,
            port_range: Some("2012-2022".to_string()),
            protocol: "UDP".to_string(),
        });
        config.users.push(MieruUser {
            name: "testuser".to_string(),
            password: "password123".to_string(),
        });

        let json = serde_json::to_string(&config).unwrap();
        assert!(json.contains("\"port\":2027"));
        assert!(json.contains("\"portRange\":\"2012-2022\""));
        assert!(json.contains("\"name\":\"testuser\""));
    }
}
