// SPDX-License-Identifier: GPL-3.0-or-later

//! Clipboard client: arboard-driven poll loop, WebSocket transport,
//! supervisor that ties them together with echo-loop suppression.

pub mod clipboard;
pub mod config;
pub mod file;
pub mod files_clipboard;
pub mod tls;
pub mod transport;
#[cfg(target_os = "linux")]
mod x11_selection;

use anyhow::{Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::protocol::{is_text_content_type, ClipFrame, IMAGE_PNG_CONTENT_TYPE, TEXT_CONTENT_TYPE};

pub use clipboard::{ClipChange, Clipboard, ImageBytes};
pub use config::ClientConfig;
pub use transport::{ClientStatus, Transport};

/// Run the clipboard client until the process is signalled or an unrecoverable
/// error occurs. Spawns the arboard thread, the transport task, and bridges
/// them in a single select loop.
pub async fn run(config: ClientConfig) -> Result<()> {
    run_with_status(config, None).await
}

/// Variant of [`run`] that emits [`ClientStatus`] transitions to the
/// provided `watch::Sender`. The tray uses this to surface connection
/// state in the menu and tooltip.
pub async fn run_with_status(
    config: ClientConfig,
    status_tx: Option<tokio::sync::watch::Sender<ClientStatus>>,
) -> Result<()> {
    let poll_ms = config.poll_ms;
    let (clipboard, _clipboard_join) = clipboard::spawn(poll_ms)?;
    let (transport, _transport_join) = transport::spawn_with_status(config, status_tx);
    run_supervisor(clipboard, transport).await;
    Ok(())
}

/// Bridge clipboard events ↔ transport frames. Public for the integration
/// tests, which inject test-controlled [`Clipboard`] and [`Transport`] handles
/// without spawning the real arboard thread or hitting the network.
pub async fn run_supervisor(mut clipboard: Clipboard, mut transport: Transport) {
    let mut file_receiver = match file::FileReceiver::new() {
        Ok(r) => Some(r),
        Err(e) => {
            warn!(
                error = %format!("{e:#}"),
                "could not initialise the file-transfer receiver; inbound files will be dropped"
            );
            None
        }
    };

    // Receiver-side multi-file batching. When the sender does a single
    // Ctrl+C of N files, the chunks arrive interleaved and each file
    // completes at a different moment. Without grouping, the OS
    // clipboard would only ever hold the *last* finished file.
    //
    // Strategy: accumulate completed paths in `pending_files`, set a
    // deadline `RECEIVE_BATCH_WINDOW` after each completion, and only
    // apply when the deadline expires without another arrival. The
    // window is wide enough to ride out the inter-file gap in a
    // multi-file send (typically a few ms) but short enough that the
    // user doesn't notice a "single file" copy as laggy.
    let mut pending_files: Vec<std::path::PathBuf> = Vec::new();
    let mut pending_deadline: Option<tokio::time::Instant> = None;

    loop {
        // Snapshot for the async closure to avoid borrowing
        // pending_deadline — the closure pins through the select
        // and would otherwise block mutation in the arms below.
        let current_deadline = pending_deadline;
        let deadline_fut = async move {
            match current_deadline {
                Some(d) => tokio::time::sleep_until(d).await,
                None => std::future::pending::<()>().await,
            }
        };
        tokio::pin!(deadline_fut);
        tokio::select! {
            local = clipboard.events_rx.recv() => {
                let Some(change) = local else {
                    debug!("clipboard events channel closed; supervisor exiting");
                    return;
                };
                match change {
                    // Text + image: one frame on the wire each.
                    ClipChange::Text(_) | ClipChange::Image(_) => {
                        let frame = match change_to_frame(change) {
                            Ok(f) => f,
                            Err(e) => {
                                warn!(error = %format!("{e:#}"), "failed to encode local clip; dropping");
                                continue;
                            }
                        };
                        info!(
                            content_type = %frame.content_type,
                            bytes = payload_len(&frame),
                            "publishing local clip"
                        );
                        if let Err(e) = transport.outbound_tx.try_send(frame) {
                            warn!(error = %e, "transport outbound full or closed; dropping local change");
                        }
                    }
                    // Files: multi-chunk transfer per path.
                    ClipChange::Files(paths) => {
                        info!(count = paths.len(), "publishing local files");
                        for path in paths {
                            if let Err(e) = file::send_file_through(
                                &path,
                                &transport.outbound_files_tx,
                            )
                            .await
                            {
                                warn!(
                                    file = %path.display(),
                                    error = %format!("{e:#}"),
                                    "failed to send file from clipboard"
                                );
                            }
                        }
                    }
                }
            }
            remote = transport.inbound_rx.recv() => {
                let Some(clip) = remote else {
                    debug!("transport inbound channel closed; supervisor exiting");
                    return;
                };
                info!(
                    from = %clip.from.map(|u| u.to_string()).unwrap_or_default(),
                    content_type = %clip.content_type,
                    bytes = payload_len(&clip),
                    "received remote clip"
                );
                let change = match frame_to_change(&clip) {
                    Ok(c) => c,
                    Err(FrameRejection::UnsupportedType) => {
                        warn!(
                            content_type = %clip.content_type,
                            "ignoring inbound clip with unsupported content_type"
                        );
                        continue;
                    }
                    Err(FrameRejection::Decode(e)) => {
                        warn!(error = %format!("{e:#}"), "failed to decode inbound clip");
                        continue;
                    }
                };
                if let Err(e) = clipboard.apply_tx.send(change) {
                    warn!(error = %e, "clipboard thread is gone");
                    return;
                }
            }
            file_chunk = recv_maybe(transport.inbound_files_rx.as_mut()) => {
                let Some(chunk) = file_chunk else {
                    // No inbound-files channel wired, or it closed.
                    // Either way, this branch just goes quiet — the
                    // other arms keep the loop alive.
                    continue;
                };
                if let Some(receiver) = file_receiver.as_mut() {
                    match receiver.receive_chunk(chunk) {
                        Ok(Some(path)) => {
                            tracing::info!(file = %path.display(), "saved received file");
                            // Stash for the batch window; the deadline
                            // arm below will apply the whole list at
                            // once when no further file completes for
                            // RECEIVE_BATCH_WINDOW.
                            pending_files.push(path);
                            pending_deadline = Some(
                                tokio::time::Instant::now() + RECEIVE_BATCH_WINDOW,
                            );
                        }
                        Ok(None) => {}
                        Err(e) => {
                            warn!(error = %format!("{e:#}"), "file chunk rejected");
                        }
                    }
                }
            }
            _ = &mut deadline_fut, if pending_deadline.is_some() => {
                pending_deadline = None;
                if !pending_files.is_empty() {
                    let batch = std::mem::take(&mut pending_files);
                    tracing::info!(
                        count = batch.len(),
                        "applying received-files batch to local clipboard"
                    );
                    if let Err(e) = clipboard.apply_tx.send(ClipChange::Files(batch)) {
                        warn!(
                            error = %e,
                            "clipboard thread gone; can't expose received files in clipboard"
                        );
                    }
                }
            }
            else => return,
        }
    }
}

/// Size of a clip's payload as carried on the wire (text bytes, or the
/// base64 length for binary types). Logged instead of the content.
fn payload_len(clip: &ClipFrame) -> usize {
    clip.content
        .as_ref()
        .or(clip.content_b64.as_ref())
        .map_or(0, String::len)
}

/// How long to wait for additional file completions before applying
/// the batch to the local clipboard. Tuned to be invisible to a
/// single-file `Ctrl+C` (~paste latency) while reliably collapsing a
/// multi-file `Ctrl+C` into one clipboard set even on slow networks.
const RECEIVE_BATCH_WINDOW: std::time::Duration = std::time::Duration::from_millis(500);

/// `tokio::select!` wants every arm to be a future. When the supervisor
/// is configured *without* inbound files (the headless `send`
/// subprocess), we still want `select!` to compile cleanly — return a
/// future that just yields `None` from a never-resolving recv if the
/// channel is absent. Equivalent to `std::future::pending()` but with
/// the same return type as the receiver.
async fn recv_maybe<T>(rx: Option<&mut tokio::sync::mpsc::Receiver<T>>) -> Option<T> {
    match rx {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

/// Encode a *single-frame* clipboard change (text or image) into a
/// wire frame. File-list changes go through a separate path —
/// [`send_files_through`] — because each file is a multi-chunk
/// transfer.
fn change_to_frame(change: ClipChange) -> Result<ClipFrame> {
    Ok(match change {
        ClipChange::Text(text) => ClipFrame {
            id: Uuid::new_v4(),
            ts: now_millis(),
            content_type: TEXT_CONTENT_TYPE.to_string(),
            content: Some(text),
            content_b64: None,
            from: None,
        },
        ClipChange::Image(img) => {
            let png = encode_png(&img).context("encoding clipboard image to PNG")?;
            ClipFrame {
                id: Uuid::new_v4(),
                ts: now_millis(),
                content_type: IMAGE_PNG_CONTENT_TYPE.to_string(),
                content: None,
                content_b64: Some(STANDARD.encode(png)),
                from: None,
            }
        }
        ClipChange::Files(_) => {
            // Should never get here — run_supervisor routes Files
            // events through the file-chunk channel.
            anyhow::bail!("ClipChange::Files cannot be encoded as a single ClipFrame")
        }
    })
}

/// Decode an inbound wire frame into a clipboard change.
fn frame_to_change(clip: &ClipFrame) -> std::result::Result<ClipChange, FrameRejection> {
    if is_text_content_type(&clip.content_type) {
        let Some(text) = clip.content.clone() else {
            return Err(FrameRejection::Decode(anyhow::anyhow!(
                "text/* frame missing `content`"
            )));
        };
        Ok(ClipChange::Text(text))
    } else if clip.content_type == IMAGE_PNG_CONTENT_TYPE {
        let Some(b64) = clip.content_b64.as_deref() else {
            return Err(FrameRejection::Decode(anyhow::anyhow!(
                "image/png frame missing `content_b64`"
            )));
        };
        let png = STANDARD
            .decode(b64)
            .map_err(|e| FrameRejection::Decode(anyhow::anyhow!("base64 decode failed: {e}")))?;
        let img = decode_png(&png).map_err(FrameRejection::Decode)?;
        Ok(ClipChange::Image(img))
    } else {
        Err(FrameRejection::UnsupportedType)
    }
}

#[derive(Debug)]
enum FrameRejection {
    UnsupportedType,
    Decode(anyhow::Error),
}

/// Encode RGBA → PNG bytes via the `image` crate.
fn encode_png(img: &ImageBytes) -> Result<Vec<u8>> {
    use std::io::Cursor;
    let buffer: image::ImageBuffer<image::Rgba<u8>, &[u8]> =
        image::ImageBuffer::from_raw(img.width, img.height, img.rgba.as_slice())
            .context("invalid RGBA buffer (width*height*4 != bytes)")?;
    let mut out = Vec::with_capacity(img.rgba.len() / 4);
    buffer
        .write_to(&mut Cursor::new(&mut out), image::ImageFormat::Png)
        .context("PNG encoder")?;
    Ok(out)
}

/// Decode PNG bytes → RGBA via the `image` crate.
fn decode_png(bytes: &[u8]) -> Result<ImageBytes> {
    let dynamic = image::load_from_memory_with_format(bytes, image::ImageFormat::Png)
        .context("PNG decoder")?;
    let rgba = dynamic.to_rgba8();
    Ok(ImageBytes {
        width: rgba.width(),
        height: rgba.height(),
        rgba: rgba.into_raw(),
    })
}

fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_frame_roundtrip() {
        let in_change = ClipChange::Text("hello".into());
        let frame = change_to_frame(in_change.clone()).unwrap();
        assert_eq!(frame.content_type, TEXT_CONTENT_TYPE);
        assert!(frame.content_b64.is_none());

        let out_change = frame_to_change(&frame).unwrap();
        assert_eq!(out_change, in_change);
    }

    #[test]
    fn image_frame_roundtrip_preserves_pixels() {
        // 2x2 pure red / green / blue / white image.
        let in_img = ImageBytes {
            width: 2,
            height: 2,
            rgba: vec![
                255, 0, 0, 255, // R
                0, 255, 0, 255, // G
                0, 0, 255, 255, // B
                255, 255, 255, 255, // W
            ],
        };
        let frame = change_to_frame(ClipChange::Image(in_img.clone())).unwrap();
        assert_eq!(frame.content_type, IMAGE_PNG_CONTENT_TYPE);
        assert!(frame.content.is_none());

        let out_change = frame_to_change(&frame).unwrap();
        match out_change {
            ClipChange::Image(out_img) => assert_eq!(out_img, in_img),
            other => panic!("expected Image, got {other:?}"),
        }
    }

    #[test]
    fn frame_with_unknown_content_type_is_rejected() {
        let frame = ClipFrame {
            id: Uuid::nil(),
            ts: 0,
            content_type: "application/octet-stream".into(),
            content: None,
            content_b64: Some(STANDARD.encode(b"xx")),
            from: None,
        };
        assert!(matches!(
            frame_to_change(&frame),
            Err(FrameRejection::UnsupportedType)
        ));
    }

    #[test]
    fn corrupt_image_b64_is_rejected() {
        let frame = ClipFrame {
            id: Uuid::nil(),
            ts: 0,
            content_type: IMAGE_PNG_CONTENT_TYPE.into(),
            content: None,
            content_b64: Some("!!!not-base64!!!".into()),
            from: None,
        };
        assert!(matches!(
            frame_to_change(&frame),
            Err(FrameRejection::Decode(_))
        ));
    }

    #[test]
    fn corrupt_png_payload_is_rejected() {
        // Valid base64, but the bytes aren't a PNG.
        let frame = ClipFrame {
            id: Uuid::nil(),
            ts: 0,
            content_type: IMAGE_PNG_CONTENT_TYPE.into(),
            content: None,
            content_b64: Some(STANDARD.encode(b"not a png file")),
            from: None,
        };
        assert!(matches!(
            frame_to_change(&frame),
            Err(FrameRejection::Decode(_))
        ));
    }
}
