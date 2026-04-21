use std::{
    any::Any,
    collections::HashMap,
    net::IpAddr,
    sync::Arc,
    time::{Duration, Instant},
};

use aes_gcm::{Aes128Gcm, Key};
use anyhow::{Error, Result};
use async_trait::async_trait;
use cidr::AnyIpCidr;
use cryonet_uapi::{Conn, ConnState};
use p256::{PublicKey, ecdh::EphemeralSecret, elliptic_curve::Generate};
use rustrtc::{IceTransportState, transports::ice::IceParameters};
use sactor::sactor;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use tokio::{
    sync::{Mutex, mpsc},
    time::{Interval, interval},
};
use tracing::{debug, error, info, warn};

use crate::{
    fullmesh::{
        conn::{Connection, ConnectionReceiver, ConnectionSender},
        registry::RegistryHandle,
    },
    mesh::{
        MeshHandle,
        packet::{NodeId, Packet, Payload},
    },
};

#[cfg_attr(not(target_arch = "wasm32"), path = "conn_ice.rs")]
#[cfg_attr(target_arch = "wasm32", path = "conn_wasm.rs")]
pub mod conn;
pub mod registry;
#[cfg(not(target_arch = "wasm32"))]
pub mod tun;

pub mod tap;

#[async_trait]
pub trait DeviceManager: Send + Sync {
    async fn connected(&mut self, node_id: NodeId, sender: ConnectionSender, receiver: ConnectionReceiver) -> Result<()>;
    async fn disconnected(&mut self, node_id: NodeId) -> Result<()>;
    async fn ips(&self) -> Result<Vec<IpAddr>>;
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IceServer {
    pub url: String,
    pub username: Option<String>,
    pub credential: Option<String>,
}

struct FullMeshConnection {
    last_rekey: Instant,
    last_received: (u64, Instant),
    once_connected: bool,
    connection: Connection,
    ecdh_key: EphemeralSecret,
}

#[derive(Debug, Clone, Copy)]
pub struct FullMeshKey {
    index: bool,
    key: Key<Aes128Gcm>,
}

pub struct FullMesh {
    handle: FullMeshHandle,

    id: NodeId,
    mesh: MeshHandle,
    registry: RegistryHandle,
    dm: Arc<Mutex<Box<dyn DeviceManager>>>,
    ice_servers: Vec<IceServer>,
    candidate_filter_prefix: Option<AnyIpCidr>,
    encrypt_local_packets: bool,
    connection_timeout: Duration,
    rekey_timeout: Duration,

    packet_rx: mpsc::Receiver<Packet>,
    connect_ticker: Interval,
    connections: HashMap<NodeId, FullMeshConnection>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
enum FullMeshPayload {
    Shake { ufrag: String, pwd: String, candidates: Vec<String>, public_key: PublicKey },
    Rekey { index: bool, public_key: PublicKey },
    RekeyConfirm { index: bool, public_key: PublicKey },
}

#[typetag::serde]
impl Payload for FullMeshPayload {}

#[sactor(pub)]
impl FullMesh {
    pub async fn new(id: NodeId, mesh: MeshHandle, registry: RegistryHandle, dm: Arc<Mutex<Box<dyn DeviceManager>>>, ice_servers: Vec<IceServer>, candidate_filter_prefix: Option<AnyIpCidr>, encrypt_local_packets: bool) -> Result<FullMeshHandle> {
        Self::new_with_parameters(id, mesh, registry, dm, ice_servers, Duration::from_secs(10), Duration::from_secs(30), Duration::from_secs(120), candidate_filter_prefix, encrypt_local_packets).await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn new_with_parameters(
        id: NodeId,
        mesh: MeshHandle,
        registry: RegistryHandle,
        dm: Arc<Mutex<Box<dyn DeviceManager>>>,
        ice_servers: Vec<IceServer>,
        connect_interval: Duration,
        connection_timeout: Duration,
        rekey_timeout: Duration,
        candidate_filter_prefix: Option<AnyIpCidr>,
        encrypt_local_packets: bool,
    ) -> Result<FullMeshHandle> {
        let packet_rx = mesh.add_dispatchee(Box::new(|packet| (packet.payload.as_ref() as &dyn Any).is::<FullMeshPayload>())).await?;
        let (future, fm) = FullMesh::run(move |handle| FullMesh {
            handle,
            id,
            mesh,
            registry,
            dm,
            ice_servers,
            connection_timeout,
            rekey_timeout,
            candidate_filter_prefix,
            encrypt_local_packets,
            packet_rx,
            connect_ticker: interval(connect_interval),
            connections: HashMap::new(),
        });
        tokio::task::spawn_local(future);
        Ok(fm)
    }

    #[select]
    fn select(&mut self) -> Vec<Selection<'_>> {
        vec![selection!(self.packet_rx.recv().await, handle_packet, it => it), selection!(self.connect_ticker.tick().await, connect_tick)]
    }

