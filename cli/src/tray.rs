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
//! - **Edit config…** — opens the settings GUI as a subprocess. When the
//!   GUI exits (Save or Cancel) the tray auto-reloads the config from
//!   disk and restarts the supervisor + hub if applicable.
//! - **Reload config** — manually re-trigger the same reload flow above.
//! - **Start / Stop / Restart hub** — only enabled when the loaded
//!   config has a `[hub]` section; controls the embedded hub task.
//! - **Quit clipboardwire** — orderly shutdown.
//!
//! On first launch (no usable config on disk) the tray also auto-spawns
//! the settings GUI so the user doesn't have to discover the menu first.

use std::path::{Path, PathBuf};
use std::process::Child;

use anyhow::{Context, Result};
use clipboardwire_core::client::{ClientConfig, ClientStatus};
use tao::event::Event;
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy};
use tokio::runtime::{Handle, Runtime};
use tokio::task::JoinHandle;
use tracing::{info, warn};
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{TrayIcon, TrayIconBuilder};

/// Events delivered to the tao event loop from background sources.
#[derive(Debug)]
enum UserEvent {
    /// A menu item was clicked.
    Menu(MenuEvent),
    /// The client supervisor task finished on its own (not via abort).
    SupervisorExited { generation: u64, summary: String },
    /// The embedded hub task finished on its own.
    HubExited { generation: u64, summary: String },
    /// The settings subprocess exited (Save or Cancel). Triggers an
    /// automatic reload of the config + restart of running tasks.
    SettingsExited,
    /// The client transport reported a connection-state transition for
    /// the supervisor identified by `generation`. Stale events from
    /// previous-generation supervisors are filtered out at the loop.
    ClientStatusChanged {
        generation: u64,
        status: ClientStatus,
    },
}

/// Tray-level handles that need to outlive a Reload event.
struct State {
    supervisor: Option<JoinHandle<Result<()>>>,
    supervisor_gen: u64,
    /// Last status reported by the current-generation supervisor.
    /// `None` when no supervisor is running (e.g. needs-config).
    client_status: Option<ClientStatus>,
    /// The embedded hub spawned from `[hub]` in the client config.
    embedded_hub: Option<JoinHandle<Result<()>>>,
    embedded_hub_gen: u64,
    /// PID of the currently-running settings subprocess, if any.
    /// We don't keep the `Child` here because the watcher thread owns
    /// it; this is just for "is one running" checks.
    settings_alive: bool,
    cfg: Option<ClientConfig>,
}

