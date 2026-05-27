// SPDX-License-Identifier: GPL-3.0-or-later

//! WebSocket transport for the client.
//!
//! Owns the connection lifecycle: connect → consume welcome → relay frames
//! → reconnect with exponential backoff. Inbound `clip` frames are surfaced
//! to the supervisor via a channel; outbound clips are pulled from another.
//!
//! Reconnect policy follows `PROTOCOL.md` §2.5: initial backoff 1 s,
//! doubling to a cap of 60 s, reset to 1 s after a connection has been
//! stable for at least 30 s. (Jitter is left as a follow-up; for a
//! single-user personal tool a thundering-herd at reconnect is impossible.)

use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Result};
use futures_util::sink::SinkExt;
use futures_util::stream::StreamExt;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, info, instrument, trace, warn};

use crate::protocol::{ClipFrame, FileChunkFrame, Frame};
use crate::server::auth::basic_header_value;

use super::config::ClientConfig;
use super::tls;

const INITIAL_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(60);
const RESET_AFTER_STABLE: Duration = Duration::from_secs(30);
const PING_INTERVAL: Duration = Duration::from_secs(30);
const INBOUND_BUF: usize = 32;
const OUTBOUND_BUF: usize = 8;
/// File chunks are bigger than clips and naturally back-pressured by
/// the network; keep the queues short so we don't accidentally buffer
/// hundreds of MB in process memory while waiting for the socket.
const INBOUND_FILES_BUF: usize = 4;
const OUTBOUND_FILES_BUF: usize = 4;

/// Connection-state snapshot the transport emits via the optional
/// `status_tx` watch channel passed to [`spawn_with_status`]. The tray
/// uses this to surface "connecting", "connected", "disconnected" in
/// the menu + tooltip without having to poll.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientStatus {
    /// Connect attempt in progress.
    Connecting,
    /// Welcome frame received; relaying clipboard traffic.
    Connected,
    /// Last connection ended (clean or with an error); will retry after
    /// `will_retry_in`. The duration mirrors the next backoff sleep.
    Disconnected { will_retry_in: Duration },
}

/// Handle held by the supervisor.
pub struct Transport {
    /// Inbound `clip` frames from the hub (other peers' publishes plus, on
    /// each new connection, the cached `last_clip` from the welcome).
    pub inbound_rx: mpsc::Receiver<ClipFrame>,
    /// Inbound `file_chunk` frames from other peers. Only carries traffic
    /// when the supervisor opted in by passing a sender; the headless
    /// `send` subcommand sets this to `None`.
    pub inbound_files_rx: Option<mpsc::Receiver<FileChunkFrame>>,
    /// Outbound `clip` frames to push to the hub.
    pub outbound_tx: mpsc::Sender<ClipFrame>,
    /// Outbound `file_chunk` frames to push to the hub.
    pub outbound_files_tx: mpsc::Sender<FileChunkFrame>,
}

/// Configuration for [`spawn_with_options`]. Optional flags layered on
/// top of the bare [`spawn`] call so the supervisor + a one-shot `send`
/// subprocess can share the same transport code without each having
/// fields the other doesn't need.
#[derive(Default)]
pub struct SpawnOptions {
    pub status_tx: Option<watch::Sender<ClientStatus>>,
    /// If true, the transport pipes inbound file chunks through a new
    /// channel surfaced on the returned [`Transport`]. False for
    /// senders that don't want to receive.
    pub receive_files: bool,
    /// If true, the transport exits cleanly after a single
    /// connect-and-serve cycle (clean close OR error). Used by the
    /// one-shot `clipboardwire send` subcommand which doesn't want
    /// the supervisor's reconnect-forever behavior.
    pub one_shot: bool,
}

/// Spawn the transport task. Returns the supervisor-facing handle and a join
/// handle for orderly shutdown (abort it to disconnect cleanly).
pub fn spawn(config: ClientConfig) -> (Transport, JoinHandle<()>) {
    spawn_with_options(config, SpawnOptions::default())
}

/// Like [`spawn`] but additionally accepts a `watch::Sender` that the
/// task uses to report [`ClientStatus`] transitions. Pass `None` if the
/// caller doesn't need status updates (the headless-server case).
pub fn spawn_with_status(
    config: ClientConfig,
    status_tx: Option<watch::Sender<ClientStatus>>,
) -> (Transport, JoinHandle<()>) {
    spawn_with_options(
        config,
        SpawnOptions {
            status_tx,
            receive_files: true,
            one_shot: false,
        },
    )
}