    #[no_reply]
    async fn handle_packet(&mut self, packet: Option<Packet>) -> Result<()> {
        let Some(packet) = packet else {
            self.handle.stop();
            return Ok(());
        };
        let src = packet.src;
        let payload = (packet.payload.as_ref() as &dyn Any).downcast_ref::<FullMeshPayload>().unwrap();
        if src == self.id {
            error!("Received packet from self (node {:X}), ignoring", self.id);
            return Ok(());
        }
        match payload {
            FullMeshPayload::Shake { ufrag, pwd, candidates, public_key } => {
                if self.id > src {
                    let (mut connection, parameters, local_candidates) = Connection::new(self.id, src, self.handle.clone(), self.ice_servers.clone(), self.candidate_filter_prefix, self.encrypt_local_packets, false).await?;
                    let ecdh_key = EphemeralSecret::generate();
                    let local_public_key = ecdh_key.public_key();
                    connection.start(IceParameters::new(ufrag, pwd)).await?;
                    connection.add_candidates(candidates);
                    let mut key = [0; 16];
                    ecdh_key.diffie_hellman(public_key).extract::<Sha256>(None).expand(b"", &mut key)?;
                    let key = FullMeshKey { index: false, key: key.into() };
                    connection.set_recv_key(key);
                    connection.set_send_key(key);
                    self.connections.insert(
                        src,
                        FullMeshConnection {
                            last_rekey: Instant::now(),
                            last_received: (0, Instant::now()),
                            connection,
                            ecdh_key,
                            once_connected: false,
                        },
                    );
                    self.mesh
                        .send_packet(
                            src,
                            Box::new(FullMeshPayload::Shake {
                                ufrag: parameters.username_fragment,
                                pwd: parameters.password,
                                candidates: local_candidates,
                                public_key: local_public_key,
                            }),
                        )
                        .await?;
                } else {
                    let conn = match self.connections.get_mut(&src) {
                        Some(conn) => conn,
                        None => {
                            warn!("Received shake from peer {:X} but no connection exists, ignoring", src);
                            return Ok(());
                        }
                    };
                    conn.last_rekey = Instant::now();
                    conn.connection.start(IceParameters::new(ufrag, pwd)).await?;
                    conn.connection.add_candidates(candidates);
                    let mut key = [0; 16];
                    conn.ecdh_key.diffie_hellman(public_key).extract::<Sha256>(None).expand(b"", &mut key)?;
                    let key = FullMeshKey { index: false, key: key.into() };
                    conn.connection.set_recv_key(key);
                    conn.connection.set_send_key(key);
                }
            }
            FullMeshPayload::Rekey { index, public_key } => {
                let conn = match self.connections.get_mut(&src) {
                    Some(conn) => conn,
                    None => {
                        warn!("Received rekey from peer {:X} but no connection exists, ignoring", src);
                        return Ok(());
                    }
                };
                if self.id > src {
                    // we received the rekey request
                    let new_ecdh_key = EphemeralSecret::generate();
                    let new_public_key = new_ecdh_key.public_key();
                    let mut key = [0; 16];
                    new_ecdh_key.diffie_hellman(public_key).extract::<Sha256>(None).expand(b"", &mut key)?;
                    conn.connection.set_recv_key(FullMeshKey { index: *index, key: key.into() });
                    conn.ecdh_key = new_ecdh_key;
                    conn.last_rekey = Instant::now();
                    self.mesh.send_packet(src, Box::new(FullMeshPayload::Rekey { index: *index, public_key: new_public_key })).await?;
                } else {
                    // we received the rekey response
                    let mut key = [0; 16];
                    conn.ecdh_key.diffie_hellman(public_key).extract::<Sha256>(None).expand(b"", &mut key)?;
                    let new_key = FullMeshKey { index: *index, key: key.into() };
                    conn.connection.set_recv_key(new_key);
                    conn.connection.set_send_key(new_key);
                    conn.last_rekey = Instant::now();
                    self.mesh
                        .send_packet(
                            src,
                            Box::new(FullMeshPayload::RekeyConfirm {
                                index: *index,
                                public_key: conn.ecdh_key.public_key(),
                            }),
                        )
                        .await?;
                }
            }
            FullMeshPayload::RekeyConfirm { index, public_key } => {
                let conn = match self.connections.get_mut(&src) {
                    Some(conn) => conn,
                    None => {
                        warn!("Received rekey ack from peer {:X} but no connection exists, ignoring", src);
                        return Ok(());
                    }
                };
                // we confirmed the rekey
                let mut key = [0; 16];
                conn.ecdh_key.diffie_hellman(public_key).extract::<Sha256>(None).expand(b"", &mut key)?;
                conn.connection.set_send_key(FullMeshKey { index: *index, key: key.into() });
            }
        }
        Ok(())
    }

