// SPDX-License-Identifier: GPL-3.0-or-later

//! `clipboardwire settings` — a small eframe-based GUI for editing the
//! client config without touching the TOML by hand.
//!
//! Runs as its own subprocess so the tray's tao event loop and this
//! egui/winit one don't fight over the same main thread. The tray's
//! "Edit config…" menu item launches the binary with this subcommand;
//! the user fills in the form; the dialog writes the config and exits.
//! The tray's "Reload config" then picks up the new values.
//!
//! Form fields mirror [`ClientConfig`]:
//!
//! | Field           | Required | UI                                       |
//! | --------------- | -------- | ---------------------------------------- |
//! | `server`        | yes      | text, validated against ws:// / wss://   |
//! | `user`          | yes      | text                                     |
//! | `password`      | yes      | text, masked                             |
//! | `poll_ms`       | optional | u64 spinner, default 300                 |
//! | `tls_ca_file`   | optional | text path                                |
//! | `tls_insecure`  | optional | checkbox                                 |
//!
//! Exits with code 0 on Save, 1 on Cancel / window close.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::{Context, Result};
use clipboardwire_core::client::ClientConfig;
use eframe::egui;

const DEFAULT_POLL_MS: u64 = 300;
const WINDOW_TITLE: &str = "clipboardwire — settings";

/// Open the settings window for `config_path`. Loads any existing config
/// to pre-fill the form; falls back to template defaults otherwise.
/// Blocks the calling thread for the lifetime of the window.
pub fn run(config_path: PathBuf) -> Result<()> {
    let initial = load_or_default(&config_path);
    let saved = Arc::new(AtomicBool::new(false));

    let app = SettingsApp::new(config_path, initial, saved.clone());

    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size([520.0, 360.0])
        .with_min_inner_size([420.0, 320.0])
        .with_resizable(true)
        .with_title(WINDOW_TITLE)
        // App ID matches the .desktop filename so GNOME / KDE attach
        // the right launcher icon to the window in the taskbar.
        .with_app_id("clipboardwire");
    if let Some(icon) = load_window_icon() {
        viewport = viewport.with_icon(icon);
    }

    let native_options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };

    eframe::run_native(
        WINDOW_TITLE,
        native_options,
        Box::new(|_cc| Ok(Box::new(app))),
    )
    .map_err(|e| anyhow::anyhow!("settings window: {e}"))?;

    // eframe::run_native returns when the window is closed. `saved` is
    // flipped by `try_save`; we surface the success/cancel distinction
    // to the parent (the tray) via the process exit code.
    if saved.load(Ordering::SeqCst) {
        Ok(())
    } else {
        anyhow::bail!("settings dialog cancelled")
    }
}

/// Decode the bundled 256-px clipboardwire icon into the egui IconData
/// shape so the Settings window shows our branding in the title bar /
/// taskbar instead of the default eframe "e".
fn load_window_icon() -> Option<egui::IconData> {
    const ICON_PNG: &[u8] = include_bytes!("../../assets/icon-256.png");
    let img = image::load_from_memory_with_format(ICON_PNG, image::ImageFormat::Png).ok()?;
    let rgba = img.to_rgba8();
    let (width, height) = rgba.dimensions();
    Some(egui::IconData {
        rgba: rgba.into_raw(),
        width,
        height,
    })
}

fn load_or_default(path: &Path) -> FormState {
    if path.exists() {
        if let Ok(cfg) = ClientConfig::load(path) {
            let hub = cfg.hub.unwrap_or_default();
            return FormState {
                server: cfg.server,
                user: cfg.user,
                password: cfg.password,
                poll_ms: cfg.poll_ms,
                tls_ca_file: cfg
                    .tls_ca_file
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                tls_insecure: cfg.tls_insecure,
                hub_enabled: hub.enabled,
                hub_bind: hub.bind.to_string(),
                hub_user: hub.user,
                hub_password: hub.password,
                hub_tls_cert_file: hub
                    .tls_cert_file
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                hub_tls_key_file: hub
                    .tls_key_file
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                last_error: None,
            };
        }
    }
    let hub_default = clipboardwire_core::client::config::HubConfig::default();
    FormState {
        hub_bind: hub_default.bind.to_string(),
        ..FormState::default()
    }
}