/// Enter the tray UI, taking the main thread for the OS event loop.
///
/// `host_hub_handle` is the long-lived hub task spawned by `host` mode
/// (env-driven, always-on). The embedded hub from `[hub]` in the client
/// config is managed *inside* this loop and is unrelated to that.
/// `auto_open_settings = true` is the first-run signal: pop the settings
/// dialog at startup since no usable config exists yet.
pub fn run(
    runtime: Runtime,
    config_path: PathBuf,
    initial_config: Option<ClientConfig>,
    host_hub_handle: Option<JoinHandle<Result<()>>>,
    auto_open_settings: bool,
) -> Result<()> {
    // CRITICAL: build the event loop FIRST. On Linux, tao's
    // EventLoopBuilder::build() runs gtk::init() — calling any
    // tray-icon constructor before that point panics with
    // "GTK has not been initialized". The Tier-1 smoke test caught
    // this regression once already; please don't reorder.
    let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();

    let menu = Menu::new();
    // The top "Status: …" line is informational (always disabled) — it
    // reflects whatever the client transport last reported via watch.
    let status_item = MenuItem::new(status_label_for(None, false), false, None);
    let sep0 = PredefinedMenuItem::separator();
    let edit_item = MenuItem::new("Edit config…", true, None);
    let reload_item = MenuItem::new("Reload config", true, None);
    let sep1 = PredefinedMenuItem::separator();
    let start_hub_item = MenuItem::new("Start hub", false, None);
    let stop_hub_item = MenuItem::new("Stop hub", false, None);
    let restart_hub_item = MenuItem::new("Restart hub", false, None);
    let sep2 = PredefinedMenuItem::separator();
    let quit_item = MenuItem::new("Quit clipboardwire", true, None);
    menu.append(&status_item)?;
    menu.append(&sep0)?;
    menu.append(&edit_item)?;
    menu.append(&reload_item)?;
    menu.append(&sep1)?;
    menu.append(&start_hub_item)?;
    menu.append(&stop_hub_item)?;
    menu.append(&restart_hub_item)?;
    menu.append(&sep2)?;
    menu.append(&quit_item)?;

    let edit_id = edit_item.id().clone();
    let reload_id = reload_item.id().clone();
    let start_hub_id = start_hub_item.id().clone();
    let stop_hub_id = stop_hub_item.id().clone();
    let restart_hub_id = restart_hub_item.id().clone();
    let quit_id = quit_item.id().clone();

    let initial_tooltip = match &initial_config {
        Some(cfg) => format!("clipboardwire — {}", cfg.server),
        None => format!(
            "clipboardwire — needs config (right-click → Edit config)\n{}",
            config_path.display()
        ),
    };

    // dark_light::detect() reaches DBus on Linux (via zbus) which needs
    // a Tokio reactor in scope; do the detection inside the runtime
    // before tao's event loop takes over the main thread.
    let theme = {
        let _guard = runtime.enter();
        dark_light::detect().ok()
    };
    let tray = TrayIconBuilder::new()
        .with_icon(build_icon(theme))
        .with_menu(Box::new(menu))
        .with_tooltip(initial_tooltip)
        .build()?;

    info!(path = %config_path.display(), "tray icon shown");

    // Pipe menu events into the tao event loop.
    let menu_proxy = proxy.clone();
    MenuEvent::set_event_handler(Some(move |event| {
        let _ = menu_proxy.send_event(UserEvent::Menu(event));
    }));

    let mut state = State {
        supervisor: None,
        supervisor_gen: 0,
        client_status: None,
        embedded_hub: None,
        embedded_hub_gen: 0,
        settings_alive: false,
        cfg: initial_config,
    };

    // Spawn supervisor + auto-start hub from the initial config.
    apply_loaded_config(runtime.handle(), &proxy, &mut state, &config_path, &tray);
    update_hub_menu(&state, &start_hub_item, &stop_hub_item, &restart_hub_item);
    update_status_item(&status_item, &state);

    // First-run / no-valid-config flow: pop the settings GUI so the
    // user has somewhere obvious to fix things from.
    if auto_open_settings {
        match launch_settings_dialog(&config_path, &proxy) {
            Ok(()) => state.settings_alive = true,
            Err(e) => warn!(
                error = %format!("{e:#}"),
                "could not auto-open settings dialog on first run"
            ),
        }
    }

    // Keep the host-mode hub task alive for the lifetime of the loop.
    let _host_hub_handle = host_hub_handle;

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;

        match event {
            Event::UserEvent(UserEvent::Menu(menu_event)) => {
                if menu_event.id == quit_id {
                    info!("Quit menu item clicked");
                    abort_running(&mut state);
                    *control_flow = ControlFlow::Exit;
                } else if menu_event.id == edit_id {
                    if state.settings_alive {
                        info!("settings dialog already open; ignoring duplicate Edit click");
                    } else {
                        match launch_settings_dialog(&config_path, &proxy) {
                            Ok(()) => state.settings_alive = true,
                            Err(e) => warn!(
                                error = %format!("{e:#}"),
                                "could not launch settings dialog"
                            ),
                        }
                    }
                } else if menu_event.id == reload_id {
                    abort_running(&mut state);
                    reload_config_into(&mut state, &config_path);
                    apply_loaded_config(runtime.handle(), &proxy, &mut state, &config_path, &tray);
                    update_hub_menu(&state, &start_hub_item, &stop_hub_item, &restart_hub_item);
                } else if menu_event.id == start_hub_id {
                    if let Err(e) =
                        start_embedded_hub(runtime.handle(), &proxy, &mut state, &config_path)
                    {
                        warn!(error=%format!("{e:#}"), "Start hub failed");
                    }
                    update_hub_menu(&state, &start_hub_item, &stop_hub_item, &restart_hub_item);
                } else if menu_event.id == stop_hub_id {
                    stop_embedded_hub(&mut state);
                    update_hub_menu(&state, &start_hub_item, &stop_hub_item, &restart_hub_item);
                } else if menu_event.id == restart_hub_id {
                    stop_embedded_hub(&mut state);
                    if let Err(e) =
                        start_embedded_hub(runtime.handle(), &proxy, &mut state, &config_path)
                    {
                        warn!(error=%format!("{e:#}"), "Restart hub failed");
                    }
                    update_hub_menu(&state, &start_hub_item, &stop_hub_item, &restart_hub_item);
                } else {
                    warn!(id = ?menu_event.id, "ignoring unknown menu event");
                }
            }
            Event::UserEvent(UserEvent::SupervisorExited {
                generation,
                summary,
            }) if Some(generation) == current_supervisor_gen(&state) => {
                state.supervisor = None;
                state.client_status = None;
                warn!(
                    gen = generation,
                    summary, "supervisor exited on its own; staying in tray"
                );
                update_status_item(&status_item, &state);
                refresh_tooltip(&tray, &state, &config_path);
            }
            Event::UserEvent(UserEvent::HubExited {
                generation,
                summary,
            }) if Some(generation) == current_hub_gen(&state) => {
                state.embedded_hub = None;
                warn!(gen = generation, summary, "embedded hub exited on its own");
                update_hub_menu(&state, &start_hub_item, &stop_hub_item, &restart_hub_item);
            }
            Event::UserEvent(UserEvent::ClientStatusChanged { generation, status })
                if generation == state.supervisor_gen =>
            {
                state.client_status = Some(status);
                update_status_item(&status_item, &state);
                refresh_tooltip(&tray, &state, &config_path);
            }
            Event::UserEvent(UserEvent::SettingsExited) => {
                info!("settings dialog closed; auto-reloading config");
                state.settings_alive = false;
                abort_running(&mut state);
                reload_config_into(&mut state, &config_path);
                apply_loaded_config(runtime.handle(), &proxy, &mut state, &config_path, &tray);
                update_hub_menu(&state, &start_hub_item, &stop_hub_item, &restart_hub_item);
                update_status_item(&status_item, &state);
                refresh_tooltip(&tray, &state, &config_path);
            }
            _ => {}
        }
    })
}

