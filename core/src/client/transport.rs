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
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, info, instrument, trace, warn};

use crate::protocol::{ClipFrame, Frame};
use crate::server::auth::basic_header_value;

use super::config::ClientConfig;

const INITIAL_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(60);
const RESET_AFTER_STABLE: Duration = Duration::from_secs(30);
const INBOUND_BUF: usize = 32;
const OUTBOUND_BUF: usize = 8;

/// Handle held by the supervisor.
pub struct Transport {
    /// Inbound `clip` frames from the hub (other peers' publishes plus, on
    /// each new connection, the cached `last_clip` from the welcome).
    pub inbound_rx: mpsc::Receiver<ClipFrame>,
    /// Outbound `clip` frames to push to the hub.
    pub outbound_tx: mpsc::Sender<ClipFrame>,
}

/// Spawn the transport task. Returns the supervisor-facing handle and a join
/// handle for orderly shutdown (abort it to disconnect cleanly).
pub fn spawn(config: ClientConfig) -> (Transport, JoinHandle<()>) {
    let (inbound_tx, inbound_rx) = mpsc::channel::<ClipFrame>(INBOUND_BUF);
    let (outbound_tx, outbound_rx) = mpsc::channel::<ClipFrame>(OUTBOUND_BUF);
    let join = tokio::spawn(async move {
        run_loop(config, inbound_tx, outbound_rx).await;
    });
    (
        Transport {
            inbound_rx,
            outbound_tx,
        },
        join,
    )
}

#[instrument(skip_all, fields(server = %config.server))]
async fn run_loop(
    config: ClientConfig,
    inbound_tx: mpsc::Sender<ClipFrame>,
    mut outbound_rx: mpsc::Receiver<ClipFrame>,
) {
    let mut backoff = INITIAL_BACKOFF;
    loop {
        let attempt_start = Instant::now();
        match connect_and_serve(&config, &inbound_tx, &mut outbound_rx).await {
            Ok(()) => debug!("connection ended cleanly"),
            Err(e) => warn!(error = %format!("{e:#}"), "connection error"),
        }
        // If we held the connection for a while, the backoff resets.
        if attempt_start.elapsed() >= RESET_AFTER_STABLE {
            backoff = INITIAL_BACKOFF;
        }
        debug!(sleep_s = backoff.as_secs_f64(), "reconnecting");
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

async fn connect_and_serve(
    config: &ClientConfig,
    inbound_tx: &mpsc::Sender<ClipFrame>,
    outbound_rx: &mut mpsc::Receiver<ClipFrame>,
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

    let (ws, _resp) = tokio_tungstenite::connect_async(req).await?;
    info!("connected");
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

    loop {
        tokio::select! {
            outbound = outbound_rx.recv() => {
                let Some(clip) = outbound else {
                    debug!("outbound channel closed; exiting");
                    return Ok(());
                };
                let json = serde_json::to_string(&Frame::Clip(clip))?;
                if let Err(e) = sink.send(Message::Text(json.into())).await {
                    warn!(error = %e, "send failed");
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
        }
    }
}
