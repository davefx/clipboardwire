// SPDX-License-Identifier: GPL-3.0-or-later

//! Server configuration loaded from environment variables.
//!
//! Variable layout is documented in `ARCHITECTURE.md` §2.4.

use std::env;
use std::fs;
use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::{anyhow, bail, Context, Result};

use crate::protocol::MAX_FRAME_BYTES;

const DEFAULT_BIND: &str = "0.0.0.0:8484";
const DEFAULT_MAX_CONNS: usize = 64;
const DEFAULT_PING_INTERVAL_SECS: u64 = 30;
const DEFAULT_READ_TIMEOUT_SECS: u64 = 90;

#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub bind: SocketAddr,
    pub user: String,
    pub password: String,
    pub max_conns: usize,
    pub max_frame_bytes: usize,
    /// PEM-encoded certificate chain. If `Some`, the server speaks `wss://`.
    pub tls_cert_file: Option<PathBuf>,
    /// PEM-encoded private key. Required if `tls_cert_file` is set.
    pub tls_key_file: Option<PathBuf>,
    /// Disable TLS entirely — serve plain `ws://` even when no cert is set.
    /// Without this flag and without explicit cert paths, the hub
    /// auto-generates a self-signed cert under `state_dir` (see below).
    pub tls_disabled: bool,
    /// Directory for state the hub persists across restarts (currently
    /// just the auto-generated self-signed TLS cert + key). If `None`,
    /// the auto-gen code falls back to a platform data dir.
    pub state_dir: Option<PathBuf>,
    /// Optional live counter the hub task updates on every client
    /// register/deregister. Embedded-hub callers (the tray) clone this
    /// into the config to surface the connected-peers count in the
    /// tray tooltip; standalone `serve` users leave it `None`.
    pub stats: Option<crate::server::hub::HubStatsSink>,
    /// WebSocket ping interval in seconds. Lower values detect dead
    /// connections faster; higher values reduce CPU wakeups and save
    /// battery on embedded/mobile deployments. Default: 30.
    pub ping_interval_secs: u64,
    /// WebSocket read timeout in seconds. A connection with no inbound
    /// frames (including pongs) for this long is considered dead.
    /// Should be at least 2-3x `ping_interval_secs`. Default: 90.
    pub read_timeout_secs: u64,
}

impl ServerConfig {
    /// Returns `true` if TLS is configured with explicit cert files.
    /// Auto-generated certs flow through a separate code path in
    /// [`crate::server::serve`] that wraps the config and routes
    /// through the TLS path — they don't flip this flag.
    pub fn tls_enabled(&self) -> bool {
        self.tls_cert_file.is_some() && self.tls_key_file.is_some()
    }
}

impl ServerConfig {
    /// Read all `CLIPBOARDWIRE_*` env vars and produce a validated config.
    /// Errors are intentionally surfaced verbatim — there is no "default
    /// admin/admin" fallback.
    pub fn from_env() -> Result<Self> {
        let bind = env::var("CLIPBOARDWIRE_BIND")
            .unwrap_or_else(|_| DEFAULT_BIND.to_string())
            .parse::<SocketAddr>()
            .context("CLIPBOARDWIRE_BIND must be a host:port")?;

        let user = env::var("CLIPBOARDWIRE_USER")
            .map_err(|_| anyhow!("CLIPBOARDWIRE_USER is required"))?;
        if user.is_empty() {
            bail!("CLIPBOARDWIRE_USER must not be empty");
        }

        let password = resolve_password()?;
        if password.is_empty() {
            bail!("password must not be empty");
        }

        let max_conns = parse_env_usize("CLIPBOARDWIRE_MAX_CONNS")?.unwrap_or(DEFAULT_MAX_CONNS);
        if max_conns == 0 {
            bail!("CLIPBOARDWIRE_MAX_CONNS must be at least 1");
        }

        let max_frame_bytes =
            parse_env_usize("CLIPBOARDWIRE_MAX_FRAME")?.unwrap_or(MAX_FRAME_BYTES);
        if max_frame_bytes == 0 {
            bail!("CLIPBOARDWIRE_MAX_FRAME must be at least 1");
        }

        let tls_cert_file = env::var("CLIPBOARDWIRE_TLS_CERT_FILE")
            .ok()
            .map(PathBuf::from);
        let tls_key_file = env::var("CLIPBOARDWIRE_TLS_KEY_FILE")
            .ok()
            .map(PathBuf::from);
        if tls_cert_file.is_some() != tls_key_file.is_some() {
            bail!(
                "CLIPBOARDWIRE_TLS_CERT_FILE and CLIPBOARDWIRE_TLS_KEY_FILE must be set together"
            );
        }

        let tls_disabled = env_bool("CLIPBOARDWIRE_TLS_DISABLE")?;
        let state_dir = env::var("CLIPBOARDWIRE_STATE_DIR")
            .ok()
            .filter(|s| !s.is_empty())
            .map(PathBuf::from);

        let ping_interval_secs = parse_env_u64("CLIPBOARDWIRE_PING_INTERVAL")?
            .unwrap_or(DEFAULT_PING_INTERVAL_SECS);
        let read_timeout_secs = parse_env_u64("CLIPBOARDWIRE_READ_TIMEOUT")?
            .unwrap_or(DEFAULT_READ_TIMEOUT_SECS);

        Ok(Self {
            bind,
            user,
            password,
            max_conns,
            max_frame_bytes,
            tls_cert_file,
            tls_key_file,
            tls_disabled,
            state_dir,
            stats: None,
            ping_interval_secs,
            read_timeout_secs,
        })
    }

