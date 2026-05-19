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
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;

fn default_poll_ms() -> u64 {
    300
}

#[derive(Debug, Clone, Deserialize)]
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

    /// Platform-default path. On Linux: `$XDG_CONFIG_HOME/clipboardwire/config.toml`
    /// or `~/.config/clipboardwire/config.toml`.
    pub fn default_path() -> Result<PathBuf> {
        let dirs = directories::ProjectDirs::from("", "", "clipboardwire")
            .ok_or_else(|| anyhow!("could not locate the user's config directory"))?;
        Ok(dirs.config_dir().join("config.toml"))
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
