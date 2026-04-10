use std::{net::SocketAddr, sync::{Arc, atomic::{AtomicU64, Ordering}}};

use aes_gcm::{Aes128Gcm, KeyInit, Nonce, aead::Aead};
use anyhow::Result;
use async_trait::async_trait;
use bytes::{Bytes, BytesMut};
use cidr::AnyIpCidr;
use rustrtc::{IceCandidate, IceCandidatePair, IceGathererState, IceRole, IceTransport, RtcConfiguration, transports::{PacketReceiver, ice::{IceParameters, IceSocketWrapper}}};
use tokio::sync::{mpsc, watch};
use tracing::debug;

use crate::{fullmesh::{FullMeshKey, IceServer}, mesh::packet::NodeId};

pub struct Connection {
    id: NodeId,
    peer_id: NodeId,
    ice: IceTransport,
    candidate_filter_prefix: Option<AnyIpCidr>,
    key: watch::Sender<FullMeshKey>,

    sent: Arc<AtomicU64>,
    received: Arc<AtomicU64>,
}

impl Connection {
    pub async fn new(id: NodeId, peer_id: NodeId, ice_servers: Vec<IceServer>, candidate_filter_prefix: Option<AnyIpCidr>, controlling: bool, key: FullMeshKey) -> Result<(Self, IceParameters)> {
        let (ice, future) = IceTransport::new(RtcConfiguration {
           ice_servers: ice_servers
                .into_iter()
                .map(|s| rustrtc::IceServer {
                    urls: vec![s.url],
                    username: s.username,
                    credential: s.credential,
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        });
        if controlling {
            ice.set_role(IceRole::Controlling);
        } else {
            ice.set_role(IceRole::Controlled);
        }
        tokio::spawn(future);
        let local_parameters = ice.local_parameters();
        Ok((Connection {
            id,
            peer_id,
            ice,
            candidate_filter_prefix,
            key: watch::channel(key).0,
            sent: Arc::new(AtomicU64::new(0)),
            received: Arc::new(AtomicU64::new(0)),
        }, local_parameters))
    }

    pub async fn start(&mut self, remote_parameters: IceParameters) -> Result<()> {
        self.ice.start(remote_parameters)
    }

    pub async fn gather(&self) -> Result<Vec<IceCandidate>> {
        let mut gather = self.ice.subscribe_gathering_state();
        loop {
            if *gather.borrow_and_update() == IceGathererState::Complete {
                break;
            }
            gather.changed().await?;
        }
        let mut candidates = self.ice.local_candidates();
        if let Some(prefix) = &self.candidate_filter_prefix {
            candidates.retain(|candidate| !prefix.contains(&candidate.address.ip()));
        }
        Ok(candidates)
    }

    pub async fn add_candidates(&self, candidates: Vec<IceCandidate>) {
        for candidate in candidates {
            if let Some(prefix) = &self.candidate_filter_prefix {
                if prefix.contains(&candidate.address.ip()) {
                    continue;
                }
            }
            self.ice.add_remote_candidate(candidate);
        }
    }

    pub fn sender(&self) -> ConnectionSender {
        ConnectionSender {
            id: self.id,
            peer_id: self.peer_id,
            socket: self.ice.subscribe_selected_socket(),
            pair: self.ice.subscribe_selected_pair(),
            key: self.key.subscribe(),
            aes: Aes128Gcm::new(&self.key.borrow().key),
            counter: 0,
            sent: self.sent.clone(),
        }
    }
    
    pub async fn receiver(&self) -> ConnectionReceiver {
        // TODO: check this
        let (tx, rx) = mpsc::channel(1024);
        self.ice.set_data_receiver(Arc::new(MpscSender(tx))).await;
        ConnectionReceiver {
            rx,
            key: self.key.subscribe(),
            aes: [Aes128Gcm::new(&self.key.borrow().key), Aes128Gcm::new(&self.key.borrow().key)],
            received: self.received.clone(),
        }
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        self.ice.stop();
    }
}

pub struct ConnectionSender {
    id: NodeId,
    peer_id: NodeId,
    socket: watch::Receiver<Option<IceSocketWrapper>>,
    pair: watch::Receiver<Option<IceCandidatePair>>,
    key: watch::Receiver<FullMeshKey>,
    aes: Aes128Gcm,
    counter: u32,

    sent: Arc<AtomicU64>,
}

impl ConnectionSender {
    pub async fn send(&mut self, data: Bytes) -> Result<usize> {
        if let Some(socket) = &*self.socket.borrow() && let Some(pair) = &*self.pair.borrow() {
            let mut key = self.key.borrow();
            if self.key.has_changed()? {
                drop(key);
                key = self.key.borrow_and_update();
                self.aes = Aes128Gcm::new(&key.key);
                self.counter = 0;
            }

            // TODO: dont encrypt local packets
            if self.counter >= 0b00011111_11111111_11111111_11111111 {
                anyhow::bail!("Counter overflow");
            }
            let mut nonce_header: u32 = 0b10000000_00000000_00000000_00000000 | self.counter;
            if self.id > self.peer_id {
                nonce_header |= 0b01000000_00000000_00000000_00000000;
            }
            if key.index {
                nonce_header |= 0b00100000_00000000_00000000_00000000;
            }
            let nonce_header = nonce_header.to_be_bytes();
            let mut nonce = [0u8; 12];
            nonce[8..].copy_from_slice(&nonce_header);
            let nonce = Nonce::from_slice(&nonce);
            let data = self.aes.encrypt(nonce, data.as_ref())?;
            self.counter += 1;

            let mut packet = BytesMut::with_capacity(nonce_header.len() + data.len());
            packet.extend_from_slice(&nonce_header);
            packet.extend_from_slice(&data);
            let packet = packet.freeze();

            self.sent.fetch_add(data.len() as u64, Ordering::Relaxed);
            return socket.send_to(&packet, pair.remote.address).await;
        }
        anyhow::bail!("Connection is not yet established")
    }
}

pub struct MpscSender(mpsc::Sender<(Bytes, SocketAddr)>);

pub struct ConnectionReceiver {
    rx: mpsc::Receiver<(Bytes, SocketAddr)>,
    key: watch::Receiver<FullMeshKey>,
    aes: [Aes128Gcm; 2],

    received: Arc<AtomicU64>,
}

#[async_trait]
impl PacketReceiver for MpscSender {
    async fn receive(&self, packet: Bytes, addr: SocketAddr) {
        if let Err(err) = self.0.send((packet, addr)).await {
            debug!("Failed to send received packet to channel: {}", err);
        }
    }
}

impl ConnectionReceiver{
    pub async fn recv(&mut self) -> Result<(Bytes, SocketAddr)> {
        if self.key.has_changed()? {
            let key = self.key.borrow_and_update();
            if key.index {
                self.aes[1] = Aes128Gcm::new(&key.key);
            } else {
                self.aes[0] = Aes128Gcm::new(&key.key);
            }
        }
        let (data, addr) = self.rx.recv().await.ok_or_else(|| anyhow::anyhow!("Channel closed"))?;
        self.received.fetch_add(data.len() as u64, Ordering::Relaxed);
        let index = (data[0] >> 5) & 0b1 == 1;
        let key = if index { &self.aes[1] } else { &self.aes[0] };
        let mut nonce = [0u8; 12];
        nonce[8..].copy_from_slice(&data[0..4]);
        let nonce = Nonce::from_slice(&nonce);
        let decrypted = key.decrypt(nonce, &data[4..])?;
        Ok((Bytes::from(decrypted), addr))
    }
}

#[cfg(test)]
mod tests {
    use aes_gcm::aead::OsRng;
    use rustrtc::IceTransportState;

    use super::*;

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn test_connection() -> Result<()> {
        let key = FullMeshKey {
            index: false,
            key: Aes128Gcm::generate_key(OsRng),
        };
        let (mut conn1, local_parameters) = Connection::new(1, 2, vec![], None, true, key.clone()).await?;
        let (mut conn2, remote_parameters) = Connection::new(2, 1, vec![], None, false, key.clone()).await?;
        conn1.start(remote_parameters).await?;
        conn2.start(local_parameters).await?;

        let candidates1 = conn1.gather().await?;
        let candidates2 = conn2.gather().await?;
        conn2.add_candidates(candidates1).await;
        conn1.add_candidates(candidates2).await;

        let mut state = conn1.ice.subscribe_state();
        loop {
            if *state.borrow() == IceTransportState::Connected {
                break;
            }
            state.changed().await?;
        }

        let mut sender1 = conn1.sender();
        let mut receiver2 = conn2.receiver().await;

        let data = Bytes::from("Hello, world!");
        sender1.send(data.clone()).await?;
        let (received, _) = receiver2.recv().await?;
        assert_eq!(data, received);

        let new_key = FullMeshKey {
            index: true,
            key: Aes128Gcm::generate_key(OsRng),
        };

        conn2.key.send(new_key.clone())?;
        sender1.send(data.clone()).await?;
        let (received, _) = receiver2.recv().await?;
        assert_eq!(data, received);

        conn1.key.send(new_key.clone())?;
        sender1.send(data.clone()).await?;
        let (received, _) = receiver2.recv().await?;
        assert_eq!(data, received);
        Ok(())
    }
}