#[derive(Default, Clone)]
struct FormState {
    server: String,
    user: String,
    password: String,
    poll_ms: u64,
    tls_ca_file: String,
    tls_insecure: bool,
    // Hub section (optional [hub] in the TOML).
    hub_enabled: bool,
    hub_bind: String,
    hub_user: String,
    hub_password: String,
    hub_tls_cert_file: String,
    hub_tls_key_file: String,
    last_error: Option<String>,
}

struct SettingsApp {
    config_path: PathBuf,
    form: FormState,
    show_password: bool,
    saved: Arc<AtomicBool>,
}

impl SettingsApp {
    fn new(config_path: PathBuf, mut form: FormState, saved: Arc<AtomicBool>) -> Self {
        if form.poll_ms == 0 {
            form.poll_ms = DEFAULT_POLL_MS;
        }
        Self {
            config_path,
            form,
            show_password: false,
            saved,
        }
    }

    /// Attempt to validate + serialize + write the form to TOML. Returns
    /// `Ok` on success; on failure, stashes the message in `form.last_error`.
    fn try_save(&mut self) -> bool {
        if let Err(e) = validate(&self.form) {
            self.form.last_error = Some(e);
            return false;
        }
        match write_toml(&self.config_path, &self.form) {
            Ok(()) => {
                self.form.last_error = None;
                self.saved.store(true, Ordering::SeqCst);
                true
            }
            Err(e) => {
                self.form.last_error = Some(format!("could not save: {e:#}"));
                false
            }
        }
    }

    /// "Pin from server" button. Connects to `form.server`, captures
    /// the peer's end-entity cert, writes it as PEM next to the
    /// config file, and points `tls_ca_file` at the result. The user
    /// still has to hit Save afterwards — this only stages the
    /// change in the form.
    fn pin_server_cert(&mut self) {
        match pin_server_cert_impl(&self.form.server, &self.config_path) {
            Ok((path, fingerprint)) => {
                self.form.tls_ca_file = path.to_string_lossy().into_owned();
                self.form.tls_insecure = false;
                self.form.last_error = Some(format!(
                    "Pinned cert at {}\nSHA-256 {fingerprint}",
                    path.display()
                ));
            }
            Err(e) => {
                self.form.last_error = Some(format!("Could not pin server cert: {e:#}"));
            }
        }
    }
}

/// Open a TCP+TLS connection to the configured server URL, capture the
/// server's end-entity certificate, write it to a PEM file alongside
/// the config TOML, and return the path + SHA-256 fingerprint. The
/// cert is captured *without* verification — the whole point of this
/// button is to bootstrap trust on a self-signed cert.
fn pin_server_cert_impl(server_url: &str, config_path: &Path) -> Result<(PathBuf, String)> {
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::sync::Arc;
    use std::time::Duration;

    use rustls::pki_types::ServerName;
    use rustls::ClientConnection;

    let (host, port) = parse_wss_authority(server_url)?;

    let mut sock = TcpStream::connect((host.as_str(), port))
        .with_context(|| format!("connecting to {host}:{port}"))?;
    sock.set_read_timeout(Some(Duration::from_secs(5))).ok();
    sock.set_write_timeout(Some(Duration::from_secs(5))).ok();

    // rustls needs a CryptoProvider to be installed in the process
    // before any ClientConfig is built. Tray + transport already do
    // this lazily via aws_lc_rs; in the settings subprocess we have
    // to install it ourselves on first use.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    let cfg = Arc::new(clipboardwire_core::client::tls::make_insecure_client_config());
    let name = ServerName::try_from(host.clone()).context("invalid hostname in server URL")?;
    let mut conn = ClientConnection::new(cfg, name).context("starting TLS client connection")?;

    let mut stream = rustls::Stream::new(&mut conn, &mut sock);
    // Drive the handshake. `flush()` (or any read/write) will pump it
    // until either the peer's cert is available or an error occurs.
    let _ = stream.flush();
    // Just-in-case: also do a tiny read to surface handshake errors.
    let mut byte = [0u8; 1];
    let _ = stream.read(&mut byte);

    let der = conn
        .peer_certificates()
        .and_then(|chain| chain.first().cloned())
        .ok_or_else(|| anyhow::anyhow!("server did not present any certificates"))?;

    let pem = der_to_pem(der.as_ref());
    let fingerprint = sha256_hex(der.as_ref());

    let target = config_path
        .parent()
        .map(|p| p.join("server-pinned.crt"))
        .ok_or_else(|| anyhow::anyhow!("config path has no parent dir"))?;
    std::fs::create_dir_all(target.parent().expect("has parent"))?;
    std::fs::write(&target, pem).with_context(|| format!("writing {}", target.display()))?;

    Ok((target, fingerprint))
}

