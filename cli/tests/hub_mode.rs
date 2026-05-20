// SPDX-License-Identifier: GPL-3.0-or-later

//! `[hub] enabled = true` in the client config makes the tray bring up
//! a hub server in-process before connecting the local client to it.
//! This test launches the binary with such a config and verifies the
//! embedded hub is actually accepting connections.

use std::io::Read;
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[test]
#[ignore = "GUI test; needs a display (xvfb on Linux)."]
fn embedded_hub_binds_when_enabled_in_config() {
    skip_if_no_display();

    // Pick a host port that's almost certainly free for the test run.
    // 8484 is the project default; nudge well above to avoid collisions
    // with any locally-running hub.
    let port = 18484u16;
    let bind_addr = format!("127.0.0.1:{port}");

    let tmp = unique_tmp_dir("hub-mode");
    std::fs::create_dir_all(&tmp).unwrap();
    let cfg_path = tmp.join("config.toml");
    std::fs::write(
        &cfg_path,
        format!(
            r#"
server   = "ws://127.0.0.1:{port}/sync"
user     = "alice"
password = "hunter2"
poll_ms  = 1000

[hub]
enabled  = true
bind     = "{bind_addr}"
user     = "alice"
password = "hunter2"
"#
        ),
    )
    .unwrap();

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&cfg_path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }

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

    // Poll the bound port until the hub is accepting TCP connections.
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut bound = false;
    while Instant::now() < deadline {
        if TcpStream::connect_timeout(&bind_addr.parse().unwrap(), Duration::from_millis(200))
            .is_ok()
        {
            bound = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    if !bound {
        let mut stderr = String::new();
        if let Some(mut s) = child.stderr.take() {
            let _ = s.read_to_string(&mut stderr);
        }
        let _ = child.kill();
        let _ = child.wait();
        let _ = std::fs::remove_dir_all(&tmp);
        panic!(
            "embedded hub did not bind at {bind_addr} within 10s\n\
             --- stderr ---\n{stderr}"
        );
    }

    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&tmp);
}

fn unique_tmp_dir(label: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "clipboardwire-hub-{label}-{}-{nanos}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn skip_if_no_display() {
    #[cfg(target_os = "linux")]
    if std::env::var_os("DISPLAY").is_none() && std::env::var_os("WAYLAND_DISPLAY").is_none() {
        eprintln!("no display available; skipping (set DISPLAY or run under xvfb-run)");
        // Same soft-skip pattern as tray_smoke.rs.
        // Returning early — libtest reports as pass.
        std::process::exit(0);
    }
}
