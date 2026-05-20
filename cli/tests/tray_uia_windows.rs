// SPDX-License-Identifier: GPL-3.0-or-later

//! Tier-3 Windows interaction harness for the tray UI.
//!
//! Wakes up `clipboardwire connect --tray`, then uses Windows UI
//! Automation to discover the tray icon by its tooltip ("Name" in the
//! AutomationElement). Catches the broad class of "tray-icon stopped
//! showing up after a refactor" regressions on Windows.
//!
//! **This test only works on a real Windows desktop session.** It
//! cross-compiles under wine for build coverage, but wine's UIA
//! implementation enumerates an empty desktop tree, so the test is
//! effectively unrunnable there (`UIAutomation::new()` succeeds; the
//! walk returns zero non-empty names). It runs in CI on the
//! `windows-latest` runner where a real user session exists.
//!
//! Local-dev cross-compile sanity:
//!   cargo build --target x86_64-pc-windows-gnu --tests
//!
//! CI invocation (windows-latest only):
//!   cargo test --workspace -- --ignored tray_uia

#![cfg(windows)]

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use uiautomation::UIAutomation;

const ICON_TOOLTIP_PREFIX: &str = "clipboardwire";
const SEARCH_DEADLINE: Duration = Duration::from_secs(15);

#[test]
#[ignore = "Windows UI Automation; runs under wine via the cargo runner."]
fn tray_icon_is_registered_and_discoverable() {
    let exe = resolve_binary_path();
    eprintln!("spawning: {exe}");
    let mut child = match Command::new(&exe)
        .args(["connect", "--tray"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => panic!("could not spawn clipboardwire at `{exe}`: {e}"),
    };

    // Wine takes a moment to initialise the desktop + notification area.
    std::thread::sleep(Duration::from_secs(2));

    // Did the child exit prematurely?
    if let Some(status) = child.try_wait().expect("try_wait") {
        panic!("clipboardwire exited prematurely with status {status:?}");
    }

    let automation = UIAutomation::new().expect("UIAutomation::new");

    let deadline = Instant::now() + SEARCH_DEADLINE;
    let mut hits: Vec<String> = Vec::new();
    let mut sample: Vec<String> = Vec::new();
    let mut found = false;
    while Instant::now() < deadline {
        let root = automation.get_root_element().expect("root element");
        hits.clear();
        sample.clear();
        walk(&automation, &root, 0, 6, &mut |elem| {
            if let Ok(name) = elem.get_name() {
                if name.contains(ICON_TOOLTIP_PREFIX) {
                    hits.push(name.clone());
                }
                if sample.len() < 30 && !name.is_empty() {
                    sample.push(name);
                }
            }
        });
        if !hits.is_empty() {
            found = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(500));
    }

    let _ = child.kill();
    let _ = child.wait();

    if !found {
        panic!(
            "no AutomationElement with a name containing `{ICON_TOOLTIP_PREFIX}` \
             was discoverable within {}s\n\
             sample of element names seen (up to 30): {sample:?}",
            SEARCH_DEADLINE.as_secs()
        );
    }
    eprintln!("found matching AutomationElement(s): {hits:?}");
}

/// `env!("CARGO_BIN_EXE_clipboardwire")` expands to a Linux filesystem
/// path at compile time. When the test binary itself runs under wine,
/// that bare Linux path can't be passed to `CreateProcess` directly —
/// wine needs a `Z:\...` style. Detect that and rewrite.
fn resolve_binary_path() -> String {
    let raw = env!("CARGO_BIN_EXE_clipboardwire");
    if raw.starts_with('/') {
        // Inside a wine process. Map / → Z:\ and convert separators.
        format!("Z:{}", raw.replace('/', "\\"))
    } else {
        raw.to_string()
    }
}

/// Depth-bounded recursive walk over the AutomationElement tree.
/// Wine + UIA can be slow to fully enumerate (and the desktop tree is
/// deep), so cap recursion at `max_depth`.
fn walk(
    automation: &UIAutomation,
    element: &uiautomation::UIElement,
    depth: u32,
    max_depth: u32,
    visit: &mut impl FnMut(&uiautomation::UIElement),
) {
    visit(element);
    if depth >= max_depth {
        return;
    }
    let walker = match automation.get_control_view_walker() {
        Ok(w) => w,
        Err(_) => return,
    };
    if let Ok(first_child) = walker.get_first_child(element) {
        let mut child = first_child;
        loop {
            walk(automation, &child, depth + 1, max_depth, visit);
            match walker.get_next_sibling(&child) {
                Ok(next) => child = next,
                Err(_) => break,
            }
        }
    }
}
