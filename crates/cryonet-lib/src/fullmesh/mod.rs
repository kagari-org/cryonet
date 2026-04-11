use std::{any::Any, collections::HashMap, time::{Duration, Instant}};

use aes_gcm::{Aes128Gcm, Key};
use anyhow::{Error, Result};
use cidr::AnyIpCidr;
use cryonet_uapi::{Conn, ConnState};
use p256::{PublicKey, ecdh::EphemeralSecret, elliptic_curve::Generate};
use rustrtc::{IceTransportState, transports::ice::IceParameters};
use sactor::sactor;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use tokio::{sync::mpsc, time::{Interval, interval}};
use tracing::{error, warn};

use crate::{fullmesh::conn::Connection, mesh::{
    MeshHandle,
    packet::{NodeId, Packet, Payload},
}};

#[cfg_attr(not(target_arch = "wasm32"), path = "conn_rustrtc.rs")]
#[cfg_attr(target_arch = "wasm32", path = "conn_wasm.rs")]
pub mod conn;
#[cfg(not(target_arch = "wasm32"))]
pub mod tun;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IceServer {
    pub url: String,
    pub username: Option<String>,
    pub credential: Option<String>,
}

struct FullMeshConnection {
    last_seen: Instant,
    last_rekey: Instant,
    once_connected: bool,
    connection: Connection,
    ecdh_key: EphemeralSecret,
}

pub enum FullMeshEvent {
    Connected {
        node_id: NodeId,
        sender: conn::ConnectionSender,
        receiver: conn::ConnectionReceiver,
    },
    Disconnected {
        node_id: NodeId,
    },
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
    ice_servers: Vec<IceServer>,
    candidate_filter_prefix: Option<AnyIpCidr>,
    timeout: Duration,
    rekey_timeout: Duration,

    packet_rx: mpsc::Receiver<Packet>,
    ticker: Interval,
    connections: HashMap<NodeId, FullMeshConnection>,

    event_tx: mpsc::Sender<FullMeshEvent>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
enum FullMeshPayload {
    Shake {
        ufrag: String,
        pwd: String,
        candidates: Vec<String>,
        public_key: PublicKey,
    },
    Rekey {
        index: bool,
        public_key: PublicKey,
    },
}

#[typetag::serde]
impl Payload for FullMeshPayload {}

#[sactor(pub)]
impl FullMesh {
    pub async fn new(id: NodeId, mesh: MeshHandle, ice_servers: Vec<IceServer>, candidate_filter_prefix: Option<AnyIpCidr>, event_tx: mpsc::Sender<FullMeshEvent>) -> Result<FullMeshHandle> {
        Self::new_with_parameters(id, mesh, ice_servers, Duration::from_secs(30), Duration::from_secs(120), candidate_filter_prefix, event_tx).await
    }

    pub async fn new_with_parameters(id: NodeId, mesh: MeshHandle, ice_servers: Vec<IceServer>, timeout: Duration, rekey_timeout: Duration, candidate_filter_prefix: Option<AnyIpCidr>, event_tx: mpsc::Sender<FullMeshEvent>) -> Result<FullMeshHandle> {
        let packet_rx = mesh.add_dispatchee(Box::new(|packet| (packet.payload.as_ref() as &dyn Any).is::<FullMeshPayload>())).await?;
        let (future, fm) = FullMesh::run(move |handle| FullMesh {
            handle,
            id,
            mesh,
            ice_servers,
            timeout,
            rekey_timeout,
            candidate_filter_prefix,
            packet_rx,
            ticker: interval(Duration::from_secs(10)),
            connections: HashMap::new(),
            event_tx,
        });
        tokio::task::spawn_local(future);
        Ok(fm)
    }

