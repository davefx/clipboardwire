# Manual smoke checklist

This is a per-release hand-driven test of the GUI bits the automated
suite doesn't cover (right-clicking a tray icon and verifying its menu
opens isn't realistically automatable across all our platforms — see the
`Tier 3+ tests` note at the bottom).

Run this before tagging any release that touches:

- `cli/src/tray.rs`
- `cli/src/main.rs`'s tray dispatch
- `cli/wix/main.wxs` (Windows installer)
- `tray-icon` or `tao` dep versions
- The Linux GTK / Windows subsystem build deps

Each platform takes ~2 minutes. **Failing any item is release-blocking.**

## Windows

1. **Fresh install.** Uninstall any previous version; install the new
   MSI. Accept all defaults (Start Menu shortcut, Desktop shortcut,
   autostart all enabled).
2. **No console window on launch.** Sign out, sign back in. The
   autostart entry should bring up the tray icon **silently** — no
   `cmd.exe` / PowerShell window should appear anywhere.
3. **Tray icon appears** in the notification area (or in the overflow
   menu, depending on Windows config).
4. **Tooltip on hover** reads either:
   - `clipboardwire — wss://...` (a config from a previous install
     survived the upgrade), or
   - `clipboardwire — needs config (right-click → Edit config)…`
     (true fresh install).
5. **Right-click on the tray icon** opens a context menu with three
   items:
   - Edit config…
   - Reload config
   - Quit clipboardwire
6. **Click "Edit config…"** opens `%APPDATA%\clipboardwire\config.toml`
   in Notepad. On a fresh install the file already contains the
   `CHANGE-ME` placeholder template; on an upgrade it contains your
   existing config unchanged.
7. **Fill in real values and save.** Close Notepad.
8. **Click "Reload config"** in the tray menu. Tooltip should change
   to `clipboardwire — wss://...` (or `ws://`).
9. **Click "Quit clipboardwire"**. The icon disappears within a
   second; Task Manager confirms no leftover process.
10. **PowerShell sanity:** `clipboardwire connect` (no `--tray`) from
    a PowerShell window with an unwritable config still shows the log
    output **in that PowerShell window** (the `AttachConsole` shim).
    Press Ctrl-C; clean exit.

## Linux (Cinnamon / GNOME / KDE / XFCE)

1. Install the `.deb` (Debian/Ubuntu/Mint) or `.rpm` (Fedora/RHEL).
2. **systemd service starts:** `sudo systemctl enable --now clipboardwire`,
   then `systemctl status clipboardwire` shows `active (running)` (for
   the hub mode; only relevant on the hub host).
3. **Client tray:** from a terminal, run `clipboardwire connect --tray`.
4. **Tray icon appears** in the system tray.
   - GNOME without a tray extension: this is expected to be invisible.
     Install `AppIndicator and KStatusNotifierItem Support` first.
5. **Right-click opens the menu** with three items (same as Windows).
6. **Click "Edit config…"** opens `~/.config/clipboardwire/config.toml`
   in your default text editor (via `xdg-open`).
7. Fill in real values, save, close the editor.
8. **Click "Reload config"**. Tooltip changes; supervisor reconnects.
9. **Click "Quit clipboardwire"**. Process exits cleanly.

## macOS

Not officially supported in v0.2.x. Skip; revisit when a macOS build
pipeline lands.

## End-to-end clipboard sync (any platform pair)

After the per-platform checklist passes:

1. Run the hub somewhere (`clipboardwire serve` on a NAS, or
   `clipboardwire host` on one machine).
2. Connect at least two clipboard clients (any combination of platforms).
3. **Text:** copy text on machine A → paste on machine B. Same text.
4. **Image:** copy a small image (e.g. screenshot, a tiny PNG from a
   web page) on A → paste on B. Same image. (v0.2.0+ only.)

## What this checklist intentionally doesn't try to automate

- **Right-click → menu open.** Each platform's tray icon is dispatched
  by a private OS API (Win32 Shell_NotifyIcon, libayatana-appindicator
  via DBus, macOS NSStatusItem). Programmatically driving them across
  platforms is a multi-day per-platform investment with brittle
  per-OS-release maintenance.
- **Notification-area icon visibility under Wayland.** Depends on the
  user's compositor + extensions. Out of scope.

If you want deeper test coverage at the cost of more dev time:

- **Linux DBus interaction harness** is planned for v0.2.3 — should
  give us proper menu-existence and menu-activation assertions on
  Linux at least.
- **Windows UI Automation harness** is planned for v0.2.4.
