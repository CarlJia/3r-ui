use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        ConnectInfo, State,
    },
    response::Response,
};
use futures_util::{SinkExt, StreamExt};
use rand::Rng;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use crate::node::manager::NodeManager;
use crate::node::protocol::{verify_signature, ClientMessage, ServerMessage};

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

#[derive(Clone)]
pub struct NodeServerState {
    pub pool: sqlx::SqlitePool,
    pub manager: Arc<NodeManager>,
}

pub async fn ws_handler(
    ws: WebSocketUpgrade,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    State(state): State<NodeServerState>,
) -> Response {
    debug!(remote_addr = %addr, "incoming agent websocket connection");
    ws.on_upgrade(move |socket| handle_socket(socket, addr, state))
}

async fn handle_socket(socket: WebSocket, addr: SocketAddr, state: NodeServerState) {
    let (mut ws_sender, mut ws_receiver) = socket.split();
    let (tx, mut rx) = mpsc::channel::<ServerMessage>(32);

    // Forward ServerMessage to WebSocket client
    let send_task = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            let json = match serde_json::to_string(&msg) {
                Ok(j) => j,
                Err(e) => {
                    error!(error = %e, "failed to serialize server message");
                    continue;
                }
            };
            if let Err(e) = ws_sender.send(Message::Text(json)).await {
                debug!(error = %e, "client disconnected while sending message");
                break;
            }
        }
    });

    let mut authenticated_node_id: Option<i64> = None;

    // Process incoming client messages
    while let Some(msg_result) = ws_receiver.next().await {
        let msg = match msg_result {
            Ok(Message::Text(t)) => t,
            Ok(Message::Binary(b)) => match String::from_utf8(b) {
                Ok(s) => s,
                Err(_) => continue,
            },
            Ok(Message::Ping(_)) | Ok(Message::Pong(_)) => continue,
            Ok(Message::Close(_)) | Err(_) => break,
        };

        let client_msg: ClientMessage = match serde_json::from_str(&msg) {
            Ok(m) => m,
            Err(e) => {
                warn!(error = %e, raw = %msg, "invalid message frame from agent");
                let _ = tx
                    .send(ServerMessage::Error {
                        message: "Invalid JSON format".to_string(),
                    })
                    .await;
                continue;
            }
        };

        match client_msg {
            ClientMessage::Handshake { token } => {
                let consumed =
                    crate::db::node::validate_and_consume_token(&state.pool, &token).await;
                match consumed {
                    Ok(Some(prov)) => {
                        let random_key: [u8; 16] = rand::thread_rng().gen();
                        let secret_key = format!(
                            "sec_{}",
                            random_key
                                .iter()
                                .map(|b| format!("{b:02x}"))
                                .collect::<String>()
                        );
                        let name = prov
                            .node_name
                            .unwrap_or_else(|| format!("Node-{}", addr.ip()));

                        match crate::db::node::create_node(
                            &state.pool,
                            &name,
                            &secret_key,
                            Some(&addr.ip().to_string()),
                            prov.group_id,
                        )
                        .await
                        {
                            Ok(node) => {
                                info!(node_id = %node.id, name = %node.name, "node successfully provisioned");
                                authenticated_node_id = Some(node.id);
                                state.manager.register_session(node.id, tx.clone()).await;

                                let _ = tx
                                    .send(ServerMessage::HandshakeAck {
                                        node_id: node.id,
                                        secret_key,
                                        server_time: now_secs(),
                                    })
                                    .await;
                            }
                            Err(e) => {
                                error!(error = %e, "failed to create node record");
                                let _ = tx
                                    .send(ServerMessage::Error {
                                        message: "Database error during provisioning".to_string(),
                                    })
                                    .await;
                            }
                        }
                    }
                    _ => {
                        warn!(token = %token, "invalid or expired provisioning token");
                        let _ = tx
                            .send(ServerMessage::Error {
                                message: "Invalid or expired provisioning token".to_string(),
                            })
                            .await;
                    }
                }
            }
            ClientMessage::Authenticate {
                node_id,
                timestamp,
                signature,
            } => {
                let node = crate::db::node::get_node_by_id(&state.pool, node_id).await;
                match node {
                    Ok(Some(n)) => {
                        let server_time = now_secs();
                        if verify_signature(
                            &n.secret_key,
                            node_id,
                            timestamp,
                            server_time,
                            &signature,
                            60,
                        ) {
                            authenticated_node_id = Some(node_id);
                            state.manager.register_session(node_id, tx.clone()).await;
                            info!(node_id = %node_id, "node authenticated successfully");

                            let _ = tx
                                .send(ServerMessage::AuthAck {
                                    success: true,
                                    server_time,
                                    error: None,
                                })
                                .await;
                        } else {
                            warn!(node_id = %node_id, "signature verification failed or clock skewed");
                            let _ = tx
                                .send(ServerMessage::AuthAck {
                                    success: false,
                                    server_time,
                                    error: Some("Signature verification failed".to_string()),
                                })
                                .await;
                        }
                    }
                    _ => {
                        warn!(node_id = %node_id, "node not found for authentication");
                        let _ = tx
                            .send(ServerMessage::AuthAck {
                                success: false,
                                server_time: now_secs(),
                                error: Some("Node not found".to_string()),
                            })
                            .await;
                    }
                }
            }
            ClientMessage::Heartbeat { telemetry } => {
                if let Some(node_id) = authenticated_node_id {
                    state.manager.update_heartbeat(node_id, telemetry).await;
                    let _ = tx
                        .send(ServerMessage::HeartbeatAck {
                            server_time: now_secs(),
                        })
                        .await;
                } else {
                    let _ = tx
                        .send(ServerMessage::Error {
                            message: "Unauthorized: authenticate first".to_string(),
                        })
                        .await;
                }
            }
            ClientMessage::ConfigAck { success, error } => {
                if let Some(node_id) = authenticated_node_id {
                    if success {
                        info!(node_id = %node_id, "agent applied config update successfully");
                    } else {
                        warn!(node_id = %node_id, error = ?error, "agent failed to apply config");
                    }
                }
            }
            ClientMessage::ServiceStatusReport { services } => {
                if let Some(node_id) = authenticated_node_id {
                    debug!(node_id = %node_id, count = %services.len(), "received services status report");
                }
            }
        }
    }

    if let Some(node_id) = authenticated_node_id {
        state.manager.unregister_session(node_id).await;
    }

    send_task.abort();
    debug!(remote_addr = %addr, "agent websocket connection terminated");
}
