// SPDX-License-Identifier: GPL-3.0-or-later

//! Windows-only system-tray UI wrapper around the headless client supervisor.
//!
//! Runs `clipboardwire_core::client::run` on the current tokio runtime while
//! displaying a tray icon with a tooltip and a "Quit" menu item. On menu
//! click or Ctrl-C the supervisor is aborted and the tray icon is dropped.
//!
//! Cross-platform tray UX is deferred to v0.2; on Linux/macOS the `--tray`
//! flag falls through to the headless code path with a one-line warning.

#![cfg(windows)]

use anyhow::Result;
use clipboardwire_core::client::ClientConfig;
use tracing::{info, warn};
use tray_icon::menu::{Menu, MenuEvent, MenuItem};
use tray_icon::{Icon, TrayIconBuilder};

/// Run the client supervisor with a Windows tray icon visible.
///
/// Returns when the user picks "Quit" from the menu, when Ctrl-C is pressed,
/// or when the supervisor exits with an unrecoverable error.
pub async fn run(cfg: ClientConfig) -> Result<()> {
    let icon = build_icon();
    let menu = Menu::new();
    let quit = MenuItem::new("Quit clipboardwire", true, None);
    menu.append(&quit)?;

    let tooltip = format!("clipboardwire — {}", cfg.server);
    let _tray = TrayIconBuilder::new()
        .with_icon(icon)
        .with_menu(Box::new(menu))
        .with_tooltip(tooltip)
        .build()?;

    info!("tray icon shown");

    let mut client_task = tokio::spawn(clipboardwire_core::client::run(cfg));
    let menu_rx = MenuEvent::receiver();
    let quit_id = quit.id().clone();

    let mut menu_poll = tokio::time::interval(std::time::Duration::from_millis(100));
    menu_poll.tick().await;

    loop {
        tokio::select! {
            res = &mut client_task => {
                return res
                    .map_err(|e| anyhow::anyhow!("client task panicked: {e}"))?
                    .map_err(|e| anyhow::anyhow!("client supervisor exited: {e:#}"));
            }
            _ = tokio::signal::ctrl_c() => {
                info!("ctrl-c received; shutting down");
                client_task.abort();
                return Ok(());
            }
            _ = menu_poll.tick() => {
                while let Ok(event) = menu_rx.try_recv() {
                    if event.id == quit_id {
                        info!("Quit menu item clicked");
                        client_task.abort();
                        return Ok(());
                    } else {
                        warn!(id = ?event.id, "ignoring unknown menu event");
                    }
                }
            }
        }
    }
}

/// 32×32 RGBA placeholder icon: a solid blue square with a small white square
/// inside, drawn programmatically so we don't ship an image asset.
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