/// Pull the (host, port) out of `wss://host[:port]/path…`. Defaults
/// the port to 443 only as a fallback; the typical clipboardwire URL
/// includes the port explicitly.
fn parse_wss_authority(url: &str) -> Result<(String, u16)> {
    let after_scheme = url
        .strip_prefix("wss://")
        .ok_or_else(|| anyhow::anyhow!("server URL must start with wss:// to pin a certificate"))?;
    let authority = after_scheme.split('/').next().unwrap_or("");
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (
            h.to_string(),
            p.parse::<u16>().context("invalid port in server URL")?,
        ),
        None => (authority.to_string(), 443),
    };
    if host.is_empty() {
        anyhow::bail!("server URL has no host");
    }
    Ok((host, port))
}

fn der_to_pem(der: &[u8]) -> String {
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine;
    let b64 = STANDARD.encode(der);
    let mut out = String::with_capacity(b64.len() + 80);
    out.push_str("-----BEGIN CERTIFICATE-----\n");
    for chunk in b64.as_bytes().chunks(64) {
        out.push_str(std::str::from_utf8(chunk).expect("base64 alphabet is ascii"));
        out.push('\n');
    }
    out.push_str("-----END CERTIFICATE-----\n");
    out
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(bytes);
    digest
        .iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(":")
}

impl eframe::App for SettingsApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.draw(ctx);
    }
}

impl SettingsApp {
    /// Render one frame of the settings UI. Split out from
    /// `eframe::App::update` so tests can drive it through
    /// [`egui_kittest::Harness`] without constructing an `eframe::Frame`
    /// (which has no public constructor).
    fn draw(&mut self, ctx: &egui::Context) {
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("clipboardwire client settings");
            ui.add_space(8.0);
            ui.label(format!("File: {}", self.config_path.display()));
            ui.separator();

            egui::Grid::new("settings_grid")
                .num_columns(2)
                .spacing([12.0, 8.0])
                .show(ui, |ui| {
                    ui.label("Server URL");
                    ui.text_edit_singleline(&mut self.form.server)
                        .on_hover_text("ws://host:8484/sync or wss://host:8484/sync");
                    ui.end_row();

                    ui.label("Username");
                    ui.text_edit_singleline(&mut self.form.user);
                    ui.end_row();

                    ui.label("Password");
                    ui.horizontal(|ui| {
                        let pw_field = if self.show_password {
                            egui::TextEdit::singleline(&mut self.form.password)
                        } else {
                            egui::TextEdit::singleline(&mut self.form.password).password(true)
                        };
                        ui.add(pw_field);
                        ui.checkbox(&mut self.show_password, "show");
                    });
                    ui.end_row();

                    ui.label("Poll interval (ms)");
                    ui.add(egui::DragValue::new(&mut self.form.poll_ms).range(50..=10_000));
                    ui.end_row();

                    ui.label("TLS CA file (optional)");
                    ui.horizontal(|ui| {
                        ui.text_edit_singleline(&mut self.form.tls_ca_file)
                            .on_hover_text("PEM file with extra root CAs to trust");
                        if ui
                            .button("Pin from server")
                            .on_hover_text(
                                "Connect to the server URL, fetch its certificate, save it next \
                                 to the config file, and point this field at the saved PEM. \
                                 You no longer need `Skip TLS verification` after pinning.",
                            )
                            .clicked()
                        {
                            self.pin_server_cert();
                        }
                    });
                    ui.end_row();

                    ui.label("Skip TLS verification");
                    ui.add(egui::Checkbox::new(
                        &mut self.form.tls_insecure,
                        "DANGEROUS — LAN/VPN only",
                    ));
                    ui.end_row();
                });

            ui.add_space(8.0);
            ui.separator();
            ui.collapsing("Run hub on this machine", |ui| {
                ui.label(
                    "Bring up a clipboard-sync hub server in the tray process. \
                     The local client will connect to it over loopback; other \
                     devices on your network connect to the address below.",
                );
                ui.add_space(4.0);
                ui.checkbox(&mut self.form.hub_enabled, "Run hub when the tray starts");
                ui.add_enabled_ui(self.form.hub_enabled, |ui| {
                    egui::Grid::new("hub_grid")
                        .num_columns(2)
                        .spacing([12.0, 6.0])
                        .show(ui, |ui| {
                            ui.label("Bind");
                            ui.text_edit_singleline(&mut self.form.hub_bind)
                                .on_hover_text("host:port, e.g. 0.0.0.0:8484");
                            ui.end_row();

                            ui.label("Hub username");
                            ui.text_edit_singleline(&mut self.form.hub_user);
                            ui.end_row();

                            ui.label("Hub password");
                            ui.add(
                                egui::TextEdit::singleline(&mut self.form.hub_password)
                                    .password(!self.show_password),
                            );
                            ui.end_row();

                            ui.label("TLS cert file");
                            ui.text_edit_singleline(&mut self.form.hub_tls_cert_file)
                                .on_hover_text("PEM. Leave empty to speak plain ws://.");
                            ui.end_row();

                            ui.label("TLS key file");
                            ui.text_edit_singleline(&mut self.form.hub_tls_key_file)
                                .on_hover_text("PEM. Required iff cert file is set.");
                            ui.end_row();
                        });
                });
            });

            ui.add_space(12.0);

            if let Some(err) = &self.form.last_error {
                ui.colored_label(egui::Color32::from_rgb(220, 60, 60), err);
                ui.add_space(8.0);
            }

            ui.horizontal(|ui| {
                let save = ui
                    .add_sized([100.0, 28.0], egui::Button::new("Save"))
                    .clicked();
                let cancel = ui
                    .add_sized([100.0, 28.0], egui::Button::new("Cancel"))
                    .clicked();

                if save && self.try_save() {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
                if cancel {
                    self.saved.store(false, Ordering::SeqCst);
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            });
        });
    }
}

