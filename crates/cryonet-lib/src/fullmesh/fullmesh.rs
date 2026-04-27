use std::{
    any::Any,
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Error, Result};
use cidr::AnyIpCidr;
use cryonet_uapi::{Conn, ConnState};
use sactor::sactor;
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, mpsc};
use tracing::{debug, error, warn};

#[cfg(not(target_arch = "wasm32"))]
use tracing::info;

#[cfg(target_arch = "wasm32")]
use crate::fullmesh::conn_wasm::ConnectionWasmDataChannel;
#[cfg(not(target_arch = "wasm32"))]
use crate::fullmesh::{
    conn_rustrtc_dc::ConnectionRustrtcDataChannel,
    conn_rustrtc_ice::{ConnectionRustrtcIce, ConnectionRustrtcIceKey},
};
#[cfg(not(target_arch = "wasm32"))]
use p256::{PublicKey, ecdh::EphemeralSecret, elliptic_curve::Generate};
#[cfg(not(target_arch = "wasm32"))]
use rustrtc::transports::ice::IceParameters;
#[cfg(not(target_arch = "wasm32"))]
use sha2::Sha256;

use crate::{
    fullmesh::{
        Connection, ConnectionState, DeviceManager, IceServer,
        registry::{ConnectionType, RegistryHandle},
    },
    mesh::{
        MeshHandle,
        packet::{NodeId, Packet, Payload},
    },
    time::{Interval, interval},
};

struct FullMeshConnection {
    connection: Box<dyn Connection>,
    last_received: (u64, Instant),
    once_connected: bool,

    // for ice connection
    #[cfg(not(target_arch = "wasm32"))]
    last_rekey: Instant,
    #[cfg(not(target_arch = "wasm32"))]
    ecdh_key: EphemeralSecret,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
enum FullMeshPayload {
    // ice
    Shake {
        ufrag: String,
        pwd: String,
        candidates: Vec<String>,
        public_key: String,
    },
    Rekey {
        index: bool,
        public_key: String,
    },
    RekeyConfirm {
        index: bool,
        public_key: String,
    },
    // data channel
    Offer {
        sdp: String,
        candidates: Vec<String>,
    },
    Answer {
        sdp: String,
        candidates: Vec<String>,
    },
}

#[typetag::serde]
impl Payload for FullMeshPayload {}

#[allow(unused)]
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

#[sactor(pub)]
impl FullMesh {
    pub async fn new(
        id: NodeId,
        mesh: MeshHandle,
        registry: RegistryHandle,
        dm: Arc<Mutex<Box<dyn DeviceManager>>>,
        ice_servers: Vec<IceServer>,
        candidate_filter_prefix: Option<AnyIpCidr>,
        encrypt_local_packets: bool,
    ) -> Result<FullMeshHandle> {
        Self::new_with_parameters(
            id,
            mesh,
            registry,
            dm,
            ice_servers,
            Duration::from_secs(10),
            Duration::from_secs(30),
            Duration::from_secs(120),
            candidate_filter_prefix,
            encrypt_local_packets,
        )
        .await
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
        let packet_rx = mesh
            .add_dispatchee(Box::new(|packet| {
                (packet.payload.as_ref() as &dyn Any).is::<FullMeshPayload>()
            }))
            .await?;
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
        vec![
            selection!(self.packet_rx.recv().await, handle_packet, it => it),
            selection!(self.connect_ticker.tick().await, connect_tick),
        ]
    }

