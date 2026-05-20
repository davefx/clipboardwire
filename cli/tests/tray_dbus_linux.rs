// SPDX-License-Identifier: GPL-3.0-or-later

//! Tier-3 Linux interaction harness for the tray UI.
//!
//! This is the "the menu actually exists with the right items and they
//! work" test that the v0.2.0–v0.2.1 tray bugs would not have escaped if
//! it had been in place. The strategy:
//!
//! 1. Spawn an isolated `dbus-daemon` so we don't pollute the host bus.
//! 2. Register a minimal `org.kde.StatusNotifierWatcher` ourselves —
//!    libayatana-appindicator only publishes a `StatusNotifierItem`
//!    when a Watcher is present, and the Watcher's
//!    `RegisterStatusNotifierItem(service)` call is also how we learn
//!    the binary's SNI service name.
//! 3. Spawn `clipboardwire connect --tray` against that bus + an Xvfb
//!    display, pointed at a config path that does **not** exist.
//! 4. Wait for the binary to call `RegisterStatusNotifierItem`.
//! 5. Query the SNI's `Menu` property to find its dbusmenu object.
//! 6. Pull the menu layout via `com.canonical.dbusmenu.GetLayout`.
//! 7. Assert the three expected items by label.
//! 8. Activate **Edit config**, verify the template file lands on disk.
//! 9. Activate **Reload config**, verify the binary survives (the
//!    CHANGE-ME template won't validate, so this exercises the
//!    "invalid-config-after-reload" path without crashing).
//! 10. Activate **Quit**, verify the binary exits cleanly.
//!
//! Failing any of these means the tray is broken in a way that would
//! show up the moment a user right-clicks the icon.

#![cfg(target_os = "linux")]

use std::collections::HashMap;
use std::io::Read;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::Notify;
use zbus::interface;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value};

/// The three menu items defined in cli/src/tray.rs.
const EXPECTED_MENU_LABELS: &[&str] = &["Edit config…", "Reload config", "Quit clipboardwire"];

/// Minimal mock of `org.kde.StatusNotifierWatcher`. Records every
/// `RegisterStatusNotifierItem` call's (sender, object_path) so the test
/// can address the right peer.
///
/// libayatana-appindicator passes an *object path* in the `service`
/// argument (e.g. `/org/ayatana/NotificationItem/clipboardwire`) and
/// relies on the bus to identify the sender. So we need both pieces.
#[derive(Debug, Clone)]
struct RegisteredItem {
    sender: String,
    path: String,
}

struct MockWatcher {
    items: tokio::sync::Mutex<Vec<RegisteredItem>>,
    notify: Arc<Notify>,
}

#[interface(name = "org.kde.StatusNotifierWatcher")]
impl MockWatcher {
    async fn register_status_notifier_item(
        &self,
        service: &str,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) {
        let sender = header.sender().map(|s| s.to_string()).unwrap_or_default();
        self.items.lock().await.push(RegisteredItem {
            sender,
            path: service.to_string(),
        });
        self.notify.notify_waiters();
    }

    async fn register_status_notifier_host(&self, _service: &str) {}

    #[zbus(property)]
    async fn registered_status_notifier_items(&self) -> Vec<String> {
        self.items
            .lock()
            .await
            .iter()
            .map(|i| format!("{}{}", i.sender, i.path))
            .collect()
    }

    #[zbus(property)]
    async fn is_status_notifier_host_registered(&self) -> bool {
        true
    }

    #[zbus(property)]
    async fn protocol_version(&self) -> i32 {
        0
    }

    #[zbus(signal)]
    async fn status_notifier_item_registered(
        emitter: &SignalEmitter<'_>,
        service: &str,
    ) -> zbus::Result<()>;
}

struct DBusDaemon {
    child: Child,
    address: String,
}

