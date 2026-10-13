// SPDX-License-Identifier: GPL-3.0-or-later

//! Run only against an isolated Wayland compositor: this changes its clipboard.

#![cfg(target_os = "linux")]

use std::path::PathBuf;
use std::time::Duration;

use clipboardwire_core::client::{clipboard, files_clipboard, ClipChange};
use wl_clipboard_rs::copy::{MimeSource, MimeType, Options, Source};

fn offer(mimes: &[(&str, &str)]) {
    let mut options = Options::new();
    options.omit_additional_text_mime_types(true);
    options
        .copy_multi(
            mimes
                .iter()
                .map(|(mime, body)| MimeSource {
                    source: Source::Bytes(body.as_bytes().into()),
                    mime_type: MimeType::Specific((*mime).into()),
                })
                .collect(),
        )
        .unwrap();
}

async fn next(cb: &mut clipboard::Clipboard) -> ClipChange {
    tokio::time::timeout(Duration::from_secs(3), cb.events_rx.recv())
        .await
        .expect("clipboard event timed out")
        .expect("clipboard thread stopped")
}

async fn quiet(cb: &mut clipboard::Clipboard) {
    assert!(
        tokio::time::timeout(Duration::from_millis(700), cb.events_rx.recv())
            .await
            .is_err(),
        "file clipboard must not be republished as text or echoed"
    );
}

#[tokio::test]
#[ignore = "Changes the Wayland clipboard; use a private test compositor only"]
async fn native_wayland_files_keep_priority_and_do_not_echo() {
    assert!(std::env::var_os("WAYLAND_DISPLAY").is_some());
    let dir = std::env::temp_dir().join(format!("cw-wayland-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&dir).unwrap();
    let paths = vec![dir.join("a b.txt"), dir.join("žluťoučký.pdf")];
    for path in &paths {
        std::fs::write(path, b"Wayland file clipboard test").unwrap();
    }
    let uris = format!(
        "file://{}/a%20b.txt\r\nfile://{}/%C5%BElu%C5%A5ou%C4%8Dk%C3%BD.pdf\r\n",
        dir.display(),
        dir.display()
    );
    let gnome = format!("copy\n{}", uris.replace("\r\n", "\n").trim_end());
    // Nautilus/GTK offer file URLs, their GNOME format, and names as text.
    offer(&[
        ("text/uri-list", &uris),
        ("x-special/gnome-copied-files", &gnome),
        ("text/plain;charset=utf-8", "a b.txt\nžluťoučký.pdf"),
    ]);
    let (mut cb, join) = clipboard::spawn(50).unwrap();
    assert_eq!(next(&mut cb).await, ClipChange::Files(paths.clone()));
    quiet(&mut cb).await;

    // The receiver uses the native Wayland clipboard and retains all files.
    let received: Vec<PathBuf> = vec![dir.join("received one.txt"), dir.join("received two.txt")];
    for path in &received {
        std::fs::write(path, b"Received file clipboard test").unwrap();
    }
    cb.apply_tx
        .send(ClipChange::Files(received.clone()))
        .unwrap();
    quiet(&mut cb).await;
    assert_eq!(files_clipboard::read_files().unwrap(), Some(received));
    let types = wl_clipboard_rs::paste::get_mime_types(
        wl_clipboard_rs::paste::ClipboardType::Regular,
        wl_clipboard_rs::paste::Seat::Unspecified,
    )
    .unwrap();
    assert!(types.contains("text/uri-list"));
    assert!(types.contains("x-special/gnome-copied-files"));
    assert!(!types.contains("text/plain"));
    assert!(!types.contains("text/plain;charset=utf-8"));

    // A text copy resets the file dedup tracker, so copying the same files
    // again later is picked up. Test GNOME-only clipboard offers as well.
    offer(&[("text/plain;charset=utf-8", "reset the file selection")]);
    assert_eq!(
        next(&mut cb).await,
        ClipChange::Text("reset the file selection".into())
    );
    offer(&[("x-special/gnome-copied-files", &gnome)]);
    assert_eq!(next(&mut cb).await, ClipChange::Files(paths));
    quiet(&mut cb).await;

    drop(cb);
    join.join().unwrap();
    std::fs::remove_dir_all(dir).unwrap();
}
