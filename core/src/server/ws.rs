// SPDX-License-Identifier: GPL-3.0-or-later

//! `/sync` WebSocket handler.
//!
//! Lifecycle:
//! 1. The HTTP upgrade request is authenticated via HTTP Basic.
//! 2. A capacity permit is acquired from a shared semaphore.
//! 3. After upgrade we register with the hub, send the `welcome` frame, and
//!    split the socket into a reader task and a writer task.
//! 4. The reader parses inbound frames and forwards `clip`s to the hub.
//! 5. The writer multiplexes:
//!    - hub-relayed `clip` frames,
//!    - locally-generated `error` frames,
//!    - periodic WebSocket Pings every 30 s.
//! 6. On either task exiting, we deregister and the semaphore permit is
//!    dropped, freeing capacity.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use futures_util::sink::SinkExt;
use futures_util::stream::StreamExt;
use tokio::sync::{mpsc, Semaphore};
use tokio::time::interval;
use tracing::{debug, error, info, instrument, warn};
use uuid::Uuid;

use crate::protocol::{
    ClipFrame, ErrorCode, ErrorFrame, FileChunkFrame, Frame, WelcomeFrame, PROTOCOL_VERSION,
};

use super::auth::check_basic_auth;
use super::config::ServerConfig;
use super::hub::{HubHandle, RegisterResult, PER_CLIENT_CHANNEL_BUF, PER_CLIENT_FILE_CHANNEL_BUF};

/// Application state passed to every request handler.
#[derive(Clone)]
pub struct AppState {
    pub hub: HubHandle,
    pub config: Arc<ServerConfig>,
    pub conn_sem: Arc<Semaphore>,
}

const PING_INTERVAL: Duration = Duration::from_secs(30);
const READ_TIMEOUT: Duration = Duration::from_secs(45);

/// Axum handler for `GET /sync`. Returns the upgrade response on success, or
/// 401 / 503 on failure.
pub async fn sync_handler(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Response {
    // 1. Auth.
    let auth = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());
    if !check_basic_auth(auth, &state.config.user, &state.config.password) {
        debug!("rejecting upgrade: bad auth");
        return (
            StatusCode::UNAUTHORIZED,
            [(
                header::WWW_AUTHENTICATE,
                "Basic realm=\"clipboardwire\", charset=\"UTF-8\"",
            )],
            "",
        )
            .into_response();
    }

    // 2. Capacity. Hold the permit across the upgrade so the count is
    // bounded between the check and the registration.
    let permit = match state.conn_sem.clone().try_acquire_owned() {
        Ok(p) => p,
        Err(_) => {
            warn!("rejecting upgrade: at capacity");
            return (StatusCode::SERVICE_UNAVAILABLE, "at capacity").into_response();
        }
    };

    // 3. Configure frame-size limits and complete the upgrade.
    let max_frame = state.config.max_frame_bytes;
    ws.max_message_size(max_frame)
        .max_frame_size(max_frame)
        .on_upgrade(move |socket| handle_socket(socket, state, permit, peer))
}