impl Drop for DBusDaemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Start an isolated dbus-daemon. Returns the address other clients can
/// use to connect.
fn spawn_dbus_daemon() -> DBusDaemon {
    let mut child = Command::new("dbus-daemon")
        .args(["--session", "--nofork", "--print-address=1"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn dbus-daemon (apt install -y dbus)");

    let mut stdout = child.stdout.take().expect("dbus-daemon stdout");
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut buf = Vec::new();
    loop {
        let mut chunk = [0u8; 256];
        let n = stdout.read(&mut chunk).expect("read dbus address");
        buf.extend_from_slice(&chunk[..n]);
        if buf.contains(&b'\n') {
            break;
        }
        if Instant::now() > deadline {
            panic!("dbus-daemon did not print address within 3s");
        }
    }
    let address = std::str::from_utf8(&buf)
        .expect("dbus address utf-8")
        .lines()
        .next()
        .expect("at least one line")
        .to_string();

    // Re-attach stdout to a thread that drains it, so the daemon
    // doesn't block on a full pipe.
    std::thread::spawn(move || {
        let mut sink = [0u8; 1024];
        while stdout.read(&mut sink).unwrap_or(0) > 0 {}
    });

    DBusDaemon { child, address }
}

fn unique_tmp_dir(label: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "clipboardwire-dbus-{label}-{}-{nanos}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "Linux DBus interaction; needs xvfb + dbus-daemon installed."]
async fn tray_menu_is_published_and_quit_terminates_process() {
    // Skip on machines without a display — xvfb is set up by CI but a
    // developer running `cargo test -- --ignored` on a headless box
    // shouldn't see a failure.
    if std::env::var_os("DISPLAY").is_none() && std::env::var_os("WAYLAND_DISPLAY").is_none() {
        eprintln!("no display; skipping (run under xvfb-run)");
        return;
    }

    let bus = spawn_dbus_daemon();

    // Connect to the test bus and host the Watcher.
    let conn = zbus::connection::Builder::address(bus.address.as_str())
        .expect("address parse")
        .build()
        .await
        .expect("connect to test bus");

    let notify = Arc::new(Notify::new());
    let watcher = MockWatcher {
        items: tokio::sync::Mutex::new(Vec::new()),
        notify: notify.clone(),
    };
    conn.object_server()
        .at("/StatusNotifierWatcher", watcher)
        .await
        .expect("export Watcher object");
    conn.request_name("org.kde.StatusNotifierWatcher")
        .await
        .expect("claim Watcher name");

    let tmp = unique_tmp_dir("menu");
    let cfg_path = tmp.join("config.toml");

    let display = std::env::var("DISPLAY").unwrap_or_default();
    let mut binary = Command::new(env!("CARGO_BIN_EXE_clipboardwire"))
        .args(["connect", "--tray", "--config"])
        .arg(&cfg_path)
        .env("DBUS_SESSION_BUS_ADDRESS", &bus.address)
        .env("DISPLAY", &display)
        .env("RUST_LOG", "clipboardwire=info,clipboardwire_core=info")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn clipboardwire");

    // Wait for the binary's appindicator to call
    // RegisterStatusNotifierItem on our Watcher. 15 s gives generous
    // room for cold-start link + GTK + eframe init under xvfb.
    let item = match tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            notify.notified().await;
            let items = conn
                .object_server()
                .interface::<_, MockWatcher>("/StatusNotifierWatcher")
                .await
                .unwrap()
                .get()
                .await
                .items
                .lock()
                .await
                .clone();
            if let Some(it) = items.into_iter().next() {
                return it;
            }
        }
    })
    .await
    {
        Ok(it) => it,
        Err(_) => {
            cleanup_binary(&mut binary, &tmp).await;
            panic!("clipboardwire did not register an SNI within 8s");
        }
    };

    eprintln!("captured SNI: sender={} path={}", item.sender, item.path);

    let sni_path: ObjectPath = item
        .path
        .as_str()
        .try_into()
        .expect("registered service is a valid object path");

    // Read the SNI's Menu property. The destination is the sender's
    // bus name (e.g. `:1.42`); the object path is what the sender
    // registered.
    let props = zbus::fdo::PropertiesProxy::builder(&conn)
        .destination(item.sender.as_str())
        .unwrap()
        .path(sni_path.clone())
        .unwrap()
        .build()
        .await
        .expect("PropertiesProxy");
    let menu_value: OwnedValue = match props
        .get("org.kde.StatusNotifierItem".try_into().unwrap(), "Menu")
        .await
    {
        Ok(v) => v,
        Err(e) => {
            cleanup_binary(&mut binary, &tmp).await;
            panic!("could not read SNI Menu property: {e}");
        }
    };
    let menu_path: OwnedObjectPath = menu_value.try_into().expect("Menu is an object path");

    // Query the dbusmenu layout.
    let menu_proxy = zbus::Proxy::new(
        &conn,
        item.sender.as_str(),
        menu_path.as_ref(),
        "com.canonical.dbusmenu",
    )
    .await
    .expect("dbusmenu proxy");

    let layout: (u32, MenuNode) = match menu_proxy
        .call("GetLayout", &(0i32, -1i32, Vec::<&str>::new()))
        .await
    {
        Ok(v) => v,
        Err(e) => {
            cleanup_binary(&mut binary, &tmp).await;
            panic!("GetLayout failed: {e}");
        }
    };

    // Collect every visible label from the layout (depth-first).
    let mut labels = Vec::new();
    collect_labels(&layout.1, &mut labels);
    for expected in EXPECTED_MENU_LABELS {
        assert!(
            labels.iter().any(|l| l == expected),
            "missing menu label `{expected}`. saw: {labels:?}"
        );
    }

    // --- Edit config: verify the binary survives and the settings
    //     subprocess is spawned ---
    activate(&menu_proxy, &layout.1, "Edit config…").await;
    // Give the binary a moment to spawn the subprocess.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        binary.try_wait().expect("try_wait").is_none(),
        "binary crashed after Edit config click"
    );
    // The Edit-config flow on Linux: tray spawns `clipboardwire settings`
    // as a subprocess. Confirm at least one such process is alive.
    let settings_pids = pgrep_settings(binary.id());
    assert!(
        !settings_pids.is_empty(),
        "no `clipboardwire settings` subprocess was spawned by the tray"
    );
    // Tidy up the dialog before the next step — leaving a window open
    // doesn't break the test but is polite.
    for pid in &settings_pids {
        let _ = std::process::Command::new("kill")
            .arg("-TERM")
            .arg(pid.to_string())
            .status();
    }

    // --- Reload config: with no valid config on disk yet, this exercises
    //     the "config still invalid after reload" branch. The binary
    //     must keep running.
    activate(&menu_proxy, &layout.1, "Reload config").await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        binary.try_wait().expect("try_wait").is_none(),
        "binary crashed after Reload config click on an invalid config"
    );

    // --- Quit: terminates the process ---
    activate(&menu_proxy, &layout.1, "Quit clipboardwire").await;
    let exited_cleanly = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match binary.try_wait() {
                Ok(Some(_)) => return true,
                Ok(None) => tokio::time::sleep(Duration::from_millis(100)).await,
                Err(_) => return false,
            }
        }
    })
    .await
    .unwrap_or(false);

    if !exited_cleanly {
        cleanup_binary(&mut binary, &tmp).await;
        panic!("clipboardwire did not exit after Quit menu item activation");
    }

    let _ = std::fs::remove_dir_all(&tmp);
}