    /// Like [`Self::from_env`] but with an optional base (typically from
    /// a TOML config) whose fields are used wherever the corresponding
    /// env var is unset. Env always wins; TOML fills gaps; defaults fill
    /// what neither provided.
    ///
    /// `clipboardwire serve` calls this so a NAS-grade install can keep
    /// its settings in `~/.config/clipboardwire/config.toml` instead of
    /// a systemd-unit Environment= block, while still letting an
    /// operator override one field via an env var.
    pub fn from_env_layered(base: Option<Self>) -> Result<Self> {
        let Some(base) = base else {
            return Self::from_env();
        };

        let bind = match env::var("CLIPBOARDWIRE_BIND") {
            Ok(s) => s
                .parse::<SocketAddr>()
                .context("CLIPBOARDWIRE_BIND must be a host:port")?,
            Err(_) => base.bind,
        };
        let user = match env::var("CLIPBOARDWIRE_USER") {
            Ok(u) if !u.is_empty() => u,
            Ok(_) => bail!("CLIPBOARDWIRE_USER must not be empty"),
            Err(_) => base.user,
        };
        let password = match (
            env::var("CLIPBOARDWIRE_PASSWORD").ok(),
            env::var("CLIPBOARDWIRE_PASSWORD_FILE").ok(),
        ) {
            (Some(_), Some(_)) => {
                bail!("set exactly one of CLIPBOARDWIRE_PASSWORD or CLIPBOARDWIRE_PASSWORD_FILE")
            }
            (Some(p), None) => p,
            (None, Some(p)) => {
                let raw = fs::read_to_string(&p).with_context(|| format!("reading {p}"))?;
                raw.trim_end_matches(['\r', '\n']).to_string()
            }
            (None, None) => base.password,
        };
        if password.is_empty() {
            bail!("password must not be empty");
        }
        let max_conns = parse_env_usize("CLIPBOARDWIRE_MAX_CONNS")?.unwrap_or(base.max_conns);
        if max_conns == 0 {
            bail!("CLIPBOARDWIRE_MAX_CONNS must be at least 1");
        }
        let max_frame_bytes =
            parse_env_usize("CLIPBOARDWIRE_MAX_FRAME")?.unwrap_or(base.max_frame_bytes);
        if max_frame_bytes == 0 {
            bail!("CLIPBOARDWIRE_MAX_FRAME must be at least 1");
        }
        let cert_env = env::var("CLIPBOARDWIRE_TLS_CERT_FILE")
            .ok()
            .map(PathBuf::from);
        let key_env = env::var("CLIPBOARDWIRE_TLS_KEY_FILE")
            .ok()
            .map(PathBuf::from);
        let (tls_cert_file, tls_key_file) = match (cert_env, key_env) {
            (None, None) => (base.tls_cert_file, base.tls_key_file),
            (Some(c), Some(k)) => (Some(c), Some(k)),
            _ => bail!(
                "CLIPBOARDWIRE_TLS_CERT_FILE and CLIPBOARDWIRE_TLS_KEY_FILE must be set together \
                 when overriding TLS via env"
            ),
        };
        let tls_disabled = match env::var("CLIPBOARDWIRE_TLS_DISABLE") {
            Ok(_) => env_bool("CLIPBOARDWIRE_TLS_DISABLE")?,
            Err(_) => base.tls_disabled,
        };
        let state_dir = env::var("CLIPBOARDWIRE_STATE_DIR")
            .ok()
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .or(base.state_dir);

        let ping_interval_secs = parse_env_u64("CLIPBOARDWIRE_PING_INTERVAL")?
            .unwrap_or(base.ping_interval_secs);
        let read_timeout_secs = parse_env_u64("CLIPBOARDWIRE_READ_TIMEOUT")?
            .unwrap_or(base.read_timeout_secs);

        Ok(Self {
            bind,
            user,
            password,
            max_conns,
            max_frame_bytes,
            tls_cert_file,
            tls_key_file,
            tls_disabled,
            state_dir,
            stats: None,
            ping_interval_secs,
            read_timeout_secs,
        })
    }
}

