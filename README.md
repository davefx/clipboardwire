# clipboardwire

A from-scratch Rust clipboard-sync server and desktop client, inspired by the
[ClipCascade](https://github.com/Sathvik-Rao/ClipCascade) project but redesigned for
single-user, personal-use deployment on a trusted LAN or VPN. The goal is to replace
the upstream Spring Boot server (and its always-on JVM) with a small static binary.
TLS is built in (rustls) — point `clipboardwire serve` at a cert and key and it
speaks `wss://` directly; no reverse proxy required, though one still works fine
as a fallback.

The project ships **one** binary, `clipboardwire`, with three modes selected by
subcommand:

- **`clipboardwire serve`** — run as a relay hub only (headless / NAS / systemd).
- **`clipboardwire host`** — run as a hub *and* a local clipboard client on the
  same machine. Your always-on workstation becomes the network's bootstrap node;
  no separate server install needed.
- **`clipboardwire`** (or `clipboardwire connect`) — join an existing hub as a
  clipboard client. Default mode.

Distribution is via **native distro packages** — `.deb` for Debian/Ubuntu,
`.rpm` for Fedora/RHEL/openSUSE, a PKGBUILD for Arch, MSI / Homebrew formula for
Windows / macOS once those clients exist. No Docker image; the binary is small
enough to install directly.

> Status: **design phase**. No code yet. See `PROTOCOL.md` and `ARCHITECTURE.md`.

## Why this exists

The upstream project ships a Java/Spring Boot server. For a personal, single-user
deployment the JVM's resident memory cost (typically 150–300 MiB) and startup time are
disproportionate to what the workload actually is: forwarding small clipboard messages
between a handful of devices. A Rust port targets:

- single statically-linked binary, no runtime to install
- low idle RSS (target: under 10 MiB)
- fast cold start (well under a second)
- same operational shape as the Java server (Docker image + systemd unit)

## Scope

We are **not** trying to be a drop-in replacement for the upstream server. We control
both server and client code, so the wire protocol is redesigned for simplicity. See
`PROTOCOL.md` for the spec.

### Kept from the original

- WebSocket transport (push, low latency, NAT-friendly).
- Per-user "last clipboard" cache so a freshly-connected client gets the current value.

### Dropped from the original

| Dropped                              | Why                                                                                                                              |
| ------------------------------------ | -------------------------------------------------------------------------------------------------------------------------------- |
| STOMP framing                        | No good Rust broker; we control all clients.                                                                                     |
| Embedded/external ActiveMQ           | Same reason. In-memory fan-out is enough for one user.                                                                           |
| Separate P2P WebSocket mode          | Single endpoint covers our use case.                                                                                             |
| Spring Security + JDBC user store    | Single user, hard-coded via env vars.                                                                                            |
| Signup / login / logout HTML pages   | No browser-facing UI.                                                                                                            |
| Brute-force throttling + CAPTCHA     | Not exposed to the public internet; LAN or VPN only.                                                                             |
| Sessions / cookies                   | Each WebSocket carries HTTP Basic credentials.                                                                                   |
| Donation + system-info endpoints     | Not relevant.                                                                                                                    |
| Scheduled tasks / system maintenance | Nothing to schedule.                                                                                                             |
| Client-side AES E2EE                 | Threat model is LAN/VPN; TLS (built-in or via a reverse proxy) covers the wire. Server is in the trust boundary, which keeps the code minimal. |

### TLS

`clipboardwire serve` speaks `wss://` directly when given a cert and key via
`CLIPBOARDWIRE_TLS_CERT_FILE` / `CLIPBOARDWIRE_TLS_KEY_FILE`. The TLS stack is
`rustls` (pure Rust, no OpenSSL system dep). If you'd rather front it with a
reverse proxy (Caddy / nginx / Traefik) for cert management, leave those env
vars unset and the server will speak plain `ws://`.

**Quick self-signed cert (LAN/VPN only):**

```sh
openssl req -x509 -newkey rsa:2048 -nodes \
  -keyout key.pem -out cert.pem \
  -days 3650 -subj '/CN=clipboardwire.lan' \
  -addext 'subjectAltName=DNS:clipboardwire.lan,IP:192.168.1.10'
```

Each client then sets `tls_insecure = true` in its config (because rustls
refuses to treat a self-signed end-entity cert as both root *and* leaf — the
standard X.509 way is a CA + a separate server cert).

**Proper PKI (validates without `tls_insecure`):**

```sh
# Make a tiny CA (do this once)
openssl req -x509 -newkey rsa:2048 -nodes -days 3650 \
  -keyout ca.key -out ca.crt -subj '/CN=clipboardwire-ca'

# Make a server cert and sign it with the CA
openssl req -new -newkey rsa:2048 -nodes \
  -keyout server.key -out server.csr \
  -subj '/CN=clipboardwire.lan' \
  -addext 'subjectAltName=DNS:clipboardwire.lan,IP:192.168.1.10'
openssl x509 -req -in server.csr -CA ca.crt -CAkey ca.key \
  -CAcreateserial -out server.crt -days 3650 \
  -copy_extensions copyall
```

Server uses `server.crt` + `server.key`; clients set `tls_ca_file = "ca.crt"`
and full chain validation works.

## Repository layout

```
clipboardwire/
├── README.md               # this file
├── PROTOCOL.md             # wire protocol spec (auth, frames)
├── ARCHITECTURE.md         # Rust implementation plan
├── LICENSE                 # GPL-3.0-or-later
├── Cargo.toml              # workspace manifest
├── core/                   # clipboardwire-core library (protocol, hub, client)
│   ├── Cargo.toml
│   └── src/
├── cli/                    # clipboardwire binary (serve / host / connect)
│   ├── Cargo.toml
│   └── src/
└── packaging/              # native package recipes (deb, rpm, PKGBUILD, …)
```

## Roadmap

- **Phase 1 — `serve` subcommand.** Hub-only mode: `axum` + `tokio-tungstenite`,
  single-user Basic auth from env vars, in-memory fan-out, last-clip cache.
  Ships as a static binary plus `.deb` and `.rpm` packages with a systemd unit.
- **Phase 2 — `connect` and `host` subcommands on Linux.** Local clipboard
  polling via `arboard`; `host` runs hub and client in one process so a single
  workstation can bootstrap the network. Proves the protocol end-to-end.
- **Phase 3 — Windows build.** Same Rust codebase, cross-built from Linux
  via `x86_64-pc-windows-gnu`. Produces a 7 MiB stripped `.exe`, plus an
  `.msi` installer (cargo-wix on the GitHub Windows runner) that installs
  to `Program Files\clipboardwire\` and prepends the directory to system
  `PATH`. A Windows system-tray UI ships with the same binary — invoke
  `clipboardwire connect --tray` to run with a tray icon and a Quit menu.
  macOS likely falls out for free; Android/iOS out of scope for v0.1.

  GitHub Actions builds for both platforms on every push and attaches
  `.deb` / `.rpm` / `.exe` / `.msi` artifacts to tagged releases.

## Threat model (informal)

- **Trusted:** the user's devices, the server host itself (operator, disk, RAM).
- **Untrusted:** the network path between devices and the server. TLS
  (terminated in the server via `rustls`, or by a reverse proxy if you'd
  rather) is what protects clipboard contents in transit.
- **Implication:** anyone who can read the server's memory or disk can read
  clipboard contents while a sync is happening. This is acceptable for a LAN or
  VPN deployment on hardware the user controls; it is **not** suitable for a
  server hosted on untrusted infrastructure. If that ever changes, E2EE with
  TOFU device pairing is the right upgrade path (see `PROTOCOL.md` §4).
- **Out of scope:** denial-of-service, side channels on the client devices.

## License

**GPL-3.0-or-later.** A `LICENSE` file with the full GPLv3 text will be added with
the first code commit. This matches the upstream project's licensing and sidesteps
any derivative-work ambiguity, since the wire protocol and operational model are
informed by the upstream design even though no upstream code is copied.
