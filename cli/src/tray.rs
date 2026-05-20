// SPDX-License-Identifier: GPL-3.0-or-later

//! System-tray UI wired into a native OS event loop.
//!
//! `tray-icon` requires the thread that creates the tray icon to run an OS
//! message pump (Win32 / GTK / NSRunLoop). v0.2.0/v0.2.1 created the tray
//! from a tokio task, which on Windows meant right-click never showed the
//! menu — the icon was visible but the OS had no thread to dispatch
//! events to. v0.2.2 fixes that by giving the main thread to `tao`'s
//! event loop and running the tokio runtime on its worker threads.
//!
//! The tray comes up **before** the config is parsed, so a fresh install
//! still gets a visible icon (with a "needs config" tooltip) rather than
//! failing silently. The menu offers:
//!
//! - **Edit config…** — writes a template at the default path if absent,
//!   then opens the file in the platform's default text editor.
//! - **Reload config** — stops any running supervisor and re-parses the
//!   config from disk. Use this after editing.
//! - **Quit clipboardwire** — orderly shutdown.

use std::path::{Path, PathBuf};

use anyhow::Result;
use clipboardwire_core::client::ClientConfig;
use tao::event::Event;
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tokio::runtime::Runtime;
use tokio::task::JoinHandle;
use tracing::{info, warn};
use tray_icon::menu::{Menu, MenuEvent, MenuItem};
use tray_icon::TrayIconBuilder;

/// Events delivered to the tao event loop from background sources.
#[derive(Debug)]
enum UserEvent {
    /// A menu item was clicked.
    Menu(MenuEvent),
    /// The client supervisor task finished on its own (not via abort).
    /// Carries the generation id so we can ignore stale completions of
    /// supervisors that were already replaced.
    SupervisorExited { generation: u64, summary: String },
}

