// SPDX-License-Identifier: GPL-3.0-or-later

//! End-to-end check for the per-user singleton lock.
//!
//! Spawns two `clipboardwire --tray` processes against the same
//! sandboxed config dir. The first should come up and keep running.
//! The second should detect the lock, write a "another instance is
//! already running" diagnostic to stderr, and exit cleanly within a
//! few seconds.
//!
//! This is the regression test for the v0.3.0/v0.3.1 bug where
//! double-launching produced two trays racing for the hub port and a
//! visible client-cycles-every-few-seconds symptom that looked like a
//! protocol bug.
//!
//! The test needs a display because the *first* process builds the
//! tao event loop and a tray icon. In CI it runs under `xvfb-run`.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// How long we give the second process to acquire-then-fail before
/// declaring the lock dead.
const SECOND_PROCESS_EXIT_DEADLINE: Duration = Duration::from_secs(10);

/// How long we wait after spawning the first process before asserting
/// "lock is held." The first process needs time to reach
/// `run_with_tray` and call `instance::acquire_or_fail` — on a cold
/// CI runner with libGL/GTK init, that's a couple of seconds.
const FIRST_PROCESS_WARMUP: Duration = Duration::from_secs(3);

#[test]
#[ignore = "Spawns the real binary in tray mode; needs a display (xvfb under CI)."]
fn second_tray_launch_exits_due_to_singleton_lock() {
    #[cfg(target_os = "linux")]
    {
        if std::env::var_os("DISPLAY").is_none() && std::env::var_os("WAYLAND_DISPLAY").is_none() {
            eprintln!("no display; skipping (run under xvfb-run)");
            return;
        }
    }

    let sandbox = unique_tmp();
    let mut first = spawn_tray(&sandbox, /* capture_stderr */ false);

    // Let the first process reach the lock-acquisition point. On a
    // cold CI runner this is the slowest hop in the test.
    std::thread::sleep(FIRST_PROCESS_WARMUP);

    if let Some(status) = first.try_wait().expect("try_wait first") {
        panic!(
            "first tray process died before we could test the lock (status: {status:?}). \
             The lock test can't run unless the first process is alive."
        );
    }

    // Second launch: should hit the lock, log + exit cleanly.
    let mut second = spawn_tray(&sandbox, /* capture_stderr */ true);
    let outcome = wait_for_exit(&mut second, SECOND_PROCESS_EXIT_DEADLINE);

    let _ = first.kill();
    let _ = first.wait();
    let _ = std::fs::remove_dir_all(&sandbox);

    let Some((status, stderr)) = outcome else {
        panic!(
            "second clipboardwire did not exit within {}s — the singleton lock didn't kick in",
            SECOND_PROCESS_EXIT_DEADLINE.as_secs()
        );
    };

    assert!(
        status.success(),
        "second process exited non-zero: {status:?}\nstderr:\n{}",
        String::from_utf8_lossy(&stderr)
    );

    let stderr_str = String::from_utf8_lossy(&stderr);
    eprintln!("---- second process stderr ----\n{stderr_str}---- end ----");
    assert!(
        stderr_str.contains("already running"),
        "second process stderr should announce the lock contention; got:\n{stderr_str}"
    );
}

fn spawn_tray(sandbox: &Path, capture_stderr: bool) -> Child {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_clipboardwire"));
    cmd.arg("--tray")
        .env("RUST_LOG", "clipboardwire=info,clipboardwire_core=warn")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(if capture_stderr {
            Stdio::piped()
        } else {
            Stdio::null()
        });

    // Isolate the config + lock directory from the developer's real
    // home so a CI/local run doesn't fight a real installed tray.
    #[cfg(target_os = "linux")]
    {
        cmd.env("XDG_CONFIG_HOME", sandbox);
        cmd.env("HOME", sandbox);
    }
    #[cfg(windows)]
    {
        cmd.env("APPDATA", sandbox);
    }

    // Don't inherit the developer's DBUS — the first process's tray
    // would otherwise re-use whatever real SNI watcher is around. The
    // existing tray_dbus_linux test sets up a mock; here we want the
    // standard one (so we deliberately *don't* set DBUS_SESSION_BUS_ADDRESS).

    cmd.spawn().expect("spawn clipboardwire")
}

fn wait_for_exit(
    child: &mut Child,
    deadline: Duration,
) -> Option<(std::process::ExitStatus, Vec<u8>)> {
    let until = Instant::now() + deadline;
    while Instant::now() < until {
        match child.try_wait().expect("try_wait") {
            Some(status) => {
                let mut buf = Vec::new();
                if let Some(mut stderr) = child.stderr.take() {
                    // We're past exit, so this is bounded.
                    let _ = stderr.read_to_end(&mut buf);
                }
                return Some((status, buf));
            }
            None => std::thread::sleep(Duration::from_millis(100)),
        }
    }
    None
}

fn unique_tmp() -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("cw-singleton-{}-{nanos}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create sandbox dir");
    dir
}
