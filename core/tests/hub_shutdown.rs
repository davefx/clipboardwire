// SPDX-License-Identifier: GPL-3.0-or-later

//! Stopping a hub must close the WebSocket connections it already
//! accepted.
//!
//! Regression test: the tray's "Restart hub" (and saving Settings)
//! aborted the `serve` task, which only stopped the accept loop.
//! Upgraded connections kept running against the orphaned hub, so a
//! client that was connected before the restart stayed attached to it
//! forever — pings kept flowing, but its clips never reached peers on
//! the new hub and vice versa.

use std::net::SocketAddr;
use std::time::Duration;

use clipboardwire_core::client::{tls, ClientConfig};
use clipboardwire_core::protocol::Frame;
use clipboardwire_core::server::auth::basic_header_value;
use clipboardwire_core::server::{serve, ServerConfig};
use futures_util::stream::StreamExt;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::Message;

const USER: &str = "alice";
const PW: &str = "hunter2";

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn bind() -> (TcpListener, SocketAddr) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    (listener, addr)
}

fn server_cfg(addr: SocketAddr) -> ServerConfig {
    ServerConfig {
        bind: addr,
        user: USER.to_string(),
        password: PW.to_string(),
        max_conns: 8,
        max_frame_bytes: 1024 * 1024,
        tls_cert_file: None,
        tls_key_file: None,
        tls_disabled: true,
        state_dir: None,
        stats: None,
    }
}

async fn connect_and_welcome(addr: SocketAddr) -> Ws {
    connect_and_welcome_url(&format!("ws://{addr}/sync")).await
}

async fn connect_and_welcome_url(url: &str) -> Ws {
    let mut req = url.into_client_request().unwrap();
    req.headers_mut().insert(
        "authorization",
        basic_header_value(USER, PW).parse().unwrap(),
    );
    let (mut ws, _resp) = if url.starts_with("wss://") {
        let client_cfg: ClientConfig = serde_json::from_value(serde_json::json!({
            "server": url, "user": USER, "password": PW, "tls_insecure": true,
        }))
        .unwrap();
        let connector = tls::make_connector(&client_cfg).unwrap();
        tokio_tungstenite::connect_async_tls_with_config(req, None, false, Some(connector)).await
    } else {
        tokio_tungstenite::connect_async(req).await
    }
    .expect("WebSocket connect failed");
    let Message::Text(text) = ws.next().await.unwrap().unwrap() else {
        panic!("expected welcome text frame");
    };
    assert!(matches!(
        serde_json::from_str::<Frame>(&text).unwrap(),
        Frame::Welcome(_)
    ));
    ws
}

/// Read until the server closes the connection (Close frame, stream end,
/// or a transport error). Panics if it's still open after 5 s.
async fn expect_closed(ws: &mut Ws) {
    let closed = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match ws.next().await {
                None | Some(Err(_)) | Some(Ok(Message::Close(_))) => return,
                Some(Ok(_)) => continue,
            }
        }
    })
    .await;
    assert!(closed.is_ok(), "connection still open after hub stopped");
}

#[tokio::test]
async fn aborting_serve_closes_existing_connections() {
    let (listener, addr) = bind().await;
    let task = tokio::spawn(serve(listener, server_cfg(addr), std::future::pending()));

    let mut ws = connect_and_welcome(addr).await;
    task.abort();

    expect_closed(&mut ws).await;
}

#[tokio::test]
async fn graceful_shutdown_closes_existing_connections() {
    let (listener, addr) = bind().await;
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let task = tokio::spawn(serve(listener, server_cfg(addr), async move {
        let _ = stop_rx.await;
    }));

    let mut ws = connect_and_welcome(addr).await;
    stop_tx.send(()).unwrap();

    expect_closed(&mut ws).await;
    let result = tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("serve did not return after shutdown");
    result.unwrap().unwrap();
}

/// The tray's embedded hub runs the auto-generated self-signed TLS path,
/// which is served by `axum_server` rather than `axum::serve`.
#[tokio::test]
async fn aborting_tls_serve_closes_existing_connections() {
    let state_dir = std::env::temp_dir().join(format!(
        "clipboardwire-hub-shutdown-{}",
        uuid::Uuid::new_v4()
    ));
    let (listener, addr) = bind().await;
    let cfg = ServerConfig {
        tls_disabled: false,
        state_dir: Some(state_dir.clone()),
        ..server_cfg(addr)
    };
    let task = tokio::spawn(serve(listener, cfg, std::future::pending()));

    // `serve` generates the cert before it starts accepting; connections
    // made before then just wait in the listener's backlog.
    let mut ws = connect_and_welcome_url(&format!("wss://127.0.0.1:{}/sync", addr.port())).await;
    task.abort();

    expect_closed(&mut ws).await;
    let _ = std::fs::remove_dir_all(&state_dir);
}
