// SPDX-License-Identifier: GPL-3.0-or-later

//! Self-healing behaviour: the hub and the client recover from the
//! failure modes seen in the field without anyone restarting the app.
//!
//! - "Restart hub" raced the old listener and failed with EADDRINUSE,
//!   leaving no hub running (`bind_with_retry`).
//! - A hub that exits on its own stayed down (`serve_supervised`).
//! - A client whose hub went silent (sleep, dropped network, wedged
//!   peer) waited forever instead of reconnecting (client read timeout).
//! - Nothing in the log said whether a clip was published or received,
//!   so a lost clip couldn't be traced to either side.

use std::io;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use clipboardwire_core::client::transport::{self, spawn_with_options, SpawnOptions};
use clipboardwire_core::client::{run_supervisor, ClientConfig, ClipChange, Clipboard};
use clipboardwire_core::protocol::{Frame, WelcomeFrame, PROTOCOL_VERSION};
use clipboardwire_core::server::{bind_with_retry, build_app, serve_supervised, tls, ServerConfig};
use futures_util::sink::SinkExt;
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::protocol::Message;

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

fn client_cfg(url: String) -> ClientConfig {
    serde_json::from_value(serde_json::json!({
        "server": url, "user": USER, "password": PW, "tls_insecure": true,
    }))
    .unwrap()
}

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("clipboardwire-{tag}-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

// ---------------------------------------------------------------- bind

#[tokio::test]
async fn bind_with_retry_waits_for_the_previous_listener_to_go_away() {
    let old = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = old.local_addr().unwrap();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        drop(old);
    });

    let (_listener, bound) = bind_with_retry(&server_cfg(addr), Duration::from_secs(3))
        .await
        .expect("should bind once the old listener is dropped");
    assert_eq!(bound, addr);
}

#[tokio::test]
async fn bind_with_retry_gives_up_after_the_deadline() {
    let _held = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = _held.local_addr().unwrap();

    let started = std::time::Instant::now();
    let err = bind_with_retry(&server_cfg(addr), Duration::from_millis(500))
        .await
        .expect_err("port is held for the whole test");
    assert!(started.elapsed() >= Duration::from_millis(500));
    let io_err = err.downcast_ref::<io::Error>().expect("io::Error in chain");
    assert_eq!(io_err.kind(), io::ErrorKind::AddrInUse);
}

// ---------------------------------------------------------- supervision

#[tokio::test]
async fn serve_supervised_restarts_the_hub_after_it_fails() {
    let dir = temp_dir("supervised");
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    // Point at cert files that don't exist yet: every `serve` attempt
    // fails until they appear.
    let cfg = ServerConfig {
        tls_disabled: false,
        tls_cert_file: Some(dir.join("self-signed.crt")),
        tls_key_file: Some(dir.join("self-signed.key")),
        ..server_cfg(addr)
    };
    let _hub = tokio::spawn(serve_supervised(listener, cfg, std::future::pending()));

    tokio::time::sleep(Duration::from_millis(300)).await;
    let ensured = tls::ensure_self_signed_cert(&dir, addr).unwrap();
    assert_eq!(ensured.cert_path, dir.join("self-signed.crt"));

    // The supervisor's next attempt picks the files up and serves.
    let url = format!("wss://127.0.0.1:{}/sync", addr.port());
    let mut connected = false;
    for _ in 0..50 {
        if try_connect(&url).await {
            connected = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        connected,
        "hub never came back after its first attempt failed"
    );
}

#[tokio::test]
async fn serve_supervised_stops_on_shutdown() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let hub = tokio::spawn(serve_supervised(listener, server_cfg(addr), async move {
        let _ = stop_rx.await;
    }));
    tokio::time::sleep(Duration::from_millis(100)).await;
    stop_tx.send(()).unwrap();

    let result = tokio::time::timeout(Duration::from_secs(5), hub)
        .await
        .expect("serve_supervised kept running after shutdown");
    result.unwrap().unwrap();
    // And the port is free again.
    TcpListener::bind(addr).await.expect("port released");
}

async fn try_connect(url: &str) -> bool {
    use clipboardwire_core::server::auth::basic_header_value;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let mut req = url.into_client_request().unwrap();
    req.headers_mut().insert(
        "authorization",
        basic_header_value(USER, PW).parse().unwrap(),
    );
    let connector =
        clipboardwire_core::client::tls::make_connector(&client_cfg(url.into())).unwrap();
    tokio_tungstenite::connect_async_tls_with_config(req, None, false, Some(connector))
        .await
        .is_ok()
}

// ------------------------------------------------------- client timeout

/// A "hub" that completes the handshake, sends `welcome`, and then goes
/// silent without closing — no frames, no pongs. Counts connections.
async fn silent_hub() -> (SocketAddr, Arc<Mutex<usize>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let accepted = Arc::new(Mutex::new(0usize));
    let count = accepted.clone();
    tokio::spawn(async move {
        let mut held = Vec::new();
        loop {
            let (tcp, _) = listener.accept().await.unwrap();
            *count.lock().unwrap() += 1;
            let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
            let welcome = Frame::Welcome(WelcomeFrame {
                server: PROTOCOL_VERSION.to_string(),
                client_id: uuid::Uuid::new_v4(),
                last_clip: None,
            });
            ws.send(Message::Text(
                serde_json::to_string(&welcome).unwrap().into(),
            ))
            .await
            .unwrap();
            // Never read again: pings go unanswered.
            held.push(ws);
        }
    });
    (addr, accepted)
}

