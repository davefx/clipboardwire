// SPDX-License-Identifier: GPL-3.0-or-later

//! System-tray UI wrapper around the headless client supervisor.
//!
//! The tray comes up **before** the config is parsed, so a fresh install
//! that hasn't been configured yet still gets a visible icon (with a
//! "needs config" tooltip) rather than failing silently. The menu offers:
//!
//! - **Edit config…** — opens the config file in the platform's default
//!   text editor (notepad / xdg-open / open). If the file doesn't exist
//!   yet, a placeholder is written first.
//! - **Reload config** — stops any running supervisor and re-parses the
//!   config from disk. Use this after editing.
//! - **Quit clipboardwire** — orderly shutdown.
//!
//! Linux uses the libayatana-appindicator backend; the binary therefore
//! requires libgtk-3 + libayatana-appindicator3 at runtime even for
//! headless `serve` use. A `tray` cargo feature so distro maintainers can
//! ship a server-only variant is on the v0.3 backlog.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Result;
use clipboardwire_core::client::ClientConfig;
use tokio::task::JoinHandle;
use tracing::{info, warn};
use tray_icon::menu::{Menu, MenuEvent, MenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

/// Run the tray UI with deferred config loading.
///
/// `initial_config` is `Some` if the binary managed to parse a config
/// before entering tray mode; `None` otherwise (first run, or a parse
/// error logged elsewhere). The tray always comes up either way.
pub async fn run(config_path: PathBuf, initial_config: Option<ClientConfig>) -> Result<()> {
    let menu = Menu::new();
    let edit_item = MenuItem::new("Edit config…", true, None);
    let reload_item = MenuItem::new("Reload config", true, None);
    let quit_item = MenuItem::new("Quit clipboardwire", true, None);
    menu.append(&edit_item)?;
    menu.append(&reload_item)?;
    menu.append(&quit_item)?;

    let initial_tooltip = match &initial_config {
        Some(cfg) => format!("clipboardwire — {}", cfg.server),
        None => format!(
            "clipboardwire — needs config (right-click → Edit config). \
             File: {}",
            config_path.display()
        ),
    };

    let tray = TrayIconBuilder::new()
        .with_icon(build_icon())
        .with_menu(Box::new(menu))
        .with_tooltip(initial_tooltip)
        .build()?;

    info!(path = %config_path.display(), "tray icon shown");

    let mut supervisor: Option<JoinHandle<Result<()>>> =
        initial_config.map(|cfg| tokio::spawn(clipboardwire_core::client::run(cfg)));

    let menu_rx = MenuEvent::receiver();
    let edit_id = edit_item.id().clone();
    let reload_id = reload_item.id().clone();
    let quit_id = quit_item.id().clone();

    let mut menu_poll = tokio::time::interval(Duration::from_millis(100));
    menu_poll.tick().await;

    loop {
        tokio::select! {
            biased;

            // Watch the supervisor when one is running. If it exits on its
            // own (e.g. unrecoverable transport error), drop it but keep
            // the tray alive so the user can fix the config and reload.
            _ = poll_supervisor(&mut supervisor) => {
                update_tooltip(&tray, &config_path, supervisor.is_some(), Some("supervisor exited; right-click → Edit config"));
            }

            _ = tokio::signal::ctrl_c() => {
                info!("ctrl-c received; shutting down");
                abort_supervisor(&mut supervisor);
                return Ok(());
            }

            _ = menu_poll.tick() => {
                while let Ok(event) = menu_rx.try_recv() {
                    if event.id == quit_id {
                        info!("Quit menu item clicked");
                        abort_supervisor(&mut supervisor);
                        return Ok(());
                    } else if event.id == edit_id {
                        if let Err(e) = ensure_template_exists(&config_path) {
                            warn!(error = %format!("{e:#}"), "could not write template config");
                        }
                        if let Err(e) = open_in_editor(&config_path) {
                            warn!(error = %format!("{e:#}"), "could not open config in editor");
                        }
                    } else if event.id == reload_id {
                        abort_supervisor(&mut supervisor);
                        match ClientConfig::load(&config_path) {
                            Ok(cfg) => {
                                info!("config reloaded; starting supervisor");
                                let new = tokio::spawn(clipboardwire_core::client::run(cfg.clone()));
                                supervisor = Some(new);
                                let _ = tray.set_tooltip(Some(format!(
                                    "clipboardwire — {}",
                                    cfg.server
                                )));
                            }
                            Err(e) => {
                                warn!(error = %format!("{e:#}"), "config still invalid after reload");
                                update_tooltip(&tray, &config_path, false, Some("config invalid; check it"));
                            }
                        }
                    } else {
                        warn!(id = ?event.id, "ignoring unknown menu event");
                    }
                }
            }
        }
    }
}

/// Update the tray tooltip with a status hint.
fn update_tooltip(tray: &TrayIcon, config_path: &Path, _connected: bool, status: Option<&str>) {
    let text = match status {
        Some(s) => format!("clipboardwire — {} (file: {})", s, config_path.display()),
        None => format!("clipboardwire — file: {}", config_path.display()),
    };
    let _ = tray.set_tooltip(Some(text));
}

/// Future that resolves only when there is a running supervisor and it
/// finishes. When `supervisor` is `None`, returns pending forever so the
/// caller's `select!` ignores this branch.
async fn poll_supervisor(supervisor: &mut Option<JoinHandle<Result<()>>>) {
    match supervisor {
        Some(handle) => {
            let result = handle.await;
            match result {
                Ok(Ok(())) => info!("client supervisor exited cleanly"),
                Ok(Err(e)) => {
                    warn!(error = %format!("{e:#}"), "client supervisor exited with error")
                }
                Err(e) if e.is_cancelled() => info!("client supervisor cancelled"),
                Err(e) => warn!(error = %e, "client supervisor task panicked"),
            }
            *supervisor = None;
        }
        None => std::future::pending::<()>().await,
    }
}

fn abort_supervisor(supervisor: &mut Option<JoinHandle<Result<()>>>) {
    if let Some(handle) = supervisor.take() {
        handle.abort();
    }
}

/// If `path` doesn't exist, write the placeholder template + chmod 0600.
fn ensure_template_exists(path: &Path) -> Result<()> {
    if !path.exists() {
        ClientConfig::write_template(path)?;
        info!(path = %path.display(), "wrote template config");
    }
    Ok(())
}

/// Open `path` in the platform's default text editor (best-effort).
fn open_in_editor(path: &Path) -> Result<()> {
    #[cfg(target_os = "windows")]
    let cmd = "notepad";
    #[cfg(target_os = "macos")]
    let cmd = "open";
    #[cfg(all(unix, not(target_os = "macos")))]
    let cmd = "xdg-open";

    std::process::Command::new(cmd).arg(path).spawn()?;
    Ok(())
}

/// 32×32 RGBA placeholder icon: a solid blue square with a small white
/// square inside, drawn programmatically so we don't ship an image asset.
fn build_icon() -> Icon {
    const SIZE: usize = 32;
    let mut rgba = vec![0u8; SIZE * SIZE * 4];
    for y in 0..SIZE {
        for x in 0..SIZE {
            let i = (y * SIZE + x) * 4;
            let inside = (8..24).contains(&x) && (8..24).contains(&y);
            let (r, g, b) = if inside {
                (255, 255, 255)
            } else {
                (32, 96, 192)
            };
            rgba[i] = r;
            rgba[i + 1] = g;
            rgba[i + 2] = b;
            rgba[i + 3] = 255;
        }
    }
    Icon::from_rgba(rgba, SIZE as u32, SIZE as u32).expect("static icon")
}