fn env_bool(name: &str) -> Result<bool> {
    match env::var(name) {
        Ok(s) => match s.trim().to_ascii_lowercase().as_str() {
            "" | "0" | "false" | "no" | "off" => Ok(false),
            "1" | "true" | "yes" | "on" => Ok(true),
            other => bail!("{name}: expected a boolean, got `{other}`"),
        },
        Err(_) => Ok(false),
    }
}

fn resolve_password() -> Result<String> {
    let inline = env::var("CLIPBOARDWIRE_PASSWORD").ok();
    let file = env::var("CLIPBOARDWIRE_PASSWORD_FILE")
        .ok()
        .map(PathBuf::from);
    match (inline, file) {
        (Some(_), Some(_)) => {
            bail!("set exactly one of CLIPBOARDWIRE_PASSWORD or CLIPBOARDWIRE_PASSWORD_FILE")
        }
        (Some(p), None) => Ok(p),
        (None, Some(p)) => {
            let raw = fs::read_to_string(&p).with_context(|| format!("reading {}", p.display()))?;
            // Trim trailing newlines but keep meaningful trailing whitespace
            // out of scope — Docker-secret files are typically `printf` or
            // here-doc, which both produce a single trailing newline.
            Ok(raw.trim_end_matches(['\r', '\n']).to_string())
        }
        (None, None) => bail!("set CLIPBOARDWIRE_PASSWORD or CLIPBOARDWIRE_PASSWORD_FILE"),
    }
}

fn parse_env_u64(name: &str) -> Result<Option<u64>> {
    match env::var(name) {
        Ok(s) => s
            .parse::<u64>()
            .map(Some)
            .with_context(|| format!("{name} must be a non-negative integer")),
        Err(env::VarError::NotPresent) => Ok(None),
        Err(e) => Err(anyhow!("{name}: {e}")),
    }
}

