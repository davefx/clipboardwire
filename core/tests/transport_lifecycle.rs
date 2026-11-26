// SPDX-License-Identifier: GPL-3.0-or-later

//! The transport task must stop once its owner (the supervisor) is gone.
//!
//! Regression test: every config reload in the tray (closing Settings,
//! "Reload config") aborted the supervisor, which dropped the
//! [`Transport`] handle, but the transport task kept running. Its
//! `connect_and_serve` saw the closed outbound channel, returned `Ok`,
//! and `run_loop` reconnected — forever, once a minute at full backoff,
//! each time with a fresh client id and a ~10 ms connection. After a
//! few reloads the hub was serving a crowd of zombie peers.

use std::net::SocketAddr;
use std::time::Duration;

use clipboardwire_core::client::transport::{spawn_with_options, ClientStatus, SpawnOptions};
use clipboardwire_core::client::ClientConfig;
use clipboardwire_core::server::{serve, ServerConfig};
use tokio::net::TcpListener;
use tokio::sync::watch;

const USER: &str = "alice";
const PW: &str = "hunter2";

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

fn client_cfg(addr: SocketAddr) -> ClientConfig {
    serde_json::from_value(serde_json::json!({
        "server": format!("ws://{addr}/sync"), "user": USER, "password": PW,
    }))
    .unwrap()
}

async fn wait_for_status(rx: &mut watch::Receiver<ClientStatus>, want: fn(&ClientStatus) -> bool) {
    tokio::time::timeout(Duration::from_secs(5), rx.wait_for(want))
        .await
        .expect("transport never reached the expected status")
        .expect("status channel closed");
}

#[tokio::test]
async fn dropping_the_handle_stops_a_connected_transport() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(serve(listener, server_cfg(addr), std::future::pending()));

    let (status_tx, mut status_rx) = watch::channel(ClientStatus::Connecting);
    let (transport, join) = spawn_with_options(
        client_cfg(addr),
        SpawnOptions {
            status_tx: Some(status_tx),
            ..SpawnOptions::default()
        },
    );
    wait_for_status(&mut status_rx, |s| matches!(s, ClientStatus::Connected)).await;

    drop(transport);

    tokio::time::timeout(Duration::from_secs(3), join)
        .await
        .expect("transport kept running after its handle was dropped")
        .unwrap();
}

#[tokio::test]
async fn dropping_the_handle_stops_a_reconnecting_transport() {
    // A port nobody listens on: the transport sits in its retry backoff.
    let addr = {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        l.local_addr().unwrap()
    };

    let (status_tx, mut status_rx) = watch::channel(ClientStatus::Connecting);
    let (transport, join) = spawn_with_options(
        client_cfg(addr),
        SpawnOptions {
            status_tx: Some(status_tx),
            ..SpawnOptions::default()
        },
    );
    wait_for_status(&mut status_rx, |s| {
        matches!(s, ClientStatus::Disconnected { .. })
    })
    .await;

    drop(transport);

    // Well under the 1 s initial backoff plus another attempt.
    tokio::time::timeout(Duration::from_millis(500), join)
        .await
        .expect("transport kept retrying after its handle was dropped")
        .unwrap();
}
