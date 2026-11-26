// SPDX-License-Identifier: GPL-3.0-or-later

//! A text clip written on Linux/X11 must be readable in every text format
//! the clipboard owner advertises.
//!
//! Regression test: arboard's X11 backend stores text only as
//! `UTF8_STRING` but also advertises `text/plain;charset=utf-8` and
//! `text/plain;charset=UTF-8`, and refuses requests for those two. On
//! GNOME/Wayland, Mutter exposes the advertised X11 targets to Wayland
//! clients as MIME types; Firefox picked one of the broken ones, so a clip
//! received from another machine pasted fine in a terminal but pasted
//! nothing in Firefox.
//!
//! Needs an X server and `xclip`, and must not run against the user's
//! real clipboard, hence `#[ignore]`. CI runs it as:
//!
//! ```text
//! env -u WAYLAND_DISPLAY xvfb-run -a cargo test -p clipboardwire-core \
//!     --test x11_text_targets -- --ignored
//! ```
#![cfg(target_os = "linux")]

use std::process::Command;
use std::time::{Duration, Instant};

use clipboardwire_core::client::clipboard::{self, ClipChange};

const TEXT: &str = "ñandú → clipboardwire";

/// Read the CLIPBOARD selection as `target` with a separate X client.
fn xclip_read(target: &str) -> Result<Vec<u8>, String> {
    let out = Command::new("xclip")
        .args(["-o", "-selection", "clipboard", "-t", target])
        .output()
        .expect("xclip must be installed to run this test");
    if out.status.success() {
        Ok(out.stdout)
    } else {
        Err(format!(
            "{target}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

#[test]
#[ignore = "needs an isolated X server (xvfb-run) and xclip; see module docs"]
fn every_advertised_text_target_returns_the_text() {
    assert!(
        std::env::var_os("WAYLAND_DISPLAY").is_none(),
        "unset WAYLAND_DISPLAY so the X11 backend is exercised"
    );
    let (mut cb, _join) = clipboard::spawn(50).unwrap();
    cb.apply_tx
        .send(ClipChange::Text(TEXT.to_string()))
        .unwrap();

    // Wait until the clipboard thread has taken ownership.
    let deadline = Instant::now() + Duration::from_secs(3);
    let targets = loop {
        if let Ok(raw) = xclip_read("TARGETS") {
            if !raw.is_empty() {
                break String::from_utf8(raw).unwrap();
            }
        }
        assert!(Instant::now() < deadline, "clip was never written");
        std::thread::sleep(Duration::from_millis(20));
    };
    let text_targets: Vec<&str> = targets
        .lines()
        .filter(|t| !matches!(*t, "TARGETS" | "SAVE_TARGETS" | "TIMESTAMP" | "MULTIPLE"))
        .collect();
    assert!(
        text_targets.contains(&"UTF8_STRING"),
        "UTF8_STRING not advertised: {targets:?}"
    );

    let failures: Vec<String> = text_targets
        .iter()
        .filter_map(|t| match xclip_read(t) {
            Ok(got) if got == TEXT.as_bytes() => None,
            Ok(got) => Some(format!(
                "{t}: wrong data {:?}",
                String::from_utf8_lossy(&got)
            )),
            Err(e) => Some(e),
        })
        .collect();
    assert!(
        failures.is_empty(),
        "advertised targets that don't return the text: {failures:?}"
    );

    // Writing a remote clip must not be reported back as a local copy.
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        cb.events_rx.try_recv().is_err(),
        "applied clip echoed back as a local change"
    );
}