    #[no_reply]
    async fn handle_packet(&mut self, packet: Option<Packet>) -> Result<()> {
        let Some(packet) = packet else {
            self.handle.stop();
            return Ok(());
        };
        let src = packet.src;
        let payload = (packet.payload.as_ref() as &dyn Any)
            .downcast_ref::<FullMeshPayload>()
            .unwrap();
        if src == self.id {
            error!("Received packet from self (node {:X}), ignoring", self.id);
            return Ok(());
        }
        match payload {
            #[cfg(not(target_arch = "wasm32"))]
            FullMeshPayload::Shake {
                ufrag,
                pwd,
                candidates,
                public_key,
            } => {
                let public_key = serde_json::from_str::<PublicKey>(public_key)?;
                if self.id > src {
                    let ecdh_key = EphemeralSecret::generate();
                    let local_public_key = ecdh_key.public_key();
                    let mut key = [0; 16];
                    ecdh_key
                        .diffie_hellman(&public_key)
                        .extract::<Sha256>(None)
                        .expand(b"", &mut key)?;
                    let key = ConnectionRustrtcIceKey {
                        index: false,
                        key: key.into(),
                    };

                    let (mut connection, parameters, local_candidates) = ConnectionRustrtcIce::new(
                        self.id,
                        src,
                        self.handle.clone(),
                        self.ice_servers.clone(),
                        self.candidate_filter_prefix,
                        self.encrypt_local_packets,
                        false,
                    )
                    .await?;
                    connection.start(IceParameters::new(ufrag, pwd)).await?;
                    connection.add_candidates(candidates);
                    connection.set_recv_key(key);
                    connection.set_send_key(key);
                    self.connections.insert(
                        src,
                        FullMeshConnection {
                            connection: Box::new(connection),
                            last_received: (0, Instant::now()),
                            once_connected: false,
                            last_rekey: Instant::now(),
                            ecdh_key,
                        },
                    );
                    self.mesh
                        .send_packet(
                            src,
                            Box::new(FullMeshPayload::Shake {
                                ufrag: parameters.username_fragment,
                                pwd: parameters.password,
                                candidates: local_candidates,
                                public_key: serde_json::to_string(&local_public_key)?,
                            }),
                        )
                        .await?;
                } else {
                    let Some(conn) = self.connections.get_mut(&src) else {
                        warn!(
                            "Received shake from peer {src:X} but no connection exists, ignoring"
                        );
                        return Ok(());
                    };
                    conn.last_rekey = Instant::now();
                    let mut key = [0; 16];
                    conn.ecdh_key
                        .diffie_hellman(&public_key)
                        .extract::<Sha256>(None)
                        .expand(b"", &mut key)?;
                    let key = ConnectionRustrtcIceKey {
                        index: false,
                        key: key.into(),
                    };
                    let Some(connection) = conn
                        .connection
                        .as_any_mut()
                        .downcast_mut::<ConnectionRustrtcIce>()
                    else {
                        warn!(
                            "Received shake from peer {src:X} but connection is not ConnectionRustrtcIce, ignoring",
                        );
                        return Ok(());
                    };
                    connection.start(IceParameters::new(ufrag, pwd)).await?;
                    connection.add_candidates(candidates);
                    connection.set_recv_key(key);
                    connection.set_send_key(key);
                }
            }
            #[cfg(not(target_arch = "wasm32"))]
            FullMeshPayload::Rekey { index, public_key } => {
                let public_key = serde_json::from_str::<PublicKey>(public_key)?;
                let Some(conn) = self.connections.get_mut(&src) else {
                    warn!("Received rekey from peer {src:X} but no connection exists, ignoring");
                    return Ok(());
                };
                let Some(connection) = conn
                    .connection
                    .as_any_mut()
                    .downcast_mut::<ConnectionRustrtcIce>()
                else {
                    warn!(
                        "Received rekey from peer {src:X} but connection is not ConnectionRustrtcIce, ignoring",
                    );
                    return Ok(());
                };
                if self.id > src {
                    // we received the rekey request
                    let new_ecdh_key = EphemeralSecret::generate();
                    let new_public_key = new_ecdh_key.public_key();
                    let mut key = [0; 16];
                    new_ecdh_key
                        .diffie_hellman(&public_key)
                        .extract::<Sha256>(None)
                        .expand(b"", &mut key)?;
                    connection.set_recv_key(ConnectionRustrtcIceKey {
                        index: *index,
                        key: key.into(),
                    });
                    conn.ecdh_key = new_ecdh_key;
                    conn.last_rekey = Instant::now();
                    self.mesh
                        .send_packet(
                            src,
                            Box::new(FullMeshPayload::Rekey {
                                index: *index,
                                public_key: serde_json::to_string(&new_public_key)?,
                            }),
                        )
                        .await?;
                } else {
                    // we received the rekey response
                    let mut key = [0; 16];
                    conn.ecdh_key
                        .diffie_hellman(&public_key)
                        .extract::<Sha256>(None)
                        .expand(b"", &mut key)?;
                    let new_key = ConnectionRustrtcIceKey {
                        index: *index,
                        key: key.into(),
                    };
                    connection.set_recv_key(new_key);
                    connection.set_send_key(new_key);
                    conn.last_rekey = Instant::now();
                    self.mesh
                        .send_packet(
                            src,
                            Box::new(FullMeshPayload::RekeyConfirm {
                                index: *index,
                                public_key: serde_json::to_string(&conn.ecdh_key.public_key())?,
                            }),
                        )
                        .await?;
                }
            }
            #[cfg(not(target_arch = "wasm32"))]
            FullMeshPayload::RekeyConfirm { index, public_key } => {
                let public_key = serde_json::from_str::<PublicKey>(public_key)?;
                let Some(conn) = self.connections.get_mut(&src) else {
                    warn!(
                        "Received rekey ack from peer {src:X} but no connection exists, ignoring",
                    );
                    return Ok(());
                };
                let Some(connection) = conn
                    .connection
                    .as_any_mut()
                    .downcast_mut::<ConnectionRustrtcIce>()
                else {
                    warn!(
                        "Received rekey ack from peer {src:X} but connection is not ConnectionRustrtcIce, ignoring",
                    );
                    return Ok(());
                };
                // we confirmed the rekey
                let mut key = [0; 16];
                conn.ecdh_key
                    .diffie_hellman(&public_key)
                    .extract::<Sha256>(None)
                    .expand(b"", &mut key)?;
                connection.set_send_key(ConnectionRustrtcIceKey {
                    index: *index,
                    key: key.into(),
                });
            }
            #[cfg(not(target_arch = "wasm32"))]
            FullMeshPayload::Offer { sdp, candidates } => {
                let connection = ConnectionRustrtcDataChannel::new(
                    self.ice_servers.clone(),
                    self.candidate_filter_prefix,
                )
                .await;
                let (answer, local_candidates) = connection.create_answer(sdp.clone()).await?;
                connection.add_candidates(candidates).await?;
                self.mesh
                    .send_packet(
                        src,
                        Box::new(FullMeshPayload::Answer {
                            sdp: answer,
                            candidates: local_candidates,
                        }),
                    )
                    .await?;
                self.connections.insert(
                    src,
                    FullMeshConnection {
                        connection: Box::new(connection),
                        last_received: (0, Instant::now()),
                        once_connected: false,
                        last_rekey: Instant::now(),
                        ecdh_key: EphemeralSecret::generate(),
                    },
                );
            }
            #[cfg(not(target_arch = "wasm32"))]
            FullMeshPayload::Answer { sdp, candidates } => {
                let Some(connection) = self.connections.get_mut(&src) else {
                    warn!("Received answer from peer {src:X} but no connection exists, ignoring");
                    return Ok(());
                };
                let Some(connection) = connection
                    .connection
                    .as_any_mut()
                    .downcast_mut::<ConnectionRustrtcDataChannel>()
                else {
                    warn!(
                        "Received answer from peer {src:X} but connection is not ConnectionRustrtcDataChannel, ignoring",
                    );
                    return Ok(());
                };
                connection.apply_answer(sdp.clone()).await?;
                connection.add_candidates(candidates).await?;
            }
            #[cfg(target_arch = "wasm32")]
            FullMeshPayload::Shake { .. }
            | FullMeshPayload::Rekey { .. }
            | FullMeshPayload::RekeyConfirm { .. } => {
                warn!("Received unsupported payload from peer {src:X}, ignoring");
            }
            #[cfg(target_arch = "wasm32")]
            FullMeshPayload::Offer { sdp, candidates } => {
                let mut connection = ConnectionWasmDataChannel::new(
                    self.ice_servers.clone(),
                    self.candidate_filter_prefix,
                )
                .await?;
                let (answer, local_candidates) = connection.create_answer(sdp.clone()).await?;
                connection.add_candidates(candidates).await?;
                self.connections.insert(
                    src,
                    FullMeshConnection {
                        connection: Box::new(connection),
                        last_received: (0, Instant::now()),
                        once_connected: false,
                    },
                );
                self.mesh
                    .send_packet(
                        src,
                        Box::new(FullMeshPayload::Answer {
                            sdp: answer,
                            candidates: local_candidates,
                        }),
                    )
                    .await?;
            }
            #[cfg(target_arch = "wasm32")]
            FullMeshPayload::Answer { sdp, candidates } => {
                let Some(conn) = self.connections.get_mut(&src) else {
                    warn!("Received answer from peer {src:X} but no connection exists, ignoring");
                    return Ok(());
                };
                let Some(connection) = conn
                    .connection
                    .as_any_mut()
                    .downcast_mut::<ConnectionWasmDataChannel>()
                else {
                    warn!(
                        "Received answer from peer {src:X} but connection is not ConnectionWasmDataChannel, ignoring",
                    );
                    return Ok(());
                };
                connection.apply_answer(sdp.clone()).await?;
                connection.add_candidates(candidates).await?;
            }
        }
        Ok(())
    }

