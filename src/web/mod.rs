//! HTTP layer: router, shared state, auth, and the authenticated admin shell.

pub mod auth;
pub mod clients;
pub mod ddns;
pub mod inbound;
pub mod logs;
pub mod middleware;
pub mod nodes;
pub mod outbound;
pub mod settings;
pub mod sub;
pub mod system;
pub mod tunnel;

use std::collections::HashMap;
use std::sync::Arc;

use axum::routing::{get, post};
use axum::Router;
use tokio::sync::Mutex;
use tower_http::trace::TraceLayer;

use crate::config::AppConfig;
use crate::web::settings::JobStatus;
use crate::xray::{Installer, SupervisorCommand};

/// How long a login session stays valid.
pub const SESSION_TTL_SECS: i64 = 7 * 24 * 60 * 60;

pub const SESSION_COOKIE: &str = "session";
pub const CSRF_COOKIE: &str = "csrf";

#[derive(Clone)]
pub struct AppState {
    pub config: AppConfig,
    pub pool: sqlx::SqlitePool,
    pub supervisor_tx: tokio::sync::mpsc::Sender<SupervisorCommand>,
    pub proxy_tx: tokio::sync::mpsc::Sender<crate::proxy::ProxyCommand>,
    pub mieru_tx: tokio::sync::mpsc::Sender<crate::mieru::MieruCommand>,
    pub tunnel_tx: tokio::sync::mpsc::Sender<crate::tunnel::TunnelCommand>,
    pub tunnel_supervisor: Arc<crate::tunnel::CloudflaredSupervisor>,
    /// In-flight and recent xray-update job statuses.
    pub update_jobs: Arc<Mutex<HashMap<String, JobStatus>>>,
    pub installer: Arc<Installer>,
    pub cert_tracker: Arc<crate::proxy::CertProgressTracker>,
    pub system_tracker: Arc<system::SystemTracker>,
    pub ddns_notifier: crate::ddns::DdnsNotifier,
    pub node_manager: Arc<crate::node::NodeManager>,
    /// Set to true before sending `Stop` to the supervisor; background update
    /// tasks check this before taking actions that require a live supervisor.
    pub shutdown_initiated: Arc<std::sync::atomic::AtomicBool>,
}

