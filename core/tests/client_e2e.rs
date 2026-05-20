// SPDX-License-Identifier: GPL-3.0-or-later

//! End-to-end test for the client supervisor.
//!
//! Spins up a real hub, two real WebSocket transports, and feeds the
//! supervisors synthetic clipboard handles whose channels we control. A
//! "local change" event injected on client A should land in client B's
//! `apply_tx` after travelling through transport → hub → transport.

use std::sync::mpsc as smpsc;
use std::time::Duration;

use clipboardwire_core::client::{
    run_supervisor, transport, ClientConfig, ClipChange, Clipboard, ImageBytes,
};
use clipboardwire_core::server::{build_app, ServerConfig};
use tokio::net::TcpListener;
use tokio::sync::mpsc;

fn server_cfg(addr: std::net::SocketAddr) -> ServerConfig {
    ServerConfig {
        bind: addr,
        user: "alice".into(),
        password: "hunter2".into(),
        max_conns: 8,
        // 4 MiB is plenty for the test images (2x2 + 32x32 PNG, well under 1 KiB).
        max_frame_bytes: 4 * 1024 * 1024,
        tls_cert_file: None,
        tls_key_file: None,
    }
}

fn client_cfg(addr: std::net::SocketAddr) -> ClientConfig {
    ClientConfig {
        server: format!("ws://{addr}/sync"),
        user: "alice".into(),
        password: "hunter2".into(),
        poll_ms: 1000,
        tls_ca_file: None,
        tls_insecure: false,
        hub: None,
    }
}

#[tokio::test]
async fn supervisor_round_trip_through_real_hub() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (app, _hub_join) = build_app(server_cfg(addr));
    let _server_task = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    // -- Client A: we inject a local clipboard change via a_events_tx --
    let (a_events_tx, a_events_rx) = mpsc::channel::<ClipChange>(8);
    let (a_apply_tx, _a_apply_rx) = smpsc::channel::<ClipChange>();
    let a_clipboard = Clipboard {
        events_rx: a_events_rx,
        apply_tx: a_apply_tx,
    };
    let (a_transport, _a_join) = transport::spawn(client_cfg(addr));
    tokio::spawn(async move { run_supervisor(a_clipboard, a_transport).await });

    // -- Client B: we read what would have been applied via b_apply_rx --
    let (_b_events_tx, b_events_rx) = mpsc::channel::<ClipChange>(8);
    let (b_apply_tx, b_apply_rx) = smpsc::channel::<ClipChange>();
    let b_clipboard = Clipboard {
        events_rx: b_events_rx,
        apply_tx: b_apply_tx,
    };
    let (b_transport, _b_join) = transport::spawn(client_cfg(addr));
    tokio::spawn(async move { run_supervisor(b_clipboard, b_transport).await });

    tokio::time::sleep(Duration::from_millis(300)).await;

    a_events_tx
        .send(ClipChange::Text("hello via the hub".into()))
        .await
        .unwrap();

    let recv = tokio::task::spawn_blocking(move || b_apply_rx.recv_timeout(Duration::from_secs(3)))
        .await
        .unwrap()
        .expect("B should receive the relayed clip");
    match recv {
        ClipChange::Text(t) => assert_eq!(t, "hello via the hub"),
        other => panic!("expected text, got {other:?}"),
    }
}

#[tokio::test]
async fn image_round_trip_through_real_hub() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (app, _hub_join) = build_app(server_cfg(addr));
    let _server_task = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    // Client A publishes the image.
    let (a_events_tx, a_events_rx) = mpsc::channel::<ClipChange>(8);
    let (a_apply_tx, _) = smpsc::channel::<ClipChange>();
    let a_clipboard = Clipboard {
        events_rx: a_events_rx,
        apply_tx: a_apply_tx,
    };
    let (a_transport, _) = transport::spawn(client_cfg(addr));
    tokio::spawn(async move { run_supervisor(a_clipboard, a_transport).await });

    // Client B receives the image.
    let (_b_events_tx, b_events_rx) = mpsc::channel::<ClipChange>(8);
    let (b_apply_tx, b_apply_rx) = smpsc::channel::<ClipChange>();
    let b_clipboard = Clipboard {
        events_rx: b_events_rx,
        apply_tx: b_apply_tx,
    };
    let (b_transport, _) = transport::spawn(client_cfg(addr));
    tokio::spawn(async move { run_supervisor(b_clipboard, b_transport).await });

    tokio::time::sleep(Duration::from_millis(300)).await;

    // A small but distinctive 2x2 image.
    let original = ImageBytes {
        width: 2,
        height: 2,
        rgba: vec![
            255, 0, 0, 255, // top-left red
            0, 255, 0, 255, // top-right green
            0, 0, 255, 255, // bottom-left blue
            128, 128, 128, 255, // bottom-right gray
        ],
    };
    a_events_tx
        .send(ClipChange::Image(original.clone()))
        .await
        .unwrap();

    let recv = tokio::task::spawn_blocking(move || b_apply_rx.recv_timeout(Duration::from_secs(3)))
        .await
        .unwrap()
        .expect("B should receive the relayed image");
    match recv {
        ClipChange::Image(got) => assert_eq!(got, original),
        other => panic!("expected image, got {other:?}"),
    }
}

#[tokio::test]
async fn late_joiner_supervisor_applies_cached_clip() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (app, _hub_join) = build_app(server_cfg(addr));
    let _server_task = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    // A publishes first.
    let (a_events_tx, a_events_rx) = mpsc::channel::<ClipChange>(8);
    let (a_apply_tx, _) = smpsc::channel::<ClipChange>();
    let a_clipboard = Clipboard {
        events_rx: a_events_rx,
        apply_tx: a_apply_tx,
    };
    let (a_transport, _) = transport::spawn(client_cfg(addr));
    tokio::spawn(async move { run_supervisor(a_clipboard, a_transport).await });

    tokio::time::sleep(Duration::from_millis(200)).await;
    a_events_tx
        .send(ClipChange::Text("cached".into()))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;

    // B joins later — its welcome should carry the cached clip, which the
    // supervisor will apply.
    let (_b_events_tx, b_events_rx) = mpsc::channel::<ClipChange>(8);
    let (b_apply_tx, b_apply_rx) = smpsc::channel::<ClipChange>();
    let b_clipboard = Clipboard {
        events_rx: b_events_rx,
        apply_tx: b_apply_tx,
    };
    let (b_transport, _) = transport::spawn(client_cfg(addr));
    tokio::spawn(async move { run_supervisor(b_clipboard, b_transport).await });

    let recv = tokio::task::spawn_blocking(move || b_apply_rx.recv_timeout(Duration::from_secs(3)))
        .await
        .unwrap()
        .expect("late joiner should apply cached clip");
    match recv {
        ClipChange::Text(t) => assert_eq!(t, "cached"),
        other => panic!("expected text, got {other:?}"),
    }
}
