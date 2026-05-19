// SPDX-License-Identifier: GPL-3.0-or-later

#[cfg(windows)]
mod tray;

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use clipboardwire_core::client::ClientConfig;
use clipboardwire_core::server::ServerConfig;

#[derive(Parser, Debug)]
#[command(version, about = "Clipboard sync over WebSocket", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    /// Path to the client config file (used by `connect` and `host`).
    #[arg(long, global = true)]
    config: Option<PathBuf>,

    /// Show a system-tray icon while running. Windows-only in v0.1;
    /// on other platforms this falls back to the headless mode with a
    /// warning.
    #[arg(long, global = true)]
    tray: bool,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Run as a relay hub only (headless / systemd / NAS).
    Serve,
    /// Run as a hub *and* as a local clipboard client on the same machine.
    Host,
    /// Connect to an existing hub as a clipboard client (default).
    Connect,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "clipboardwire=info,clipboardwire_core=info".into()),
        )
        .init();

    let cli = Cli::parse();
    let cmd = cli.command.unwrap_or(Command::Connect);

    match cmd {
        Command::Serve => {
            if cli.tray {
                tracing::warn!("`--tray` is ignored in serve mode");
            }
            let cfg = ServerConfig::from_env()?;
            clipboardwire_core::server::run(cfg).await
        }
        Command::Connect => {
            let cfg = load_client_config(cli.config.as_deref())?;
            run_client(cfg, cli.tray).await
        }
        Command::Host => run_host(cli.config.as_deref(), cli.tray).await,
    }
}

async fn run_client(cfg: ClientConfig, tray: bool) -> Result<()> {
    if tray {
        #[cfg(windows)]
        {
            return tray::run(cfg).await;
        }
        #[cfg(not(windows))]
        {
            tracing::warn!("--tray is Windows-only in v0.1; running headless");
        }
    }
    tokio::select! {
        res = clipboardwire_core::client::run(cfg) => res,
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("shutting down");
            Ok(())
        }
    }
}

fn load_client_config(override_path: Option<&std::path::Path>) -> Result<ClientConfig> {
    let path = match override_path {
        Some(p) => p.to_path_buf(),
        None => ClientConfig::default_path()
            .context("could not determine the default client config path")?,
    };
    ClientConfig::load(&path)
        .with_context(|| format!("loading client config at {}", path.display()))
}

/// `host` mode: bind the server first (so the client connects to a real
/// listener), then spawn both the server and a client pointed at loopback.
///
/// If a client config file is supplied via `--config`, the user/password and
/// poll_ms come from there; the `server` URL field is ignored and we use
/// `ws://127.0.0.1:<bound_port>/sync` instead. With no `--config`, we derive
/// the client credentials from the same env vars the server reads.
async fn run_host(client_config_path: Option<&std::path::Path>, tray: bool) -> Result<()> {
    let server_cfg = ServerConfig::from_env()?;
    let (listener, addr) = clipboardwire_core::server::bind(&server_cfg).await?;
    tracing::info!(addr = %addr, "hub listening (host mode)");

    let port = addr.port();
    let scheme = if server_cfg.tls_enabled() {
        "wss"
    } else {
        "ws"
    };
    let loopback_url = format!("{scheme}://127.0.0.1:{port}/sync");

    let (user, password, poll_ms) = match client_config_path {
        Some(p) => {
            let cfg = ClientConfig::load(p).with_context(|| format!("loading {}", p.display()))?;
            (cfg.user, cfg.password, cfg.poll_ms)
        }
        None => (server_cfg.user.clone(), server_cfg.password.clone(), 300),
    };

    let client_cfg = ClientConfig {
        server: loopback_url,
        user,
        password,
        poll_ms,
        // `host` connects to its own embedded server on loopback. If that
        // server is configured for TLS, the cert is most likely self-signed
        // and SAN'd for the public hostname rather than 127.0.0.1, so skip
        // verification on the loopback hop. The blast radius is bounded to
        // this in-process client.
        tls_ca_file: None,
        tls_insecure: server_cfg.tls_enabled(),
    };

    let server_task = tokio::spawn(async move {
        clipboardwire_core::server::serve(listener, server_cfg, std::future::pending()).await
    });

    let client_future = run_client(client_cfg, tray);

    tokio::select! {
        res = server_task => {
            res.context("server task panicked")??;
        }
        res = client_future => {
            res?;
        }
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("shutting down");
        }
    }
    Ok(())
}
