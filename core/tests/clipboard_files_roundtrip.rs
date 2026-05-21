// SPDX-License-Identifier: GPL-3.0-or-later

//! Roundtrip test for the OS file-clipboard adapter.
//!
//! Writes a list of file paths to the system clipboard via
//! [`files_clipboard::write_files`], then reads them back via
//! [`files_clipboard::read_files`] and asserts the round-tripped
//! list matches. This is the only end-to-end coverage of the
//! per-OS NSPasteboard / CF_HDROP / X11 `text/uri-list` backends —
//! the unit tests in the module only validate the parse / encode
//! glue, not the live OS handoff.
//!
//! Marked `#[ignore]` because it touches the *real* user clipboard
//! and parallel test runs would fight. Run explicitly:
//!
//! ```bash
//! # Linux (needs xvfb on a headless box):
//! xvfb-run -a cargo test --test clipboard_files_roundtrip -- --ignored
//!
//! # Windows / macOS (a desktop session is implicit):
//! cargo test --test clipboard_files_roundtrip -- --ignored
//! ```
//!
//! CI runs this on every push under Linux + Windows + macOS so a
//! regression in any OS adapter fails the gate.

use std::path::PathBuf;

use clipboardwire_core::client::files_clipboard;

#[cfg(target_os = "linux")]
fn require_display_or_skip() -> bool {
    if std::env::var_os("DISPLAY").is_none() && std::env::var_os("WAYLAND_DISPLAY").is_none() {
        eprintln!("no display; skipping (run under xvfb-run)");
        return false;
    }
    true
}

#[cfg(not(target_os = "linux"))]
fn require_display_or_skip() -> bool {
    true
}

fn fixture_paths() -> Vec<PathBuf> {
    // Use real existing paths so the OS treats them as legitimate
    // file targets — on macOS NSURL.fileURLWithPath cleans up
    // relative paths, and on Linux some file managers verify the
    // path exists before honoring a paste.
    let dir = std::env::temp_dir();
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p1 = dir.join(format!("cw-clip-roundtrip-{pid}-{nanos}-a.txt"));
    let p2 = dir.join(format!("cw-clip-roundtrip-{pid}-{nanos}-b.txt"));
    std::fs::write(&p1, b"a").unwrap();
    std::fs::write(&p2, b"b").unwrap();
    vec![p1, p2]
}

#[test]
#[ignore = "Touches the live OS clipboard. Run via -- --ignored only."]
fn write_then_read_round_trips_via_os_clipboard() {
    if !require_display_or_skip() {
        return;
    }

    let paths = fixture_paths();

    files_clipboard::write_files(&paths).expect("write_files");

    // On X11, `store` returns once we've registered as the selection
    // owner; on macOS / Windows the write is synchronous. Give the
    // system a brief moment to settle — paste-from-clipboard
    // sometimes lags the "set" call on slow CI runners.
    std::thread::sleep(std::time::Duration::from_millis(150));

    let got = files_clipboard::read_files()
        .expect("read_files")
        .expect("clipboard should hold the files we just set");

    // Compare the canonicalised paths because:
    // - macOS NSURL strips trailing slashes and normalises segments.
    // - Linux x11-clipboard may roundtrip via percent-encoding.
    // Both produce equivalent paths, just different lexical forms.
    let canon = |v: &[PathBuf]| -> Vec<PathBuf> {
        v.iter()
            .map(|p| std::fs::canonicalize(p).unwrap_or_else(|_| p.clone()))
            .collect()
    };
    assert_eq!(
        canon(&got),
        canon(&paths),
        "round-tripped paths differ — set: {paths:?}, got: {got:?}"
    );

    for p in &paths {
        let _ = std::fs::remove_file(p);
    }
}
