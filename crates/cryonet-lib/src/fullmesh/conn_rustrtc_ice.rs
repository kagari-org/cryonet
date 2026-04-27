use std::{
    any::Any,
    net::{IpAddr, SocketAddr},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use aes_gcm::{Aes128Gcm, Key, KeyInit, Nonce, aead::Aead};
use anyhow::Result;
use async_trait::async_trait;
use bytes::{Bytes, BytesMut};
use cidr::AnyIpCidr;
use memoize::memoize;
use rustrtc::{
    IceCandidate, IceCandidatePair, IceGathererState, IceRole, IceTransport, IceTransportState,
    RtcConfiguration,
    transports::{
        PacketReceiver,
        ice::{IceParameters, IceSocketWrapper},
    },
};
use tokio::sync::{mpsc, watch};
use tracing::debug;

use crate::{
    errors::CryonetError,
    fullmesh::{
        Connection, ConnectionReceiver, ConnectionSender, ConnectionState, IceServer,
        fm_rustrtc_ice::FullMeshIceHandle,
    },
    mesh::packet::NodeId,
};

#[derive(Debug, Clone, Copy)]
pub struct ConnectionRustrtcIceKey {
    pub index: bool,
    pub key: Key<Aes128Gcm>,
}

pub struct ConnectionRustrtcIce {
    id: NodeId,
    peer_id: NodeId,
    ice: IceTransport,
    candidate_filter_prefix: Option<AnyIpCidr>,
    encrypt_local_packets: bool,
    send_key: watch::Sender<Option<ConnectionRustrtcIceKey>>,
    recv_key: watch::Sender<Option<ConnectionRustrtcIceKey>>,

    sent: Arc<AtomicU64>,
    received: Arc<AtomicU64>,
}

impl ConnectionRustrtcIce {
    #[allow(clippy::too_many_arguments)]
    pub async fn new(
        id: NodeId,
        peer_id: NodeId,
        fm: FullMeshIceHandle,
        ice_servers: Vec<IceServer>,
        candidate_filter_prefix: Option<AnyIpCidr>,
        encrypt_local_packets: bool,
        controlling: bool,
    ) -> Result<(Self, IceParameters, Vec<String>)> {
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
        let mut state = ice.subscribe_state();

        // notify when state changes
        tokio::task::spawn_local(async move {
            loop {
                use IceTransportState::*;
                match *state.borrow_and_update() {
                    Failed | Closed => {
                        let _ = fm.connect_tick().await;
                        break;
                    }
                    _ => {}
                }
                if let Err(err) = state.changed().await {
                    debug!("ICE state change error: {err}");
                    break;
                }
            }
        });

        let local_parameters = ice.local_parameters();

        ice.start_gathering()?;
        let mut gather = ice.subscribe_gathering_state();
        loop {
            if *gather.borrow_and_update() == IceGathererState::Complete {
                break;
            }
            gather.changed().await?;
        }
        let mut candidates = ice.local_candidates();
        if let Some(prefix) = &candidate_filter_prefix {
            candidates.retain(|candidate| !prefix.contains(&candidate.address.ip()));
        }
        let candidates = candidates.into_iter().map(|c| c.to_sdp()).collect();

        Ok((
            ConnectionRustrtcIce {
                id,
                peer_id,
                ice,
                candidate_filter_prefix,
                encrypt_local_packets,
                send_key: watch::channel(None).0,
                recv_key: watch::channel(None).0,
                sent: Arc::new(AtomicU64::new(0)),
                received: Arc::new(AtomicU64::new(0)),
            },
            local_parameters,
            candidates,
        ))
    }

    pub async fn start(&mut self, remote_parameters: IceParameters) -> Result<()> {
        self.ice.start(remote_parameters)
    }

    pub fn add_candidates(&self, candidates: &Vec<String>) {
        for candidate in candidates {
            if let Ok(candidate) = IceCandidate::from_sdp(candidate) {
                if let Some(prefix) = &self.candidate_filter_prefix
                    && prefix.contains(&candidate.address.ip())
                {
                    continue;
                }
                self.ice.add_remote_candidate(candidate);
            }
        }
    }

    pub fn set_recv_key(&self, key: ConnectionRustrtcIceKey) {
        self.recv_key.send_replace(Some(key));
    }

    pub fn set_send_key(&self, key: ConnectionRustrtcIceKey) {
        self.send_key.send_replace(Some(key));
    }

    pub fn key(&self) -> Option<ConnectionRustrtcIceKey> {
        *self.send_key.borrow()
    }
}

impl Drop for ConnectionRustrtcIce {
    fn drop(&mut self) {
        self.ice.stop();
    }
}

#[async_trait]
impl Connection for ConnectionRustrtcIce {
    async fn sender(&self) -> Result<Box<dyn ConnectionSender>> {
        Ok(Box::new(ConnectionRustrtcIceSender {
            id: self.id,
            peer_id: self.peer_id,
            encrypt_local_packets: self.encrypt_local_packets,
            socket: self.ice.subscribe_selected_socket(),
            pair: self.ice.subscribe_selected_pair(),
            key: self.send_key.subscribe(),
            aes: Aes128Gcm::new(&[0u8; 16].into()),
            // We use counter=0 to indicate unencrypted packets.
            counter: 1,
            sent: self.sent.clone(),
        }))
    }

    async fn receiver(&self) -> Result<Box<dyn ConnectionReceiver>> {
        let (tx, rx) = mpsc::channel(16384);
        self.ice.set_data_receiver(Arc::new(MpscSender(tx))).await;
        Ok(Box::new(ConnectionRustrtcIceReceiver {
            encrypt_local_packets: self.encrypt_local_packets,
            rx,
            key: self.recv_key.subscribe(),
            aes: [
                Aes128Gcm::new(&[0u8; 16].into()),
                Aes128Gcm::new(&[0u8; 16].into()),
            ],
            received: self.received.clone(),
        }))
    }

    fn sent(&self) -> u64 {
        self.sent.load(Ordering::Relaxed)
    }

    fn received(&self) -> u64 {
        self.received.load(Ordering::Relaxed)
    }

    fn status(&self) -> ConnectionState {
        use IceTransportState::*;
        match self.ice.state() {
            New | Checking => ConnectionState::Connecting,
            Connected | Completed | Disconnected => ConnectionState::Connected,
            Failed | Closed => ConnectionState::Closed,
        }
    }

    async fn selected_candidate(&self) -> Option<String> {
        self.ice
            .get_selected_pair()
            .await
            .map(|pair| pair.remote.to_sdp())
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

pub struct ConnectionRustrtcIceSender {
    id: NodeId,
    peer_id: NodeId,
    encrypt_local_packets: bool,
    socket: watch::Receiver<Option<IceSocketWrapper>>,
    pair: watch::Receiver<Option<IceCandidatePair>>,
    key: watch::Receiver<Option<ConnectionRustrtcIceKey>>,
    aes: Aes128Gcm,
    counter: u32,

    sent: Arc<AtomicU64>,
}

#[async_trait]
impl ConnectionSender for ConnectionRustrtcIceSender {
    async fn send(&mut self, data: Bytes) -> Result<usize> {
        let socket = self.socket.borrow().clone();
        let pair = self.pair.borrow().clone();
        if let Some(socket) = socket
            && let Some(pair) = pair
        {
            let Some(mut key) = *self.key.borrow() else {
                anyhow::bail!("Encryption key is not set yet");
            };
            if self.key.has_changed()? {
                key = self.key.borrow_and_update().unwrap();
                self.aes = Aes128Gcm::new(&key.key);
                self.counter = 0;
            }

            if self.encrypt_local_packets || !is_private(pair.remote.address) {
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
            } else {
                let nonce_header = 0b10000000_00000000_00000000_00000000u32.to_be_bytes();
                let mut packet = BytesMut::with_capacity(nonce_header.len() + data.len());
                packet.extend_from_slice(&nonce_header);
                packet.extend_from_slice(&data);
                let packet = packet.freeze();
                self.sent.fetch_add(data.len() as u64, Ordering::Relaxed);
                return socket.send_to(&packet, pair.remote.address).await;
            }
        }
        anyhow::bail!("Connection is not yet established")
    }
}

pub struct MpscSender(mpsc::Sender<(Bytes, SocketAddr)>);

pub struct ConnectionRustrtcIceReceiver {
    encrypt_local_packets: bool,

    rx: mpsc::Receiver<(Bytes, SocketAddr)>,
    key: watch::Receiver<Option<ConnectionRustrtcIceKey>>,
    aes: [Aes128Gcm; 2],

    received: Arc<AtomicU64>,
}

#[async_trait]
impl PacketReceiver for MpscSender {
    async fn receive(&self, packet: Bytes, addr: SocketAddr) {
        if let Err(err) = self.0.send((packet, addr)).await {
            debug!("Failed to send received packet to channel: {err}");
        }
    }
}

#[async_trait]
impl ConnectionReceiver for ConnectionRustrtcIceReceiver {
    async fn recv(&mut self) -> Result<(Bytes, SocketAddr)> {
        if self.key.has_changed()? {
            let key = self.key.borrow_and_update().unwrap();
            if key.index {
                self.aes[1] = Aes128Gcm::new(&key.key);
            } else {
                self.aes[0] = Aes128Gcm::new(&key.key);
            }
        }
        let (data, addr) = self.rx.recv().await.ok_or(CryonetError::ChannelClosed)?;
        if data.len() < 4 {
            anyhow::bail!("Received packet is too short");
        }
        self.received
            .fetch_add(data.len() as u64, Ordering::Relaxed);
        let counter = u32::from_be_bytes([data[0], data[1], data[2], data[3]])
            & 0b00011111_11111111_11111111_11111111;
        if counter == 0 {
            // unencrypted packet
            if self.encrypt_local_packets || !is_private(addr) {
                anyhow::bail!("Received unexpected unencrypted packet from {addr}");
            }
            return Ok((data.slice(4..), addr));
        }
        let index = (data[0] >> 5) & 0b1 == 1;
        let key = if index { &self.aes[1] } else { &self.aes[0] };
        let mut nonce = [0u8; 12];
        nonce[8..].copy_from_slice(&data[0..4]);
        let nonce = Nonce::from_slice(&nonce);
        let decrypted = key.decrypt(nonce, &data[4..])?;
        Ok((Bytes::from(decrypted), addr))
    }
}

#[memoize]
fn is_private(addr: SocketAddr) -> bool {
    match addr.ip() {
        IpAddr::V4(ip) => ip.is_private() || ip.is_loopback() || ip.is_link_local(),
        IpAddr::V6(ip) => ip.is_unique_local() || ip.is_loopback() || ip.is_unicast_link_local(),
    }
}
