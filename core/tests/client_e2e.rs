// SPDX-License-Identifier: GPL-3.0-or-later

//! End-to-end test for the client supervisor.
//!
//! Spins up a real hub, two real WebSocket transports, and feeds the
//! supervisors synthetic clipboard handles whose channels we control. A
//! "local change" event injected on client A should land in client B's
//! `apply_tx` after travelling through transport → hub → transport.

use std::sync::mpsc as smpsc;
use std::time::Duration;

use clipboardwire_core::client::{run_supervisor, transport, ClientConfig, Clipboard};
use clipboardwire_core::server::{build_app, ServerConfig};
use tokio::net::TcpListener;
use tokio::sync::mpsc;

#[tokio::test]
async fn supervisor_round_trip_through_real_hub() {
    // -- Hub --
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server_cfg = ServerConfig {
        bind: addr,
        user: "alice".into(),
        password: "hunter2".into(),
        max_conns: 8,
        max_frame_bytes: 1024 * 1024,
    };
    let (app, _hub_join) = build_app(server_cfg);
    let _server_task = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let make_cfg = || ClientConfig {
        server: format!("ws://{addr}/sync"),
        user: "alice".into(),
        password: "hunter2".into(),
        poll_ms: 1000,
    };

    // -- Client A: we inject a local clipboard change via a_events_tx --
    let (a_events_tx, a_events_rx) = mpsc::channel::<String>(8);
    let (a_apply_tx, _a_apply_rx) = smpsc::channel::<String>();
    let a_clipboard = Clipboard {
        events_rx: a_events_rx,
        apply_tx: a_apply_tx,
    };
    let (a_transport, _a_join) = transport::spawn(make_cfg());
    tokio::spawn(async move { run_supervisor(a_clipboard, a_transport).await });

    // -- Client B: we read what would have been applied via b_apply_rx --
    let (_b_events_tx, b_events_rx) = mpsc::channel::<String>(8);
    let (b_apply_tx, b_apply_rx) = smpsc::channel::<String>();
    let b_clipboard = Clipboard {
        events_rx: b_events_rx,
        apply_tx: b_apply_tx,
    };
    let (b_transport, _b_join) = transport::spawn(make_cfg());
    tokio::spawn(async move { run_supervisor(b_clipboard, b_transport).await });

    // Give the transports time to connect, exchange welcome, and register.
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Inject a "local clipboard change" on A.
    a_events_tx
        .send("hello via the hub".to_string())
        .await
        .unwrap();

    // B should receive an apply request with the same content within a bit.
    let recv = tokio::task::spawn_blocking(move || b_apply_rx.recv_timeout(Duration::from_secs(3)))
        .await
        .unwrap()
        .expect("B should receive the relayed clip");
    assert_eq!(recv, "hello via the hub");
}

#[tokio::test]
async fn late_joiner_supervisor_applies_cached_clip() {
    // Hub.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server_cfg = ServerConfig {
        bind: addr,
        user: "alice".into(),
        password: "hunter2".into(),
        max_conns: 8,
        max_frame_bytes: 1024 * 1024,
    };
    let (app, _hub_join) = build_app(server_cfg);
    let _server_task = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let make_cfg = || ClientConfig {
        server: format!("ws://{addr}/sync"),
        user: "alice".into(),
        password: "hunter2".into(),
        poll_ms: 1000,
    };

    // A publishes first.
    let (a_events_tx, a_events_rx) = mpsc::channel::<String>(8);
    let (a_apply_tx, _) = smpsc::channel::<String>();
    let a_clipboard = Clipboard {
        events_rx: a_events_rx,
        apply_tx: a_apply_tx,
    };
    let (a_transport, _) = transport::spawn(make_cfg());
    tokio::spawn(async move { run_supervisor(a_clipboard, a_transport).await });

    tokio::time::sleep(Duration::from_millis(200)).await;
    a_events_tx.send("cached".into()).await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;

    // B joins later — its welcome should carry the cached clip, which the
    // supervisor will apply.
    let (_b_events_tx, b_events_rx) = mpsc::channel::<String>(8);
    let (b_apply_tx, b_apply_rx) = smpsc::channel::<String>();
    let b_clipboard = Clipboard {
        events_rx: b_events_rx,
        apply_tx: b_apply_tx,
    };
    let (b_transport, _) = transport::spawn(make_cfg());
    tokio::spawn(async move { run_supervisor(b_clipboard, b_transport).await });

    let recv = tokio::task::spawn_blocking(move || b_apply_rx.recv_timeout(Duration::from_secs(3)))
        .await
        .unwrap()
        .expect("late joiner should apply cached clip");
    assert_eq!(recv, "cached");
}