/// Full-control spawn. Used by the supervisor (wants status + files)
/// and by the `send` subprocess (one-shot, no status, no inbound file
/// stream — it just publishes).
pub fn spawn_with_options(
    config: ClientConfig,
    options: SpawnOptions,
) -> (Transport, JoinHandle<()>) {
    let (inbound_tx, inbound_rx) = mpsc::channel::<ClipFrame>(INBOUND_BUF);
    let (outbound_tx, outbound_rx) = mpsc::channel::<ClipFrame>(OUTBOUND_BUF);
    let (outbound_files_tx, outbound_files_rx) =
        mpsc::channel::<FileChunkFrame>(OUTBOUND_FILES_BUF);
    let (inbound_files_tx, inbound_files_rx) = if options.receive_files {
        let (tx, rx) = mpsc::channel::<FileChunkFrame>(INBOUND_FILES_BUF);
        (Some(tx), Some(rx))
    } else {
        (None, None)
    };
    let SpawnOptions {
        status_tx,
        one_shot,
        ..
    } = options;
    let join = tokio::spawn(async move {
        run_loop(
            config,
            inbound_tx,
            inbound_files_tx,
            outbound_rx,
            outbound_files_rx,
            status_tx,
            one_shot,
        )
        .await;
    });
    (
        Transport {
            inbound_rx,
            inbound_files_rx,
            outbound_tx,
            outbound_files_tx,
        },
        join,
    )
}

