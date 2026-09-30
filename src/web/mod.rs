//! HTTP layer: router, shared state, auth, and the authenticated admin shell.

pub mod auth;
pub mod middleware;

use axum::routing::{get, post};
use axum::Router;
use tower_http::trace::TraceLayer;

use crate::config::AppConfig;
use crate::xray::SupervisorCommand;

/// How long a login session stays valid.
pub const SESSION_TTL_SECS: i64 = 7 * 24 * 60 * 60;

pub const SESSION_COOKIE: &str = "session";
pub const CSRF_COOKIE: &str = "csrf";

#[derive(Clone)]
pub struct AppState {
    pub config: AppConfig,
    pub pool: sqlx::SqlitePool,
    pub supervisor_tx: tokio::sync::mpsc::Sender<SupervisorCommand>,
}

/// Build the full router. Routes under `/admin` require a valid session; every
/// non-GET request additionally requires a matching CSRF header.
pub fn router(state: AppState) -> Router {
    let public = Router::new()
        .route("/", get(auth::login_page))
        .route("/login", post(auth::login_submit));

    let admin = Router::new()
        .route("/admin", get(auth::dashboard))
        .route("/admin/logout", post(auth::logout))
        .route("/admin/password", post(auth::change_password))
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            middleware::require_session,
        ));

    public
        .merge(admin)
        .fallback(auth::not_found)
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}