/// Enter the tray UI, taking the main thread for the OS event loop.
/// The runtime is moved into the loop so spawned tasks keep running for
/// its lifetime. `initial_config` is `Some` when we successfully parsed
/// a config before entering tray mode. `host_hub_handle` is `Some` only
/// in `host` mode — keeps the embedded server alive.
pub fn run(
    runtime: Runtime,
    config_path: PathBuf,
    initial_config: Option<ClientConfig>,
    host_hub_handle: Option<JoinHandle<Result<()>>>,
) -> Result<()> {
    // CRITICAL: build the event loop FIRST. On Linux, tao's
    // EventLoopBuilder::build() runs gtk::init() — calling any
    // tray-icon constructor before that point panics with
    // "GTK has not been initialized". The Tier-1 smoke test caught
    // this regression once already; please don't reorder.
    let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();

    let menu = Menu::new();
    let edit_item = MenuItem::new("Edit config…", true, None);
    let reload_item = MenuItem::new("Reload config", true, None);
    let quit_item = MenuItem::new("Quit clipboardwire", true, None);
    menu.append(&edit_item)?;
    menu.append(&reload_item)?;
    menu.append(&quit_item)?;

    let edit_id = edit_item.id().clone();
    let reload_id = reload_item.id().clone();
    let quit_id = quit_item.id().clone();

    let initial_tooltip = match &initial_config {
        Some(cfg) => format!("clipboardwire — {}", cfg.server),
        None => format!(
            "clipboardwire — needs config (right-click → Edit config)\n{}",
            config_path.display()
        ),
    };

    let tray = TrayIconBuilder::new()
        .with_icon(build_icon())
        .with_menu(Box::new(menu))
        .with_tooltip(initial_tooltip)
        .build()?;

    info!(path = %config_path.display(), "tray icon shown");

    // Pipe menu events into the tao event loop. This is the integration
    // point that tray-icon's docs recommend: their menu_channel fires on
    // the OS-event thread; we forward each event as a UserEvent so the
    // main loop wakes up to handle it.
    let menu_proxy = proxy.clone();
    MenuEvent::set_event_handler(Some(move |event| {
        let _ = menu_proxy.send_event(UserEvent::Menu(event));
    }));

    let mut supervisor: Option<JoinHandle<Result<()>>> = None;
    let mut supervisor_gen: u64 = 0;

    if let Some(cfg) = initial_config {
        supervisor_gen += 1;
        supervisor = Some(spawn_supervisor(
            runtime.handle(),
            cfg,
            supervisor_gen,
            proxy.clone(),
        ));
    }

    // Keep the host-mode hub task alive for the lifetime of the loop.
    let _hub_handle = host_hub_handle;

    event_loop.run(move |event, _, control_flow| {
        // The supervisor signals completion via UserEvent, so we don't
        // need to poll; idle on Wait until the OS or a UserEvent wakes us.
        *control_flow = ControlFlow::Wait;

        match event {
            Event::UserEvent(UserEvent::Menu(menu_event)) => {
                if menu_event.id == quit_id {
                    info!("Quit menu item clicked");
                    if let Some(s) = supervisor.take() {
                        s.abort();
                    }
                    *control_flow = ControlFlow::Exit;
                } else if menu_event.id == edit_id {
                    if let Err(e) = ensure_template_exists(&config_path) {
                        warn!(error = %format!("{e:#}"), "could not write template config");
                    }
                    if let Err(e) = open_in_editor(&config_path) {
                        warn!(error = %format!("{e:#}"), "could not open config in editor");
                    }
                } else if menu_event.id == reload_id {
                    if let Some(s) = supervisor.take() {
                        s.abort();
                    }
                    match ClientConfig::load(&config_path) {
                        Ok(cfg) => {
                            supervisor_gen += 1;
                            let new = spawn_supervisor(
                                runtime.handle(),
                                cfg.clone(),
                                supervisor_gen,
                                proxy.clone(),
                            );
                            supervisor = Some(new);
                            let _ =
                                tray.set_tooltip(Some(format!("clipboardwire — {}", cfg.server)));
                            info!("config reloaded; supervisor restarted");
                        }
                        Err(e) => {
                            warn!(error = %format!("{e:#}"), "config invalid; left disconnected");
                            let _ = tray.set_tooltip(Some(format!(
                                "clipboardwire — config invalid (right-click → Edit config)\n{}",
                                config_path.display()
                            )));
                        }
                    }
                } else {
                    warn!(id = ?menu_event.id, "ignoring unknown menu event");
                }
            }
            Event::UserEvent(UserEvent::SupervisorExited {
                generation,
                summary,
            }) if Some(generation) == current_generation(&supervisor, supervisor_gen) => {
                supervisor = None;
                warn!(
                    gen = generation,
                    summary, "supervisor exited on its own; staying in tray"
                );
                let _ = tray.set_tooltip(Some(format!(
                    "clipboardwire — disconnected ({}). Right-click → Reload config",
                    summary
                )));
            }
            _ => {}
        }
    })
}

/// The "current generation" is `supervisor_gen` iff a supervisor is in
/// flight. Used to filter stale UserEvent::SupervisorExited deliveries
/// from supervisors that were aborted before completing their await.
fn current_generation(
    supervisor: &Option<JoinHandle<Result<()>>>,
    supervisor_gen: u64,
) -> Option<u64> {
    if supervisor.is_some() {
        Some(supervisor_gen)
    } else {
        None
    }
}

/// Spawn the client supervisor and arrange for a UserEvent on natural
/// completion. Abortion via `.abort()` cancels the future and skips the
/// post-await event-send, so generation tracking handles the rest.
fn spawn_supervisor(
    handle: &tokio::runtime::Handle,
    cfg: ClientConfig,
    generation: u64,
    proxy: tao::event_loop::EventLoopProxy<UserEvent>,
) -> JoinHandle<Result<()>> {
    handle.spawn(async move {
        let result = clipboardwire_core::client::run(cfg).await;
        let summary = match &result {
            Ok(()) => "clean exit".to_string(),
            Err(e) => format!("{e:#}"),
        };
        let _ = proxy.send_event(UserEvent::SupervisorExited {
            generation,
            summary,
        });
        result
    })
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
fn build_icon() -> tray_icon::Icon {
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
    tray_icon::Icon::from_rgba(rgba, SIZE as u32, SIZE as u32).expect("static icon")
}
