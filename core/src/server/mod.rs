// SPDX-License-Identifier: GPL-3.0-or-later

//! Hub server: WebSocket relay, in-memory last-clip cache, Basic auth.

pub mod auth;
pub mod config;
pub mod hub;
pub mod ws;

use std::sync::Arc;

use anyhow::{Context, Result};
use axum::Router;
use axum::routing::get;
use tokio::sync::Semaphore;
use tracing::info;

pub use config::ServerConfig;
pub use ws::AppState;

/// Build the axum router from a pre-constructed [`AppState`]. Exposed for tests.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/sync", get(ws::sync_handler))
        .route("/healthz", get(healthz))
        .with_state(state)
}

/// Spin up the hub task and build a fully-stateful axum router. Returns the
/// router plus the hub's join handle (useful for orderly shutdown in tests).
pub fn build_app(config: ServerConfig) -> (Router, tokio::task::JoinHandle<()>) {
    let (hub, hub_join) = hub::spawn(config.max_conns);
    let conn_sem = Arc::new(Semaphore::new(config.max_conns));
    let state = AppState {
        hub,
        config: Arc::new(config),
        conn_sem,
    };
    (router(state), hub_join)
}

async fn healthz() -> &'static str {
    "ok\n"
}

/// Run the hub server until Ctrl-C (SIGINT) or SIGTERM.
pub async fn run(config: ServerConfig) -> Result<()> {
    let listener = tokio::net::TcpListener::bind(&config.bind)
        .await
        .with_context(|| format!("binding {}", config.bind))?;
    let local = listener.local_addr().unwrap_or(config.bind);
    info!(addr = %local, "listening");

    let (app, _hub_join) = build_app(config);

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("axum::serve")?;

    info!("server exited");
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        use tokio::signal::unix::{SignalKind, signal};
        let mut sig = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(_) => return std::future::pending::<()>().await,
        };
        sig.recv().await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
    info!("shutdown signal received");
}
