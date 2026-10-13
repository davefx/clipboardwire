// SPDX-License-Identifier: GPL-3.0-or-later

//! Dedicated-thread clipboard adapter.
//!
//! The `arboard::Clipboard` handle is created and owned by a single OS thread
//! (X11 / Wayland / Win32 / NSPasteboard all benefit from a stable owner).
//! The thread runs a single hybrid loop that does two things on each tick:
//!
//! 1. **Apply** any pending write requests (from the supervisor).
//! 2. **Poll** the local clipboard for changes and emit any new value.
//!
//! v0.2 polls both text and image clipboard. Each kind has its own
//! `last_seen` tracker so a copy of text doesn't clobber the image cache
//! (or vice versa).
//!
//! Both directions are exposed as channels so the supervisor is portable and
//! easily mockable: tests construct a [`Clipboard`] directly with their own
//! channel ends, without ever spawning the real arboard thread.
//!
//! Echo-loop suppression: when we apply an inbound clip, we update the
//! thread's `last_seen_*` tracker to the value we just wrote. The next poll
//! won't see it as "new" and so won't bounce it back to the hub.

use std::path::PathBuf;
use std::sync::mpsc as smpsc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::Result;
use tokio::sync::mpsc;
use tracing::{debug, error, trace, warn};

use super::files_clipboard;

/// Default-buffer size for the outbound events channel.
const EVENT_BUFFER: usize = 32;

/// Owned RGBA image, in row-major order (4 bytes per pixel, R-G-B-A).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageBytes {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// A change to the local clipboard, emitted by the poll loop and accepted
/// by the apply path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClipChange {
    Text(String),
    Image(ImageBytes),
    /// A list of local file paths the user copied (e.g. selected
    /// files in Nautilus / Explorer / Finder + Ctrl+C). On the
    /// sender, the supervisor turns each path into a sequence of
    /// `file_chunk` frames. On the receiver, the clipboard adapter
    /// sets the OS clipboard to these paths so a `Ctrl+V` in the
    /// peer's file manager pastes the saved files.
    Files(Vec<std::path::PathBuf>),
}

/// Async-side handle to a clipboard:
/// - `events_rx` yields the local clipboard's content whenever it changes.
/// - `apply_tx` requests that a value be written to the local clipboard.
///
/// Construct via [`spawn`] for the production arboard-backed loop, or
/// directly (the fields are `pub`) for tests.
pub struct Clipboard {
    pub events_rx: mpsc::Receiver<ClipChange>,
    pub apply_tx: smpsc::Sender<ClipChange>,
}

/// Spawn the arboard-backed clipboard thread. Returns the [`Clipboard`] handle
/// and a [`JoinHandle`] so callers can wait on graceful shutdown.
pub fn spawn(poll_ms: u64) -> Result<(Clipboard, JoinHandle<()>)> {
    let (apply_tx, apply_rx) = smpsc::channel::<ClipChange>();
    let (events_tx, events_rx) = mpsc::channel::<ClipChange>(EVENT_BUFFER);
    let join = std::thread::Builder::new()
        .name("clipboardwire-clipboard".into())
        .spawn(move || run_arboard_loop(apply_rx, events_tx, poll_ms))?;
    Ok((
        Clipboard {
            events_rx,
            apply_tx,
        },
        join,
    ))
}

fn run_arboard_loop(
    apply_rx: smpsc::Receiver<ClipChange>,
    events_tx: mpsc::Sender<ClipChange>,
    poll_ms: u64,
) {
    let mut cb = match arboard::Clipboard::new() {
        Ok(c) => c,
        Err(e) => {
            error!(error = %e, "arboard initialization failed; clipboard thread exiting");
            return;
        }
    };
    debug!(poll_ms, "clipboard thread started");

    // Per-kind last-seen trackers — copying text doesn't reset the image
    // cache, so each medium needs its own slot.
    let mut last_text: Option<String> = None;
    let mut last_image: Option<ImageBytes> = None;
    let mut last_files: Option<Vec<PathBuf>> = None;

    let poll = Duration::from_millis(poll_ms);
    let mut next_poll = Instant::now() + poll;

    loop {
        let now = Instant::now();
        let wait = next_poll.saturating_duration_since(now);

        match apply_rx.recv_timeout(wait) {
            Ok(change) => {
                apply_change(
                    &mut cb,
                    change,
                    &mut last_text,
                    &mut last_image,
                    &mut last_files,
                );
                continue;
            }
            Err(smpsc::RecvTimeoutError::Timeout) => {}
            Err(smpsc::RecvTimeoutError::Disconnected) => {
                debug!("apply channel closed; clipboard thread exiting");
                return;
            }
        }

        next_poll = Instant::now() + poll;

        if let Some(change) =
            poll_clipboard(&mut cb, &mut last_text, &mut last_image, &mut last_files)
        {
            match events_tx.try_send(change) {
                Ok(()) => {}
                Err(mpsc::error::TrySendError::Full(_)) => {
                    warn!("events channel full; dropping clipboard change");
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    debug!("events channel closed; clipboard thread exiting");
                    return;
                }
            }
        }
    }
}

