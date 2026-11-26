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
use tracing::{debug, error, info, trace, warn};

use super::files_clipboard;

/// Default-buffer size for the outbound events channel.
const EVENT_BUFFER: usize = 32;

/// Waits between attempts to write a clip while another program holds the
/// clipboard (Windows only lets one process open it at a time, and
/// clipboard managers, RDP and Office grab it briefly after every copy).
/// About 2.5 s in total before the clip is given up on.
const OCCUPIED_RETRY_DELAYS: [Duration; 6] = [
    Duration::from_millis(50),
    Duration::from_millis(100),
    Duration::from_millis(200),
    Duration::from_millis(400),
    Duration::from_millis(800),
    Duration::from_millis(1000),
];

/// Which code path writes text clips to the OS clipboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TextWriter {
    /// `arboard::Clipboard::set_text` (Windows, macOS, Wayland data-control).
    Arboard,
    /// Our own X11 selection owner. arboard's X11 backend advertises text
    /// targets it won't serve; see [`super::x11_selection`].
    #[cfg(target_os = "linux")]
    X11,
}

/// Mirror arboard's own Linux backend choice (made in
/// `arboard::Clipboard::new`): Wayland data-control when a Wayland session
/// offers it, X11 otherwise. Only arboard's X11 text path is broken, so
/// that's the only case we take over.
#[cfg(target_os = "linux")]
fn pick_text_writer(wayland_session: bool, data_control: impl FnOnce() -> bool) -> TextWriter {
    if wayland_session && data_control() {
        TextWriter::Arboard
    } else {
        TextWriter::X11
    }
}

#[cfg(target_os = "linux")]
fn detect_text_writer() -> TextWriter {
    pick_text_writer(std::env::var_os("WAYLAND_DISPLAY").is_some(), || {
        wl_clipboard_rs::utils::is_primary_selection_supported().is_ok()
    })
}

#[cfg(not(target_os = "linux"))]
fn detect_text_writer() -> TextWriter {
    TextWriter::Arboard
}

fn write_text(
    cb: &mut arboard::Clipboard,
    writer: TextWriter,
    text: &str,
) -> Result<(), arboard::Error> {
    match writer {
        TextWriter::Arboard => cb.set_text(text),
        #[cfg(target_os = "linux")]
        TextWriter::X11 => {
            super::x11_selection::store_text(text).map_err(|e| arboard::Error::Unknown {
                description: format!("{e:#}"),
            })
        }
    }
}

/// Run a clipboard write, retrying it after each of `delays` while it fails
/// with [`arboard::Error::ClipboardOccupied`]. Any other error is returned
/// at once.
fn retry_while_occupied<T>(
    delays: &[Duration],
    mut op: impl FnMut() -> Result<T, arboard::Error>,
) -> Result<T, arboard::Error> {
    let mut delays = delays.iter();
    loop {
        match op() {
            Err(arboard::Error::ClipboardOccupied) => match delays.next() {
                Some(delay) => {
                    debug!(retry_in = ?delay, "clipboard held by another program; retrying");
                    std::thread::sleep(*delay);
                }
                None => return Err(arboard::Error::ClipboardOccupied),
            },
            other => return other,
        }
    }
}

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
    let text_writer = detect_text_writer();
    debug!(poll_ms, ?text_writer, "clipboard thread started");

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
                    text_writer,
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
    text_writer: TextWriter,
    change: ClipChange,
    last_text: &mut Option<String>,
    last_image: &mut Option<ImageBytes>,
    last_files: &mut Option<Vec<PathBuf>>,
) {
    match change {
        ClipChange::Text(text) => {
            trace!(kind = "text", len = text.len(), "applying clip");
            if let Err(e) = retry_while_occupied(&OCCUPIED_RETRY_DELAYS, || {
                write_text(cb, text_writer, &text)
            }) {
                warn!(error = %e, "clipboard set_text failed");
            } else {
                info!(
                    kind = "text",
                    bytes = text.len(),
                    "applied remote clip to clipboard"
                );
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
                bytes: std::borrow::Cow::Borrowed(&img.rgba),
            };
            if let Err(e) = retry_while_occupied(&OCCUPIED_RETRY_DELAYS, || {
                cb.set_image(arboard_image.clone())
            }) {
                warn!(error = %e, "clipboard set_image failed");
            } else {
                info!(
                    kind = "image",
                    width = img.width,
                    height = img.height,
                    "applied remote clip to clipboard"
                );
                *last_image = Some(img);
            }
        }
        ClipChange::Files(paths) => {
            trace!(kind = "files", count = paths.len(), "applying clip");
            match files_clipboard::write_files(&paths) {
                Ok(()) => {
                    info!(
                        kind = "files",
                        count = paths.len(),
                        "applied remote clip to clipboard"
                    );
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
        }
        Ok(_) => {}
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

    const NO_WAIT: [Duration; 3] = [Duration::ZERO; 3];

    #[cfg(target_os = "linux")]
    #[test]
    fn text_goes_through_x11_without_a_wayland_session() {
        let probed = std::cell::Cell::new(false);
        let writer = pick_text_writer(false, || {
            probed.set(true);
            true
        });
        assert_eq!(writer, TextWriter::X11);
        assert!(!probed.get(), "no point probing Wayland without a session");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn text_goes_through_x11_when_arboard_falls_back_to_it() {
        // GNOME: a Wayland session without the data-control protocol.
        assert_eq!(pick_text_writer(true, || false), TextWriter::X11);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn text_goes_through_arboard_on_wayland_data_control() {
        // KDE, sway, …: arboard talks Wayland natively and serves text fine.
        assert_eq!(pick_text_writer(true, || true), TextWriter::Arboard);
    }

    #[test]
    fn retry_succeeds_once_the_clipboard_is_released() {
        let mut calls = 0;
        let result = retry_while_occupied(&NO_WAIT, || {
            calls += 1;
            if calls < 3 {
                Err(arboard::Error::ClipboardOccupied)
            } else {
                Ok(calls)
            }
        });
        assert_eq!(result.unwrap(), 3);
    }

    #[test]
    fn retry_gives_up_after_the_last_delay() {
        let mut calls = 0;
        let result: Result<(), _> = retry_while_occupied(&NO_WAIT, || {
            calls += 1;
            Err(arboard::Error::ClipboardOccupied)
        });
        assert!(matches!(result, Err(arboard::Error::ClipboardOccupied)));
        assert_eq!(calls, 1 + NO_WAIT.len());
    }

    #[test]
    fn retry_does_not_repeat_other_errors() {
        let mut calls = 0;
        let result: Result<(), _> = retry_while_occupied(&NO_WAIT, || {
            calls += 1;
            Err(arboard::Error::ConversionFailure)
        });
        assert!(matches!(result, Err(arboard::Error::ConversionFailure)));
        assert_eq!(calls, 1);
    }

    #[test]
    fn retry_waits_between_attempts() {
        let delays = [Duration::from_millis(30), Duration::from_millis(30)];
        let start = Instant::now();
        let _: Result<(), _> =
            retry_while_occupied(&delays, || Err(arboard::Error::ClipboardOccupied));
        assert!(start.elapsed() >= Duration::from_millis(60));
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
