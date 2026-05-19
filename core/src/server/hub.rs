// SPDX-License-Identifier: GPL-3.0-or-later

//! Central fan-out hub.
//!
//! A single Tokio task owns the canonical state — the set of connected
//! clients and the `last_clip` cache. WebSocket connection tasks talk to it
//! over an mpsc inbox. Single-ownership keeps the state lock-free.
//!
//! Per-client outbound channels are bounded (see [`PER_CLIENT_CHANNEL_BUF`]).
//! When a slow peer's buffer is full, the hub drops the frame for that one
//! peer rather than blocking everyone — clipboard sync is last-write-wins,
//! so dropping an intermediate value is acceptable.

use std::collections::HashMap;

use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tracing::{debug, info, instrument, warn};
use uuid::Uuid;

use crate::protocol::ClipFrame;

/// Capacity of each per-client outbound mpsc.
pub const PER_CLIENT_CHANNEL_BUF: usize = 32;

/// Capacity of the hub's own inbox. Held by every connection task plus the
/// supervisor; sized generously since the hub drains synchronously.
const HUB_INBOX_BUF: usize = 1024;

#[derive(Debug)]
pub enum HubMessage {
    Register {
        id: Uuid,
        outbound: mpsc::Sender<ClipFrame>,
        ack: oneshot::Sender<RegisterResult>,
    },
    Deregister {
        id: Uuid,
    },
    Publish {
        from: Uuid,
        clip: ClipFrame,
    },
}

#[derive(Debug)]
pub enum RegisterResult {
    Accepted { last_clip: Option<ClipFrame> },
    AtCapacity,
}

/// Cheaply-cloneable handle that connection tasks use to talk to the hub.
#[derive(Clone)]
pub struct HubHandle {
    tx: mpsc::Sender<HubMessage>,
}

impl HubHandle {
    /// Register a connection. On success, returns the `last_clip` snapshot
    /// that the connection should embed in its outbound `welcome` frame.
    pub async fn register(
        &self,
        id: Uuid,
        outbound: mpsc::Sender<ClipFrame>,
    ) -> anyhow::Result<RegisterResult> {
        let (ack_tx, ack_rx) = oneshot::channel();
        self.tx
            .send(HubMessage::Register {
                id,
                outbound,
                ack: ack_tx,
            })
            .await
            .map_err(|_| anyhow::anyhow!("hub task is gone"))?;
        ack_rx
            .await
            .map_err(|_| anyhow::anyhow!("hub dropped registration ack"))
    }

    /// Best-effort deregistration; ignores send failure because if the hub
    /// is gone, there is no state to clean up anyway.
    pub async fn deregister(&self, id: Uuid) {
        let _ = self.tx.send(HubMessage::Deregister { id }).await;
    }

    /// Publish a clip frame from the given origin connection. The hub stamps
    /// `clip.from = Some(from)` before relaying to peers.
    pub async fn publish(&self, from: Uuid, clip: ClipFrame) -> anyhow::Result<()> {
        self.tx
            .send(HubMessage::Publish { from, clip })
            .await
            .map_err(|_| anyhow::anyhow!("hub task is gone"))
    }
}

pub(crate) struct Hub {
    clients: HashMap<Uuid, mpsc::Sender<ClipFrame>>,
    last_clip: Option<ClipFrame>,
    max_clients: usize,
}

impl Hub {
    pub(crate) fn new(max_clients: usize) -> Self {
        Self {
            clients: HashMap::new(),
            last_clip: None,
            max_clients,
        }
    }

    #[instrument(skip_all)]
    pub(crate) async fn run(mut self, mut inbox: mpsc::Receiver<HubMessage>) {
        info!(max_clients = self.max_clients, "hub started");
        while let Some(msg) = inbox.recv().await {
            self.handle(msg);
        }
        info!("hub stopping (inbox closed)");
    }

