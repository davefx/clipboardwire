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
use clipboardwire_core::server::hub::HubStatsSink;
use tao::event::Event;
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy};
use tokio::runtime::{Handle, Runtime};
use tokio::task::JoinHandle;
use tracing::{info, warn};
#[cfg(windows)]
use tray_icon::menu::CheckMenuItem;
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
    /// The system theme changed (dark ↔ light) since we last looked.
    /// Triggers an icon rebuild so the mono variant matches the new
    /// tray background.
    ThemeChanged(Option<dark_light::Mode>),
    /// The embedded hub's connected-peer count changed since the last
    /// poll. The handler just calls `refresh_tooltip`, which reads
    /// the live count straight from `state.hub_stats`.
    HubPeerCountChanged,
}

/// How often the background task re-checks the system theme. 5 s is
/// short enough that a manual light/dark toggle feels live, long
/// enough that the per-poll dark_light::detect() (which reaches DBus
/// on Linux) doesn't show up in profiles.
const THEME_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);

/// How often the embedded-hub poller re-reads its connected-peer
/// count. The read is a single atomic load, so a 1 s cadence has no
/// measurable cost.
const HUB_STATS_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

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
    /// Set when the most recent hub-bind attempt failed (e.g. another
    /// process is on the same port). Shown in the tray status line so
    /// the user doesn't have to read the log to understand why nothing
    /// works.
    hub_bind_error: Option<String>,
    /// Live counter the embedded hub task updates on every client
    /// register/deregister. `Some` while the hub is running, `None`
    /// otherwise. The poller task reads from this; the tooltip folds
    /// it into the "X peers connected" suffix.
    hub_stats: Option<HubStatsSink>,
    /// JoinHandle for the per-hub-session background task that polls
    /// `hub_stats` and posts a `HubPeerCountChanged` event when the
    /// count shifts. Aborted alongside the hub itself.
    hub_stats_poller: Option<JoinHandle<()>>,
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
    // The top lines are informational (always disabled) — they show
    // connection status, server URL, and hub peer count. Tooltips are
    // unreliable on Linux (libayatana-appindicator doesn't support
    // them), so we surface everything in the menu itself.
    let status_item = MenuItem::new(status_label_for(None, false), false, None);
    let server_item = MenuItem::new(server_label(initial_config.as_ref()), false, None);
    let hub_info_item = MenuItem::new("", false, None);
    hub_info_item.set_enabled(false);
    let sep0 = PredefinedMenuItem::separator();
    let edit_item = MenuItem::new("Edit config…", true, None);
    let reload_item = MenuItem::new("Reload config", true, None);
    #[cfg(windows)]
    let autostart_item = CheckMenuItem::new(
        "Start at login",
        true,
        crate::autostart_win::is_enabled(),
        None,
    );
    let sep1 = PredefinedMenuItem::separator();
    let start_hub_item = MenuItem::new("Start hub", false, None);
    let stop_hub_item = MenuItem::new("Stop hub", false, None);
    let restart_hub_item = MenuItem::new("Restart hub", false, None);
    let sep2 = PredefinedMenuItem::separator();
    let quit_item = MenuItem::new("Quit clipboardwire", true, None);
    menu.append(&status_item)?;
    menu.append(&server_item)?;
    menu.append(&hub_info_item)?;
    menu.append(&sep0)?;
    menu.append(&edit_item)?;
    menu.append(&reload_item)?;
    #[cfg(windows)]
    menu.append(&autostart_item)?;
    menu.append(&sep1)?;
    menu.append(&start_hub_item)?;
    menu.append(&stop_hub_item)?;
    menu.append(&restart_hub_item)?;
    menu.append(&sep2)?;
    menu.append(&quit_item)?;

    let edit_id = edit_item.id().clone();
    let reload_id = reload_item.id().clone();
    #[cfg(windows)]
    let autostart_id = autostart_item.id().clone();
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
        .with_icon(build_icon(theme, None, false))
        .with_menu(Box::new(menu))
        .with_tooltip(initial_tooltip)
        .build()?;

    info!(path = %config_path.display(), "tray icon shown");

    // Pipe menu events into the tao event loop.
    let menu_proxy = proxy.clone();
    MenuEvent::set_event_handler(Some(move |event| {
        let _ = menu_proxy.send_event(UserEvent::Menu(event));
    }));

    // Background task that re-detects the system theme every
    // THEME_POLL_INTERVAL and posts a UserEvent if it changed since
    // the last check. dark_light has no subscription API so polling
    // is the cross-platform option; the detect() call is cheap
    // (~ms) and only fires the event when the value actually flips,
    // so the cost is negligible.
    let theme_proxy = proxy.clone();
    runtime.handle().spawn(async move {
        let mut last = dark_light::detect().ok();
        let mut tick = tokio::time::interval(THEME_POLL_INTERVAL);
        tick.tick().await; // skip the immediate first tick
        loop {
            tick.tick().await;
            let now = dark_light::detect().ok();
            if now != last {
                last = now;
                let _ = theme_proxy.send_event(UserEvent::ThemeChanged(now));
            }
        }
    });

    let mut state = State {
        supervisor: None,
        supervisor_gen: 0,
        client_status: None,
        embedded_hub: None,
        embedded_hub_gen: 0,
        hub_bind_error: None,
        hub_stats: None,
        hub_stats_poller: None,
        settings_alive: false,
        cfg: initial_config,
    };

    // Spawn supervisor + auto-start hub from the initial config.
    apply_loaded_config(runtime.handle(), &proxy, &mut state, &config_path, &tray);
    update_hub_menu(&state, &start_hub_item, &stop_hub_item, &restart_hub_item);
    update_status_item(&status_item, &state);
    update_server_item(&server_item, &state);
    update_hub_info_item(&hub_info_item, &state);
    refresh_icon(&tray, theme, &state);

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

    let mut theme = theme;
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
                    update_status_item(&status_item, &state);
                    update_server_item(&server_item, &state);
                    update_hub_info_item(&hub_info_item, &state);
                    refresh_icon(&tray, theme, &state);
                } else if menu_event.id == start_hub_id {
                    if let Err(e) =
                        start_embedded_hub(runtime.handle(), &proxy, &mut state, &config_path)
                    {
                        warn!(error=%format!("{e:#}"), "Start hub failed");
                    }
                    update_hub_menu(&state, &start_hub_item, &stop_hub_item, &restart_hub_item);
                    update_status_item(&status_item, &state);
                    update_hub_info_item(&hub_info_item, &state);
                    refresh_tooltip(&tray, &state, &config_path);
                    refresh_icon(&tray, theme, &state);
                } else if menu_event.id == stop_hub_id {
                    stop_embedded_hub(&mut state);
                    update_hub_menu(&state, &start_hub_item, &stop_hub_item, &restart_hub_item);
                    update_hub_info_item(&hub_info_item, &state);
                    refresh_icon(&tray, theme, &state);
                } else if menu_event.id == restart_hub_id {
                    stop_embedded_hub(&mut state);
                    if let Err(e) =
                        start_embedded_hub(runtime.handle(), &proxy, &mut state, &config_path)
                    {
                        warn!(error=%format!("{e:#}"), "Restart hub failed");
                    }
                    update_hub_menu(&state, &start_hub_item, &stop_hub_item, &restart_hub_item);
                    update_status_item(&status_item, &state);
                    update_hub_info_item(&hub_info_item, &state);
                    refresh_tooltip(&tray, &state, &config_path);
                    refresh_icon(&tray, theme, &state);
                } else {
                    #[cfg(windows)]
                    {
                        if menu_event.id == autostart_id {
                            // CheckMenuItem auto-toggles before the event fires, so
                            // `is_checked()` reflects the new desired state.
                            if autostart_item.is_checked() {
                                if let Err(e) = crate::autostart_win::enable() {
                                    warn!(error=%format!("{e:#}"), "could not enable autostart");
                                    autostart_item.set_checked(false);
                                } else {
                                    info!("autostart enabled");
                                }
                            } else {
                                if let Err(e) = crate::autostart_win::disable() {
                                    warn!(error=%format!("{e:#}"), "could not disable autostart");
                                    autostart_item.set_checked(true);
                                } else {
                                    info!("autostart disabled");
                                }
                            }
                        } else {
                            warn!(id = ?menu_event.id, "ignoring unknown menu event");
                        }
                    }
                    #[cfg(not(windows))]
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
                refresh_icon(&tray, theme, &state);
            }
            Event::UserEvent(UserEvent::HubExited {
                generation,
                summary,
            }) if Some(generation) == current_hub_gen(&state) => {
                state.embedded_hub = None;
                warn!(gen = generation, summary, "embedded hub exited on its own");
                update_hub_menu(&state, &start_hub_item, &stop_hub_item, &restart_hub_item);
            }
            Event::UserEvent(UserEvent::HubPeerCountChanged) => {
                update_hub_info_item(&hub_info_item, &state);
                refresh_tooltip(&tray, &state, &config_path);
            }
            Event::UserEvent(UserEvent::ThemeChanged(new_theme)) => {
                info!(?new_theme, "system theme changed; refreshing tray icon");
                theme = new_theme;
                refresh_icon(&tray, theme, &state);
            }
            Event::UserEvent(UserEvent::ClientStatusChanged { generation, status })
                if generation == state.supervisor_gen =>
            {
                state.client_status = Some(status);
                update_status_item(&status_item, &state);
                update_server_item(&server_item, &state);
                refresh_tooltip(&tray, &state, &config_path);
                refresh_icon(&tray, theme, &state);
            }
            Event::UserEvent(UserEvent::SettingsExited) => {
                info!("settings dialog closed; auto-reloading config");
                state.settings_alive = false;
                abort_running(&mut state);
                reload_config_into(&mut state, &config_path);
                apply_loaded_config(runtime.handle(), &proxy, &mut state, &config_path, &tray);
                update_hub_menu(&state, &start_hub_item, &stop_hub_item, &restart_hub_item);
                update_status_item(&status_item, &state);
                update_server_item(&server_item, &state);
                update_hub_info_item(&hub_info_item, &state);
                refresh_tooltip(&tray, &state, &config_path);
                refresh_icon(&tray, theme, &state);
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
    state.hub_bind_error = None;
    state.hub_stats = None;
    if let Some(p) = state.hub_stats_poller.take() {
        p.abort();
    }
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
    // Attach a live stats sink so the tooltip can surface the
    // connected-peer count. We keep our own clone in `state` and
    // hand the second clone to the hub task.
    let stats = HubStatsSink::new();
    server_cfg.stats = Some(stats.clone());

    let (listener, addr) = match handle.block_on(clipboardwire_core::server::bind(&server_cfg)) {
        Ok((l, a)) => l_and_a_clear(state, l, a),
        Err(e) => {
            // Visible failure: the user almost always hits this when
            // they have a second clipboardwire process running. Store
            // the message so the status item + tooltip can show it.
            let msg = format!("{} ({})", server_cfg.bind, friendly_bind_error(&e));
            state.hub_bind_error = Some(msg.clone());
            return Err(e).with_context(|| format!("binding embedded hub to {}", server_cfg.bind));
        }
    };
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

    // Background stats poller — fires HubPeerCountChanged on the tao
    // event loop whenever the connected-peer count shifts, so the
    // tooltip stays accurate without us having to interrupt the
    // hub task or contend on the inbox.
    let stats_for_poller = stats.clone();
    let proxy_for_stats = proxy.clone();
    let poller = handle.spawn(async move {
        let mut last = stats_for_poller.current();
        // Tell the tray the initial value once, then only on change.
        let _ = proxy_for_stats.send_event(UserEvent::HubPeerCountChanged);
        let mut tick = tokio::time::interval(HUB_STATS_POLL_INTERVAL);
        tick.tick().await; // skip the immediate first tick
        loop {
            tick.tick().await;
            let now = stats_for_poller.current();
            if now != last {
                last = now;
                let _ = proxy_for_stats.send_event(UserEvent::HubPeerCountChanged);
            }
        }
    });

    state.hub_stats = Some(stats);
    state.hub_stats_poller = Some(poller);
    Ok(())
}

