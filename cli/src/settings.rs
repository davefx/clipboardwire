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

    let app = SettingsApp::new(config_path, initial);

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([520.0, 360.0])
            .with_min_inner_size([420.0, 320.0])
            .with_resizable(true)
            .with_title(WINDOW_TITLE),
        ..Default::default()
    };

    eframe::run_native(
        WINDOW_TITLE,
        native_options,
        Box::new(|_cc| Ok(Box::new(app))),
    )
    .map_err(|e| anyhow::anyhow!("settings window: {e}"))?;

    // eframe::run_native returns when the window is closed. The exit
    // code is decided by `SettingsApp::on_exit` via SAVE/CANCEL state
    // recorded into a static; cleaner to surface via process exit code
    // so the parent (tray) can react if it wants.
    if SAVED.load(std::sync::atomic::Ordering::SeqCst) {
        Ok(())
    } else {
        anyhow::bail!("settings dialog cancelled")
    }
}

static SAVED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

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
}

impl SettingsApp {
    fn new(config_path: PathBuf, mut form: FormState) -> Self {
        if form.poll_ms == 0 {
            form.poll_ms = DEFAULT_POLL_MS;
        }
        Self {
            config_path,
            form,
            show_password: false,
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
                SAVED.store(true, std::sync::atomic::Ordering::SeqCst);
                true
            }
            Err(e) => {
                self.form.last_error = Some(format!("could not save: {e:#}"));
                false
            }
        }
    }
}

impl eframe::App for SettingsApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
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
                    ui.text_edit_singleline(&mut self.form.tls_ca_file)
                        .on_hover_text("PEM file with extra root CAs to trust");
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
                    SAVED.store(false, std::sync::atomic::Ordering::SeqCst);
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
}
