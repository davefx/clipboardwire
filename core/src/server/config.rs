// SPDX-License-Identifier: GPL-3.0-or-later

//! Server configuration loaded from environment variables.
//!
//! Variable layout is documented in `ARCHITECTURE.md` §2.4.

use std::env;
use std::fs;
use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::{Context, Result, anyhow, bail};

use crate::protocol::MAX_FRAME_BYTES;

const DEFAULT_BIND: &str = "0.0.0.0:8484";
const DEFAULT_MAX_CONNS: usize = 64;

#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub bind: SocketAddr,
    pub user: String,
    pub password: String,
    pub max_conns: usize,
    pub max_frame_bytes: usize,
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

        Ok(Self {
            bind,
            user,
            password,
            max_conns,
            max_frame_bytes,
        })
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
            let raw = fs::read_to_string(&p)
                .with_context(|| format!("reading {}", p.display()))?;
            // Trim trailing newlines but keep meaningful trailing whitespace
            // out of scope — Docker-secret files are typically `printf` or
            // here-doc, which both produce a single trailing newline.
            Ok(raw.trim_end_matches(['\r', '\n']).to_string())
        }
        (None, None) => bail!("set CLIPBOARDWIRE_PASSWORD or CLIPBOARDWIRE_PASSWORD_FILE"),
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