#[instrument(skip_all, fields(server = %config.server))]
async fn run_loop(
    config: ClientConfig,
    inbound_tx: mpsc::Sender<ClipFrame>,
    inbound_files_tx: Option<mpsc::Sender<FileChunkFrame>>,
    mut outbound_rx: mpsc::Receiver<ClipFrame>,
    mut outbound_files_rx: mpsc::Receiver<FileChunkFrame>,
    status_tx: Option<watch::Sender<ClientStatus>>,
    one_shot: bool,
) {
    let emit = |s: ClientStatus| {
        if let Some(tx) = status_tx.as_ref() {
            // send_replace is fine even when there are no receivers;
            // the watch is "latest state," nobody starves.
            let _ = tx.send(s);
        }
    };
    let mut backoff = INITIAL_BACKOFF;
    loop {
        emit(ClientStatus::Connecting);
        let attempt_start = Instant::now();
        let outcome = connect_and_serve(
            &config,
            &inbound_tx,
            inbound_files_tx.as_ref(),
            &mut outbound_rx,
            &mut outbound_files_rx,
            status_tx.as_ref(),
        )
        .await;
        match &outcome {
            Ok(()) => debug!("connection ended cleanly"),
            Err(e) => warn!(error = %format!("{e:#}"), "connection error"),
        }
        if one_shot {
            // The `clipboardwire send` path: a single connect+publish+
            // close cycle is the whole job. Don't reconnect.
            return;
        }
        // If we held the connection for a while, the backoff resets.
        if attempt_start.elapsed() >= RESET_AFTER_STABLE {
            backoff = INITIAL_BACKOFF;
        }
        emit(ClientStatus::Disconnected {
            will_retry_in: backoff,
        });
        debug!(sleep_s = backoff.as_secs_f64(), "reconnecting");
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

/// Drain any file chunks still buffered in `outbound_files_rx` to the
/// WebSocket sink. Called when the outbound clip channel closes (the
/// caller is winding down) and we want to make sure we don't drop a
/// queued file mid-flight just because `select!` happened to fire the
/// clip-empty arm first. `recv()` here returns `Some` for queued
/// items, then `None` when the file channel is also closed.
async fn drain_outbound_files<S>(
    outbound_files_rx: &mut mpsc::Receiver<FileChunkFrame>,
    sink: &mut S,
) -> Result<()>
where
    S: futures_util::sink::Sink<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin,
{
    while let Some(chunk) = outbound_files_rx.recv().await {
        let json = serde_json::to_string(&Frame::FileChunk(chunk))?;
        if let Err(e) = sink.send(Message::Text(json.into())).await {
            warn!(error = %e, "drain: file_chunk send failed");
            return Err(e.into());
        }
    }
    Ok(())
}

async fn connect_and_serve(
    config: &ClientConfig,
    inbound_tx: &mpsc::Sender<ClipFrame>,
    inbound_files_tx: Option<&mpsc::Sender<FileChunkFrame>>,
    outbound_rx: &mut mpsc::Receiver<ClipFrame>,
    outbound_files_rx: &mut mpsc::Receiver<FileChunkFrame>,
    status_tx: Option<&watch::Sender<ClientStatus>>,
) -> Result<()> {
    let mut req = config
        .server
        .as_str()
        .into_client_request()
        .map_err(|e| anyhow!("invalid server URL: {e}"))?;
    req.headers_mut().insert(
        "authorization",
        basic_header_value(&config.user, &config.password)
            .parse()
            .map_err(|e| anyhow!("auth header: {e}"))?,
    );

    let (ws, _resp) = if config.server.starts_with("wss://") {
        let connector = tls::make_connector(config)?;
        tokio_tungstenite::connect_async_tls_with_config(req, None, false, Some(connector)).await?
    } else {
        tokio_tungstenite::connect_async(req).await?
    };
    info!("connected");
    if let Some(tx) = status_tx {
        let _ = tx.send(ClientStatus::Connected);
    }
    let (mut sink, mut stream) = ws.split();

    // First frame must be `welcome`.
    let first = stream
        .next()
        .await
        .ok_or_else(|| anyhow!("connection closed before welcome"))??;
    let welcome = match first {
        Message::Text(s) => match serde_json::from_str::<Frame>(&s)? {
            Frame::Welcome(w) => w,
            other => bail!("expected welcome, got {other:?}"),
        },
        other => bail!("expected text welcome, got {other:?}"),
    };
    debug!(
        server = %welcome.server,
        client_id = %welcome.client_id,
        has_last_clip = welcome.last_clip.is_some(),
        "welcomed"
    );

    // Surface a cached clip (if any) to the supervisor as an inbound event.
    if let Some(clip) = welcome.last_clip {
        let _ = inbound_tx.send(clip).await;
    }

    let mut ping_timer = tokio::time::interval(PING_INTERVAL);
    ping_timer.tick().await; // skip the immediate first tick

    loop {
        tokio::select! {
            outbound = outbound_rx.recv() => {
                let Some(clip) = outbound else {
                    debug!("outbound channel closed; draining outbound_files before exit");
                    // Drain any buffered file chunks BEFORE bailing.
                    // Otherwise a `select!` race on a simultaneous drop
                    // of both outbound senders can pick the empty clip
                    // arm first and discard the queued file chunks —
                    // which is exactly what the `clipboardwire send`
                    // one-shot path does on completion.
                    drain_outbound_files(outbound_files_rx, &mut sink).await?;
                    return Ok(());
                };
                let json = serde_json::to_string(&Frame::Clip(clip))?;
                if let Err(e) = sink.send(Message::Text(json.into())).await {
                    warn!(error = %e, "send failed");
                    return Err(e.into());
                }
            }
            outbound_file = outbound_files_rx.recv() => {
                let Some(chunk) = outbound_file else {
                    debug!("outbound files channel closed; exiting");
                    return Ok(());
                };
                let json = serde_json::to_string(&Frame::FileChunk(chunk))?;
                if let Err(e) = sink.send(Message::Text(json.into())).await {
                    warn!(error = %e, "file_chunk send failed");
                    return Err(e.into());
                }
            }
            msg = stream.next() => {
                let Some(msg) = msg else {
                    debug!("stream ended");
                    return Ok(());
                };
                match msg? {
                    Message::Text(s) => {
                        let frame: Frame = match serde_json::from_str(&s) {
                            Ok(f) => f,
                            Err(e) => {
                                warn!(error = %e, "rejecting bad frame from server");
                                continue;
                            }
                        };
                        match frame {
                            Frame::Clip(clip) => {
                                if inbound_tx.send(clip).await.is_err() {
                                    return Ok(());
                                }
                            }
                            Frame::FileChunk(chunk) => {
                                if let Some(tx) = inbound_files_tx.as_ref() {
                                    if tx.send(chunk).await.is_err() {
                                        return Ok(());
                                    }
                                } else {
                                    trace!("ignoring inbound file_chunk; no receiver wired");
                                }
                            }
                            Frame::Error(err) => {
                                warn!(code = ?err.code, message = %err.message, "server error");
                            }
                            Frame::Welcome(_) => {
                                warn!("ignoring unexpected second welcome");
                            }
                        }
                    }
                    Message::Binary(_) => warn!("ignoring binary frame"),
                    Message::Ping(_) | Message::Pong(_) => {
                        // Pings are auto-pong'd by the WebSocket layer.
                        trace!("ping/pong");
                    }
                    Message::Close(_) => {
                        debug!("server sent close");
                        return Ok(());
                    }
                    _ => {}
                }
            }
            _ = ping_timer.tick() => {
                if let Err(e) = sink.send(Message::Ping(Default::default())).await {
                    warn!(error = %e, "ping send failed");
                    return Err(e.into());
                }
            }
        }
    }
}
