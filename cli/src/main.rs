// SPDX-License-Identifier: GPL-3.0-or-later

// On Windows release builds, use the "windows" subsystem so launching the
// binary from a Start Menu shortcut, the Desktop icon, or the autostart
// HKCU\…\Run entry doesn't pop a console window. The
// `attach_parent_console` shim re-attaches us to the calling terminal
// when the user ran us from cmd / PowerShell, so logs still appear
// inline there.
#![cfg_attr(
    all(target_os = "windows", not(debug_assertions)),
    windows_subsystem = "windows"
)]

mod instance;
mod settings;
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

    /// Show a system-tray icon while running.
    #[arg(long, global = true)]
    tray: bool,
}

#[derive(Subcommand, Debug, Clone)]
enum Command {
    /// Run as a relay hub only (headless / systemd / NAS).
    Serve,
    /// Run as a hub *and* as a local clipboard client on the same machine.
    Host,
    /// Connect to an existing hub as a clipboard client (default).
    Connect,
    /// Open the GUI settings dialog for the client config file.
    Settings,
}

fn main() -> Result<()> {
    #[cfg(target_os = "windows")]
    attach_parent_console();

    init_tracing();
    let cli = Cli::parse();
    let no_subcommand = cli.command.is_none();
    let cmd = cli.command.clone().unwrap_or(Command::Connect);

    // Settings mode owns the main thread for its own eframe/winit event
    // loop. No tokio runtime needed — the dialog is a pure GUI tool.
    if matches!(cmd, Command::Settings) {
        let path = match cli.config.clone() {
            Some(p) => p,
            None => {
                ClientConfig::default_path().context("could not determine default config path")?
            }
        };
        return settings::run(path);
    }

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("building tokio runtime")?;

    // Tray policy: tray mode is the default for desktop launches
    // (`clipboardwire` with no subcommand, or a Start Menu shortcut
    // launch). Explicit `--tray` keeps the old behavior. `serve` never
    // gets a tray.
    let needs_tray = matches!(cmd, Command::Connect | Command::Host) && (cli.tray || no_subcommand);
    if needs_tray {
        run_with_tray(runtime, cli, cmd)
    } else {
        runtime.block_on(run_headless(cli, cmd))
    }
}

fn init_tracing() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "clipboardwire=info,clipboardwire_core=info".into()),
        )
        .init();
}

/// Best-effort: re-attach to the parent console so `cargo run`, PowerShell,
/// and cmd.exe still surface our stderr logs even with the "windows"
/// subsystem. If there's no parent console (e.g. launched from Start Menu
/// or autostart), this is a no-op and we stay silent.
#[cfg(target_os = "windows")]
fn attach_parent_console() {
    unsafe extern "system" {
        fn AttachConsole(pid: u32) -> i32;
    }
    const ATTACH_PARENT_PROCESS: u32 = 0xFFFF_FFFF;
    unsafe {
        let _ = AttachConsole(ATTACH_PARENT_PROCESS);
    }
}

async fn run_headless(cli: Cli, cmd: Command) -> Result<()> {
    match cmd {
        Command::Serve => {
            if cli.tray {
                tracing::warn!("`--tray` is ignored in serve mode");
            }
            let cfg = resolve_server_config(cli.config.as_deref())?;
            clipboardwire_core::server::run(cfg).await
        }
        Command::Connect => {
            let cfg = load_client_config_or_bail(cli.config.as_deref())?;
            run_client_headless(cfg).await
        }
        Command::Host => run_host_headless(cli.config.as_deref()).await,
        Command::Settings => unreachable!("settings handled before runtime build"),
    }
}

/// Pick the directory the singleton-lock file lives in. We co-locate
/// it with the config file (or its platform default) so users find
/// "what is this `clipboardwire.lock` file" right where they expect.
fn singleton_lock_dir(override_path: Option<&std::path::Path>) -> Result<PathBuf> {
    let config_path = match override_path {
        Some(p) => p.to_path_buf(),
        None => ClientConfig::default_path()
            .context("could not determine the default config path for the singleton lock")?,
    };
    Ok(config_path
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from(".")))
}