#[tokio::test]
async fn client_reconnects_when_the_hub_goes_silent() {
    let (addr, accepted) = silent_hub().await;
    let (_transport, _join) = spawn_with_options(
        client_cfg(format!("ws://{addr}/sync")),
        SpawnOptions {
            read_timeout: Some(Duration::from_millis(500)),
            ..SpawnOptions::default()
        },
    );

    // 500 ms of silence + 1 s initial backoff → second connection.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while *accepted.lock().unwrap() < 2 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "client never gave up on the silent hub"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

// -------------------------------------------------------------- logging

#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl io::Write for Captured {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Captured {
    type Writer = Captured;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

// Current-thread runtime (the `tokio::test` default): every spawned task
// runs on this thread, so the thread-local subscriber sees all of them.
#[tokio::test]
async fn supervisor_logs_published_and_received_clips_without_content() {
    let logs = Captured::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(logs.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::INFO)
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (app, _hub_join) = build_app(server_cfg(addr));
    tokio::spawn(async move {
        let _ = axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await;
    });

    let url = format!("ws://{addr}/sync");
    let (a_events_tx, a_events_rx) = mpsc::channel::<ClipChange>(8);
    let (a_apply_tx, _a_apply_rx) = std::sync::mpsc::channel::<ClipChange>();
    let (a_transport, _a) = transport::spawn(client_cfg(url.clone()));
    tokio::spawn(run_supervisor(
        Clipboard {
            events_rx: a_events_rx,
            apply_tx: a_apply_tx,
        },
        a_transport,
    ));
    let (_b_events_tx, b_events_rx) = mpsc::channel::<ClipChange>(8);
    let (b_apply_tx, b_apply_rx) = std::sync::mpsc::channel::<ClipChange>();
    let (b_transport, _b) = transport::spawn(client_cfg(url));
    tokio::spawn(run_supervisor(
        Clipboard {
            events_rx: b_events_rx,
            apply_tx: b_apply_tx,
        },
        b_transport,
    ));
    tokio::time::sleep(Duration::from_millis(300)).await;

    let secret = "s3cret clipboard text";
    a_events_tx
        .send(ClipChange::Text(secret.into()))
        .await
        .unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        if let Ok(ClipChange::Text(t)) = b_apply_rx.try_recv() {
            assert_eq!(t, secret);
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "B never got the clip"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let out = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
    let published = out.lines().find(|l| l.contains("publishing local clip"));
    let received = out.lines().find(|l| l.contains("received remote clip"));
    let published = published.unwrap_or_else(|| panic!("no publish log in:\n{out}"));
    let received = received.unwrap_or_else(|| panic!("no receive log in:\n{out}"));
    let len = format!("bytes={}", secret.len());
    assert!(published.contains(&len), "{published}");
    assert!(received.contains(&len), "{received}");
    assert!(received.contains("from="), "{received}");
    assert!(!out.contains(secret), "clip content leaked into the log");
}

// ------------------------------------------------- tray restart sequence

/// What the tray's "Restart hub" does: abort the running hub task and
/// immediately bind + serve the same port again. A client that was
/// connected before must end up relaying through the new hub.
#[tokio::test]
async fn restart_like_the_tray_keeps_existing_clients_working() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let old_hub = tokio::spawn(serve_supervised(
        listener,
        server_cfg(addr),
        std::future::pending(),
    ));

    let url = format!("ws://{addr}/sync");
    let (a_transport, _a) = transport::spawn(client_cfg(url.clone()));
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Restart: abort, then re-bind right away (this used to fail with
    // EADDRINUSE and leave no hub at all).
    old_hub.abort();
    let (listener, _) = bind_with_retry(&server_cfg(addr), Duration::from_secs(3))
        .await
        .expect("re-bind after abort");
    let _new_hub = tokio::spawn(serve_supervised(
        listener,
        server_cfg(addr),
        std::future::pending(),
    ));

    // A fresh client B on the new hub must hear from A once A has
    // reconnected (1 s backoff after the 1012 close).
    let (mut b_transport, _b) = transport::spawn(client_cfg(url));
    let deadline = tokio::time::Instant::now() + Duration::from_secs(6);
    loop {
        let clip = clipboardwire_core::protocol::ClipFrame {
            id: uuid::Uuid::new_v4(),
            ts: 0,
            content_type: clipboardwire_core::protocol::TEXT_CONTENT_TYPE.to_string(),
            content: Some("after restart".into()),
            content_b64: None,
            from: None,
        };
        let _ = a_transport.outbound_tx.try_send(clip);
        if let Ok(Some(got)) =
            tokio::time::timeout(Duration::from_millis(300), b_transport.inbound_rx.recv()).await
        {
            if got.content.as_deref() == Some("after restart") {
                break;
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "client connected before the restart never reached the new hub"
        );
    }
}