    #[select]
    fn select(&mut self) -> Vec<Selection<'_>> {
        vec![selection!(self.packet_rx.recv().await, handle_packet, it => it), selection!(self.ticker.tick().await, tick)]
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
                if src < self.id {
                    let (mut connection, parameters, local_candidates) = Connection::new(self.id, src, self.handle.clone(), self.ice_servers.clone(), self.candidate_filter_prefix, false).await?;
                    let ecdh_key = EphemeralSecret::generate();
                    let local_public_key = ecdh_key.public_key();
                    connection.start(IceParameters::new(ufrag, pwd)).await?;
                    connection.add_candidates(candidates);
                    let mut key = [0; 16];
                    ecdh_key.diffie_hellman(public_key).extract::<Sha256>(None).expand(b"", &mut key)?;
                    connection.set_key(FullMeshKey {
                        index: false,
                        key: key.into(),
                    });
                    self.connections.insert(src, FullMeshConnection {
                        last_seen: Instant::now(),
                        last_rekey: Instant::now(),
                        connection,
                        ecdh_key,
                        once_connected: false,
                    });
                    self.mesh.send_packet(src, Box::new(FullMeshPayload::Shake {
                        ufrag: parameters.username_fragment,
                        pwd: parameters.password,
                        candidates: local_candidates,
                        public_key: local_public_key,
                    })).await?;
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
                    conn.connection.set_key(FullMeshKey {
                        index: false,
                        key: key.into(),
                    });
                }
            },
            FullMeshPayload::Rekey { index, public_key } => {
                let conn = match self.connections.get_mut(&src) {
                    Some(conn) => conn,
                    None => {
                        warn!("Received rekey from peer {:X} but no connection exists, ignoring", src);
                        return Ok(());
                    }
                };
                if src < self.id {
                    let new_ecdh_key = EphemeralSecret::generate();
                    let new_public_key = new_ecdh_key.public_key();
                    let mut key = [0; 16];
                    new_ecdh_key.diffie_hellman(public_key).extract::<Sha256>(None).expand(b"", &mut key)?;
                    // TODO: ack?
                    conn.connection.set_key(FullMeshKey {
                        index: *index,
                        key: key.into(),
                    });
                    conn.ecdh_key = new_ecdh_key;
                    conn.last_rekey = Instant::now();
                    self.mesh.send_packet(src, Box::new(FullMeshPayload::Rekey {
                        index: *index,
                        public_key: new_public_key,
                    })).await?;
                } else {
                    let mut key = [0; 16];
                    conn.ecdh_key.diffie_hellman(public_key).extract::<Sha256>(None).expand(b"", &mut key)?;
                    conn.connection.set_key(FullMeshKey {
                        index: *index,
                        key: key.into(),
                    });
                    conn.last_rekey = Instant::now();
                }
            },
        }
        Ok(())
    }

    #[no_reply]
    async fn tick(&mut self) -> Result<()> {
        let time = Instant::now();
        // gc
        let mut disconnected = Vec::new();
        self.connections.retain(|node_id, conn| {
            use IceTransportState::*;
            let keep = match conn.connection.status() {
                Connected | Completed => {
                    conn.last_seen = time;
                    true
                }
                Failed | Closed => false,
                New | Checking | Disconnected => {
                    time.duration_since(conn.last_seen) < self.timeout
                }
            };
            if !keep && conn.once_connected {
                disconnected.push(*node_id);
            }
            keep
        });
        for node_id in disconnected {
            let _ = self.event_tx.send(FullMeshEvent::Disconnected { node_id });
        }
        // mark connected
        for (node_id, conn) in &mut self.connections {
            if !conn.once_connected && conn.connection.status() == IceTransportState::Connected {
                conn.once_connected = true;
                let _ = self.event_tx.send(FullMeshEvent::Connected {
                    node_id: *node_id,
                    sender: conn.connection.sender(),
                    receiver: conn.connection.receiver().await,
                });
            }
        }
        // connect
        let peers = self.mesh.get_routes().await?.keys().cloned().collect::<Vec<_>>();
        for peer_id in peers {
            if peer_id > self.id {
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
                let new_ecdh_key = EphemeralSecret::generate();
                let new_public_key = new_ecdh_key.public_key();
                conn.ecdh_key = new_ecdh_key;
                self.mesh.send_packet(peer_id, Box::new(FullMeshPayload::Rekey {
                    index: !key.index,
                    public_key: new_public_key,
                })).await?;
            } else {
                // new connection
                let result: Result<()> = try {
                    let (connection, parameters, candidates) = Connection::new(self.id, peer_id, self.handle.clone(), self.ice_servers.clone(), self.candidate_filter_prefix, true).await?;
                    let ecdh_key = EphemeralSecret::generate();
                    let public_key = ecdh_key.public_key();
                    self.connections.insert(peer_id, FullMeshConnection {
                        last_seen: Instant::now(),
                        last_rekey: Instant::now(),
                        connection,
                        ecdh_key,
                        once_connected: false,
                    });
                    self.mesh.send_packet(peer_id, Box::new(FullMeshPayload::Shake {
                        ufrag: parameters.username_fragment,
                        pwd: parameters.password,
                        candidates,
                        public_key,
                    })).await?;
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
            result.insert(*node_id, Conn {
                state,
                selected_candidate,
            });
        }
        result
    }

    #[handle_error]
    fn handle_error(&mut self, err: &Error) {
        error!("Error: {:?}", err);
    }
}