/// Build the server config used by `clipboardwire serve`.
///
/// Sources, in order of precedence:
/// 1. Env vars (`CLIPBOARDWIRE_*`) — always win.
/// 2. The `[hub]` section of the TOML at `--config` (or the platform
///    default if `--config` is not supplied and the file exists).
/// 3. Built-in defaults from [`ServerConfig::from_env`].
///
/// The TOML's parent directory becomes the default `state_dir` so the
/// auto-generated self-signed cert lands next to the config file rather
/// than in a far-off platform data dir.
fn resolve_server_config(config_path: Option<&std::path::Path>) -> Result<ServerConfig> {
    let toml_path = match config_path {
        Some(p) => Some(p.to_path_buf()),
        None => ClientConfig::default_path().ok().filter(|p| p.exists()),
    };

    let base = if let Some(ref p) = toml_path {
        match ClientConfig::load(p) {
            Ok(client_cfg) => client_cfg.hub.map(|h| {
                let mut sc = h.to_server_config();
                if sc.state_dir.is_none() {
                    sc.state_dir = p.parent().map(|d| d.to_path_buf());
                }
                sc
            }),
            Err(e) => {
                tracing::warn!(
                    path = %p.display(),
                    error = %format!("{e:#}"),
                    "config file present but unreadable; falling back to env-only server config"
                );
                None
            }
        }
    } else {
        None
    };

    ServerConfig::from_env_layered(base)
}

async fn run_client_headless(cfg: ClientConfig) -> Result<()> {
    tokio::select! {
        res = clipboardwire_core::client::run(cfg) => res,
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("shutting down");
            Ok(())
        }
    }
}

/// Headless config loader: hard-errors if the file is missing. When the
/// user is at a terminal (no `--tray`) the diagnostic message is the right
/// thing to surface, and we write a template at the default path so they
/// have something to edit.
fn load_client_config_or_bail(override_path: Option<&std::path::Path>) -> Result<ClientConfig> {
    let (path, using_default) = match override_path {
        Some(p) => (p.to_path_buf(), false),
        None => (
            ClientConfig::default_path()
                .context("could not determine the default client config path")?,
            true,
        ),
    };

    if using_default && !path.exists() {
        ClientConfig::write_template(&path)
            .with_context(|| format!("writing template config at {}", path.display()))?;
        tracing::warn!(
            "no client config found; wrote a template to {} — edit it (set the server URL \
             and password) and re-run",
            path.display()
        );
        anyhow::bail!(
            "config missing; template written to {} — edit it and re-run",
            path.display()
        );
    }

    ClientConfig::load(&path)
        .with_context(|| format!("loading client config at {}", path.display()))
}

/// `host` mode without tray. The tray-mode variant is below in
/// `run_with_tray` because it has to share the runtime with the event loop.
async fn run_host_headless(client_config_path: Option<&std::path::Path>) -> Result<()> {
    let server_cfg = ServerConfig::from_env()?;
    let (listener, addr) = clipboardwire_core::server::bind(&server_cfg).await?;
    tracing::info!(addr = %addr, "hub listening (host mode)");

    let client_cfg = build_host_client_config(&server_cfg, addr.port(), client_config_path)?;

    let server_task = tokio::spawn(async move {
        clipboardwire_core::server::serve(listener, server_cfg, std::future::pending()).await
    });

    tokio::select! {
        res = server_task => { res.context("server task panicked")??; }
        res = run_client_headless(client_cfg) => { res?; }
        _ = tokio::signal::ctrl_c() => { tracing::info!("shutting down"); }
    }
    Ok(())
}

fn build_host_client_config(
    server_cfg: &ServerConfig,
    port: u16,
    override_path: Option<&std::path::Path>,
) -> Result<ClientConfig> {
    let scheme = if server_cfg.tls_enabled() {
        "wss"
    } else {
        "ws"
    };
    let loopback_url = format!("{scheme}://127.0.0.1:{port}/sync");

    let (user, password, poll_ms) = match override_path {
        Some(p) => {
            let cfg = ClientConfig::load(p).with_context(|| format!("loading {}", p.display()))?;
            (cfg.user, cfg.password, cfg.poll_ms)
        }
        None => (server_cfg.user.clone(), server_cfg.password.clone(), 300),
    };

    Ok(ClientConfig {
        server: loopback_url,
        user,
        password,
        poll_ms,
        tls_ca_file: None,
        // Loopback in host mode often pairs with a public-cert TLS hub;
        // the cert's SAN won't cover 127.0.0.1. Skip cert verification on
        // this in-process hop only.
        tls_insecure: server_cfg.tls_enabled(),
        hub: None,
    })
}