fn current_supervisor_gen(state: &State) -> Option<u64> {
    state.supervisor.as_ref().map(|_| state.supervisor_gen)
}

fn current_hub_gen(state: &State) -> Option<u64> {
    state.embedded_hub.as_ref().map(|_| state.embedded_hub_gen)
}

/// Tear down both the supervisor and the embedded hub. Used before a
/// reload, before quit, and in the settings-exit handler.
fn abort_running(state: &mut State) {
    if let Some(s) = state.supervisor.take() {
        s.abort();
    }
    if let Some(h) = state.embedded_hub.take() {
        h.abort();
    }
    state.client_status = None;
}

fn reload_config_into(state: &mut State, config_path: &Path) {
    match ClientConfig::load(config_path) {
        Ok(cfg) => {
            state.cfg = Some(cfg);
            info!("config reloaded from disk");
        }
        Err(e) => {
            state.cfg = None;
            warn!(
                error = %format!("{e:#}"),
                path = %config_path.display(),
                "could not load config after reload"
            );
        }
    }
}

/// Given `state.cfg`, (re)spawn whatever the config says should be
/// running. Updates the tray tooltip too.
fn apply_loaded_config(
    handle: &Handle,
    proxy: &EventLoopProxy<UserEvent>,
    state: &mut State,
    config_path: &Path,
    tray: &TrayIcon,
) {
    let Some(client_cfg_orig) = state.cfg.clone() else {
        refresh_tooltip(tray, state, config_path);
        return;
    };

    let mut client_cfg = client_cfg_orig.clone();

    // If [hub] enabled, start the embedded hub first and rewrite the
    // client's server URL to point at it.
    let hub_enabled = client_cfg.hub.as_ref().is_some_and(|h| h.enabled);
    if hub_enabled {
        if let Err(e) =
            start_embedded_hub_with_cfg(handle, proxy, state, &mut client_cfg, config_path)
        {
            warn!(
                error = %format!("{e:#}"),
                "[hub] is enabled but could not start the embedded hub at startup"
            );
        }
    }

    state.supervisor_gen += 1;
    let gen = state.supervisor_gen;
    let proxy_for_status = proxy.clone();
    let (status_tx, mut status_rx) = tokio::sync::watch::channel(ClientStatus::Connecting);
    // Forwarder: every watch change → UserEvent so the tao loop wakes
    // up and updates the menu/tooltip. Lives until the supervisor
    // exits and drops the sender, at which point `changed().await`
    // returns Err and we break.
    handle.spawn(async move {
        let initial = *status_rx.borrow_and_update();
        let _ = proxy_for_status.send_event(UserEvent::ClientStatusChanged {
            generation: gen,
            status: initial,
        });
        while status_rx.changed().await.is_ok() {
            let s = *status_rx.borrow_and_update();
            let _ = proxy_for_status.send_event(UserEvent::ClientStatusChanged {
                generation: gen,
                status: s,
            });
        }
    });

    let proxy_clone = proxy.clone();
    let client_cfg_for_log = client_cfg.clone();
    let sup = handle.spawn(async move {
        let result = clipboardwire_core::client::run_with_status(client_cfg, Some(status_tx)).await;
        let summary = match &result {
            Ok(()) => "clean exit".to_string(),
            Err(e) => format!("{e:#}"),
        };
        let _ = proxy_clone.send_event(UserEvent::SupervisorExited {
            generation: gen,
            summary,
        });
        result
    });
    state.supervisor = Some(sup);
    state.client_status = Some(ClientStatus::Connecting);
    let _ = client_cfg_for_log;
    refresh_tooltip(tray, state, config_path);
}