fn stop_embedded_hub(state: &mut State) {
    if let Some(h) = state.embedded_hub.take() {
        h.abort();
        info!("embedded hub stopped");
    }
    state.hub_stats = None;
    if let Some(p) = state.hub_stats_poller.take() {
        p.abort();
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
    item.set_text(status_label_with_hub(
        state.client_status,
        state.cfg.is_some(),
        state.hub_bind_error.as_deref(),
    ));
}

/// Refresh the disabled server-URL menu item. Shows the server URL when
/// a config is loaded, hidden otherwise.
fn update_server_item(item: &MenuItem, state: &State) {
    item.set_text(server_label(state.cfg.as_ref()));
}

/// Render the server-URL label for the informational menu item.
fn server_label(cfg: Option<&ClientConfig>) -> String {
    match cfg {
        Some(c) => c.server.clone(),
        None => String::new(),
    }
}

/// Refresh the disabled hub-info menu item. Shows the connected-peer
/// count when the hub is running, empty otherwise. Tooltips are
/// unreliable on Linux, so this surfaces the same info in the menu.
fn update_hub_info_item(item: &MenuItem, state: &State) {
    let text = hub_info_label(state);
    // Hide the item when there's nothing to show (no hub running).
    if text.is_empty() {
        item.set_text("");
    } else {
        item.set_text(text);
    }
}

/// Render the hub-info label for the menu item.
fn hub_info_label(state: &State) -> String {
    if let Some(stats) = &state.hub_stats {
        let n = stats.current();
        match n {
            0 => "Hub: 0 clients connected".to_string(),
            1 => "Hub: 1 client connected".to_string(),
            n => format!("Hub: {n} clients connected"),
        }
    } else if state.embedded_hub.is_some() {
        "Hub: running".to_string()
    } else {
        String::new()
    }
}

/// Hot-swap the tray icon with one whose colored status dot matches
/// the current state. Called from every place that bumps status.
fn refresh_icon(tray: &TrayIcon, theme: Option<dark_light::Mode>, state: &State) {
    let icon = build_icon(theme, state.client_status, state.hub_bind_error.is_some());
    let _ = tray.set_icon(Some(icon));
}

/// Render the user-facing status label. Kept short + prefix-stable so
/// the menu doesn't reflow on every status change.
fn status_label_for(status: Option<ClientStatus>, has_cfg: bool) -> String {
    status_label_with_hub(status, has_cfg, None)
}

/// Variant of [`status_label_for`] that prioritises a hub-bind failure
/// when one is present — that's the user-actionable problem and they
/// shouldn't have to dig through logs to see it.
fn status_label_with_hub(
    status: Option<ClientStatus>,
    has_cfg: bool,
    hub_bind_error: Option<&str>,
) -> String {
    if let Some(err) = hub_bind_error {
        return format!("Status: hub bind failed — {err}");
    }
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

/// Helper used inside `start_embedded_hub_with_cfg` to clear the
/// "hub bind failed" sticky state once a fresh bind has succeeded.
fn l_and_a_clear<L, A>(state: &mut State, listener: L, addr: A) -> (L, A) {
    state.hub_bind_error = None;
    (listener, addr)
}

/// Best-effort: shorten the typical OS errors into a human phrase. The
/// "address already in use" case is the one we expect 95% of the time.
fn friendly_bind_error(e: &anyhow::Error) -> String {
    let s = format!("{e:#}");
    let lower = s.to_lowercase();
    if lower.contains("address already in use") || lower.contains("address in use") {
        "another process is already on this port".to_string()
    } else if lower.contains("permission denied") {
        "permission denied".to_string()
    } else {
        // Keep just the deepest message — anyhow's chain is noisy.
        s.lines().last().unwrap_or("bind failed").to_string()
    }
}

/// Refresh the tray tooltip from current state.
fn refresh_tooltip(tray: &TrayIcon, state: &State, config_path: &Path) {
    // Hub bind failure wins the tooltip — it's the actionable problem.
    if let Some(err) = &state.hub_bind_error {
        let _ = tray.set_tooltip(Some(format!(
            "clipboardwire — hub bind failed ({err})\n{}",
            config_path.display()
        )));
        return;
    }
    let mut tip = match (&state.cfg, state.client_status) {
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
    // Append the embedded-hub peer count when the hub is running.
    // Count includes our own loopback supervisor (the hub treats it
    // as a peer like any other), so a freshly-started embedded hub
    // with no remote peers shows "1 client".
    if let Some(stats) = &state.hub_stats {
        tip.push_str(&format_hub_peer_line(stats.current()));
    }
    let _ = tray.set_tooltip(Some(tip));
}

/// Render the per-peer-count suffix that gets appended to the
/// tooltip when the embedded hub is running. Pulled out for the
/// unit test below.
fn format_hub_peer_line(count: usize) -> String {
    match count {
        0 => "\nhub running (0 clients connected)".to_string(),
        1 => "\nhub running (1 client connected)".to_string(),
        n => format!("\nhub running ({n} clients connected)"),
    }
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
/// based on the user's current system theme and overlaying a small
/// status dot in the bottom-right corner.
fn build_icon(
    theme: Option<dark_light::Mode>,
    status: Option<ClientStatus>,
    hub_bind_error: bool,
) -> tray_icon::Icon {
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
    let mut rgba = img.to_rgba8();
    let (width, height) = rgba.dimensions();

    // Pick an overlay colour. Hub-bind failure dominates (the user
    // can't ignore it). Otherwise track the client status:
    // - green: connected
    // - amber: connecting (transient)
    // - red:   disconnected → retrying
    // - none:  no supervisor / starting up (don't add a dot — the bare
    //          icon is the right signal for "config-needed")
    let overlay = if hub_bind_error {
        Some([220u8, 60, 60, 255])
    } else {
        match status {
            Some(ClientStatus::Connected) => Some([60u8, 180, 80, 255]),
            Some(ClientStatus::Connecting) => Some([235u8, 175, 50, 255]),
            Some(ClientStatus::Disconnected { .. }) => Some([220u8, 60, 60, 255]),
            None => None,
        }
    };

    if let Some(colour) = overlay {
        draw_status_dot(&mut rgba, width, height, colour);
    }

    tray_icon::Icon::from_rgba(rgba.into_raw(), width, height).expect("tray-icon accepts RGBA")
}

/// Composite a filled circle (with a 1-px contrast halo) in the
/// bottom-right of the tray icon. Drawn directly into the RGBA buffer
/// to avoid pulling in an extra drawing crate.
fn draw_status_dot(rgba: &mut image::RgbaImage, width: u32, height: u32, color: [u8; 4]) {
    let r = (width.min(height) as i32) * 11 / 50; // ≈ 22% of the icon side
    let cx = width as i32 - r - 1;
    let cy = height as i32 - r - 1;
    let halo_r = r + 1;

    for y in 0..height as i32 {
        for x in 0..width as i32 {
            let dx = x - cx;
            let dy = y - cy;
            let d2 = dx * dx + dy * dy;
            if d2 <= r * r {
                rgba.put_pixel(x as u32, y as u32, image::Rgba(color));
            } else if d2 <= halo_r * halo_r {
                // 1-px translucent dark halo so the dot reads against
                // a light tray and the icon itself stays visible.
                rgba.put_pixel(x as u32, y as u32, image::Rgba([0, 0, 0, 200]));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hub_peer_line_pluralises() {
        assert_eq!(
            format_hub_peer_line(0),
            "\nhub running (0 clients connected)"
        );
        assert_eq!(
            format_hub_peer_line(1),
            "\nhub running (1 client connected)"
        );
        assert_eq!(
            format_hub_peer_line(7),
            "\nhub running (7 clients connected)"
        );
    }
}
