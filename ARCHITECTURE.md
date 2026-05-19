# clipboardwire — implementation architecture

Companion to `PROTOCOL.md`. This document covers the **Rust implementation** of
the server and the desktop client: how the code is laid out, which crates are
used and why, and the deployment shape. The protocol itself is language-agnostic
and lives in `PROTOCOL.md`.

## 1. Cargo workspace

A single workspace with one library crate and one binary crate:

```
clipboardwire/
├── Cargo.toml                  # [workspace] members = ["core", "cli"]
├── core/
│   ├── Cargo.toml              # name = "clipboardwire-core" (library)
│   └── src/
├── cli/
│   ├── Cargo.toml              # name = "clipboardwire" (binary)
│   └── src/
└── packaging/                  # cargo-deb metadata, rpm spec, PKGBUILD, systemd unit
```

- **`clipboardwire-core`** owns *all* protocol and runtime code: frame types,
  the hub task, the WebSocket server endpoint, the clipboard poll loop, the
  client supervisor. No `main`.
- **`clipboardwire`** is a thin binary that uses `clap` to dispatch one of three
  subcommands into the library:
  - `serve` → start hub only (env-var config).
  - `host` → start hub *and* a local client connecting to it on loopback (env
    vars for the hub + config file for the client).
  - `connect` (default) → start client only (config file).

The `host` mode deliberately uses real WebSocket-on-loopback rather than an
in-memory shortcut. It keeps every code path the same as a multi-machine
deployment, costing only a few μs of localhost overhead per frame.

Rust edition: **2021**. MSRV: latest stable; no nightly features. Workspace
pins for shared deps (`tokio`, `serde`, `serde_json`, `tracing`, `uuid`, `rand`).

## 2. Hub server (`clipboardwire-core::server`)

The server-side code lives in `clipboardwire-core` and is invoked by the CLI's
`serve` subcommand (or as one half of `host`).

### 2.1 Module layout

```
core/src/
├── lib.rs           # re-exports
├── protocol.rs      # ClipFrame, WelcomeFrame, ErrorFrame, ClientId, version const
├── server/
│   ├── mod.rs       # `pub async fn run(config: ServerConfig) -> Result<()>`
│   ├── config.rs    # ServerConfig struct + env parser
│   ├── auth.rs      # parse Authorization header, constant-time compare
│   ├── hub.rs       # central fan-out hub + last_clip cache
│   ├── ws.rs        # /sync WebSocket handler
│   └── error.rs     # AppError, IntoResponse impl
└── client/          # see §3
```

### 2.2 Dependencies (rationale)

| Crate                 | Purpose                                    | Why this one                                                                                       |
| --------------------- | ------------------------------------------ | -------------------------------------------------------------------------------------------------- |
| `axum` 0.7            | HTTP routing + WebSocket upgrade           | Best-supported Rust HTTP framework; built-in WS extractor, plays well with tokio.                  |
| `tokio` (full)        | Async runtime                              | The only realistic choice for an axum + tungstenite stack.                                         |
| `tokio-tungstenite`   | Pulled in transitively by axum WS         | Not used directly.                                                                                 |
| `serde` / `serde_json`| Frame (de)serialization                    | Standard.                                                                                          |
| `tracing` + `tracing-subscriber` | Logging                          | Async-aware structured logging.                                                                    |
| `uuid`                | Generate per-connection client ids         | v4.                                                                                                |
| `subtle`              | Constant-time comparison for Basic auth    | Resists timing oracles.                                                                            |
| `tower-http`          | Request limits, optional access log layer  | Plugs into axum.                                                                                   |
| `anyhow` / `thiserror`| Error plumbing                             | `thiserror` for typed library errors, `anyhow` only at the top of `main`.                          |

TLS comes from `axum-server` with the `tls-rustls` feature, plus `rustls`
and `rustls-pemfile` for cert loading. **Not** used: any database crate, any
session/cookie crate, any templating engine, any application-layer
cryptography crate (no E2EE in v0.1). The server has zero persistent state.

### 2.3 Concurrency model