/// Start the embedded hub from `state.cfg.hub`. Returns Err if no hub
/// section is present.
fn start_embedded_hub(
    handle: &Handle,
    proxy: &EventLoopProxy<UserEvent>,
    state: &mut State,
    config_path: &Path,
) -> Result<()> {
    let Some(cfg) = state.cfg.as_mut() else {
        anyhow::bail!("no config loaded; cannot start hub");
    };
    // Start-hub from the menu brings the hub up but leaves the
    // supervisor's URL alone (it's still pointed at whatever the
    // config specified). To make the supervisor use the new hub, the
    // user can hit Reload or restart from the settings dialog.
    let mut throwaway = cfg.clone();
    start_embedded_hub_with_cfg(handle, proxy, state, &mut throwaway, config_path)
}

/// Internal: spawn the hub, rewrite `client_cfg.server` to point at the
/// loopback port.
fn start_embedded_hub_with_cfg(
    handle: &Handle,
    proxy: &EventLoopProxy<UserEvent>,
    state: &mut State,
    client_cfg: &mut ClientConfig,
    config_path: &Path,
) -> Result<()> {
    let hub_cfg = client_cfg
        .hub
        .as_ref()
        .context("config has no [hub] section")?;
    let mut server_cfg = hub_cfg.to_server_config();
    // Default the auto-gen cert location to the config file's parent
    // dir so the user knows where to find the cert (and so trusting
    // it from another client is a matter of copying the .crt next to
    // the well-known config file).
    if server_cfg.state_dir.is_none() {
        server_cfg.state_dir = config_path.parent().map(|p| p.to_path_buf());
    }

    let (listener, addr) = handle
        .block_on(clipboardwire_core::server::bind(&server_cfg))
        .with_context(|| format!("binding embedded hub to {}", server_cfg.bind))?;
    info!(addr = %addr, "embedded hub bound");

    let scheme = if !server_cfg.tls_disabled {
        "wss"
    } else {
        "ws"
    };
    client_cfg.server = format!("{scheme}://127.0.0.1:{}/sync", addr.port());
    // Loopback hop: cert SAN never covers 127.0.0.1 for arbitrary
    // server certs, and we don't auto-pin the generated self-signed
    // cert at the client level. Trust the loopback.
    if !server_cfg.tls_disabled {
        client_cfg.tls_insecure = true;
    }

    state.embedded_hub_gen += 1;
    let gen = state.embedded_hub_gen;
    let proxy_clone = proxy.clone();
    let cfg_for_serve = server_cfg.clone();
    let h = handle.spawn(async move {
        let result =
            clipboardwire_core::server::serve(listener, cfg_for_serve, std::future::pending())
                .await;
        let summary = match &result {
            Ok(()) => "clean exit".to_string(),
            Err(e) => format!("{e:#}"),
        };
        let _ = proxy_clone.send_event(UserEvent::HubExited {
            generation: gen,
            summary,
        });
        result
    });
    state.embedded_hub = Some(h);
    Ok(())
}

fn stop_embedded_hub(state: &mut State) {
    if let Some(h) = state.embedded_hub.take() {
        h.abort();
        info!("embedded hub stopped");
    }
}

