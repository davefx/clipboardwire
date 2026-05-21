# Changelog

All notable changes to clipboardwire are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project
follows [Semantic Versioning](https://semver.org).

## [0.4.0] — 2026-05-21

### Added
- **Tray icon status overlay.** Colored dot in the bottom-right of the
  tray icon mirrors the menu's `Status: …` line — green = connected,
  amber = connecting, red = disconnected / hub-bind failure, none =
  needs config. Composited into the existing 32 px PNG at runtime
  via `image`; no new asset files.
- **"Pin from server" button** in the Settings dialog. One click does
  a sync TLS handshake to the configured server URL, captures the
  peer's end-entity certificate, writes it as PEM to
  `<config_dir>/server-pinned.crt`, and points `tls_ca_file` at the
  saved file. Removes the need to copy the cert manually or set
  `tls_insecure = true`.
- **Settings window icon.** eframe's ViewportBuilder now `.with_icon`
  and `.with_app_id` so the title bar / taskbar entry / alt-tab
  thumbnail all match the tray + .desktop launcher.
- **macOS .app bundle + .dmg.** Release workflow assembles a proper
  `clipboardwire.app` (Info.plist + .icns generated from the bundled
  PNG, LSUIElement=true so it lives in the menu bar) and an
  installer-style `.dmg` with an `/Applications` drag target. Also
  ships the raw `.app` as a `.tar.gz` and keeps the previous
  universal / per-arch binaries.

### Notes
- File-clipboard support is deferred to v0.5 — needs per-OS adapters
  (X11 `text/uri-list`, Wayland `wl_data_device`, Windows `CF_HDROP`,
  macOS `NSPasteboardTypeFileURL`) plus a protocol bump and a
  streaming wire format for the typical sizes. The smaller v0.4
  items ship first.

## [0.3.3] — 2026-05-21

### Added
- **macOS release artifacts.** Universal binary (Apple Silicon + Intel)
  plus per-arch builds attached to every tagged release. `.app` /
  `.dmg` bundling deferred to v0.4.
- README rewrite for public-launch readiness: install commands for
  every platform, quick-start walkthrough, configuration reference,
  architecture diagram, threat model summary.
- `CHANGELOG.md` and `SECURITY.md`.

### Changed
- macOS added to the CI matrix so build regressions are caught before
  they land on a release tag.

## [0.3.2] — 2026-05-20

### Added
- Tray shows live client connection status in the menu (**Status: connecting…**,
  **Status: connected**, **Status: disconnected — retrying in N s**) and reflects
  it in the tray tooltip. Backed by a new `ClientStatus` watch channel
  emitted by the transport.
- **Per-user singleton lock** for tray-mode launches. Duplicate launches no
  longer race for the hub port; the second process writes a clear stderr
  diagnostic and exits cleanly. This was the root cause of the
  "client cycles connect/disconnect every few seconds" symptom that some
  users hit after upgrading with a stale older process still around.
- Hub-bind failures (port already in use, wedged lock file, etc.) now
  surface in the tray's **Status:** line and tooltip instead of being
  hidden in the log.
- Linux desktop integration: `clipboardwire.desktop` in
  `/usr/share/applications/` + a 256-px hicolor icon (so it shows up in
  GNOME's Settings → Apps and the launcher), and
  `/etc/xdg/autostart/clipboardwire.desktop` with `Autostart-enabled=false`
  so it's discoverable in Startup Applications without auto-launching
  until the user opts in.
- Tier-1 integration test (`cli/tests/singleton_lock.rs`) that spawns
  two `clipboardwire --tray` processes against a sandboxed config dir
  and asserts the second exits with the expected stderr.

### Changed
- Minimum supported Rust raised to **1.89** (for the now-stable
  `std::fs::File::try_lock` used by the singleton check — no extra dep).

## [0.3.1] — 2026-05-20

### Added
- **Self-signed TLS auto-generation.** When the hub has no cert files set
  (and `tls_disabled = false`), it generates a self-signed cert via
  `rcgen` with sensible SANs (`localhost`, `127.0.0.1`, `::1`, the bind
  IP, and the machine hostname), saves it under
  `<state_dir>/self-signed.{crt,key}`, and reuses it across restarts.
  The SHA-256 fingerprint is logged on first generation for client-side
  pinning.
- `clipboardwire serve` now reads the client config TOML's `[hub]`
  section as its base, with env vars overriding. NAS-style installs
  can drop the systemd `Environment=` block.
- **Tray is the default for desktop launches.** Running `clipboardwire`
  with no subcommand opens the tray, same as the Start Menu shortcut.
- **First-run UX:** when no usable config exists on disk, the tray
  auto-spawns the Settings GUI on startup so the user has somewhere
  obvious to fill in server / user / password.
- Tray menu adds **Start / Stop / Restart hub** items, enabled only
  when the loaded config has a `[hub]` section.
- Settings-window close auto-reloads the config and respawns the
  supervisor + hub.

### Changed (breaking)
- Existing v0.3.0 clients pointed at `ws://…` and connecting to a v0.3.1
  hub will see TLS errors, because the hub now defaults to `wss://`.
  Either set `tls_disabled = true` in the hub config to keep plain
  `ws://`, or switch the client URL to `wss://` and set
  `tls_insecure = true` (or pin the cert via `tls_ca_file`).

## [0.3.0] — 2026-05-20

### Added
- **Settings dialog.** Right-click → *Edit config…* now opens a real
  eframe-based GUI for editing the client TOML (server URL, user,
  password, TLS settings, hub toggle). Replaces the v0.2 fallback of
  "open the file in Notepad."
- **Embedded-hub mode.** Clients with `[hub] enabled = true` bring up
  an in-process hub on the tray binary so a single install can both
  host the sync server and join it. No separate `clipboardwire serve`
  needed for the local node.
- **Theme-aware tray icons.** Detect light / dark system theme at
  startup and pick the matching mono-dark, mono-light, or color icon.
- Extensive interaction tests:
  - Linux DBus harness that exercises the menu (Edit / Reload / Quit)
    via a mock StatusNotifierWatcher.
  - Windows UI Automation harness that discovers the tray icon
    through the Win 11 "Show Hidden Icons" overflow popup.
  - `egui_kittest` tests for the Settings dialog widget tree.

## [0.2.1] — 2026-05

### Added
- First-run template config bootstrap. On a new install the tray
  writes a placeholder `config.toml` and opens it for editing
  instead of failing with "file not found."
- Tier-1 tray smoke test and `MANUAL_SMOKE.md` checklist for
  interactive bits CI can't cover.

## [0.2.0] — 2026-05

### Added
- **PROTOCOL v0.2:** new `content_b64` field on clip frames carries
  base64'd binary payloads.
- **Image clipboard support** (PNG over the wire).
- **Cross-platform tray** — Linux (GTK + libayatana-appindicator)
  and macOS in addition to Windows.

## [0.1.1] — 2026-05

### Fixed
- MSI now installs Start Menu, Desktop, and AutoStart shortcuts (v0.1.0
  was a silent install — the binary was on PATH but unfindable from the
  GUI).
- Windows config path no longer doubles up the `clipboardwire/` folder
  (`%APPDATA%\clipboardwire\config.toml` instead of
  `%APPDATA%\clipboardwire\config\config.toml`).

## [0.1.0] — 2026-05

### Added
- Initial release: `clipboardwire serve / connect / host`, built-in
  TLS via `rustls`, native `.deb` / `.rpm` / `.msi` packages, GitHub
  Actions CI matrix on Linux + Windows.

[0.4.0]: https://github.com/davefx/clipboardwire/releases/tag/v0.4.0
[0.3.3]: https://github.com/davefx/clipboardwire/releases/tag/v0.3.3
[0.3.2]: https://github.com/davefx/clipboardwire/releases/tag/v0.3.2
[0.3.1]: https://github.com/davefx/clipboardwire/releases/tag/v0.3.1
[0.3.0]: https://github.com/davefx/clipboardwire/releases/tag/v0.3.0
[0.2.1]: https://github.com/davefx/clipboardwire/releases/tag/v0.2.1
[0.2.0]: https://github.com/davefx/clipboardwire/releases/tag/v0.2.0
[0.1.1]: https://github.com/davefx/clipboardwire/releases/tag/v0.1.1
[0.1.0]: https://github.com/davefx/clipboardwire/releases/tag/v0.1.0
