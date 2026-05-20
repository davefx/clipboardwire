// SPDX-License-Identifier: GPL-3.0-or-later

//! Client configuration loaded from a TOML file at
//! `~/.config/clipboardwire/config.toml` (or platform equivalent).
//!
//! Format:
//! ```toml
//! server   = "ws://nas.lan:8484/sync"
//! user     = "alice"
//! password = "hunter2"
//! poll_ms  = 300  # optional
//! ```

use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::protocol::MAX_FRAME_BYTES;
use crate::server::ServerConfig;

fn default_poll_ms() -> u64 {
    300
}

fn default_hub_bind() -> SocketAddr {
    "0.0.0.0:8484".parse().expect("static address")
}

fn default_hub_max_conns() -> usize {
    64
}

fn default_hub_max_frame() -> usize {
    MAX_FRAME_BYTES
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ClientConfig {
    /// WebSocket endpoint, e.g. `ws://nas.lan:8484/sync` or `wss://…`.
    pub server: String,
    /// HTTP Basic auth username.
    pub user: String,
    /// HTTP Basic auth password.
    pub password: String,
    /// Clipboard polling interval in milliseconds. Default 300.
    #[serde(default = "default_poll_ms")]
    pub poll_ms: u64,
    /// Optional PEM-encoded CA bundle. Certificates here are trusted *in
    /// addition to* the built-in Mozilla root list. Use this to trust a
    /// self-signed server certificate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls_ca_file: Option<std::path::PathBuf>,
    /// **DANGEROUS.** When `true`, the client skips TLS certificate
    /// validation entirely. Only safe on a fully trusted network (e.g.
    /// loopback in `host` mode). Default `false`.
    #[serde(default, skip_serializing_if = "is_false")]
    pub tls_insecure: bool,
    /// Optional `[hub]` section. When `hub.enabled = true`, launching
    /// `clipboardwire connect --tray` first binds a hub server in-process,
    /// then connects the local client to it over loopback. Useful when
    /// you want one machine to be both the relay and a clipboard
    /// participant without running two `clipboardwire` invocations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hub: Option<HubConfig>,
}

fn is_false(b: &bool) -> bool {
    !*b
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HubConfig {
    /// Whether to bring up a hub on this machine alongside the client.
    #[serde(default)]
    pub enabled: bool,
    /// `host:port` to bind to. Default `0.0.0.0:8484`.
    #[serde(default = "default_hub_bind")]
    pub bind: SocketAddr,
    /// HTTP Basic auth username for the hub. Other clients connect with
    /// this user; the local in-process client always passes auth so
    /// it doesn't need to match `ClientConfig::user`, but using the same
    /// credentials keeps the config simple.
    pub user: String,
    /// HTTP Basic auth password for the hub.
    pub password: String,
    /// Maximum concurrent connected clients.
    #[serde(default = "default_hub_max_conns")]
    pub max_conns: usize,
    /// Maximum WebSocket frame size, in bytes.
    #[serde(default = "default_hub_max_frame")]
    pub max_frame_bytes: usize,
    /// Optional PEM cert file. When set together with `tls_key_file`,
    /// the hub speaks `wss://`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls_cert_file: Option<PathBuf>,
    /// Optional PEM key file. Required iff `tls_cert_file` is set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls_key_file: Option<PathBuf>,
}

impl Default for HubConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            bind: default_hub_bind(),
            user: String::new(),
            password: String::new(),
            max_conns: default_hub_max_conns(),
            max_frame_bytes: default_hub_max_frame(),
            tls_cert_file: None,
            tls_key_file: None,
        }
    }
}

impl HubConfig {
    /// Convert this hub config into the [`ServerConfig`] the existing
    /// `server::bind` / `server::serve` helpers expect.
    pub fn to_server_config(&self) -> ServerConfig {
        ServerConfig {
            bind: self.bind,
            user: self.user.clone(),
            password: self.password.clone(),
            max_conns: self.max_conns,
            max_frame_bytes: self.max_frame_bytes,
            tls_cert_file: self.tls_cert_file.clone(),
            tls_key_file: self.tls_key_file.clone(),
        }
    }
}