    #[no_reply]
    pub async fn connect_tick(&mut self) -> Result<()> {
        let time = Instant::now();
        // gc
        let mut disconnected = Vec::new();
        self.connections.retain(|node_id, conn| {
            if conn.connection.status() == ConnectionState::Closed {
                return false;
            }
            let received = conn.connection.received();
            if received != conn.last_received.0 {
                conn.last_received = (received, time);
            }
            let keep = time.duration_since(conn.last_received.1) < self.connection_timeout;
            if !keep && conn.once_connected {
                disconnected.push(*node_id);
            }
            keep
        });
        {
            let mut dm = self.dm.lock().await;
            for node_id in disconnected {
                debug!("Connection to peer {node_id:X} timed out, disconnecting");
                dm.disconnected(node_id).await?;
            }
            // mark connected
            for (node_id, conn) in &mut self.connections {
                if !conn.once_connected && conn.connection.status() == ConnectionState::Connected {
                    conn.once_connected = true;
                    dm.connected(
                        *node_id,
                        conn.connection.sender().await?,
                        conn.connection.receiver().await?,
                    )
                    .await?;
                }
            }
        }
        // rekey
        #[cfg(not(target_arch = "wasm32"))]
        for peer_id in self.registry.get_nodes().await?.keys() {
            if self.id > *peer_id {
                continue;
            }
            let Some(conn) = self.connections.get_mut(peer_id) else {
                continue;
            };
            let Some(connection) = conn
                .connection
                .as_any_mut()
                .downcast_mut::<ConnectionRustrtcIce>()
            else {
                continue;
            };
            // TODO: rekey by sent/received bytes
            if connection.status() != ConnectionState::Connected
                || time.duration_since(conn.last_rekey) < self.rekey_timeout
            {
                continue;
            }
            let Some(key) = connection.key() else {
                continue;
            };
            info!("Rekeying connection to peer {peer_id:X}");
            let new_ecdh_key = EphemeralSecret::generate();
            let new_public_key = new_ecdh_key.public_key();
            conn.ecdh_key = new_ecdh_key;
            self.mesh
                .send_packet(
                    *peer_id,
                    Box::new(FullMeshPayload::Rekey {
                        index: !key.index,
                        public_key: serde_json::to_string(&new_public_key)?,
                    }),
                )
                .await?;
        }
        // connect
        for (peer_id, node) in self.registry.get_nodes().await? {
            if self.id > peer_id {
                continue;
            }
            if self.connections.get_mut(&peer_id).is_some() {
                continue;
            }
            #[cfg(not(target_arch = "wasm32"))]
            if node.0.connection_types.contains(&ConnectionType::Ice) {
                let result: Result<()> = try {
                    let ecdh_key = EphemeralSecret::generate();
                    let public_key = ecdh_key.public_key();
                    let (connection, parameters, candidates) = ConnectionRustrtcIce::new(
                        self.id,
                        peer_id,
                        self.handle.clone(),
                        self.ice_servers.clone(),
                        self.candidate_filter_prefix,
                        self.encrypt_local_packets,
                        true,
                    )
                    .await?;
                    self.connections.insert(
                        peer_id,
                        FullMeshConnection {
                            connection: Box::new(connection),
                            last_rekey: Instant::now(),
                            last_received: (0, Instant::now()),
                            once_connected: false,
                            ecdh_key,
                        },
                    );
                    self.mesh
                        .send_packet(
                            peer_id,
                            Box::new(FullMeshPayload::Shake {
                                ufrag: parameters.username_fragment,
                                pwd: parameters.password,
                                candidates,
                                // TODO: https://github.com/rust-lang/rust/issues/149488
                                public_key: serde_json::to_string(&public_key).unwrap(),
                            }),
                        )
                        .await?;
                };
                if let Err(err) = result {
                    error!("Failed to connect to peer {peer_id:X}: {err:?}");
                }
            } else if node
                .0
                .connection_types
                .contains(&ConnectionType::DataChannel)
            {
                let result: Result<()> = try {
                    let connection = ConnectionRustrtcDataChannel::new(
                        self.ice_servers.clone(),
                        self.candidate_filter_prefix,
                    )
                    .await;
                    let (offer, candidates) = connection.create_offer().await?;
                    self.connections.insert(
                        peer_id,
                        FullMeshConnection {
                            connection: Box::new(connection),
                            last_received: (0, Instant::now()),
                            once_connected: false,
                            last_rekey: Instant::now(),
                            ecdh_key: EphemeralSecret::generate(),
                        },
                    );
                    self.mesh
                        .send_packet(
                            peer_id,
                            Box::new(FullMeshPayload::Offer {
                                sdp: offer,
                                candidates,
                            }),
                        )
                        .await?;
                };
                if let Err(err) = result {
                    error!("Failed to connect to peer {peer_id:X}: {err:?}");
                }
            }
            #[cfg(target_arch = "wasm32")]
            if node
                .0
                .connection_types
                .contains(&ConnectionType::DataChannel)
            {
                let result: Result<()> = try {
                    let mut connection = ConnectionWasmDataChannel::new(
                        self.ice_servers.clone(),
                        self.candidate_filter_prefix,
                    )
                    .await?;
                    let (offer, candidates) = connection.create_offer().await?;
                    self.connections.insert(
                        peer_id,
                        FullMeshConnection {
                            connection: Box::new(connection),
                            last_received: (0, Instant::now()),
                            once_connected: false,
                        },
                    );
                    self.mesh
                        .send_packet(
                            peer_id,
                            Box::new(FullMeshPayload::Offer {
                                sdp: offer,
                                candidates,
                            }),
                        )
                        .await?;
                };
                if let Err(err) = result {
                    error!("Failed to connect to peer {peer_id:X}: {err:?}");
                }
            }
        }
        Ok(())
    }

    pub async fn get_peers(&self) -> HashMap<NodeId, Conn> {
        let mut result = HashMap::new();
        for (node_id, conn) in &self.connections {
            let state = match conn.connection.status() {
                ConnectionState::Connecting => ConnState::Connecting,
                ConnectionState::Connected => ConnState::Connected,
                ConnectionState::Closed => ConnState::Closed,
            };
            let selected_candidate = conn.connection.selected_candidate().await;
            let sent = conn.connection.sent();
            let received = conn.connection.received();
            result.insert(
                *node_id,
                Conn {
                    state,
                    selected_candidate,
                    sent,
                    received,
                },
            );
        }
        result
    }

    #[handle_error]
    fn handle_error(&mut self, err: &Error) {
        error!("Error: {err:?}");
    }
}