fn apply_change(
    cb: &mut arboard::Clipboard,
    change: ClipChange,
    last_text: &mut Option<String>,
    last_image: &mut Option<ImageBytes>,
    last_files: &mut Option<Vec<PathBuf>>,
) {
    match change {
        ClipChange::Text(text) => {
            trace!(kind = "text", len = text.len(), "applying clip");
            if let Err(e) = cb.set_text(text.clone()) {
                warn!(error = %e, "clipboard set_text failed");
            } else {
                *last_text = Some(text);
            }
        }
        ClipChange::Image(img) => {
            trace!(
                kind = "image",
                width = img.width,
                height = img.height,
                "applying clip"
            );
            let arboard_image = arboard::ImageData {
                width: img.width as usize,
                height: img.height as usize,
                bytes: std::borrow::Cow::Owned(img.rgba.clone()),
            };
            if let Err(e) = cb.set_image(arboard_image) {
                warn!(error = %e, "clipboard set_image failed");
            } else {
                *last_image = Some(img);
            }
        }
        ClipChange::Files(paths) => {
            trace!(kind = "files", count = paths.len(), "applying clip");
            match files_clipboard::write_files(&paths) {
                Ok(()) => {
                    // Echo suppression: when our own poll next runs
                    // it'll find these exact paths and skip them.
                    *last_files = Some(paths);
                }
                Err(e) => {
                    warn!(error = %format!("{e:#}"), "clipboard set_files failed");
                }
            }
        }
    }
}

fn poll_clipboard(
    cb: &mut arboard::Clipboard,
    last_text: &mut Option<String>,
    last_image: &mut Option<ImageBytes>,
    last_files: &mut Option<Vec<PathBuf>>,
) -> Option<ClipChange> {
    // Files first: they're the most explicit "user intent to share"
    // (Ctrl+C in a file manager) and have higher priority than a
    // stray text selection that may also be on the clipboard.
    match files_clipboard::read_files() {
        Ok(Some(paths)) if !paths.is_empty() => {
            if last_files.as_deref() != Some(paths.as_slice()) {
                *last_files = Some(paths.clone());
                return Some(ClipChange::Files(paths));
            }
            // File managers also offer the names as plain text. Once the
            // files are sent (or applied), do not overwrite the peer's file
            // clipboard with those names on the next poll.
            return None;
        }
        Ok(_) => *last_files = None,
        Err(e) => {
            trace!(error = %format!("{e:#}"), "files_clipboard read transient failure");
        }
    }

    // Image: on most platforms the image clipboard slot is distinct
    // from the text slot, and screenshots are the common "I want this on
    // another machine" case. We still check text every tick.
    match cb.get_image() {
        Ok(img) => {
            let owned = ImageBytes {
                width: img.width as u32,
                height: img.height as u32,
                rgba: img.bytes.into_owned(),
            };
            if last_image.as_ref() != Some(&owned) {
                *last_image = Some(owned.clone());
                return Some(ClipChange::Image(owned));
            }
        }
        Err(arboard::Error::ContentNotAvailable) => {
            // No image on the clipboard right now. Don't clear last_image:
            // a *text* copy doesn't invalidate the prior image, and on
            // X11/Wayland the image slot may transiently fail to read.
        }
        Err(e) => {
            trace!(error = %e, "clipboard get_image transient failure");
        }
    }

    match cb.get_text() {
        Ok(text) => {
            if last_text.as_deref() != Some(text.as_str()) {
                *last_text = Some(text.clone());
                return Some(ClipChange::Text(text));
            }
        }
        Err(arboard::Error::ContentNotAvailable) => {
            // Empty or non-text clipboard.
            *last_text = None;
        }
        Err(e) => {
            trace!(error = %e, "clipboard get_text transient failure");
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    // The arboard backend can't be exercised without a display; these tests
    // construct Clipboard directly with test-controlled channels.

    #[tokio::test]
    async fn supervisor_receives_text_events() {
        let (apply_tx, _apply_rx) = smpsc::channel::<ClipChange>();
        let (events_tx, events_rx) = mpsc::channel::<ClipChange>(4);
        let cb = Clipboard {
            events_rx,
            apply_tx,
        };

        events_tx
            .send(ClipChange::Text("hello".to_string()))
            .await
            .unwrap();
        drop(events_tx);

        let mut cb = cb;
        match cb.events_rx.recv().await.unwrap() {
            ClipChange::Text(t) => assert_eq!(t, "hello"),
            other => panic!("expected text change, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn supervisor_receives_image_events() {
        let (apply_tx, _apply_rx) = smpsc::channel::<ClipChange>();
        let (events_tx, events_rx) = mpsc::channel::<ClipChange>(4);
        let cb = Clipboard {
            events_rx,
            apply_tx,
        };

        let img = ImageBytes {
            width: 2,
            height: 1,
            rgba: vec![255, 0, 0, 255, 0, 255, 0, 255],
        };
        events_tx
            .send(ClipChange::Image(img.clone()))
            .await
            .unwrap();
        drop(events_tx);

        let mut cb = cb;
        match cb.events_rx.recv().await.unwrap() {
            ClipChange::Image(got) => assert_eq!(got, img),
            other => panic!("expected image change, got {other:?}"),
        }
    }

    #[test]
    fn supervisor_can_send_apply_requests_for_both_kinds() {
        let (apply_tx, apply_rx) = smpsc::channel::<ClipChange>();
        let (_events_tx, events_rx) = mpsc::channel::<ClipChange>(4);
        let cb = Clipboard {
            events_rx,
            apply_tx,
        };

        cb.apply_tx
            .send(ClipChange::Text("write me".into()))
            .unwrap();
        cb.apply_tx
            .send(ClipChange::Image(ImageBytes {
                width: 1,
                height: 1,
                rgba: vec![1, 2, 3, 4],
            }))
            .unwrap();
        match apply_rx.recv().unwrap() {
            ClipChange::Text(t) => assert_eq!(t, "write me"),
            other => panic!("expected text, got {other:?}"),
        }
        match apply_rx.recv().unwrap() {
            ClipChange::Image(img) => {
                assert_eq!(img.width, 1);
                assert_eq!(img.rgba, vec![1, 2, 3, 4]);
            }
            other => panic!("expected image, got {other:?}"),
        }
    }
}
