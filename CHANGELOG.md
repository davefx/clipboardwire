# Changelog

All notable changes to clipboardwire are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project
follows [Semantic Versioning](https://semver.org).

## [0.5.0] — 2026-06-01

### Added
- **Android client.** Native Kotlin + Jetpack Compose app with OkHttp
  WebSocket, foreground service for background sync, DataStore
  preferences, boot-start receiver, and exponential-backoff reconnect.
  The APK is now attached to every GitHub release for sideloading.
- **Android ↔ Rust server integration tests.** A new CI job builds the
  Rust hub, starts it, spins up an Android emulator, and verifies the
  full clip relay flow end-to-end (welcome, multi-client fan-out,
  auth rejection). 35 Android tests total (23 JVM + 12 instrumented).
- **Release workflow builds the Android APK** and attaches it to the
  GitHub release automatically.
- **Release notes auto-populated from CHANGELOG.md** — the release
  workflow extracts the matching version section and fills the GitHub
  release body.
- **`scripts/release.sh` bumps Android version** — versionName and
  auto-incremented versionCode are now updated alongside Cargo.toml.

### Fixed
- **Android cleartext ws:// connections now work to any IP.** The
  previous network security config used `<domain>` entries for private
  IP ranges, but Android treats those as literal hostnames — so
  `ws://192.168.1.50` would silently fail. Switched to
  `base-config cleartextTrafficPermitted=true` since this is a
  LAN-first tool where users configure arbitrary IPs.

## [0.4.6] — 2026-05-27

### Fixed
- **Windows client no longer disconnects every ~60 seconds.** The
  client was not sending WebSocket pings, relying entirely on the
  server's pings and the auto-pong response. On Windows, power-managed
  network adapters can delay or drop a pong just long enough to trip
  the server's read timeout. Fixed by adding client-side pings every
  30 s (matching the server) and raising the server's read timeout
  from 45 s to 90 s so a single missed pong doesn't kill the
  connection.

## [0.4.5] — 2026-05-27

### Added
- **Persistent file logging.** All tracing output now goes to both
  stderr and a daily-rotating log file under the config directory
  (`~/.config/clipboardwire/clipboardwire.log.YYYY-MM-DD` on Linux).
  If the process disappears unexpectedly, the log file shows what
  happened last.
- **Panic hook with crash file.** Panics are caught by a custom hook
  that writes the panic message plus a full backtrace to
  `~/.config/clipboardwire/crash.log` before aborting. This survives
  even if the tracing layer hasn't flushed yet.

## [0.4.4] — 2026-05-26

### Changed
- **Hub logs now include peer IP address.** Every WebSocket connection
  log line (opened, closed, errors) includes the client's IP:port in
  the tracing span via axum's `ConnectInfo` extractor, making it easy
  to identify which device is connecting or disconnecting.

## [0.4.3] — 2026-05-25

### Changed
- **Tray menu now shows server URL and hub peer count** as disabled
  info lines below the status item. Linux tray icons (libayatana-
  appindicator) don't support tooltips, so the server address and
  hub connection count are now always visible in the right-click
  menu. The tooltip is still maintained for platforms that support it.
- **Smaller status-dot overlay on the tray icon.** Reduced from ~35%
  to ~22% of the icon side so the actual icon artwork is more
  visible while the colored connection-status dot still reads clearly.

## [0.4.2] — 2026-05-24

### Added
- Tray tooltip now shows the connected-peer count when the embedded
  hub is running. Lines like `hub running (3 clients connected)`
  appear below the existing status / server line so you can see at a
  glance how many of your devices are wired into the local hub. The
  count includes our own loopback supervisor (the hub treats it as
  a peer like any other), so a freshly-started embedded hub with no
  remote peers reads `1 client connected`.

  Backed by a new `HubStatsSink` (Arc&lt;AtomicUsize&gt;) the hub task
  updates on every register/deregister, plugged into `ServerConfig`
  by the tray. A 1-second poller posts a `HubPeerCountChanged`
  event on count shifts; the tooltip rebuilds on each.

## [0.4.1] — 2026-05-21

### Added
- **Multi-file Ctrl+C now arrives as a single clipboard set.**
  Receiver-side debounce: file completions accumulate for 500 ms,
  then the whole batch is applied to the OS clipboard at once. A
  `Ctrl+C` of 5 files on the sender ends up with all 5 ready to
  paste on the receiver, not just the last one to finish
  transferring (the v0.4.0 known limitation).
- **Tray icon live-reloads on system theme change.** A 5-second
  background poll re-runs `dark_light::detect()`; if the value
  flips, the icon is rebuilt and swapped in place. No more stuck
  mono-dark icon after a manual light-mode toggle mid-session.

## [0.4.0] — 2026-05-21

### Added
- **File transfer (PROTOCOL v0.3.0).** New `file_chunk` frame type
  carries 4 MiB pieces of a single file; the receiver assembles them
  into a temp file, verifies the SHA-256 carried in every chunk, and
  moves the finished file into `~/Downloads/clipboardwire/`. Wire-
  format only for v0.4 — OS clipboard integration (drag files from
  Explorer/Finder/Nautilus) follows in a later release.
- **`clipboardwire send <FILE>` subcommand.** One-shot: opens a fresh
  connection to the configured hub, publishes the file in chunks,
  exits. Receivers running with the tray (or any client whose
  supervisor is live) save the file under their configured downloads
  dir automatically.
- **OS file-clipboard integration.** Copy a file in the file manager
  (Ctrl+C) on the sender, and the receiver pastes it (Ctrl+V) in its
  own file manager — the received file lands in
  `~/Downloads/clipboardwire/` and the OS clipboard is set to that
  path so paste-into-Explorer/Nautilus/Finder works. Backends:
  - Linux X11: `text/uri-list` selection target via `x11-clipboard`.
  - Windows: `CF_HDROP` via `clipboard-win`.
  - macOS: stub for now (use the `send` CLI command on macOS;
    `NSPasteboardTypeFileURL` follows in a v0.4.x patch).
  - Linux Wayland: Xwayland fallback covers most setups; native
    `wl_data_device` follows.
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
- **Homebrew tap** ([davefx/homebrew-clipboardwire](https://github.com/davefx/homebrew-clipboardwire)).
  `brew install --cask davefx/clipboardwire/clipboardwire` does the
  DMG drag-into-Applications dance automatically. A GitHub Action
  in this repo bumps the cask on every release.
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

[0.5.0]: https://github.com/davefx/clipboardwire/releases/tag/v0.5.0
[0.4.6]: https://github.com/davefx/clipboardwire/releases/tag/v0.4.6
[0.4.5]: https://github.com/davefx/clipboardwire/releases/tag/v0.4.5
[0.4.4]: https://github.com/davefx/clipboardwire/releases/tag/v0.4.4
[0.4.3]: https://github.com/davefx/clipboardwire/releases/tag/v0.4.3
[0.4.2]: https://github.com/davefx/clipboardwire/releases/tag/v0.4.2
[0.4.1]: https://github.com/davefx/clipboardwire/releases/tag/v0.4.1
[0.4.0]: https://github.com/davefx/clipboardwire/releases/tag/v0.4.0
[0.3.3]: https://github.com/davefx/clipboardwire/releases/tag/v0.3.3
[0.3.2]: https://github.com/davefx/clipboardwire/releases/tag/v0.3.2
[0.3.1]: https://github.com/davefx/clipboardwire/releases/tag/v0.3.1
[0.3.0]: https://github.com/davefx/clipboardwire/releases/tag/v0.3.0
[0.2.1]: https://github.com/davefx/clipboardwire/releases/tag/v0.2.1
[0.2.0]: https://github.com/davefx/clipboardwire/releases/tag/v0.2.0
[0.1.1]: https://github.com/davefx/clipboardwire/releases/tag/v0.1.1
[0.1.0]: https://github.com/davefx/clipboardwire/releases/tag/v0.1.0