impl ClientConfig {
    /// Load and validate a config from disk. Refuses to start if the file is
    /// readable by group or world on Unix (since the file holds a password).
    pub fn load(path: &Path) -> Result<Self> {
        check_perms(path)?;
        let raw =
            fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let cfg: ClientConfig =
            toml::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Platform-default path for the client config file:
    ///
    /// - Linux: `$XDG_CONFIG_HOME/clipboardwire/config.toml`
    ///   (falls back to `~/.config/clipboardwire/config.toml`)
    /// - macOS: `~/Library/Application Support/clipboardwire/config.toml`
    /// - Windows: `%APPDATA%\clipboardwire\config.toml`
    ///
    /// The Windows layout intentionally avoids `directories::ProjectDirs`'
    /// default nested `\config\` subdirectory — `%APPDATA%\clipboardwire\config\config.toml`
    /// reads as redundant and surprises Windows users. Linux/macOS keep
    /// their XDG-compliant locations via `BaseDirs::config_dir()`.
    pub fn default_path() -> Result<PathBuf> {
        #[cfg(windows)]
        {
            let appdata = std::env::var_os("APPDATA")
                .ok_or_else(|| anyhow!("APPDATA environment variable is not set"))?;
            Ok(PathBuf::from(appdata)
                .join("clipboardwire")
                .join("config.toml"))
        }
        #[cfg(not(windows))]
        {
            let base = directories::BaseDirs::new()
                .ok_or_else(|| anyhow!("could not locate the user's config directory"))?;
            Ok(base.config_dir().join("clipboardwire").join("config.toml"))
        }
    }

    /// Write a placeholder config to `path`, creating parent directories as
    /// needed. The file is `chmod 0600` on Unix and contains comments
    /// pointing at each setting. Used on first run to bootstrap a new
    /// install — see `cli/src/main.rs`.
    pub fn write_template(path: &Path) -> Result<()> {
        const TEMPLATE: &str = "# clipboardwire client config\n\
            # Edit this file with your hub's URL and credentials, then re-run.\n\
            \n\
            # WebSocket endpoint. Use wss:// when the hub has TLS configured.\n\
            server   = \"wss://CHANGE-ME-host.lan:8484/sync\"\n\
            \n\
            # HTTP Basic credentials, shared with the hub's\n\
            # CLIPBOARDWIRE_USER / CLIPBOARDWIRE_PASSWORD env vars.\n\
            user     = \"CHANGE-ME\"\n\
            password = \"CHANGE-ME\"\n\
            \n\
            # Optional: clipboard polling interval in milliseconds (default 300).\n\
            # poll_ms = 300\n\
            \n\
            # Optional: PEM bundle of extra CAs to trust (e.g. your private CA).\n\
            # tls_ca_file = \"C:\\\\path\\\\to\\\\ca.crt\"\n\
            \n\
            # Set to true only on a fully trusted LAN/VPN with a self-signed\n\
            # server cert. Skips all TLS certificate verification.\n\
            # tls_insecure = true\n";

        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        }
        fs::write(path, TEMPLATE)
            .with_context(|| format!("writing template to {}", path.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(path)
                .with_context(|| format!("stat {}", path.display()))?
                .permissions();
            perms.set_mode(0o600);
            fs::set_permissions(path, perms)
                .with_context(|| format!("chmod 600 {}", path.display()))?;
        }
        Ok(())
    }

    fn validate(&self) -> Result<()> {
        if !self.server.starts_with("ws://") && !self.server.starts_with("wss://") {
            bail!(
                "`server` must start with ws:// or wss:// (got `{}`)",
                self.server
            );
        }
        if self.user.is_empty() {
            bail!("`user` must not be empty");
        }
        if self.password.is_empty() {
            bail!("`password` must not be empty");
        }
        if self.poll_ms == 0 {
            bail!("`poll_ms` must be greater than 0");
        }
        Ok(())
    }
}

#[cfg(unix)]
fn check_perms(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let md = fs::metadata(path).with_context(|| format!("stat {}", path.display()))?;
    let mode = md.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        bail!(
            "config file {} is readable by group/other (mode {:o}); run `chmod 600 {}`",
            path.display(),
            mode,
            path.display()
        );
    }
    Ok(())
}