/// Build the full router. Routes under `/admin` require a valid session; every
/// non-GET request additionally requires a matching CSRF header.
pub fn router(state: AppState) -> Router {
    let public = Router::new()
        .route("/", get(auth::login_page))
        .route("/login", post(auth::login_submit))
        .route("/sub/:token", get(sub::handle_subscription))
        .route(
            "/api/node/ws",
            get(crate::node::server::ws_handler).with_state(crate::node::server::NodeServerState {
                pool: state.pool.clone(),
                manager: state.node_manager.clone(),
            }),
        );

    let admin = Router::new()
        .route("/admin", get(auth::dashboard))
        .route("/admin/nodes", get(nodes::list_page))
        .route("/admin/nodes/token", post(nodes::generate_token))
        .route("/admin/nodes/:id/delete", post(nodes::delete_node))
        .route("/admin/node-groups", post(nodes::create_group))
        .route("/admin/node-groups/:id/delete", post(nodes::delete_group))
        .route("/admin/nodes/install.sh", get(nodes::install_script))
        .route("/admin/api/nodes", get(nodes::api_list_nodes))
        .route("/admin/outbounds", get(outbound::list))
        .route("/admin/outbounds/parse_link", post(outbound::parse_link))
        .route("/admin/outbounds/new", get(outbound::new_form))
        .route("/admin/outbounds/new", post(outbound::save_new))
        .route("/admin/outbounds/:id/edit", get(outbound::edit_form))
        .route("/admin/outbounds/:id/edit", post(outbound::save_edit))
        .route("/admin/outbounds/:id/delete", post(outbound::delete))
        .route("/admin/logout", post(auth::logout))
        .route("/admin/password", post(auth::change_password))
        .route("/admin/inbound", get(inbound::list))
        .route("/admin/inbound", post(inbound::save))
        .route("/admin/inbound/new", get(inbound::new_form))
        .route("/admin/inbound/:id", get(inbound::edit_form))
        .route("/admin/inbound/:id/edit", get(inbound::edit_form))
        .route("/admin/inbound/:id", post(inbound::update))
        .route("/admin/inbound/:id/delete", post(inbound::delete_by_id))
        .route("/admin/inbound/:id/toggle", post(inbound::toggle))
        .route("/admin/inbound/delete", post(inbound::delete))
        .route(
            "/admin/api/network-interfaces",
            get(inbound::api_network_interfaces),
        )
        .route("/admin/clients", get(clients::list))
        .route("/admin/clients/new", get(clients::new_form))
        .route("/admin/clients", post(clients::create))
        .route("/admin/clients/:id", get(clients::detail))
        .route("/admin/clients/:id/edit", get(clients::edit_form))
        .route("/admin/clients/:id", post(clients::update))
        .route("/admin/clients/:id/toggle", post(clients::toggle))
        .route("/admin/clients/:id/delete", post(clients::delete))
        .route("/admin/clients/:id/qr.png", get(clients::qr_png))
        .route("/admin/logs", get(logs::list))
        .route("/admin/logs/xray", get(logs::xray_logs_page))
        .route("/admin/logs/xray/raw", get(logs::xray_logs_raw))
        .route("/admin/proxy", get(settings::proxy_page))
        .route("/admin/proxy", post(settings::save_proxy_settings))
        .route(
            "/admin/proxy/request-cert",
            post(settings::request_cert_now),
        )
        .route(
            "/admin/proxy/cert-status",
            get(settings::cert_request_status),
        )
        .route("/admin/proxy/certs", get(settings::certs_dir_json))
        .route(
            "/admin/proxy/certs/download",
            get(settings::cert_file_download),
        )
        .route("/admin/tunnel", get(tunnel::tunnel_page))
        .route("/admin/tunnel", post(tunnel::save_tunnel_settings))
        .route("/admin/tunnel/action", post(tunnel::tunnel_action_api))
        .route("/admin/tunnel/status", get(tunnel::tunnel_status_api))
        .route("/admin/ddns", get(ddns::ddns_page))
        .route("/admin/ddns", post(ddns::save_ddns_settings))
        .route("/admin/ddns/sync", post(ddns::trigger_ddns_sync))
        .route("/admin/ddns/status", get(ddns::ddns_status_api))
        .route("/admin/settings", get(settings::settings_page))
        .route("/admin/settings/proxy", post(settings::save_proxy_settings))
        .route(
            "/admin/settings/proxy/request-cert",
            post(settings::request_cert_now),
        )
        .route(
            "/admin/settings/proxy/cert-status",
            get(settings::cert_request_status),
        )
        .route("/admin/settings/certs", get(settings::certs_dir_json))
        .route(
            "/admin/settings/certs/download",
            get(settings::cert_file_download),
        )
        .route(
            "/admin/settings/xray/update",
            post(settings::trigger_update),
        )
        .route("/admin/settings/xray/status", get(settings::update_status))
        .route(
            "/admin/settings/system",
            post(settings::save_system_settings),
        )
        .route("/admin/settings/detect-ip", get(settings::detect_ip_api))
        .route("/admin/settings/ddns", post(settings::save_ddns_settings))
        .route(
            "/admin/settings/ddns/sync",
            post(settings::trigger_ddns_sync),
        )
        .route(
            "/admin/settings/ddns/status",
            get(settings::ddns_status_api),
        )
        .route(
            "/admin/settings/tunnel",
            post(settings::save_tunnel_settings),
        )
        .route(
            "/admin/settings/tunnel/action",
            post(settings::tunnel_action_api),
        )
        .route(
            "/admin/settings/tunnel/status",
            get(settings::tunnel_status_api),
        )
        .route(
            "/admin/settings/security",
            post(settings::save_security_settings),
        )
        .route(
            "/admin/settings/fail2ban/status",
            get(settings::fail2ban_status_api),
        )
        .route(
            "/admin/settings/fail2ban/unban",
            post(settings::fail2ban_unban_api),
        )
        .route(
            "/admin/settings/fail2ban/install",
            post(settings::fail2ban_install_api),
        )
        .route("/admin/api/system-status", get(system::system_status_api))
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            middleware::require_session,
        ));

    public
        .merge(admin)
        .fallback(auth::not_found)
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            middleware::base_path_middleware,
        ))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}