    pub(crate) fn handle(&mut self, msg: HubMessage) {
        match msg {
            HubMessage::Register {
                id,
                outbound,
                ack,
            } => {
                if self.clients.len() >= self.max_clients {
                    warn!(client = %id, "rejecting registration: at capacity");
                    let _ = ack.send(RegisterResult::AtCapacity);
                    return;
                }
                self.clients.insert(id, outbound);
                let snapshot = self.last_clip.clone();
                debug!(client = %id, total = self.clients.len(), "registered");
                let _ = ack.send(RegisterResult::Accepted {
                    last_clip: snapshot,
                });
            }
            HubMessage::Deregister { id } => {
                if self.clients.remove(&id).is_some() {
                    debug!(client = %id, total = self.clients.len(), "deregistered");
                }
            }
            HubMessage::Publish { from, mut clip } => {
                clip.from = Some(from);
                self.last_clip = Some(clip.clone());

                for (peer_id, tx) in self.clients.iter() {
                    if *peer_id == from {
                        continue;
                    }
                    match tx.try_send(clip.clone()) {
                        Ok(()) => {}
                        Err(mpsc::error::TrySendError::Full(_)) => {
                            warn!(client = %peer_id, "outbound buffer full; dropping frame");
                        }
                        Err(mpsc::error::TrySendError::Closed(_)) => {
                            // Peer's task already exited; deregistration is
                            // either in flight or will happen shortly. We
                            // *could* eagerly remove from the map here, but
                            // that would mean mutating during iteration.
                        }
                    }
                }
            }
        }
    }
}