fn parse_env_usize(name: &str) -> Result<Option<usize>> {
    match env::var(name) {
        Ok(s) => s
            .parse::<usize>()
            .map(Some)
            .with_context(|| format!("{name} must be a non-negative integer")),
        Err(env::VarError::NotPresent) => Ok(None),
        Err(e) => Err(anyhow!("{name}: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // Env vars are process-global, so tests that mutate them must serialize.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn clear_env() {
        // SAFETY: tests are serialized by ENV_LOCK; no concurrent threads
        // can be reading the environment while we mutate it.
        unsafe {
            for v in [
                "CLIPBOARDWIRE_BIND",
                "CLIPBOARDWIRE_USER",
                "CLIPBOARDWIRE_PASSWORD",
                "CLIPBOARDWIRE_PASSWORD_FILE",
                "CLIPBOARDWIRE_MAX_CONNS",
                "CLIPBOARDWIRE_MAX_FRAME",
                "CLIPBOARDWIRE_TLS_CERT_FILE",
                "CLIPBOARDWIRE_TLS_KEY_FILE",
                "CLIPBOARDWIRE_PING_INTERVAL",
                "CLIPBOARDWIRE_READ_TIMEOUT",
            ] {
                env::remove_var(v);
            }
        }
    }

    fn set(k: &str, v: &str) {
        // SAFETY: see clear_env.
        unsafe { env::set_var(k, v) }
    }

    #[test]
    fn rejects_missing_user() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_env();
        set("CLIPBOARDWIRE_PASSWORD", "pw");
        let err = ServerConfig::from_env().unwrap_err();
        assert!(format!("{err}").contains("USER is required"));
    }

    #[test]
    fn rejects_missing_password() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_env();
        set("CLIPBOARDWIRE_USER", "alice");
        let err = ServerConfig::from_env().unwrap_err();
        assert!(format!("{err}").contains("CLIPBOARDWIRE_PASSWORD"));
    }

    #[test]
    fn rejects_both_password_sources() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_env();
        set("CLIPBOARDWIRE_USER", "alice");
        set("CLIPBOARDWIRE_PASSWORD", "pw");
        set("CLIPBOARDWIRE_PASSWORD_FILE", "/tmp/pw");
        let err = ServerConfig::from_env().unwrap_err();
        assert!(format!("{err}").contains("exactly one"));
    }

    #[test]
    fn reads_password_from_file_and_trims_newline() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_env();
        let mut p = std::env::temp_dir();
        p.push(format!("clipboardwire-test-pw-{}", std::process::id()));
        std::fs::write(&p, "secret\n").unwrap();
        set("CLIPBOARDWIRE_USER", "alice");
        set("CLIPBOARDWIRE_PASSWORD_FILE", p.to_str().unwrap());
        let cfg = ServerConfig::from_env().unwrap();
        assert_eq!(cfg.password, "secret");
        std::fs::remove_file(&p).unwrap();
    }

    #[test]
    fn defaults_are_applied() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_env();
        set("CLIPBOARDWIRE_USER", "alice");
        set("CLIPBOARDWIRE_PASSWORD", "pw");
        let cfg = ServerConfig::from_env().unwrap();
        assert_eq!(cfg.bind.to_string(), "0.0.0.0:8484");
        assert_eq!(cfg.max_conns, 64);
        assert_eq!(cfg.max_frame_bytes, MAX_FRAME_BYTES);
    }

    #[test]
    fn tls_disabled_by_default() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_env();
        set("CLIPBOARDWIRE_USER", "alice");
        set("CLIPBOARDWIRE_PASSWORD", "pw");
        let cfg = ServerConfig::from_env().unwrap();
        assert!(!cfg.tls_enabled());
        assert!(cfg.tls_cert_file.is_none());
        assert!(cfg.tls_key_file.is_none());
    }

    #[test]
    fn tls_enabled_when_both_files_set() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_env();
        set("CLIPBOARDWIRE_USER", "alice");
        set("CLIPBOARDWIRE_PASSWORD", "pw");
        set("CLIPBOARDWIRE_TLS_CERT_FILE", "/etc/clipboardwire/cert.pem");
        set("CLIPBOARDWIRE_TLS_KEY_FILE", "/etc/clipboardwire/key.pem");
        let cfg = ServerConfig::from_env().unwrap();
        assert!(cfg.tls_enabled());
    }

    #[test]
    fn tls_requires_both_cert_and_key() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_env();
        set("CLIPBOARDWIRE_USER", "alice");
        set("CLIPBOARDWIRE_PASSWORD", "pw");
        set("CLIPBOARDWIRE_TLS_CERT_FILE", "/etc/clipboardwire/cert.pem");
        let err = ServerConfig::from_env().unwrap_err();
        assert!(format!("{err}").contains("must be set together"));
    }

    #[test]
    fn invalid_bind_address_is_rejected() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_env();
        set("CLIPBOARDWIRE_USER", "alice");
        set("CLIPBOARDWIRE_PASSWORD", "pw");
        set("CLIPBOARDWIRE_BIND", "not-an-addr");
        let err = ServerConfig::from_env().unwrap_err();
        assert!(format!("{err}").contains("CLIPBOARDWIRE_BIND"));
    }
}
