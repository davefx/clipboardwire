// SPDX-License-Identifier: GPL-3.0-or-later

use std::path::PathBuf;

use clap::{Parser, Subcommand};

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
    /// Run as a relay hub only (headless / Docker).
    Serve,
    /// Run as a hub *and* as a local clipboard client on the same machine.
    Host,
    /// Connect to an existing hub as a clipboard client (default).
    Connect,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
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
            let cfg = clipboardwire_core::server::ServerConfig::from_env()?;
            clipboardwire_core::server::run(cfg).await
        }
        Command::Host => anyhow::bail!("`host` not implemented yet"),
        Command::Connect => anyhow::bail!("`connect` not implemented yet"),
    }
}