/// Spawn the hub task on the current Tokio runtime.
///
/// Returns a cloneable handle for connection tasks and the join handle for
/// the supervisor (used for orderly shutdown).
pub fn spawn(max_clients: usize) -> (HubHandle, JoinHandle<()>) {
    let (tx, rx) = mpsc::channel::<HubMessage>(HUB_INBOX_BUF);
    let hub = Hub::new(max_clients);
    let join = tokio::spawn(hub.run(rx));
    (HubHandle { tx }, join)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::SUPPORTED_CONTENT_TYPE;

    fn dummy_clip(content: &str) -> ClipFrame {
        ClipFrame {
            id: Uuid::new_v4(),
            ts: 0,
            content_type: SUPPORTED_CONTENT_TYPE.to_string(),
            content: content.to_string(),
            from: None,
        }
    }

    #[tokio::test]
    async fn publish_fans_out_to_peers_but_not_sender() {
        let (hub, _join) = spawn(8);

        let id_a = Uuid::new_v4();
        let (tx_a, mut rx_a) = mpsc::channel(8);
        assert!(matches!(
            hub.register(id_a, tx_a).await.unwrap(),
            RegisterResult::Accepted { last_clip: None }
        ));

        let id_b = Uuid::new_v4();
        let (tx_b, mut rx_b) = mpsc::channel(8);
        hub.register(id_b, tx_b).await.unwrap();

        hub.publish(id_a, dummy_clip("hello")).await.unwrap();

        let received = rx_b.recv().await.expect("B should receive the clip");
        assert_eq!(received.content, "hello");
        assert_eq!(received.from, Some(id_a));

        // A should NOT receive its own publish.
        let try_a = tokio::time::timeout(std::time::Duration::from_millis(50), rx_a.recv()).await;
        assert!(
            try_a.is_err(),
            "sender should not get an echo of its own publish"
        );
    }

    #[tokio::test]
    async fn last_clip_is_served_to_late_joiners() {
        let (hub, _join) = spawn(8);

        let id_a = Uuid::new_v4();
        let (tx_a, _rx_a) = mpsc::channel(8);
        hub.register(id_a, tx_a).await.unwrap();
        hub.publish(id_a, dummy_clip("cached")).await.unwrap();

        // Give the hub a tick to apply the publish.
        tokio::task::yield_now().await;

        let id_b = Uuid::new_v4();
        let (tx_b, _rx_b) = mpsc::channel(8);
        let result = hub.register(id_b, tx_b).await.unwrap();
        match result {
            RegisterResult::Accepted { last_clip } => {
                let clip = last_clip.expect("late joiner should see the cached clip");
                assert_eq!(clip.content, "cached");
                assert_eq!(clip.from, Some(id_a));
            }
            other => panic!("expected Accepted, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn registration_is_rejected_at_capacity() {
        let (hub, _join) = spawn(2);

        for _ in 0..2 {
            let (tx, _rx) = mpsc::channel(8);
            assert!(matches!(
                hub.register(Uuid::new_v4(), tx).await.unwrap(),
                RegisterResult::Accepted { .. }
            ));
        }

        let (tx, _rx) = mpsc::channel(8);
        let third = hub.register(Uuid::new_v4(), tx).await.unwrap();
        assert!(matches!(third, RegisterResult::AtCapacity));
    }

    #[tokio::test]
    async fn deregister_frees_capacity() {
        let (hub, _join) = spawn(1);

        let id_a = Uuid::new_v4();
        let (tx_a, _rx_a) = mpsc::channel(8);
        hub.register(id_a, tx_a).await.unwrap();

        // Second registration is rejected.
        let (tx_b, _rx_b) = mpsc::channel(8);
        assert!(matches!(
            hub.register(Uuid::new_v4(), tx_b).await.unwrap(),
            RegisterResult::AtCapacity
        ));

        hub.deregister(id_a).await;
        tokio::task::yield_now().await;

        // Now there is room again.
        let (tx_c, _rx_c) = mpsc::channel(8);
        assert!(matches!(
            hub.register(Uuid::new_v4(), tx_c).await.unwrap(),
            RegisterResult::Accepted { .. }
        ));
    }

    #[tokio::test]
    async fn slow_peer_does_not_block_others() {
        let (hub, _join) = spawn(8);

        // Peer A registers with a 0-capacity channel. Any try_send will fail
        // immediately with Full — but that must not affect deliveries to B.
        let id_a = Uuid::new_v4();
        let (tx_a, _rx_a) = mpsc::channel(1);
        hub.register(id_a, tx_a).await.unwrap();

        // Fill A's buffer by sending from someone else first.
        let id_pub = Uuid::new_v4();
        let (tx_pub, _rx_pub) = mpsc::channel(8);
        hub.register(id_pub, tx_pub).await.unwrap();

        let id_b = Uuid::new_v4();
        let (tx_b, mut rx_b) = mpsc::channel(8);
        hub.register(id_b, tx_b).await.unwrap();

        // First publish fills A's slot (capacity 1).
        hub.publish(id_pub, dummy_clip("first")).await.unwrap();
        // Second publish — A's buffer is now full and the frame should be
        // dropped *for A only*. B should still receive both.
        hub.publish(id_pub, dummy_clip("second")).await.unwrap();

        let m1 = rx_b.recv().await.unwrap();
        let m2 = rx_b.recv().await.unwrap();
        assert_eq!(m1.content, "first");
        assert_eq!(m2.content, "second");
    }

    #[tokio::test]
    async fn last_clip_carries_from_field() {
        let (hub, _join) = spawn(8);

        let id_a = Uuid::new_v4();
        let (tx_a, _rx_a) = mpsc::channel(8);
        hub.register(id_a, tx_a).await.unwrap();

        let mut clip = dummy_clip("x");
        clip.from = None;
        hub.publish(id_a, clip).await.unwrap();
        tokio::task::yield_now().await;

        let id_b = Uuid::new_v4();
        let (tx_b, _rx_b) = mpsc::channel(8);
        let r = hub.register(id_b, tx_b).await.unwrap();
        if let RegisterResult::Accepted {
            last_clip: Some(cached),
        } = r
        {
            assert_eq!(cached.from, Some(id_a));
        } else {
            panic!("expected cached clip with from set");
        }
    }
}