    #[no_reply]
    async fn connect_tick(&mut self) -> Result<()> {
        let time = Instant::now();
        // gc
        let mut disconnected = Vec::new();
        self.connections.retain(|node_id, conn| {
            use IceTransportState::*;
            let keep = match conn.connection.status() {
                Failed | Closed => false,
                _ => {
                    let received = conn.connection.received();
                    if received != conn.last_received.0 {
                        conn.last_received = (received, time);
                    }
                    time.duration_since(conn.last_received.1) < self.connection_timeout
                }
            };
            if !keep && conn.once_connected {
                disconnected.push(*node_id);
            }
            keep
        });
        {
            let mut dm = self.dm.lock().await;
            for node_id in disconnected {
                debug!("Connection to peer {:X} timed out, disconnecting", node_id);
                dm.disconnected(node_id).await?;
            }
            // mark connected
            for (node_id, conn) in &mut self.connections {
                if !conn.once_connected && conn.connection.status() == IceTransportState::Connected {
                    conn.once_connected = true;
                    dm.connected(*node_id, conn.connection.sender(), conn.connection.receiver().await).await?;
                }
            }
        }
        // connect
        for (peer_id, _) in self.registry.get_nodes().await? {
            if self.id > peer_id {
                continue;
            }
            if let Some(conn) = self.connections.get_mut(&peer_id) {
                // rekey
                // TODO: rekey by sent/received bytes
                if conn.connection.status() != IceTransportState::Connected || time.duration_since(conn.last_rekey) < self.rekey_timeout {
                    continue;
                }
                let Some(key) = conn.connection.key() else {
                    continue;
                };
                info!("Rekeying connection to peer {:X}", peer_id);
                let new_ecdh_key = EphemeralSecret::generate();
                let new_public_key = new_ecdh_key.public_key();
                conn.ecdh_key = new_ecdh_key;
                self.mesh.send_packet(peer_id, Box::new(FullMeshPayload::Rekey { index: !key.index, public_key: new_public_key })).await?;
            } else {
                // new connection
                let result: Result<()> = try {
                    let (connection, parameters, candidates) = Connection::new(self.id, peer_id, self.handle.clone(), self.ice_servers.clone(), self.candidate_filter_prefix, self.encrypt_local_packets, true).await?;
                    let ecdh_key = EphemeralSecret::generate();
                    let public_key = ecdh_key.public_key();
                    self.connections.insert(
                        peer_id,
                        FullMeshConnection {
                            last_rekey: Instant::now(),
                            last_received: (0, Instant::now()),
                            connection,
                            ecdh_key,
                            once_connected: false,
                        },
                    );
                    self.mesh
                        .send_packet(
                            peer_id,
                            Box::new(FullMeshPayload::Shake {
                                ufrag: parameters.username_fragment,
                                pwd: parameters.password,
                                candidates,
                                public_key,
                            }),
                        )
                        .await?;
                };
                if let Err(err) = result {
                    error!("Failed to connect to peer {:X}: {:?}", peer_id, err);
                }
            }
        }
        Ok(())
    }

    pub async fn get_peers(&self) -> HashMap<NodeId, Conn> {
        let mut result = HashMap::new();
        for (node_id, conn) in &self.connections {
            let state = match conn.connection.status() {
                IceTransportState::New => ConnState::New,
                IceTransportState::Checking => ConnState::Connecting,
                IceTransportState::Connected | IceTransportState::Completed => ConnState::Connected,
                IceTransportState::Disconnected => ConnState::Disconnected,
                IceTransportState::Failed => ConnState::Failed,
                IceTransportState::Closed => ConnState::Closed,
            };
            let selected_candidate = conn.connection.selected_candidate().await;
            let sent = conn.connection.sent();
            let received = conn.connection.received();
            result.insert(*node_id, Conn { state, selected_candidate, sent, received });
        }
        result
    }

    #[handle_error]
    fn handle_error(&mut self, err: &Error) {
        error!("Error: {:?}", err);
    }
}