One **hub task** owns the canonical state:

```rust
struct Hub {
    clients: HashMap<ClientId, mpsc::Sender<ClipFrame>>,
    last_clip: Option<ClipFrame>,
}
```

The hub is driven by a single inbound `mpsc::Receiver<HubMessage>` where
`HubMessage` is one of `{ Register, Deregister, Publish }`. Each WebSocket
connection task:

1. Performs the auth + upgrade in `ws.rs`.
2. Sends `Register { client_id, tx }` to the hub and awaits the snapshot
   `last_clip` so it can emit the `welcome` frame.
3. Spawns a *reader* task that pulls WS frames and forwards parsed `clip`s as
   `Publish { from, frame }` to the hub.
4. Spawns a *writer* task that pulls from its mpsc and writes to the WS sink.
5. On disconnect, sends `Deregister { client_id }`.

Single-owner design keeps `last_clip` consistent without locks. The hub never
blocks: each per-client mpsc has a small bounded buffer (e.g. 32); if a slow
peer fills it, the hub drops the frame for that one peer (preferring liveness
over delivery — clipboard is last-write-wins anyway).

### 2.4 Configuration

All config from environment variables, no config file:

| Variable                    | Default                          | Notes                                  |
| --------------------------- | -------------------------------- | -------------------------------------- |
| `CLIPBOARDWIRE_BIND`        | `0.0.0.0:8484`                   | `host:port` to listen on               |
| `CLIPBOARDWIRE_USER`        | *required*                       | Basic-auth username                    |
| `CLIPBOARDWIRE_PASSWORD`    | *required\**                     | Basic-auth password.                    |
| `CLIPBOARDWIRE_PASSWORD_FILE` | unset                          | Path to a file containing the password (trailing newline trimmed). Mutually exclusive with `CLIPBOARDWIRE_PASSWORD`; one of the two must be set. Docker-secret friendly. |
| `CLIPBOARDWIRE_MAX_CONNS`   | `64`                             | Cap (see `PROTOCOL.md` §5)             |
| `CLIPBOARDWIRE_MAX_FRAME`   | `10485760`                       | Bytes                                  |
| `CLIPBOARDWIRE_TLS_CERT_FILE` | unset                          | PEM cert file. When set together with `CLIPBOARDWIRE_TLS_KEY_FILE`, the server speaks `wss://`. |
| `CLIPBOARDWIRE_TLS_KEY_FILE`  | unset                          | PEM private key file. Required iff `CLIPBOARDWIRE_TLS_CERT_FILE` is set. |
| `RUST_LOG`                  | `clipboardwire_server=info`      | `tracing-subscriber` env filter        |

Refusing to start without `USER`/`PASSWORD` is intentional — no "default
admin/admin".

### 2.5 Deployment

- **Static binary.** Build with `--release`. Final size: ~7 MiB stripped on
  Linux, ~7 MiB on Windows (rustls + arboard + axum included).
- **Native distro packages.** Built via `cargo-deb` (Debian, Ubuntu, Mint),
  `cargo-generate-rpm` (Fedora, RHEL, openSUSE) and a hand-written `PKGBUILD`
  for Arch. Each ships the binary plus a `clipboardwire.service` systemd unit
  with `ExecStart=/usr/bin/clipboardwire serve`, `DynamicUser=yes`,
  `ProtectSystem=strict`, `ProtectHome=yes`, `NoNewPrivileges=yes`, and a
  `/etc/clipboardwire/clipboardwire.env` for the user/password env vars.
  Recipes live in `packaging/`.
