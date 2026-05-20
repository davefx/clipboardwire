// SPDX-License-Identifier: GPL-3.0-or-later

//! Tier-1 GUI smoke test for the tray.
//!
//! Catches the broad class of "the tray subsystem panics on init" bugs
//! that bit us in v0.2.0 and v0.2.1: linked dep mismatches, tao + tokio
//! ownership issues, GTK init failing silently, etc.
//!
//! Does **not** test interactive behavior (right-click → menu open, menu
//! item activation). Those are Tier-3 territory, shipped in later releases
//! as platform-specific harnesses (Linux DBus inspection, Windows UI
//! Automation).
//!
//! `#[ignore]` by default because it needs a display. Run with:
//!
//! ```bash
//! # Linux (needs xvfb installed)
//! xvfb-run -a cargo test -- --ignored connect_tray_starts
//!
//! # Windows / macOS (a desktop session is implicit)
//! cargo test -- --ignored connect_tray_starts
//! ```

use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

/// Soft-skip on Linux when no display is available — `cargo test --
/// --ignored` from a tty without xvfb shouldn't fail loudly. The macro
/// expands to a `return` from the calling test, which libtest reports
/// as a pass; we accept that minor dishonesty because libtest has no
/// skip API.
macro_rules! skip_if_no_display {
    () => {
        #[cfg(target_os = "linux")]
        {
            if std::env::var_os("DISPLAY").is_none()
                && std::env::var_os("WAYLAND_DISPLAY").is_none()
            {
                eprintln!("no display available; skipping (set DISPLAY or run under xvfb-run)");
                return;
            }
        }
    };
}

#[test]
#[ignore = "GUI test; requires a display. Run under xvfb-run on Linux or natively on Windows/macOS."]
fn connect_tray_starts_without_panicking() {
    skip_if_no_display!();

    let tmp = unique_tmp_dir("connect-tray");
    let cfg_path = tmp.join("config.toml");

    // Explicit --config to a path that doesn't exist yet: the tray must
    // still come up (in "needs config" state) instead of bailing. We're
    // testing init, not the eventual reconnect path.
    let binary = env!("CARGO_BIN_EXE_clipboardwire");
    let mut child = Command::new(binary)
        .args(["connect", "--tray", "--config"])
        .arg(&cfg_path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("RUST_LOG", "clipboardwire=info,clipboardwire_core=info")
        .spawn()
        .expect("spawn clipboardwire");

    // Give the binary enough wall-clock time to bring up tokio + tao +
    // tray-icon. 2 seconds is conservative; in practice init is ~10 ms.
    std::thread::sleep(Duration::from_secs(2));

    if let Some(status) = child.try_wait().expect("try_wait") {
        let mut stderr_buf = String::new();
        if let Some(mut s) = child.stderr.take() {
            let _ = s.read_to_string(&mut stderr_buf);
        }
        let _ = std::fs::remove_dir_all(&tmp);
        panic!(
            "clipboardwire connect --tray exited prematurely (status: {status:?})\n\
             --- stderr ---\n{stderr_buf}"
        );
    }

    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
#[ignore = "GUI test; requires a display."]
fn connect_tray_writes_template_at_default_path() {
    skip_if_no_display!();

    // Sandbox HOME / APPDATA so default_path() resolves inside a tempdir.
    let tmp = unique_tmp_dir("default-path");
    std::fs::create_dir_all(&tmp).unwrap();

    let binary = env!("CARGO_BIN_EXE_clipboardwire");
    let mut cmd = Command::new(binary);
    cmd.args(["connect", "--tray"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("RUST_LOG", "clipboardwire=info,clipboardwire_core=info");

    let expected_path: PathBuf;
    #[cfg(windows)]
    {
        cmd.env("APPDATA", &tmp);
        expected_path = tmp.join("clipboardwire").join("config.toml");
    }
    #[cfg(not(windows))]
    {
        // BaseDirs::config_dir() respects XDG_CONFIG_HOME on Linux and
        // falls back to $HOME/.config. Forcing both keeps the test
        // deterministic across platforms.
        cmd.env("HOME", &tmp);
        cmd.env("XDG_CONFIG_HOME", tmp.join(".config"));
        #[cfg(target_os = "linux")]
        {
            expected_path = tmp
                .join(".config")
                .join("clipboardwire")
                .join("config.toml");
        }
        #[cfg(target_os = "macos")]
        {
            expected_path = tmp
                .join("Library")
                .join("Application Support")
                .join("clipboardwire")
                .join("config.toml");
        }
    }

    let mut child = cmd.spawn().expect("spawn clipboardwire");

    // Poll briefly for the template — it should land within the first
    // tick of the tray-mode startup. Two seconds is a generous ceiling.
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while !expected_path.exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }

    let exists = expected_path.exists();
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&tmp);

    assert!(
        exists,
        "template was not written at the expected default path: {}",
        expected_path.display()
    );
}

fn unique_tmp_dir(label: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "clipboardwire-smoke-{label}-{}-{nanos}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}