#[cfg(not(unix))]
fn check_perms(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_config(content: &str) -> PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "clipboardwire-test-cfg-{}-{}.toml",
            std::process::id(),
            // tiny disambiguator so parallel tests don't clobber each other
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::write(&path, content).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        }
        path
    }

    #[test]
    fn parses_minimal_config() {
        let path = write_config(
            r#"
            server = "ws://nas.lan:8484/sync"
            user = "alice"
            password = "hunter2"
        "#,
        );
        let cfg = ClientConfig::load(&path).unwrap();
        assert_eq!(cfg.server, "ws://nas.lan:8484/sync");
        assert_eq!(cfg.user, "alice");
        assert_eq!(cfg.password, "hunter2");
        assert_eq!(cfg.poll_ms, 300);
        fs::remove_file(&path).ok();
    }

    #[test]
    fn parses_poll_ms_override() {
        let path = write_config(
            r#"
            server = "wss://example.com/sync"
            user = "u"
            password = "p"
            poll_ms = 100
        "#,
        );
        let cfg = ClientConfig::load(&path).unwrap();
        assert_eq!(cfg.poll_ms, 100);
        fs::remove_file(&path).ok();
    }

    #[test]
    fn rejects_unknown_fields() {
        let path = write_config(
            r#"
            server = "ws://localhost/sync"
            user = "u"
            password = "p"
            mystery = "nope"
        "#,
        );
        let err = ClientConfig::load(&path).unwrap_err();
        assert!(format!("{err:#}").to_lowercase().contains("mystery"));
        fs::remove_file(&path).ok();
    }

    #[test]
    fn rejects_non_ws_scheme() {
        let path = write_config(
            r#"
            server = "http://localhost/sync"
            user = "u"
            password = "p"
        "#,
        );
        let err = ClientConfig::load(&path).unwrap_err();
        assert!(format!("{err:#}").contains("ws://"));
        fs::remove_file(&path).ok();
    }

    #[test]
    fn rejects_empty_password() {
        let path = write_config(
            r#"
            server = "ws://localhost/sync"
            user = "u"
            password = ""
        "#,
        );
        let err = ClientConfig::load(&path).unwrap_err();
        assert!(format!("{err:#}").contains("password"));
        fs::remove_file(&path).ok();
    }

    #[test]
    fn write_template_creates_parent_and_file() {
        let dir = std::env::temp_dir().join(format!(
            "clipboardwire-template-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let path = dir.join("nested").join("config.toml");
        let _ = fs::remove_dir_all(&dir);

        ClientConfig::write_template(&path).expect("template write");
        assert!(path.exists());
        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains("CHANGE-ME"));
        assert!(content.contains("server"));

        // The template itself should parse minus the CHANGE-ME placeholders —
        // i.e. it's syntactically valid TOML that would just fail validation.
        let parsed: Result<ClientConfig, _> = toml::from_str(&content);
        assert!(parsed.is_ok(), "template should be valid TOML");

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    #[cfg(unix)]
    fn write_template_sets_0600_mode() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!(
            "clipboardwire-template-mode-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let path = dir.join("config.toml");
        let _ = fs::remove_dir_all(&dir);

        ClientConfig::write_template(&path).unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "template should be chmod 600");

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    #[cfg(unix)]
    fn rejects_world_readable_config() {
        use std::os::unix::fs::PermissionsExt;
        let path = write_config(
            r#"
            server = "ws://localhost/sync"
            user = "u"
            password = "p"
        "#,
        );
        // Loosen permissions to 644.
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        let err = ClientConfig::load(&path).unwrap_err();
        assert!(format!("{err:#}").contains("chmod 600"));
        fs::remove_file(&path).ok();
    }
}