- **Windows.** Cross-compiled from Linux via the `x86_64-pc-windows-gnu`
  target. Produces a single ~7 MiB stripped `.exe` with no DLL dependencies
  beyond what ships with Windows. Local cross-build:
  ```sh
  rustup target add x86_64-pc-windows-gnu
  sudo apt install -y gcc-mingw-w64-x86-64   # one-time
  cargo build --release --target x86_64-pc-windows-gnu
  ```
  An MSI installer is produced by `cargo wix` on the windows-latest CI
  runner; the WiX 3 source lives in `cli/wix/main.wxs`. The MSI installs to
  `%ProgramFiles%\clipboardwire\`, drops a license file alongside the
  binary, and prepends that directory to the system `PATH`. The
  `UpgradeCode` is stable so future versions upgrade in place.
- **macOS.** Expected to fall out of the same source tree (`arboard` and the
  axum stack both target macOS); will be wired up alongside the macOS client
  phase.
- **No Docker image.** The binary is small enough that direct install via a
  distro package is the supported deployment path.

## 3. Clipboard client (`clipboardwire-core::client`)

The client-side code lives in `clipboardwire-core` and is invoked by the CLI's
`connect` subcommand (or as the other half of `host`). No GUI in v0.1; runs as
a foreground process that logs to stderr and exits non-zero on fatal errors.
Tray UI is a Phase-3+ follow-up.

### 3.1 Module layout

```
core/src/client/
├── mod.rs           # `pub async fn run(config: ClientConfig) -> Result<()>`
├── config.rs        # ClientConfig struct + TOML loader
├── transport.rs     # WS connect, auth, reconnect with backoff, framed I/O
└── clipboard.rs     # arboard wrapper, polling loop, dedup of self-set values
```

### 3.2 Dependencies (rationale)

| Crate                  | Purpose                                  | Why this one                                                       |
| ---------------------- | ---------------------------------------- | ------------------------------------------------------------------ |
| `tokio` (full)         | Async runtime                            | Symmetry with the server.                                          |
| `tokio-tungstenite`    | WebSocket client                         | Mature, idiomatic, works on all our targets.                       |
| `http`                 | Build the `Authorization` header         | Used by tungstenite handshake.                                     |
| `arboard`              | Read/write the platform clipboard        | Single API across Linux (X11/Wayland), Windows, macOS.             |
| `rand`                 | UUID randomness, jitter                  | CSPRNG via `OsRng`.                                                |
| `uuid`                 | Generate frame ids                       | v4.                                                                |
| `serde` / `serde_json` | Frame (de)serialization                  | Same shape as server.                                              |
| `toml`                 | Config                                   | Light, deterministic, friendly for hand-editing.                   |
| `clap` (derive)        | CLI parsing                              | `--config`, `--server`, `--user`, etc.                             |
| `tracing` + `tracing-subscriber` | Logging                        | Same stack as server.                                              |
| `directories`          | Locate `~/.config/clipboardwire/`        | XDG/AppData-aware across platforms.                                |
| `rustls` + `rustls-pemfile` + `webpki-roots` | `wss://` support       | Custom root store for self-signed certs, plus an opt-in "skip verification" verifier for trusted-LAN deployments. |
| `anyhow` / `thiserror` | Error plumbing                           | Same pattern.                                                      |

### 3.3 Config file

`~/.config/clipboardwire/config.toml` (or platform equivalent):

```toml
# WebSocket endpoint; use wss:// when fronted by a TLS proxy.
server = "ws://nas.lan:8484/sync"

# HTTP Basic credentials shared with the server.
user     = "alice"
password = "..."

# Optional: polling interval for clipboard changes.
poll_ms = 300

# Optional: TLS trust configuration.
# tls_ca_file  = "/etc/clipboardwire/ca.crt"   # extra root CA to trust
# tls_insecure = false                          # set to true on LAN/VPN only
```

Permissions: client refuses to start if the file is world-readable on Unix
(mode `0o077`-clean check). The Basic-auth password is the only secret in
the file; OS-keyring integration is a possible Phase-3 follow-up.

### 3.4 Clipboard plumbing

`arboard` is a polling API: the client samples the clipboard at `poll_ms`
intervals. Two failure modes to handle:

- **Inbound echo (from the server).** The hub never echoes our own publishes
  back — `PROTOCOL.md` §3.1 stamps every relayed frame with `from`, and the
  hub skips the originating connection during fan-out. The client confirms
  this defensively by dropping any frame where `from == self.client_id` (paranoia
  against future server bugs).
