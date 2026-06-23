// SPDX-License-Identifier: GPL-3.0-or-later

//! JNI bridge that embeds the clipboardwire hub server inside the
//! Android app. A single background thread runs a current-thread
//! Tokio runtime with the axum WebSocket server — no multi-thread
//! runtime overhead, minimal battery impact.

use std::net::SocketAddr;
use std::path::PathBuf;

use jni::objects::{JObject, JString};
use jni::sys::{jboolean, jint, jlong};
use jni::JNIEnv;
use tracing::info;

use clipboardwire_core::protocol::MAX_FRAME_BYTES;
use clipboardwire_core::server::hub::HubStatsSink;
use clipboardwire_core::server::{self, ServerConfig};

struct ServerHandle {
    shutdown_tx: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
    stats: HubStatsSink,
}

fn to_rust_string(env: &mut JNIEnv, s: &JString) -> Result<String, jni::errors::Error> {
    Ok(env.get_string(s)?.into())
}

/// Start the hub server on a background thread.
///
/// Returns an opaque handle (non-zero) on success, or 0 on failure
/// (a Java RuntimeException is also thrown with the error message).
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_davefx_clipboardwire_service_NativeServer_nativeStart<'local>(
    mut env: JNIEnv<'local>,
    _this: JObject<'local>,
    bind_port: jint,
    user: JString<'local>,
    password: JString<'local>,
    state_dir: JString<'local>,
    tls_disabled: jboolean,
    ping_interval_secs: jlong,
    read_timeout_secs: jlong,
) -> jlong {
    let result = (|| -> anyhow::Result<jlong> {
        let user = to_rust_string(&mut env, &user)?;
        let password = to_rust_string(&mut env, &password)?;
        let state_dir_str = to_rust_string(&mut env, &state_dir)?;

        let bind: SocketAddr = format!("0.0.0.0:{bind_port}")
            .parse()
            .map_err(|e| anyhow::anyhow!("bad port: {e}"))?;

        let stats = HubStatsSink::new();
        let config = ServerConfig {
            bind,
            user,
            password,
            max_conns: 16,
            max_frame_bytes: MAX_FRAME_BYTES,
            tls_cert_file: None,
            tls_key_file: None,
            tls_disabled: tls_disabled != 0,
            state_dir: Some(PathBuf::from(state_dir_str)),
            stats: Some(stats.clone()),
            ping_interval_secs: ping_interval_secs as u64,
            read_timeout_secs: read_timeout_secs as u64,
        };

        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();

        // Channel to wait for bind result before returning to Kotlin.
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel::<Result<SocketAddr, String>>(1);

        let thread = std::thread::Builder::new()
            .name("cw-server".into())
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("tokio runtime");

                rt.block_on(async {
                    match server::bind(&config).await {
                        Ok((listener, addr)) => {
                            let _ = ready_tx.send(Ok(addr));
                            let shutdown = async {
                                let _ = shutdown_rx.await;
                            };
                            if let Err(e) = server::serve(listener, config, shutdown).await {
                                tracing::error!("server error: {e}");
                            }
                        }
                        Err(e) => {
                            let _ = ready_tx.send(Err(e.to_string()));
                        }
                    }
                });
            })?;

        match ready_rx.recv() {
            Ok(Ok(addr)) => {
                info!(addr = %addr, "embedded server started");
                let handle = Box::new(ServerHandle {
                    shutdown_tx: Some(shutdown_tx),
                    thread: Some(thread),
                    stats,
                });
                Ok(Box::into_raw(handle) as jlong)
            }
            Ok(Err(e)) => anyhow::bail!("server failed to bind: {e}"),
            Err(_) => anyhow::bail!("server thread exited before reporting bind result"),
        }
    })();

    match result {
        Ok(handle) => handle,
        Err(e) => {
            let _ = env.throw_new("java/lang/RuntimeException", e.to_string());
            0
        }
    }
}

/// Stop the hub server and release all resources.
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_davefx_clipboardwire_service_NativeServer_nativeStop(
    _env: JNIEnv,
    _this: JObject,
    handle: jlong,
) {
    if handle == 0 {
        return;
    }
    let mut handle = unsafe { Box::from_raw(handle as *mut ServerHandle) };
    if let Some(tx) = handle.shutdown_tx.take() {
        let _ = tx.send(());
    }
    if let Some(thread) = handle.thread.take() {
        let _ = thread.join();
    }
    info!("embedded server stopped");
}

/// Return the number of clients currently connected to the hub.
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_davefx_clipboardwire_service_NativeServer_nativeGetClientCount(
    _env: JNIEnv,
    _this: JObject,
    handle: jlong,
) -> jint {
    if handle == 0 {
        return 0;
    }
    let handle = unsafe { &*(handle as *const ServerHandle) };
    handle.stats.current() as jint
}
