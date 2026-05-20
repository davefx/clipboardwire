// SPDX-License-Identifier: GPL-3.0-or-later

//! Tier-3 Windows interaction harness for the tray UI.
//!
//! Wakes up `clipboardwire connect --tray`, then uses Windows UI
//! Automation to discover the tray icon by its tooltip ("Name" in the
//! AutomationElement). Catches the broad class of "tray-icon stopped
//! showing up after a refactor" regressions on Windows.
//!
//! Win 11 detail: newly-registered tray icons hide in the "Show Hidden
//! Icons" overflow popup by default. The first walk through the visible
//! tree won't find them; we invoke the overflow chevron and walk again.
//!
//! **This test only works on a real Windows desktop session.** It
//! cross-compiles under wine for build coverage, but wine's UIA
//! implementation enumerates an empty desktop tree, so the test is
//! effectively unrunnable there (`UIAutomation::new()` succeeds; the
//! walk returns zero non-empty names). Runs in CI on `windows-latest`
//! where a real user session exists.

#![cfg(windows)]

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use uiautomation::UIAutomation;

const ICON_TOOLTIP_PREFIX: &str = "clipboardwire";
const OVERFLOW_BUTTON_NAMES: &[&str] = &[
    // Win 11 (current)
    "Show Hidden Icons",
    // Win 10 fallback
    "Notification Chevron",
    "Show hidden icons",
];
/// Total wall-clock budget for the whole discovery: spawn → look in
/// visible tray → invoke overflow → look again.
const SEARCH_DEADLINE: Duration = Duration::from_secs(30);
/// Max recursion depth when walking the UIA tree.
const MAX_DEPTH: u32 = 10;

#[test]
#[ignore = "Windows UI Automation; real Windows desktop session required."]
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

    // Give Windows a moment to wire the new NotifyIcon into the
    // notification area.
    std::thread::sleep(Duration::from_secs(2));

    if let Some(status) = child.try_wait().expect("try_wait") {
        panic!("clipboardwire exited prematurely with status {status:?}");
    }

    let automation = UIAutomation::new().expect("UIAutomation::new");

    let mut hits: Vec<String> = Vec::new();
    let mut visible_sample: Vec<String> = Vec::new();
    let mut overflow_sample: Vec<String> = Vec::new();
    let mut tried_overflow = false;

    let deadline = Instant::now() + SEARCH_DEADLINE;
    while Instant::now() < deadline {
        // First pass: look through everything that's currently visible.
        let root = automation.get_root_element().expect("root element");
        hits.clear();
        let mut new_sample: Vec<String> = Vec::new();
        walk(&automation, &root, 0, MAX_DEPTH, &mut |elem| {
            if let Ok(name) = elem.get_name() {
                if !name.is_empty() {
                    if name.contains(ICON_TOOLTIP_PREFIX) {
                        hits.push(name.clone());
                    }
                    if new_sample.len() < 50 {
                        new_sample.push(name);
                    }
                }
            }
        });

        if !hits.is_empty() {
            break;
        }

        if !tried_overflow {
            visible_sample = new_sample.clone();
            // Try expanding the "Show Hidden Icons" overflow popup.
            // On Win 11 new tray icons are auto-hidden there by default.
            if try_invoke_overflow(&automation) {
                tried_overflow = true;
                // Let the popup paint + UIA tree update.
                std::thread::sleep(Duration::from_millis(800));
                continue;
            }
            // No overflow button visible — either we're on an older
            // Windows that shows everything by default, or the chevron
            // is missing. Either way, retrying won't help.
            break;
        } else {
            overflow_sample = new_sample;
        }

        std::thread::sleep(Duration::from_millis(500));
    }

    let _ = child.kill();
    let _ = child.wait();

    if hits.is_empty() {
        panic!(
            "no AutomationElement with a name containing `{ICON_TOOLTIP_PREFIX}` \
             was discoverable within {}s.\n\
             tried_overflow: {tried_overflow}\n\
             visible sample (first 50 non-empty names): {visible_sample:?}\n\
             overflow sample (first 50 non-empty names): {overflow_sample:?}",
            SEARCH_DEADLINE.as_secs(),
        );
    }
    eprintln!("found matching AutomationElement(s) (tried overflow: {tried_overflow}): {hits:?}");
}

/// Find the "Show Hidden Icons" element (or its Win 10 equivalent) and
/// invoke it. Returns `true` if we invoked something, `false` if we
/// couldn't find a candidate at all.
fn try_invoke_overflow(automation: &UIAutomation) -> bool {
    let root = automation
        .get_root_element()
        .expect("root element for overflow search");
    let mut target: Option<uiautomation::UIElement> = None;
    walk(automation, &root, 0, MAX_DEPTH, &mut |elem| {
        if target.is_some() {
            return;
        }
        let name = elem.get_name().unwrap_or_default();
        if OVERFLOW_BUTTON_NAMES
            .iter()
            .any(|n| name.eq_ignore_ascii_case(n))
            || name.to_lowercase().contains("hidden icons")
        {
            target = Some(elem.clone());
        }
    });

    let Some(elem) = target else {
        eprintln!("could not locate the 'Show Hidden Icons' chevron in UIA tree");
        return false;
    };
    eprintln!(
        "invoking overflow element: name=`{}`, class=`{}`",
        elem.get_name().unwrap_or_default(),
        elem.get_classname().unwrap_or_default()
    );

    // Prefer InvokePattern (semantically a click), fall back to
    // physically clicking the element's bounding rect.
    if let Ok(invoke) = elem.get_pattern::<uiautomation::patterns::UIInvokePattern>() {
        if let Err(e) = invoke.invoke() {
            eprintln!("InvokePattern.invoke() failed: {e}; falling back to click()");
            let _ = elem.click();
        }
    } else if let Err(e) = elem.click() {
        eprintln!("click() failed too: {e}");
        return false;
    }
    true
}

/// `env!("CARGO_BIN_EXE_clipboardwire")` expands to a filesystem path
/// at compile time. When the test binary itself runs under wine, that
/// bare Linux path can't be passed to `CreateProcess` directly — wine
/// needs a `Z:\…` form. Detect that and rewrite. On real Windows the
/// path is already absolute Windows-style.
fn resolve_binary_path() -> String {
    let raw = env!("CARGO_BIN_EXE_clipboardwire");
    if raw.starts_with('/') {
        format!("Z:{}", raw.replace('/', "\\"))
    } else {
        raw.to_string()
    }
}

/// Depth-bounded recursive walk over the AutomationElement tree. The
/// desktop tree on Windows is wide and deep; we cap at `max_depth` to
/// keep enumeration time bounded.
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