- **Outbound echo (from our own write).** When we apply an incoming clip, the
  next clipboard poll will read it back. Fix: hash the value we just wrote and
  suppress the next matching poll result.
- **Read flap during a copy.** On some platforms `arboard` briefly fails while
  another app holds the clipboard. Treat transient errors as "no change".

Pseudocode:

```rust
let mut last_seen: Option<Hash> = None;
loop {
    interval.tick().await;
    let Ok(text) = clipboard.get_text() else { continue };
    let h = blake3::hash(text.as_bytes());
    if Some(h) == last_seen { continue }
    last_seen = Some(h);
    if Some(h) == self_set_hash { continue }  // we wrote this
    publish(text).await?;
}
```

For incoming frames the writer task does the symmetric thing:

```rust
let text = &frame.content;
let h = blake3::hash(text.as_bytes());
self_set_hash = Some(h);
clipboard.set_text(text)?;
last_seen = Some(h);  // suppress immediate poll echo
```

(`blake3` is a candidate hash dep; `siphash` or even a `u64` of `xxhash` works
too. Settle on one in implementation.)

### 3.5 Platform notes

- **Linux/X11.** `arboard` works. Note that X11 clipboard data lives in the app
  that copied it; if that app exits, the clipboard goes empty. Acceptable for
  our use case — we only need to *observe* changes while we run.
- **Linux/Wayland.** `arboard` uses Wayland's clipboard protocols (when built
  with the relevant feature). Polling works on most compositors, but some
  restrict clipboard access to focused windows, which prevents true background
  polling. Document as a known limitation; a "headless companion" foreground
  window is a possible workaround.
- **Windows.** `arboard` uses the Win32 clipboard API. Polling is fine. A future
  improvement is `AddClipboardFormatListener` for push events, but it requires
  a real HWND.
- **macOS.** `arboard` uses NSPasteboard. Polling against `changeCount` is
  cheap. Should work without changes.

### 3.6 Tray UI (Windows)

A thin Windows-only wrapper around the headless supervisor, gated by
`#[cfg(windows)]` in `cli/src/tray.rs`. Builds a `tray_icon::TrayIcon` with
a placeholder icon (drawn programmatically — no asset shipped) and a
single "Quit" menu item, then spawns `client::run` as a tokio task and
multiplexes three sources via `tokio::select!`:

