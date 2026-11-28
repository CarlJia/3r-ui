use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use subtle::ConstantTimeEq;

type HmacSha256 = Hmac<Sha256>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceStatusItem {
    pub name: String,
    pub running: bool,
    pub pid: Option<u32>,
    pub uptime_secs: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeTelemetry {
    pub cpu_percent: f64,
    pub memory_used_bytes: i64,
    pub memory_total_bytes: i64,
    pub disk_used_bytes: i64,
    pub disk_total_bytes: i64,
    pub services: Vec<ServiceStatusItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MieruNodeConfig {
    pub port_range: String,
    pub transport_protocol: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload")]
pub enum ClientMessage {
    Handshake {
        token: String,
    },
    Authenticate {
        node_id: i64,
        timestamp: i64,
        signature: String,
    },
    Heartbeat {
        telemetry: NodeTelemetry,
    },
    ConfigAck {
        success: bool,
        error: Option<String>,
    },
    ServiceStatusReport {
        services: Vec<ServiceStatusItem>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload")]
pub enum ServerMessage {
    HandshakeAck {
        node_id: i64,
        secret_key: String,
        server_time: i64,
    },
    AuthAck {
        success: bool,
        server_time: i64,
        error: Option<String>,
    },
    HeartbeatAck {
        server_time: i64,
    },
    SyncServicesConfig {
        xray_config_json: Option<String>,
        mieru_config: Option<MieruNodeConfig>,
        tunnel_token: Option<String>,
        fail2ban_enabled: bool,
    },
    RestartService {
        service_name: String,
    },
    Error {
        message: String,
    },
}

/// Compute HMAC-SHA256 signature for authenticating a node connection.
/// Format: `HMAC_SHA256(secret_key, "{node_id}:{timestamp}")` as hex string.
pub fn compute_signature(secret_key: &str, node_id: i64, timestamp: i64) -> String {
    let mut mac =
        HmacSha256::new_from_slice(secret_key.as_bytes()).expect("HMAC can take key of any size");
    let msg = format!("{node_id}:{timestamp}");
    mac.update(msg.as_bytes());
    let result = mac.finalize().into_bytes();
    result.iter().map(|b| format!("{b:02x}")).collect()
}

/// Verify HMAC-SHA256 signature with clock skew tolerance.
pub fn verify_signature(
    secret_key: &str,
    node_id: i64,
    timestamp: i64,
    server_time: i64,
    signature: &str,
    max_skew_secs: i64,
) -> bool {
    // Check if timestamp is within clock skew window
    let diff = (server_time - timestamp).abs();
    if diff > max_skew_secs {
        return false;
    }

    let expected = compute_signature(secret_key, node_id, timestamp);
    if expected.len() != signature.len() {
        return false;
    }
    expected.as_bytes().ct_eq(signature.as_bytes()).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_signature_verification() {
        let secret = "test_node_secret_key_123456789";
        let node_id = 42;
        let now = 1700000000;

        let sig = compute_signature(secret, node_id, now);
        assert!(verify_signature(secret, node_id, now, now, &sig, 60));
        assert!(verify_signature(secret, node_id, now, now + 30, &sig, 60));
        assert!(verify_signature(secret, node_id, now, now - 30, &sig, 60));

        // Beyond skew window
        assert!(!verify_signature(secret, node_id, now, now + 70, &sig, 60));

        // Wrong secret or wrong node
        assert!(!verify_signature(
            "wrong_secret",
            node_id,
            now,
            now,
            &sig,
            60
        ));
        assert!(!verify_signature(secret, 999, now, now, &sig, 60));
    }

    #[test]
    fn test_message_serde() {
        let msg = ClientMessage::Authenticate {
            node_id: 1,
            timestamp: 123456,
            signature: "abcdef".to_string(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        let de: ClientMessage = serde_json::from_str(&json).unwrap();
        match de {
            ClientMessage::Authenticate { node_id, .. } => assert_eq!(node_id, 1),
            _ => panic!("wrong message type"),
        }
    }
}
