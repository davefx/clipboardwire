// SPDX-License-Identifier: GPL-3.0-or-later

//! End-to-end file-transfer test.
//!
//! Spins up a real hub, two real WebSocket transports, and pushes a
//! file through the sender's `outbound_files_tx`. A small relay task
//! pumps the receiver's `inbound_files_rx` into a `FileReceiver` with
//! a sandboxed save dir. Assert: the assembled file matches the
//! original bytes.
//!
//! Exercises the v0.3.0 protocol additions (`file_chunk` frame +
//! per-client file fan-out channels in the hub).

use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use clipboardwire_core::client::file::{send_file_through, FileReceiver};
use clipboardwire_core::client::transport::{spawn_with_options, SpawnOptions};
use clipboardwire_core::client::{ClientConfig, ClientStatus};
use clipboardwire_core::server::{build_app, ServerConfig};
use tokio::net::TcpListener;
use tokio::sync::watch;

fn server_cfg(addr: std::net::SocketAddr) -> ServerConfig {
    ServerConfig {
        bind: addr,
        user: "alice".into(),
        password: "hunter2".into(),
        max_conns: 8,
        // Big enough for the test files plus a 4 MiB chunk + base64
        // inflation; matches the receiver's expectations.
        max_frame_bytes: 16 * 1024 * 1024,
        tls_cert_file: None,
        tls_key_file: None,
        tls_disabled: true,
        state_dir: None,
        stats: None,
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

async fn wait_for_connected(rx: &mut watch::Receiver<ClientStatus>) {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if matches!(*rx.borrow(), ClientStatus::Connected) {
            return;
        }
        if std::time::Instant::now() >= deadline {
            panic!("transport never reached Connected within 5s");
        }
        if rx.changed().await.is_err() {
            panic!("status channel dropped before Connected");
        }
    }
}

fn unique_tmp_dir(label: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "cw-file-e2e-{label}-{}-{nanos}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn file_sent_by_one_client_arrives_intact_at_the_other() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (app, _hub_join) = build_app(server_cfg(addr));
    let _server_task = tokio::spawn(async move {
        let _ = axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await;
    });

    // -- Receiver: subscribes to inbound files and pipes them into a
    //    FileReceiver whose save_dir is a sandboxed tempdir. --
    let receiver_save_dir = unique_tmp_dir("recv");
    let (recv_status_tx, mut recv_status_rx) = watch::channel(ClientStatus::Connecting);
    let (mut receiver_transport, _r_join) = spawn_with_options(
        client_cfg(addr),
        SpawnOptions {
            status_tx: Some(recv_status_tx),
            receive_files: true,
            one_shot: false,
        },
    );
    let mut inbound_files_rx = receiver_transport
        .inbound_files_rx
        .take()
        .expect("we asked for receive_files=true");

    let receiver_save_dir_for_task = receiver_save_dir.clone();
    let (done_tx, done_rx) = tokio::sync::oneshot::channel();
    let receiver_task = tokio::spawn(async move {
        let mut receiver = FileReceiver::with_save_dir(receiver_save_dir_for_task).unwrap();
        let mut done_tx = Some(done_tx);
        while let Some(chunk) = inbound_files_rx.recv().await {
            match receiver.receive_chunk(chunk) {
                Ok(Some(path)) => {
                    if let Some(tx) = done_tx.take() {
                        let _ = tx.send(path);
                    }
                }
                Ok(None) => {}
                Err(e) => panic!("receiver rejected a chunk: {e:#}"),
            }
        }
    });

    // -- Sender: one-shot, no inbound channels --
    let (send_status_tx, mut send_status_rx) = watch::channel(ClientStatus::Connecting);
    let (sender_transport, sender_join) = spawn_with_options(
        client_cfg(addr),
        SpawnOptions {
            status_tx: Some(send_status_tx),
            receive_files: false,
            one_shot: true,
        },
    );

    // Wait until BOTH transports report Connected before publishing.
    wait_for_connected(&mut recv_status_rx).await;
    wait_for_connected(&mut send_status_rx).await;
    // ClientStatus::Connected fires inside connect_and_serve *before*
    // it consumes the welcome frame and enters its inner select loop.
    // Give both peers a couple of poll ticks so they're actually
    // listening on their outbound channels by the time we publish.
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Small payload first to keep this test fast — a separate test
    // below exercises the multi-chunk path.
    let payload: Vec<u8> = b"hello clipboardwire file transfer".to_vec();
    let send_dir = unique_tmp_dir("send");
    let src_path = send_dir.join("payload.bin");
    fs::write(&src_path, &payload).unwrap();

    send_file_through(&src_path, &sender_transport.outbound_files_tx)
        .await
        .expect("send_file_through");

    // Close the sender's channels so the one-shot run_loop exits.
    drop(sender_transport);
    let _ = tokio::time::timeout(Duration::from_secs(5), sender_join).await;

    // Wait for the receiver to assemble the file (timeout ⇒ test fail).
    let final_path = tokio::time::timeout(Duration::from_secs(10), done_rx)
        .await
        .expect("file should be assembled within 10s")
        .expect("oneshot channel");

    // The receiver is allowed to keep running until the test scope ends.
    drop(receiver_task);

    let received = fs::read(&final_path).expect("assembled file readable");
    assert_eq!(received.len(), payload.len(), "size mismatch");
    assert_eq!(received, payload, "content mismatch");

    let _ = fs::remove_dir_all(&send_dir);
    let _ = fs::remove_dir_all(&receiver_save_dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn multi_chunk_file_round_trips_intact() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    // The 9-MiB payload crosses FILE_CHUNK_BYTES (4 MiB), so we need a
    // bigger max_frame_bytes on the hub than the default test config.
    let mut cfg = server_cfg(addr);
    cfg.max_frame_bytes = 16 * 1024 * 1024;
    let (app, _hub_join) = build_app(cfg);
    let _server_task = tokio::spawn(async move {
        let _ = axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await;
    });

    let receiver_save_dir = unique_tmp_dir("multi-recv");
    let (recv_status_tx, mut recv_status_rx) = watch::channel(ClientStatus::Connecting);
    let (mut receiver_transport, _r_join) = spawn_with_options(
        client_cfg(addr),
        SpawnOptions {
            status_tx: Some(recv_status_tx),
            receive_files: true,
            one_shot: false,
        },
    );
    let mut inbound_files_rx = receiver_transport.inbound_files_rx.take().unwrap();
    let receiver_save_dir_for_task = receiver_save_dir.clone();
    let (done_tx, done_rx) = tokio::sync::oneshot::channel();
    let _receiver_task = tokio::spawn(async move {
        let mut receiver = FileReceiver::with_save_dir(receiver_save_dir_for_task).unwrap();
        let mut done_tx = Some(done_tx);
        while let Some(chunk) = inbound_files_rx.recv().await {
            if let Ok(Some(path)) = receiver.receive_chunk(chunk) {
                if let Some(tx) = done_tx.take() {
                    let _ = tx.send(path);
                }
            }
        }
    });

    let (send_status_tx, mut send_status_rx) = watch::channel(ClientStatus::Connecting);
    let (sender_transport, sender_join) = spawn_with_options(
        client_cfg(addr),
        SpawnOptions {
            status_tx: Some(send_status_tx),
            receive_files: false,
            one_shot: true,
        },
    );

    wait_for_connected(&mut recv_status_rx).await;
    wait_for_connected(&mut send_status_rx).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    // 9 MiB ⇒ 3 chunks at 4 MiB each.
    let mut payload = vec![0u8; 9 * 1024 * 1024];
    for (i, b) in payload.iter_mut().enumerate() {
        *b = ((i.wrapping_mul(31)).wrapping_add(7) & 0xff) as u8;
    }
    let send_dir = unique_tmp_dir("multi-send");
    let src_path = send_dir.join("blob.bin");
    fs::write(&src_path, &payload).unwrap();

    send_file_through(&src_path, &sender_transport.outbound_files_tx)
        .await
        .expect("send_file_through");
    drop(sender_transport);
    let _ = tokio::time::timeout(Duration::from_secs(10), sender_join).await;

    let final_path = tokio::time::timeout(Duration::from_secs(15), done_rx)
        .await
        .expect("multi-chunk file should assemble within 15s")
        .expect("oneshot channel");
    let received = fs::read(&final_path).unwrap();
    assert_eq!(received.len(), payload.len());
    assert_eq!(received, payload);

    let _ = fs::remove_dir_all(&send_dir);
    let _ = fs::remove_dir_all(&receiver_save_dir);
}
