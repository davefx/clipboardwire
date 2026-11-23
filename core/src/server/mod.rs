// SPDX-License-Identifier: GPL-3.0-or-later

//! Hub server: WebSocket relay, in-memory last-clip cache, Basic auth.

pub mod auth;
pub mod config;
pub mod hub;
pub mod tls;
pub mod ws;

use std::sync::Arc;

use anyhow::{Context, Result};
use axum::routing::get;
use axum::Router;
use tokio::sync::{watch, Semaphore};
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
///
/// Connections accepted by this router are never closed by the server
/// stopping; [`serve`] uses [`build_app_with_shutdown`] instead.
pub fn build_app(config: ServerConfig) -> (Router, tokio::task::JoinHandle<()>) {
    build_app_inner(config, None)
}

/// Like [`build_app`], but every WebSocket connection is closed (with
/// `1012 Service Restart`) once `shutdown`'s sender is dropped.
pub fn build_app_with_shutdown(
    config: ServerConfig,
    shutdown: watch::Receiver<()>,
) -> (Router, tokio::task::JoinHandle<()>) {
    build_app_inner(config, Some(shutdown))
}

fn build_app_inner(
    config: ServerConfig,
    shutdown: Option<watch::Receiver<()>>,
) -> (Router, tokio::task::JoinHandle<()>) {
    let (hub, hub_join) = hub::spawn_with_stats(config.max_conns, config.stats.clone());
    let conn_sem = Arc::new(Semaphore::new(config.max_conns));
    let state = AppState {
        hub,
        config: Arc::new(config),
        conn_sem,
        shutdown,
    };
    (router(state), hub_join)
}

async fn healthz() -> &'static str {
    "ok\n"
}

/// Bind a TcpListener without starting the server. Returns the listener and
/// the actual local address (which may differ from `config.bind` when port 0
/// is requested).
pub async fn bind(
    config: &ServerConfig,
) -> Result<(tokio::net::TcpListener, std::net::SocketAddr)> {
    let listener = tokio::net::TcpListener::bind(&config.bind)
        .await
        .with_context(|| format!("binding {}", config.bind))?;
    let addr = listener.local_addr().unwrap_or(config.bind);
    Ok((listener, addr))
}

/// Serve the hub on a pre-bound listener until `shutdown` resolves.
///
/// Open WebSocket connections are closed when this future returns *or is
/// dropped* (e.g. its task aborted), so clients reconnect to whichever hub
/// takes over the port instead of lingering on this one.
///
/// Wire protocol decision tree:
/// - `tls_disabled = true`  → plain `ws://` (LAN/VPN escape hatch).
/// - cert+key paths set     → `wss://` using those files.
/// - neither (the default)  → auto-generate (or reload) a self-signed
///   cert under `state_dir` and serve `wss://` with it.
pub async fn serve(
    listener: tokio::net::TcpListener,
    mut config: ServerConfig,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> Result<()> {
    // Held for the lifetime of this future; dropping it closes every
    // connection the hub accepted.
    let (_conns_open, conns_rx) = watch::channel(());

    if config.tls_disabled {
        let (app, _hub_join) = build_app_with_shutdown(config, conns_rx);
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .with_graceful_shutdown(shutdown)
        .await
        .context("axum::serve")?;
        return Ok(());
    }

    if !config.tls_enabled() {
        // Auto-gen mode: materialize a self-signed cert under state_dir
        // (defaulting to a platform data dir) and promote the config to
        // the explicit-files TLS path. Subsequent restarts find the
        // same files on disk and reuse them.
        let state_dir = resolved_state_dir(&config)?;
        let ensured = tls::ensure_self_signed_cert(&state_dir, config.bind)
            .context("ensuring self-signed TLS cert")?;
        config.tls_cert_file = Some(ensured.cert_path);
        config.tls_key_file = Some(ensured.key_path);
    }

    serve_tls(listener, config, shutdown, conns_rx).await
}

/// Resolve where to persist auto-generated TLS state. Order:
/// 1. `config.state_dir` if set.
/// 2. Platform data dir (`~/.local/share/clipboardwire/` on Linux,
///    `~/Library/Application Support/clipboardwire/` on macOS,
///    `%APPDATA%\clipboardwire\` on Windows).
fn resolved_state_dir(config: &ServerConfig) -> Result<std::path::PathBuf> {
    if let Some(d) = &config.state_dir {
        return Ok(d.clone());
    }
    let base =
        directories::BaseDirs::new().context("could not locate the platform's data directory")?;
    Ok(base.data_dir().join("clipboardwire"))
}

async fn serve_tls(
    listener: tokio::net::TcpListener,
    config: ServerConfig,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
    conns_rx: watch::Receiver<()>,
) -> Result<()> {
    let cert = config
        .tls_cert_file
        .as_ref()
        .expect("tls_enabled() returned true with no cert file");
    let key = config
        .tls_key_file
        .as_ref()
        .expect("tls_enabled() returned true with no key file");
    let tls = axum_server::tls_rustls::RustlsConfig::from_pem_file(cert, key)
        .await
        .with_context(|| {
            format!(
                "loading TLS cert/key from {} and {}",
                cert.display(),
                key.display()
            )
        })?;

    let std_listener = listener
        .into_std()
        .context("converting tokio listener to std")?;
    std_listener
        .set_nonblocking(true)
        .context("set_nonblocking on TLS listener")?;
    let handle = axum_server::Handle::new();
    let handle_for_shutdown = handle.clone();
    tokio::spawn(async move {
        shutdown.await;
        handle_for_shutdown.graceful_shutdown(Some(std::time::Duration::from_secs(5)));
    });

    let (app, _hub_join) = build_app_with_shutdown(config, conns_rx);
    axum_server::from_tcp_rustls(std_listener, tls)
        .handle(handle)
        .serve(app.into_make_service_with_connect_info::<std::net::SocketAddr>())
        .await
        .context("axum_server::serve (tls)")?;
    Ok(())
}

/// Bind + serve in one call, using Ctrl-C / SIGTERM as the shutdown trigger.
pub async fn run(config: ServerConfig) -> Result<()> {
    let (listener, addr) = bind(&config).await?;
    info!(addr = %addr, "listening");
    serve(listener, config, shutdown_signal()).await?;
    info!("server exited");
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        use tokio::signal::unix::{signal, SignalKind};
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