/// Activate the menu item with the given label via
/// `com.canonical.dbusmenu.Event(<id>, "clicked", ...)`.
async fn activate(menu_proxy: &zbus::Proxy<'_>, root: &MenuNode, label: &str) {
    let id =
        find_id_by_label(root, label).unwrap_or_else(|| panic!("menu item `{label}` not found"));
    let timestamp: u32 = 0;
    menu_proxy
        .call::<_, _, ()>("Event", &(id, "clicked", Value::from(0i32), timestamp))
        .await
        .unwrap_or_else(|e| panic!("dbusmenu.Event for `{label}`: {e}"));
}

/// Find PIDs of `clipboardwire` processes whose argv contains `settings`
/// and whose parent is the binary we spawned. Cheap and Linux-specific.
fn pgrep_settings(parent_pid: u32) -> Vec<u32> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return out;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Ok(pid) = name.parse::<u32>() else {
            continue;
        };
        let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).ok();
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok();
        let Some(cmdline) = cmdline else { continue };
        let argv = String::from_utf8_lossy(&cmdline);
        if !argv.contains("clipboardwire") || !argv.contains("settings") {
            continue;
        }
        // Parse parent pid from /proc/<pid>/stat: format is
        // `pid (comm) state ppid ...`. We need to skip the comm field
        // because process names can contain spaces.
        if let Some(stat) = stat {
            if let Some(close) = stat.rfind(')') {
                let rest: Vec<&str> = stat[close + 1..].split_whitespace().collect();
                if let Some(ppid_str) = rest.get(1) {
                    if let Ok(ppid) = ppid_str.parse::<u32>() {
                        if ppid == parent_pid {
                            out.push(pid);
                        }
                    }
                }
            }
        }
    }
    out
}

/// The recursive dbusmenu layout shape from GetLayout.
type MenuNode = (i32, HashMap<String, OwnedValue>, Vec<OwnedValue>);

fn collect_labels(node: &MenuNode, out: &mut Vec<String>) {
    if let Some(v) = node.1.get("label") {
        if let Ok(s) = String::try_from(v.try_clone().unwrap()) {
            out.push(s.replace('_', "")); // dbusmenu may include accelerator underscores
        }
    }
    for child in &node.2 {
        if let Ok(child_node) = MenuNode::try_from(child.try_clone().unwrap()) {
            collect_labels(&child_node, out);
        }
    }
}

fn find_id_by_label(node: &MenuNode, wanted: &str) -> Option<i32> {
    if let Some(v) = node.1.get("label") {
        if let Ok(s) = String::try_from(v.try_clone().unwrap()) {
            if s.replace('_', "") == wanted {
                return Some(node.0);
            }
        }
    }
    for child in &node.2 {
        if let Ok(child_node) = MenuNode::try_from(child.try_clone().unwrap()) {
            if let Some(id) = find_id_by_label(&child_node, wanted) {
                return Some(id);
            }
        }
    }
    None
}

async fn cleanup_binary(binary: &mut Child, tmp: &std::path::Path) {
    let _ = binary.kill();
    let _ = binary.wait();
    let _ = std::fs::remove_dir_all(tmp);
}
