#![allow(clippy::result_large_err)]

use askama::Template;
use axum::{
    extract::{Extension, Path, State},
    http::{HeaderMap, StatusCode},
    response::{Html, IntoResponse, Response},
    Json,
};
use axum_extra::extract::CookieJar;
use serde::{Deserialize, Serialize};

use crate::db::node::{Node, NodeGroup};
use crate::web::middleware::{csrf_from_cookies, BasePath};
use crate::web::AppState;

#[derive(Template)]
#[template(path = "nodes.html")]
pub struct NodesTemplate {
    pub base_path: String,
    pub nav_active: &'static str,
    pub nodes: Vec<Node>,
    pub groups: Vec<NodeGroup>,
    pub csrf_token: String,
    pub online_count: usize,
    pub total_count: usize,
}

#[allow(clippy::result_large_err)]
pub async fn list_page(
    State(state): State<AppState>,
    base_path_ext: Option<Extension<BasePath>>,
    jar: CookieJar,
) -> Result<Html<String>, Response> {
    let base_path = base_path_ext.map(|Extension(p)| p.0).unwrap_or_default();
    let nodes = crate::db::node::list_nodes(&state.pool)
        .await
        .map_err(|e| {
            (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")).into_response()
        })?;

    let groups = crate::db::node::list_groups(&state.pool)
        .await
        .map_err(|e| {
            (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")).into_response()
        })?;

    let online_count = nodes.iter().filter(|n| n.status == "online").count();
    let total_count = nodes.len();
    let csrf_token = csrf_from_cookies(&jar);

    let template = NodesTemplate {
        base_path,
        nav_active: "nodes",
        nodes,
        groups,
        csrf_token,
        online_count,
        total_count,
    };

    template.render().map(Html).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Template error: {e}"),
        )
            .into_response()
    })
}

#[derive(Serialize)]
pub struct NodeInfo {
    pub id: i64,
    pub name: String,
    pub is_local: bool,
    pub status: String,
    pub server_address: Option<String>,
    pub group_id: Option<i64>,
    pub cpu_percent: Option<f64>,
    pub memory_used_bytes: Option<i64>,
    pub memory_total_bytes: Option<i64>,
    pub disk_used_bytes: Option<i64>,
    pub disk_total_bytes: Option<i64>,
    pub last_heartbeat_at: Option<i64>,
}

pub async fn api_list_nodes(
    State(state): State<AppState>,
) -> Result<Json<Vec<NodeInfo>>, Response> {
    let nodes = crate::db::node::list_nodes(&state.pool)
        .await
        .map_err(|e| {
            (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")).into_response()
        })?;

    let list = nodes
        .into_iter()
        .map(|n| NodeInfo {
            id: n.id,
            name: n.name,
            is_local: n.is_local == 1,
            status: n.status,
            server_address: n.server_address,
            group_id: n.group_id,
            cpu_percent: n.cpu_percent,
            memory_used_bytes: n.memory_used_bytes,
            memory_total_bytes: n.memory_total_bytes,
            disk_used_bytes: n.disk_used_bytes,
            disk_total_bytes: n.disk_total_bytes,
            last_heartbeat_at: n.last_heartbeat_at,
        })
        .collect();

    Ok(Json(list))
}

#[derive(Deserialize)]
pub struct GenerateTokenRequest {
    pub group_id: Option<i64>,
    pub node_name: Option<String>,
}

#[derive(Serialize)]
pub struct GenerateTokenResponse {
    pub token: String,
    pub install_command: String,
}

pub async fn generate_token(
    State(state): State<AppState>,
    base_path_ext: Option<Extension<BasePath>>,
    headers: HeaderMap,
    Json(payload): Json<GenerateTokenRequest>,
) -> Result<Json<GenerateTokenResponse>, Response> {
    let base_path = base_path_ext.map(|Extension(p)| p.0).unwrap_or_default();
    let token = crate::db::node::create_provisioning_token(
        &state.pool,
        payload.group_id,
        payload.node_name.as_deref(),
        900, // 15 mins
    )
    .await
    .map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to create token: {e}"),
        )
            .into_response()
    })?;

    let host = headers
        .get("x-forwarded-host")
        .or_else(|| headers.get(axum::http::header::HOST))
        .and_then(|v| v.to_str().ok())
        .unwrap_or("127.0.0.1:2053");

    let proto = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("http");

    let master_url = format!("{proto}://{host}{base_path}");
    let install_command = format!(
        "curl -fsSL \"{master_url}/admin/nodes/install.sh\" | sh -s -- --server \"{master_url}\" --token \"{token}\""
    );

    Ok(Json(GenerateTokenResponse {
        token,
        install_command,
    }))
}