1. The supervisor's `JoinHandle` (returns when the client exits).
2. `MenuEvent::receiver()` (polled every 100 ms — the receiver is a
   `crossbeam_channel` so we can't await it directly).
3. `tokio::signal::ctrl_c()` for clean shutdown from a terminal.

The Linux build sees `cfg(not(windows))` for the module; the `--tray` flag
falls through to the headless code path with a one-line warning. Adding a
cross-platform tray would mean either splitting the Linux distro package
in two (server-only vs. desktop) or forcing a GTK dep on headless servers
— deferred to v0.2.

### 3.7 Supervisor

`client::run` owns a small supervisor:

1. The clipboard poll task.
2. The transport task (connect → run reader+writer → exit on error/close).
3. A `JoinSet` so one task's failure tears the rest down.

On transport exit it sleeps the backoff (`PROTOCOL.md` §2.5) and retries. The
clipboard task keeps running across reconnects; outbound frames are dropped
while disconnected (clipboard is last-write-wins, no queueing needed).

## 4. The `clipboardwire` binary (`cli/`)

A thin wrapper around the library. `cli/src/main.rs` parses arguments with
`clap` (derive macros) into one of three subcommands:

```
clipboardwire serve           # hub only
clipboardwire host            # hub + local client (same process)
clipboardwire connect         # client only (also the default if no subcommand)
clipboardwire [global flags]  # --config <path>, --log-level <lvl>
```

Each subcommand:

- `serve` — load `ServerConfig` from env vars; call `core::server::run(cfg)`.
- `connect` — load `ClientConfig` from `--config` or the platform default path;
  call `core::client::run(cfg)`.
- `host` — load both. Spawn `core::server::run(server_cfg)` and, after the
  listener is up, spawn `core::client::run(client_cfg)` pointed at
  `ws://127.0.0.1:<bound_port>/sync`. Use a `JoinSet`; if either task exits,
  shut the other down and exit the process.

### 4.1 CLI dependencies

| Crate                            | Purpose                  |
| -------------------------------- | ------------------------ |
| `clap` (derive)                  | subcommand routing       |
| `tokio` (full)                   | runtime                  |
| `tracing` + `tracing-subscriber` | logging setup            |
| `anyhow`                         | top-level error plumbing |
| `clipboardwire-core` (path dep)  | everything substantive   |

## 5. Cross-cutting concerns

### 5.1 Tests

- **Server unit tests** — hub register/publish/deregister, last-clip cache,
  frame size enforcement, auth parsing.
- **Server integration test** — spin up the axum app with `tower::ServiceExt`
  and drive a fake WebSocket pair.
- **Client unit tests** — config parsing, echo-loop suppression logic,
  frame (de)serialization round-trip.
- **End-to-end test** (gated behind a feature flag because it needs a real
  network port) — server + two clients on loopback, assert clip flows A→B.

### 5.2 CI

Two GitHub Actions workflows live in `.github/workflows/`:

- **`ci.yml`** — runs on every push to `main` and on pull requests.
  - `lint` job (ubuntu): `cargo fmt --all -- --check` and
    `cargo clippy --workspace --all-targets -- -D warnings`.
  - `test` job (matrix: `ubuntu-latest`, `windows-latest`):
    `cargo test --workspace`.
- **`release.yml`** — runs on tagged pushes (`v*`).
  - Linux job builds the release binary, then `cargo deb` and
    `cargo generate-rpm`, and uploads `.deb` / `.rpm` / raw binary to the
    GitHub release.
  - Windows job builds the release `.exe` and uploads it.

Both workflows use `Swatinem/rust-cache` for compile-time caching.

### 5.3 Observability

- `tracing` everywhere with INFO default. Each WebSocket connection gets a
  `client_id` span; every published frame logs `id`, `len`, `from` at DEBUG.
- The server logs frame *metadata* (id, length, content_type, from) at DEBUG and
  the `content` itself only at TRACE — operators who don't want clipboard text
  in their logs can simply leave `RUST_LOG` at INFO (the default).
- A `/healthz` endpoint on the server returns 200 OK with a tiny body for
  Docker healthchecks.

## 6. Build & run targets (cheatsheet)

```sh
# Workspace build
cargo build --release

# Hub only (headless / Docker target)
CLIPBOARDWIRE_USER=alice CLIPBOARDWIRE_PASSWORD=hunter2 \
  ./target/release/clipboardwire serve

# Hub + local clipboard client (one machine bootstrapping the network)
CLIPBOARDWIRE_USER=alice CLIPBOARDWIRE_PASSWORD=hunter2 \
  ./target/release/clipboardwire host \
    --config ~/.config/clipboardwire/config.toml

# Client connecting to a remote hub
./target/release/clipboardwire connect \
  --config ~/.config/clipboardwire/config.toml
# (or just `./target/release/clipboardwire` — `connect` is the default)

# Native packages (deb / rpm)
cargo deb -p clipboardwire                                  # → target/debian/*.deb
cargo generate-rpm -p clipboardwire                         # → target/generate-rpm/*.rpm
# After install:
sudo systemctl enable --now clipboardwire
```

## 7. Out of scope for v0.1 (intentionally)

- Cross-platform tray UI on Linux/macOS (Windows ships in v0.1; Linux deferred
  due to libgtk/libappindicator runtime deps that would either split the
  Linux package or force a GTK dep on headless servers).
- Image / file clipboard payloads (protocol bump required).
- Multi-user / per-room separation.
- TLS in the server (use a reverse proxy).
- Application-layer encryption / TOFU device pairing (see `PROTOCOL.md` §4).
- OS keyring integration for storing the Basic-auth password.
- Persistent `last_clip` across restarts.