#[instrument(skip_all, fields(client = tracing::field::Empty, peer = %peer))]
async fn handle_socket(
    socket: WebSocket,
    state: AppState,
    _permit: tokio::sync::OwnedSemaphorePermit,
    peer: SocketAddr,
) {
    let client_id = Uuid::new_v4();
    tracing::Span::current().record("client", tracing::field::display(client_id));
    info!("connection opened");

    let (clip_tx, mut clip_rx) = mpsc::channel::<ClipFrame>(PER_CLIENT_CHANNEL_BUF);
    let (file_tx, mut file_rx) = mpsc::channel::<FileChunkFrame>(PER_CLIENT_FILE_CHANNEL_BUF);
    let (internal_tx, mut internal_rx) = mpsc::channel::<Frame>(8);

    let registration = state.hub.register(client_id, clip_tx, file_tx).await;
    let last_clip = match registration {
        Ok(RegisterResult::Accepted { last_clip }) => last_clip,
        Ok(RegisterResult::AtCapacity) | Err(_) => {
            // Should not happen: the semaphore bounds connections strictly
            // tighter than the hub. Defend anyway.
            warn!("hub refused registration; closing");
            close_with(socket, ErrorCode::DuplicateSession, "hub at capacity").await;
            return;
        }
    };

    let welcome = Frame::Welcome(WelcomeFrame {
        server: PROTOCOL_VERSION.to_string(),
        client_id,
        last_clip,
    });

    // Inject the welcome ahead of anything else.
    if internal_tx.send(welcome).await.is_err() {
        state.hub.deregister(client_id).await;
        return;
    }

    let (sink, mut stream) = socket.split();

    // Writer task: owns the sink, multiplexes the three sources.
    let writer_state = state.clone();
    let writer = tokio::spawn(async move {
        let mut sink = sink;
        let mut ping_timer = interval(PING_INTERVAL);
        // Skip the immediate first tick so we don't ping on connect.
        ping_timer.tick().await;

        loop {
            tokio::select! {
                biased;

                maybe_frame = internal_rx.recv() => {
                    let Some(frame) = maybe_frame else { break };
                    let json = match serde_json::to_string(&frame) {
                        Ok(s) => s,
                        Err(e) => { error!(error = %e, "serializing internal frame"); break }
                    };
                    if sink.send(Message::Text(json.into())).await.is_err() {
                        break;
                    }
                    // If we just sent an Error frame, the protocol asks us
                    // to close the socket.
                    if let Frame::Error(_) = frame {
                        let _ = sink.close().await;
                        break;
                    }
                }

                maybe_clip = clip_rx.recv() => {
                    let Some(clip) = maybe_clip else { break };
                    let json = match serde_json::to_string(&Frame::Clip(clip)) {
                        Ok(s) => s,
                        Err(e) => { error!(error = %e, "serializing clip"); continue }
                    };
                    if sink.send(Message::Text(json.into())).await.is_err() {
                        break;
                    }
                }

                maybe_file = file_rx.recv() => {
                    let Some(chunk) = maybe_file else { break };
                    let json = match serde_json::to_string(&Frame::FileChunk(chunk)) {
                        Ok(s) => s,
                        Err(e) => { error!(error = %e, "serializing file_chunk"); continue }
                    };
                    if sink.send(Message::Text(json.into())).await.is_err() {
                        break;
                    }
                }

                _ = ping_timer.tick() => {
                    if sink.send(Message::Ping(Default::default())).await.is_err() {
                        break;
                    }
                }
            }
        }

        debug!("writer exiting");
        drop(writer_state);
    });

    // Reader task: parses inbound frames and either forwards `clip` to the
    // hub or emits an `error` and exits.
    let reader_hub = state.hub.clone();
    let reader_internal_tx = internal_tx.clone();
    let reader = tokio::spawn(async move {
        loop {
            let next = tokio::time::timeout(READ_TIMEOUT, stream.next()).await;
            let msg = match next {
                Ok(Some(Ok(m))) => m,
                Ok(Some(Err(e))) => {
                    debug!(error = %e, "ws read error");
                    break;
                }
                Ok(None) => {
                    debug!("ws stream ended");
                    break;
                }
                Err(_) => {
                    warn!("read timeout; closing");
                    break;
                }
            };

            match msg {
                Message::Text(s) => match serde_json::from_str::<Frame>(&s) {
                    Ok(Frame::Clip(mut clip)) => {
                        // The hub is content-type agnostic — v0.2 added image
                        // payloads and reserves additional types for future
                        // revisions. We only validate that the frame carries
                        // exactly one of `content` / `content_b64` matching
                        // its content_type.
                        if let Err(reason) = clip.validate() {
                            debug!(reason, "rejecting bad clip frame");
                            let _ = reader_internal_tx
                                .send(Frame::Error(ErrorFrame {
                                    code: ErrorCode::BadFrame,
                                    message: reason.into(),
                                }))
                                .await;
                            break;
                        }
                        // Clients should not set `from`; reset it before
                        // forwarding.
                        clip.from = None;
                        if reader_hub.publish(client_id, clip).await.is_err() {
                            break;
                        }
                    }
                    Ok(Frame::FileChunk(mut chunk)) => {
                        if let Err(reason) = chunk.validate() {
                            debug!(reason, "rejecting bad file_chunk");
                            let _ = reader_internal_tx
                                .send(Frame::Error(ErrorFrame {
                                    code: ErrorCode::BadFrame,
                                    message: reason.into(),
                                }))
                                .await;
                            break;
                        }
                        chunk.from = None;
                        if reader_hub.publish_file(client_id, chunk).await.is_err() {
                            break;
                        }
                    }
                    Ok(_) => {
                        // Clients are not supposed to send welcome/error.
                        let _ = reader_internal_tx
                            .send(Frame::Error(ErrorFrame {
                                code: ErrorCode::BadFrame,
                                message: "unexpected frame type from client".into(),
                            }))
                            .await;
                        break;
                    }
                    Err(e) => {
                        debug!(error = %e, "rejecting unparseable frame");
                        let _ = reader_internal_tx
                            .send(Frame::Error(ErrorFrame {
                                code: ErrorCode::BadFrame,
                                message: "malformed frame".into(),
                            }))
                            .await;
                        break;
                    }
                },
                Message::Binary(_) => {
                    let _ = reader_internal_tx
                        .send(Frame::Error(ErrorFrame {
                            code: ErrorCode::BadFrame,
                            message: "binary WebSocket frames are not supported".into(),
                        }))
                        .await;
                    break;
                }
                Message::Ping(_) | Message::Pong(_) => {
                    // Pings are auto-responded by the WebSocket layer;
                    // pongs reset our deadline via the timeout reset on the
                    // next loop iteration. Either way we just continue.
                }
                Message::Close(_) => {
                    debug!("peer sent Close");
                    break;
                }
            }
        }

        debug!("reader exiting");
    });

    // Wait for either side to end, then tear the other down.
    tokio::select! {
        _ = reader => {},
        _ = writer => {},
    }
    state.hub.deregister(client_id).await;
    info!("connection closed");
}

async fn close_with(socket: WebSocket, code: ErrorCode, msg: &str) {
    let mut socket = socket;
    let _ = socket
        .send(Message::Close(Some(CloseFrame {
            code: code.close_code(),
            reason: msg.to_string().into(),
        })))
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    fn test_state() -> AppState {
        let (hub, _join) = crate::server::hub::spawn(8);
        AppState {
            hub,
            config: Arc::new(ServerConfig {
                bind: "127.0.0.1:0".parse().unwrap(),
                user: "alice".into(),
                password: "hunter2".into(),
                max_conns: 8,
                max_frame_bytes: 1024,
                tls_cert_file: None,
                tls_key_file: None,
                tls_disabled: true,
                state_dir: None,
                stats: None,
            }),
            conn_sem: Arc::new(Semaphore::new(8)),
        }
    }

    // Real WebSocket handshake / auth flows are covered by the integration
    // test in `tests/end_to_end.rs` (task #9). Unit tests here cover only
    // request paths that the WebSocketUpgrade extractor doesn't gate.

    #[tokio::test]
    async fn healthz_responds_200() {
        let state = test_state();
        let app = crate::server::router(state);
        let req = Request::builder()
            .uri("/healthz")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }
}