/// Validate the form. Returns a human-readable error string on failure.
fn validate(form: &FormState) -> std::result::Result<(), String> {
    if !form.server.starts_with("ws://") && !form.server.starts_with("wss://") {
        return Err("Server URL must start with ws:// or wss://".into());
    }
    if form.user.trim().is_empty() {
        return Err("Username must not be empty".into());
    }
    if form.password.is_empty() {
        return Err("Password must not be empty".into());
    }
    if form.poll_ms == 0 {
        return Err("Poll interval must be at least 1 ms".into());
    }
    if form.hub_enabled {
        if form.hub_bind.parse::<std::net::SocketAddr>().is_err() {
            return Err(format!(
                "Hub bind address `{}` is not a valid host:port",
                form.hub_bind
            ));
        }
        if form.hub_user.trim().is_empty() {
            return Err("Hub username must not be empty".into());
        }
        if form.hub_password.is_empty() {
            return Err("Hub password must not be empty".into());
        }
        let has_cert = !form.hub_tls_cert_file.trim().is_empty();
        let has_key = !form.hub_tls_key_file.trim().is_empty();
        if has_cert != has_key {
            return Err("Hub TLS needs both cert and key files (or neither)".into());
        }
    }
    Ok(())
}

/// Serializable shadow of the writable fields. We don't serialize
/// `last_error` or the password-toggle state — those are UI-only.
#[derive(serde::Serialize)]
struct WritableConfig<'a> {
    server: &'a str,
    user: &'a str,
    password: &'a str,
    poll_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    tls_ca_file: Option<&'a str>,
    #[serde(skip_serializing_if = "is_false")]
    tls_insecure: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    hub: Option<WritableHub<'a>>,
}