/// Enable/disable the three hub menu items based on whether `[hub]` is
/// present in the loaded config and whether the hub is currently up.
fn update_hub_menu(state: &State, start: &MenuItem, stop: &MenuItem, restart: &MenuItem) {
    let has_hub_section = state.cfg.as_ref().is_some_and(|c| c.hub.is_some());
    let running = state.embedded_hub.is_some();
    start.set_enabled(has_hub_section && !running);
    stop.set_enabled(has_hub_section && running);
    restart.set_enabled(has_hub_section && running);
}

/// Refresh the disabled "Status: …" menu item text from the current state.
fn update_status_item(item: &MenuItem, state: &State) {
    item.set_text(status_label_for(state.client_status, state.cfg.is_some()));
}

/// Render the user-facing status label. Kept short + prefix-stable so
/// the menu doesn't reflow on every status change.
fn status_label_for(status: Option<ClientStatus>, has_cfg: bool) -> String {
    if !has_cfg {
        return "Status: needs configuration".to_string();
    }
    match status {
        None => "Status: starting…".to_string(),
        Some(ClientStatus::Connecting) => "Status: connecting…".to_string(),
        Some(ClientStatus::Connected) => "Status: connected".to_string(),
        Some(ClientStatus::Disconnected { will_retry_in }) => format!(
            "Status: disconnected — retrying in {}s",
            will_retry_in.as_secs().max(1)
        ),
    }
}

/// Refresh the tray tooltip from current state.
fn refresh_tooltip(tray: &TrayIcon, state: &State, config_path: &Path) {
    let tip = match (&state.cfg, state.client_status) {
        (None, _) => format!(
            "clipboardwire — needs config (right-click → Edit config)\n{}",
            config_path.display()
        ),
        (Some(cfg), None) => format!("clipboardwire — starting\n{}", cfg.server),
        (Some(cfg), Some(ClientStatus::Connecting)) => {
            format!("clipboardwire — connecting…\n{}", cfg.server)
        }
        (Some(cfg), Some(ClientStatus::Connected)) => {
            format!("clipboardwire — connected\n{}", cfg.server)
        }
        (Some(cfg), Some(ClientStatus::Disconnected { will_retry_in })) => format!(
            "clipboardwire — disconnected, retrying in {}s\n{}",
            will_retry_in.as_secs().max(1),
            cfg.server
        ),
    };
    let _ = tray.set_tooltip(Some(tip));
}

/// Launch `clipboardwire settings --config <path>` as a subprocess.
/// Spawns a watcher thread that posts `SettingsExited` to the event
/// loop when the child exits.
fn launch_settings_dialog(path: &Path, proxy: &EventLoopProxy<UserEvent>) -> Result<()> {
    let exe = std::env::current_exe().context("locating current_exe")?;
    let child: Child = std::process::Command::new(exe)
        .arg("settings")
        .arg("--config")
        .arg(path)
        .spawn()
        .context("spawning settings subprocess")?;
    info!(path = %path.display(), pid = child.id(), "settings dialog launched");

    let proxy = proxy.clone();
    std::thread::spawn(move || {
        let mut child = child;
        let _ = child.wait();
        info!("settings subprocess exited; posting reload event");
        let _ = proxy.send_event(UserEvent::SettingsExited);
    });

    Ok(())
}

/// Load the bundled tray icon, choosing a colour-/monochrome variant
/// based on the user's current system theme.
fn build_icon(theme: Option<dark_light::Mode>) -> tray_icon::Icon {
    const ICON_COLOR: &[u8] = include_bytes!("../../assets/icon-32.png");
    const ICON_MONO_DARK: &[u8] = include_bytes!("../../assets/icon-mono-dark-32.png");
    const ICON_MONO_LIGHT: &[u8] = include_bytes!("../../assets/icon-mono-light-32.png");

    let bytes = match theme {
        Some(dark_light::Mode::Light) => ICON_MONO_DARK,
        Some(dark_light::Mode::Dark) => ICON_MONO_LIGHT,
        _ => ICON_COLOR,
    };

    let img = image::load_from_memory_with_format(bytes, image::ImageFormat::Png)
        .expect("embedded tray icon PNG decodes");
    let rgba = img.to_rgba8();
    let (width, height) = rgba.dimensions();
    tray_icon::Icon::from_rgba(rgba.into_raw(), width, height).expect("tray-icon accepts RGBA")
}
