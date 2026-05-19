// SPDX-License-Identifier: GPL-3.0-or-later

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
            let cfg = ServerConfig::from_env()?;
            clipboardwire_core::server::run(cfg).await
        }
        Command::Connect => {
            let cfg = load_client_config(cli.config.as_deref())?;
            tokio::select! {
                res = clipboardwire_core::client::run(cfg) => res,
                _ = tokio::signal::ctrl_c() => {
                    tracing::info!("shutting down");
                    Ok(())
                }
            }
        }
        Command::Host => run_host(cli.config.as_deref()).await,
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
async fn run_host(client_config_path: Option<&std::path::Path>) -> Result<()> {
    let server_cfg = ServerConfig::from_env()?;
    let (listener, addr) = clipboardwire_core::server::bind(&server_cfg).await?;
    tracing::info!(addr = %addr, "hub listening (host mode)");

    let port = addr.port();
    let loopback_url = format!("ws://127.0.0.1:{port}/sync");

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
    };

    let server_task = tokio::spawn(async move {
        clipboardwire_core::server::serve(listener, server_cfg, std::future::pending()).await
    });
    let client_task =
        tokio::spawn(async move { clipboardwire_core::client::run(client_cfg).await });

    tokio::select! {
        res = server_task => {
            res.context("server task panicked")??;
        }
        res = client_task => {
            res.context("client task panicked")??;
        }
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("shutting down");
        }
    }
    Ok(())
}