/// Tray mode: the main thread is given to tao's event loop so the OS
/// dispatches right-click, menu activation, etc. The tokio runtime stays
/// alive on its worker threads for the duration of the loop.
fn run_with_tray(runtime: tokio::runtime::Runtime, cli: Cli, cmd: Command) -> Result<()> {
    // Take the singleton lock *first*, before bringing up the tray or
    // the embedded hub. Two trays would race on the hub port and the
    // arboard X11 connection — and previously did, manifesting as
    // "client cycles connect/disconnect every few seconds." See
    // `instance::acquire_or_fail`.
    let lock_dir = singleton_lock_dir(cli.config.as_deref())?;
    if let Err(e) = instance::acquire_or_fail(&lock_dir) {
        tracing::error!(
            error = %format!("{e:#}"),
            "another clipboardwire instance is already running; exiting"
        );
        return Ok(());
    }

    let (config_path, initial_cfg, host_hub_handle, auto_open_settings) = match cmd {
        Command::Connect => {
            let (path, cfg, auto_open) = prepare_connect_tray_args(cli.config.as_deref())?;
            (path, cfg, None, auto_open)
        }
        Command::Host => {
            let (path, cfg, hub) = prepare_host_tray_args(&runtime, cli.config.as_deref())?;
            (path, Some(cfg), Some(hub), false)
        }
        Command::Serve | Command::Settings => unreachable!("non-tray subcommand"),
    };

    tray::run(
        runtime,
        config_path,
        initial_cfg,
        host_hub_handle,
        auto_open_settings,
    )
}

/// Returns (config path, optional loaded config, auto-open-settings).
/// The tray handles embedded-hub startup internally on the new flow.
fn prepare_connect_tray_args(
    override_path: Option<&std::path::Path>,
) -> Result<(PathBuf, Option<ClientConfig>, bool)> {
    let path = match override_path {
        Some(p) => p.to_path_buf(),
        None => ClientConfig::default_path()
            .context("could not determine the default client config path")?,
    };

    let template_written = if override_path.is_none() && !path.exists() {
        match ClientConfig::write_template(&path) {
            Ok(()) => {
                tracing::info!(
                    "no client config found; wrote a placeholder at {} — the settings \
                     dialog will open so you can fill it in",
                    path.display()
                );
                true
            }
            Err(e) => {
                tracing::warn!(
                    error = %format!("{e:#}"),
                    "could not write template config at {}",
                    path.display()
                );
                false
            }
        }
    } else {
        false
    };

    let (cfg, load_failed) = match ClientConfig::load(&path) {
        Ok(c) => (Some(c), false),
        Err(e) => {
            tracing::warn!(
                error = %format!("{e:#}"),
                "no usable client config yet; tray will auto-open the settings dialog"
            );
            (None, true)
        }
    };

    // Auto-open the settings dialog at startup if we don't have a
    // usable config — that's the only path for a fresh install to
    // become useful without the user discovering the tray menu.
    let auto_open_settings = template_written || load_failed;

    Ok((path, cfg, auto_open_settings))
}

/// Returns (a stand-in path for the tray's Edit-config menu, client_cfg,
/// server JoinHandle to keep alive).
fn prepare_host_tray_args(
    runtime: &tokio::runtime::Runtime,
    override_path: Option<&std::path::Path>,
) -> Result<(PathBuf, ClientConfig, tokio::task::JoinHandle<Result<()>>)> {
    let server_cfg = ServerConfig::from_env()?;
    let (listener, addr) = runtime.block_on(clipboardwire_core::server::bind(&server_cfg))?;
    tracing::info!(addr = %addr, "hub listening (host mode)");

    let client_cfg = build_host_client_config(&server_cfg, addr.port(), override_path)?;

    let path = override_path.map(|p| p.to_path_buf()).unwrap_or_else(|| {
        ClientConfig::default_path().unwrap_or_else(|_| PathBuf::from("config.toml"))
    });

    let hub_handle = runtime.spawn(async move {
        clipboardwire_core::server::serve(listener, server_cfg, std::future::pending()).await
    });

    Ok((path, client_cfg, hub_handle))
}
