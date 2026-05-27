// SPDX-License-Identifier: GPL-3.0-or-later

//! End-to-end integration test: real TCP listener, real WebSocket clients.
//!
//! Each test starts a hub on `127.0.0.1:0`, connects one or more
//! tokio-tungstenite clients over `ws://`, and exercises the protocol from
//! the outside. This is the layer that catches wiring bugs the unit tests
//! cannot — header propagation, framing, auth, fan-out, and graceful close.

use std::net::SocketAddr;
use std::time::Duration;

use clipboardwire_core::protocol::{
    ClipFrame, Frame, WelcomeFrame, PROTOCOL_VERSION, TEXT_CONTENT_TYPE,
};
use clipboardwire_core::server::auth::basic_header_value;
use clipboardwire_core::server::hub::HubStatsSink;
use clipboardwire_core::server::{build_app, ServerConfig};
use futures_util::sink::SinkExt;
use futures_util::stream::StreamExt;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::handshake::client::Request as WsRequest;
use tokio_tungstenite::tungstenite::http::StatusCode;
use tokio_tungstenite::tungstenite::protocol::Message;
use uuid::Uuid;

const USER: &str = "alice";
const PW: &str = "hunter2";

struct TestServer {
    addr: SocketAddr,
    _task: JoinHandle<()>,
}

async fn start_server(max_conns: usize) -> TestServer {
    start_server_with_stats(max_conns, None).await
}

async fn start_server_with_stats(max_conns: usize, stats: Option<HubStatsSink>) -> TestServer {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let cfg = ServerConfig {
        bind: addr,
        user: USER.to_string(),
        password: PW.to_string(),
        max_conns,
        max_frame_bytes: 1024 * 1024,
        tls_cert_file: None,
        tls_key_file: None,
        tls_disabled: true,
        state_dir: None,
        stats,
    };
    let (app, _hub_join) = build_app(cfg);
    let task = tokio::spawn(async move {
        let _ = axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await;
    });
    TestServer { addr, _task: task }
}

fn ws_request(addr: SocketAddr, user: &str, pw: &str) -> WsRequest {
    let url = format!("ws://{addr}/sync");
    let mut req = url.into_client_request().unwrap();
    req.headers_mut().insert(
        "authorization",
        basic_header_value(user, pw).parse().unwrap(),
    );
    req
}

/// Connect, consume the welcome frame, and return the WebSocket plus the
/// server-assigned client id.
async fn connect_and_welcome(
    addr: SocketAddr,
    user: &str,
    pw: &str,
) -> (
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    Uuid,
) {
    let (mut ws, _resp) = tokio_tungstenite::connect_async(ws_request(addr, user, pw))
        .await
        .expect("WebSocket connect failed");
    let msg = ws.next().await.unwrap().unwrap();
    let Message::Text(text) = msg else {
        panic!("expected welcome text frame, got {msg:?}");
    };
    let Frame::Welcome(WelcomeFrame {
        server,
        client_id,
        last_clip: _,
    }) = serde_json::from_str(&text).unwrap()
    else {
        panic!("first frame should be welcome");
    };
    assert_eq!(server, PROTOCOL_VERSION);
    (ws, client_id)
}

fn clip(content: &str) -> ClipFrame {
    ClipFrame {
        id: Uuid::new_v4(),
        ts: 0,
        content_type: TEXT_CONTENT_TYPE.to_string(),
        content: Some(content.to_string()),
        content_b64: None,
        from: None,
    }
}