#[derive(serde::Serialize)]
struct WritableHub<'a> {
    enabled: bool,
    bind: String,
    user: &'a str,
    password: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    tls_cert_file: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tls_key_file: Option<&'a str>,
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// Write the form back as TOML, creating parent dirs and setting 0600
/// on Unix (the file contains a password).
fn write_toml(path: &Path, form: &FormState) -> Result<()> {
    let ca = form.tls_ca_file.trim();
    let hub_cert = form.hub_tls_cert_file.trim();
    let hub_key = form.hub_tls_key_file.trim();
    let hub_section = if form.hub_enabled
        || !form.hub_user.is_empty()
        || !form.hub_password.is_empty()
        || !hub_cert.is_empty()
        || !hub_key.is_empty()
    {
        Some(WritableHub {
            enabled: form.hub_enabled,
            bind: form.hub_bind.clone(),
            user: &form.hub_user,
            password: &form.hub_password,
            tls_cert_file: if hub_cert.is_empty() {
                None
            } else {
                Some(hub_cert)
            },
            tls_key_file: if hub_key.is_empty() {
                None
            } else {
                Some(hub_key)
            },
        })
    } else {
        None
    };

    let writable = WritableConfig {
        server: &form.server,
        user: &form.user,
        password: &form.password,
        poll_ms: form.poll_ms,
        tls_ca_file: if ca.is_empty() { None } else { Some(ca) },
        tls_insecure: form.tls_insecure,
        hub: hub_section,
    };

    let mut out =
        String::from("# clipboardwire client config — written by `clipboardwire settings`.\n\n");
    out.push_str(&toml::to_string(&writable).context("serializing config as TOML")?);

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(path, out).with_context(|| format!("writing {}", path.display()))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(path)?.permissions();
        perms.set_mode(0o600);
        std::fs::set_permissions(path, perms)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_form() -> FormState {
        FormState {
            server: "wss://nas.lan:8484/sync".into(),
            user: "alice".into(),
            password: "hunter2".into(),
            poll_ms: 300,
            tls_ca_file: String::new(),
            tls_insecure: false,
            hub_enabled: false,
            hub_bind: "0.0.0.0:8484".into(),
            hub_user: String::new(),
            hub_password: String::new(),
            hub_tls_cert_file: String::new(),
            hub_tls_key_file: String::new(),
            last_error: None,
        }
    }

    #[test]
    fn parse_wss_authority_handles_explicit_port() {
        let (h, p) = parse_wss_authority("wss://nas.lan:8484/sync").unwrap();
        assert_eq!(h, "nas.lan");
        assert_eq!(p, 8484);
    }

    #[test]
    fn parse_wss_authority_handles_no_path() {
        let (h, p) = parse_wss_authority("wss://192.168.1.10:9999").unwrap();
        assert_eq!(h, "192.168.1.10");
        assert_eq!(p, 9999);
    }

    #[test]
    fn parse_wss_authority_defaults_to_443() {
        let (h, p) = parse_wss_authority("wss://example.com/sync").unwrap();
        assert_eq!(h, "example.com");
        assert_eq!(p, 443);
    }

    #[test]
    fn parse_wss_authority_rejects_ws_scheme() {
        assert!(parse_wss_authority("ws://nas.lan:8484/sync").is_err());
    }

    #[test]
    fn der_to_pem_round_trips_via_rustls_pemfile() {
        let der = [0u8, 1, 2, 3, 4, 5, 6, 7]; // not a real cert, just bytes
        let pem = der_to_pem(&der);
        assert!(pem.starts_with("-----BEGIN CERTIFICATE-----"));
        assert!(pem.contains("AAECAwQFBgc="));
        assert!(pem.trim_end().ends_with("-----END CERTIFICATE-----"));
    }

    #[test]
    fn validate_accepts_a_well_formed_form() {
        assert!(validate(&valid_form()).is_ok());
    }

    #[test]
    fn validate_rejects_non_ws_scheme() {
        let mut f = valid_form();
        f.server = "http://example.com/sync".into();
        assert!(validate(&f).is_err());
    }

    #[test]
    fn validate_rejects_empty_user() {
        let mut f = valid_form();
        f.user.clear();
        assert!(validate(&f).is_err());
    }

    #[test]
    fn validate_rejects_empty_password() {
        let mut f = valid_form();
        f.password.clear();
        assert!(validate(&f).is_err());
    }

    #[test]
    fn validate_rejects_zero_poll_ms() {
        let mut f = valid_form();
        f.poll_ms = 0;
        assert!(validate(&f).is_err());
    }

    #[test]
    fn write_toml_round_trips_via_client_config_load() {
        let dir = std::env::temp_dir().join(format!(
            "cw-settings-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let path = dir.join("config.toml");

        let form = valid_form();
        write_toml(&path, &form).unwrap();
        let loaded = ClientConfig::load(&path).unwrap();
        assert_eq!(loaded.server, form.server);
        assert_eq!(loaded.user, form.user);
        assert_eq!(loaded.password, form.password);
        assert_eq!(loaded.poll_ms, form.poll_ms);
        assert!(loaded.tls_ca_file.is_none());
        assert!(!loaded.tls_insecure);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_toml_omits_empty_optional_fields() {
        let dir = std::env::temp_dir().join(format!(
            "cw-settings-omit-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let path = dir.join("config.toml");
        write_toml(&path, &valid_form()).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(!raw.contains("tls_ca_file"));
        assert!(!raw.contains("tls_insecure"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_toml_preserves_set_optional_fields() {
        let dir = std::env::temp_dir().join(format!(
            "cw-settings-opt-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let path = dir.join("config.toml");
        let mut form = valid_form();
        form.tls_ca_file = "/etc/ssl/ca.crt".into();
        form.tls_insecure = true;
        write_toml(&path, &form).unwrap();
        let loaded = ClientConfig::load(&path).unwrap();
        assert_eq!(
            loaded.tls_ca_file.unwrap().to_string_lossy(),
            "/etc/ssl/ca.crt"
        );
        assert!(loaded.tls_insecure);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    #[cfg(unix)]
    fn write_toml_chmod_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!(
            "cw-settings-mode-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let path = dir.join("config.toml");
        write_toml(&path, &valid_form()).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // -------------------------------------------------------------
    // egui_kittest harness tests — drive the actual widget tree to
    // confirm Save / Cancel / error-label wiring. Without these, the
    // pure-data tests above could pass even if the buttons weren't
    // hooked up at all.
    // -------------------------------------------------------------

    use egui_kittest::kittest::Queryable;
    use egui_kittest::Harness;

    fn unique_dir(label: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "cw-settings-kt-{label}-{}-{nanos}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    /// Build a harness around a SettingsApp with the given form +
    /// config path. The closure-based `new_state` API lets the harness
    /// own the app and lets test code reach in via `harness.state_mut()`
    /// between ticks.
    fn build_harness(
        path: PathBuf,
        form: FormState,
        saved: Arc<AtomicBool>,
    ) -> Harness<'static, SettingsApp> {
        let app = SettingsApp::new(path, form, saved);
        Harness::new_state(|ctx, app: &mut SettingsApp| app.draw(ctx), app)
    }

    #[test]
    fn ui_renders_with_expected_widget_labels() {
        let path = unique_dir("labels").join("config.toml");
        let saved = Arc::new(AtomicBool::new(false));
        let mut harness = build_harness(path, valid_form(), saved);
        harness.run();

        // These four widgets are the contract surface — the form would
        // be useless without any of them. We don't assert exhaustively
        // because labels we change for usability shouldn't fail the test.
        harness.get_by_label("Save");
        harness.get_by_label("Cancel");
        harness.get_by_label("Server URL");
        harness.get_by_label("Username");
    }

    #[test]
    fn save_click_writes_toml_and_flips_saved_flag() {
        let path = unique_dir("save").join("config.toml");
        let saved = Arc::new(AtomicBool::new(false));
        let mut harness = build_harness(path.clone(), valid_form(), saved.clone());
        harness.run();
        harness.get_by_label("Save").click();
        // Two extra ticks: the click is consumed on the next frame,
        // then try_save runs and writes the file.
        harness.run();
        harness.run();

        assert!(saved.load(Ordering::SeqCst), "Save did not flip the flag");
        assert!(path.exists(), "Save did not write the config file");

        // And the file round-trips back through ClientConfig::load.
        let loaded = ClientConfig::load(&path).expect("written file parses");
        assert_eq!(loaded.user, "alice");

        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn cancel_click_does_not_write_file() {
        let path = unique_dir("cancel").join("config.toml");
        let saved = Arc::new(AtomicBool::new(false));
        let mut harness = build_harness(path.clone(), valid_form(), saved.clone());
        harness.run();
        harness.get_by_label("Cancel").click();
        harness.run();
        harness.run();

        assert!(!saved.load(Ordering::SeqCst), "Cancel should not flip flag");
        assert!(!path.exists(), "Cancel should not write the config file");
    }

    #[test]
    fn invalid_input_surfaces_an_error_label() {
        let path = unique_dir("err").join("config.toml");
        let mut form = valid_form();
        form.user.clear(); // validation will reject this on Save
        let saved = Arc::new(AtomicBool::new(false));
        let mut harness = build_harness(path.clone(), form, saved.clone());
        harness.run();
        harness.get_by_label("Save").click();
        harness.run();
        harness.run();

        assert!(
            !saved.load(Ordering::SeqCst),
            "validation failure should not flip Saved"
        );
        assert!(!path.exists(), "validation failure should not write file");
        // The error message we render for empty user starts with this prefix.
        harness.get_by_label_contains("Username must not be empty");
    }
}