pub async fn delete_node(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<serde_json::Value>, Response> {
    crate::db::node::delete_node(&state.pool, id)
        .await
        .map_err(|e| {
            (StatusCode::BAD_REQUEST, format!("Cannot delete node: {e}")).into_response()
        })?;

    Ok(Json(serde_json::json!({ "success": true })))
}

#[derive(Deserialize)]
pub struct CreateGroupRequest {
    pub name: String,
    pub tag: String,
    pub description: Option<String>,
}

pub async fn create_group(
    State(state): State<AppState>,
    Json(payload): Json<CreateGroupRequest>,
) -> Result<Json<NodeGroup>, Response> {
    let group = crate::db::node::create_group(
        &state.pool,
        &payload.name,
        &payload.tag,
        payload.description.as_deref(),
    )
    .await
    .map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            format!("Failed to create group: {e}"),
        )
            .into_response()
    })?;

    Ok(Json(group))
}

pub async fn delete_group(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<serde_json::Value>, Response> {
    crate::db::node::delete_group(&state.pool, id)
        .await
        .map_err(|e| {
            (
                StatusCode::BAD_REQUEST,
                format!("Failed to delete group: {e}"),
            )
                .into_response()
        })?;

    Ok(Json(serde_json::json!({ "success": true })))
}

pub async fn install_script() -> Response {
    let script = r#"#!/bin/sh
set -e

# 3R-UI Sub-Agent Automated Installer
echo "=== Installing 3R-UI VPS Node Sub-Agent ==="

SERVER_URL=""
TOKEN=""

while [ "$#" -gt 0 ]; do
  case "$1" in
    --server)
      SERVER_URL="$2"
      shift 2
      ;;
    --token)
      TOKEN="$2"
      shift 2
      ;;
    *)
      shift 1
      ;;
  esac
done

if [ -z "$SERVER_URL" ] || [ -z "$TOKEN" ]; then
  echo "Error: Missing required arguments --server or --token" >&2
  exit 1
fi

INSTALL_DIR="/usr/local/bin"
DATA_DIR="/etc/3r-agent"

mkdir -p "$INSTALL_DIR"
mkdir -p "$DATA_DIR"
chmod 700 "$DATA_DIR"

echo "Downloading 3r-ui binary from $SERVER_URL ..."
if [ ! -f "$INSTALL_DIR/3r-ui" ]; then
  echo "Place 3r-ui binary into $INSTALL_DIR/3r-ui"
fi

cat <<EOF > /etc/systemd/system/3r-agent.service
[Unit]
Description=3R-UI Distributed VPS Sub-Agent
After=network.target

[Service]
Type=simple
User=root
ExecStart=$INSTALL_DIR/3r-ui agent --server $SERVER_URL --token $TOKEN --data-dir $DATA_DIR
Restart=always
RestartSec=5
LimitNOFILE=65536

[Install]
WantedBy=multi-user.target
EOF

systemctl daemon-reload || true
echo "3r-agent service created at /etc/systemd/system/3r-agent.service"
echo "To start: systemctl enable --now 3r-agent"
"#;

    (
        [(axum::http::header::CONTENT_TYPE, "text/x-shellscript")],
        script,
    )
        .into_response()
}