#[tokio::test]
async fn rejects_connection_with_no_auth() {
    let srv = start_server(4).await;
    let url = format!("ws://{}/sync", srv.addr);
    let err = tokio_tungstenite::connect_async(url).await.unwrap_err();
    let http_err = match err {
        tokio_tungstenite::tungstenite::Error::Http(resp) => resp,
        other => panic!("expected Http error, got {other:?}"),
    };
    assert_eq!(http_err.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn rejects_connection_with_wrong_password() {
    let srv = start_server(4).await;
    let err = tokio_tungstenite::connect_async(ws_request(srv.addr, USER, "wrong"))
        .await
        .unwrap_err();
    match err {
        tokio_tungstenite::tungstenite::Error::Http(resp) => {
            assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        }
        other => panic!("expected Http error, got {other:?}"),
    }
}

#[tokio::test]
async fn welcome_frame_advertises_version() {
    let srv = start_server(4).await;
    let (mut a, _id) = connect_and_welcome(srv.addr, USER, PW).await;
    a.close(None).await.ok();
}

#[tokio::test]
async fn clip_fans_out_to_peer_but_not_sender() {
    let srv = start_server(4).await;
    let (mut a, id_a) = connect_and_welcome(srv.addr, USER, PW).await;
    let (mut b, id_b) = connect_and_welcome(srv.addr, USER, PW).await;
    assert_ne!(id_a, id_b);

    // A sends a clip.
    let frame = Frame::Clip(clip("hello from A"));
    let json = serde_json::to_string(&frame).unwrap();
    a.send(Message::Text(json.into())).await.unwrap();

    // B must receive it within a reasonable timeout.
    let msg = tokio::time::timeout(Duration::from_secs(2), b.next())
        .await
        .expect("B should receive the clip")
        .unwrap()
        .unwrap();
    let Message::Text(text) = msg else {
        panic!("expected text frame, got {msg:?}");
    };
    let Frame::Clip(received) = serde_json::from_str(&text).unwrap() else {
        panic!("expected clip frame");
    };
    assert_eq!(received.content.as_deref(), Some("hello from A"));
    assert_eq!(received.from, Some(id_a));

    // A must NOT receive its own publish back. Give the hub a beat to settle
    // and confirm no message arrives.
    let echo = tokio::time::timeout(Duration::from_millis(200), a.next()).await;
    assert!(
        echo.is_err(),
        "sender should not see an echo of its own publish"
    );

    a.close(None).await.ok();
    b.close(None).await.ok();
}

#[tokio::test]
async fn late_joiner_sees_last_clip_in_welcome() {
    let srv = start_server(4).await;

    // A publishes, then disconnects.
    let (mut a, id_a) = connect_and_welcome(srv.addr, USER, PW).await;
    let frame = Frame::Clip(clip("cached value"));
    a.send(Message::Text(serde_json::to_string(&frame).unwrap().into()))
        .await
        .unwrap();

    // Tiny pause to let the hub apply the publish before C connects.
    tokio::time::sleep(Duration::from_millis(50)).await;

    // C connects fresh — its welcome should carry the cached clip.
    let url = format!("ws://{}/sync", srv.addr);
    let mut req = url.into_client_request().unwrap();
    req.headers_mut().insert(
        "authorization",
        basic_header_value(USER, PW).parse().unwrap(),
    );
    let (mut c, _resp) = tokio_tungstenite::connect_async(req).await.unwrap();
    let msg = c.next().await.unwrap().unwrap();
    let Message::Text(text) = msg else {
        panic!("expected welcome");
    };
    let Frame::Welcome(WelcomeFrame { last_clip, .. }) = serde_json::from_str(&text).unwrap()
    else {
        panic!("expected welcome variant");
    };
    let cached = last_clip.expect("late joiner should see cached last_clip");
    assert_eq!(cached.content.as_deref(), Some("cached value"));
    assert_eq!(cached.from, Some(id_a));

    a.close(None).await.ok();
    c.close(None).await.ok();
}

#[tokio::test]
async fn connection_beyond_capacity_is_rejected_with_503() {
    let srv = start_server(1).await;
    let (mut a, _) = connect_and_welcome(srv.addr, USER, PW).await;

    // Second connect attempt should be rejected at the HTTP layer with 503.
    let err = tokio_tungstenite::connect_async(ws_request(srv.addr, USER, PW))
        .await
        .unwrap_err();
    match err {
        tokio_tungstenite::tungstenite::Error::Http(resp) => {
            assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        }
        other => panic!("expected Http error, got {other:?}"),
    }

    a.close(None).await.ok();
}

#[tokio::test]
async fn hub_stats_sink_tracks_connections_through_real_websockets() {
    let stats = HubStatsSink::new();
    assert_eq!(stats.current(), 0);
    let srv = start_server_with_stats(8, Some(stats.clone())).await;

    // The poller pattern used by the tray reads the sink without
    // holding any reference to the hub task — this test exercises
    // the same code path: open a real WebSocket, wait for welcome
    // (which the server only sends after registering us), check
    // the sink reflects us.
    let (mut a, _id_a) = connect_and_welcome(srv.addr, USER, PW).await;
    await_count(&stats, 1).await;

    let (mut b, _id_b) = connect_and_welcome(srv.addr, USER, PW).await;
    await_count(&stats, 2).await;

    a.close(None).await.ok();
    drop(a);
    await_count(&stats, 1).await;

    b.close(None).await.ok();
    drop(b);
    await_count(&stats, 0).await;
}

/// Spin on the sink until it reads `expected`, with a hard deadline.
/// Register / deregister round-trips through the hub task's inbox so
/// the count doesn't update synchronously with the WebSocket
/// handshake; a polite poll keeps the test deterministic without an
/// arbitrary sleep.
async fn await_count(stats: &HubStatsSink, expected: usize) {
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while std::time::Instant::now() < deadline {
        if stats.current() == expected {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!(
        "hub stats sink never reached {expected}; last seen {}",
        stats.current()
    );
}
