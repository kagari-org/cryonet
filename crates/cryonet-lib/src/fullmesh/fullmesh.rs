use std::{any::Any, collections::HashMap, sync::Arc, time::Duration};

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
        Connection, ConnectionState, DeviceManager, IceServer, NotifyFn, is_initiator,
        registry::{ConnectionType, RegistryHandle},
    },
    mesh::{
        MeshHandle,
        packet::{NodeId, Packet, Payload},
    },
    time::{Instant, Interval, interval},
};

struct FullMeshConnection {
    connection: Box<dyn Connection>,
    last_received: (u64, Instant),
    once_connected: bool,
    peer_session: u64,
    conn_gen: u64,

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
        session: u64,
        conn_gen: u64,
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
        session: u64,
        conn_gen: u64,
    },
    Answer {
        sdp: String,
        candidates: Vec<String>,
        session: u64,
    },
    ConnectRequest {
        responder_session: u64,
        last_seen_gen: Option<u64>,
        connection_types: Vec<ConnectionType>,
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
    candidate_filter_prefixes: Vec<AnyIpCidr>,
    encrypt_local_packets: bool,
    connection_timeout: Duration,
    connect_timeout: Duration,
    rekey_timeout: Duration,
    connect_interval: Duration,
    connection_types: Vec<ConnectionType>,
    session: u64,
    next_conn_gen: u64,
    last_seen_gen: HashMap<NodeId, u64>,

    packet_rx: mpsc::Receiver<Packet>,
    connect_ticker: Interval,
    connections: HashMap<NodeId, FullMeshConnection>,
    connect_backoff: HashMap<NodeId, (Instant, Duration)>,

    notifier: Arc<NotifyFn>,
}

#[sactor(pub)]
impl FullMesh {
    pub async fn new(
        id: NodeId,
        mesh: MeshHandle,
        registry: RegistryHandle,
        dm: Arc<Mutex<Box<dyn DeviceManager>>>,
        ice_servers: Vec<IceServer>,
        candidate_filter_prefixes: Vec<AnyIpCidr>,
        encrypt_local_packets: bool,
        connection_types: Vec<ConnectionType>,
    ) -> Result<FullMeshHandle> {
        Self::new_with_parameters(
            id,
            mesh,
            registry,
            dm,
            ice_servers,
            Duration::from_secs(1),
            Duration::from_secs(12),
            Duration::from_secs(10),
            Duration::from_secs(120),
            candidate_filter_prefixes,
            encrypt_local_packets,
            connection_types,
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
        connect_timeout: Duration,
        rekey_timeout: Duration,
        candidate_filter_prefixes: Vec<AnyIpCidr>,
        encrypt_local_packets: bool,
        connection_types: Vec<ConnectionType>,
    ) -> Result<FullMeshHandle> {
        let packet_rx = mesh
            .add_dispatchee(Box::new(|packet| {
                (packet.payload.as_ref() as &dyn Any).is::<FullMeshPayload>()
            }))
            .await?;
        let session = new_session();
        let (future, fm) = FullMesh::run(move |handle| FullMesh {
            handle: handle.clone(),
            id,
            mesh,
            registry,
            dm,
            ice_servers,
            connection_timeout,
            connect_timeout,
            rekey_timeout,
            connect_interval,
            connection_types,
            session,
            next_conn_gen: 1,
            last_seen_gen: HashMap::new(),
            candidate_filter_prefixes,
            encrypt_local_packets,
            packet_rx,
            connect_ticker: interval(connect_interval),
            connections: HashMap::new(),
            connect_backoff: HashMap::new(),
            notifier: Arc::new(Box::new(move || {
                let handle = handle.clone();
                Box::pin(async move { handle.connect_tick().await })
            })),
        });
        tokio::task::spawn_local(future);
        fm.register_notifier().await??;
        Ok(fm)
    }

    async fn register_notifier(&self) -> Result<()> {
        self.registry
            .register_notifier(Arc::downgrade(&self.notifier))
            .await
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
                session,
                conn_gen,
            } => {
                let public_key = serde_json::from_str::<PublicKey>(public_key)?;
                if !is_initiator(self.id, src) {
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
                        self.candidate_filter_prefixes.clone(),
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
                            peer_session: *session,
                            conn_gen: *conn_gen,
                            last_rekey: Instant::now(),
                            ecdh_key,
                        },
                    );
                    self.last_seen_gen.insert(src, *conn_gen);
                    self.mesh
                        .send_packet(
                            src,
                            Box::new(FullMeshPayload::Shake {
                                ufrag: parameters.username_fragment,
                                pwd: parameters.password,
                                candidates: local_candidates,
                                public_key: serde_json::to_string(&local_public_key)?,
                                session: self.session,
                                conn_gen: *conn_gen,
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
                    conn.peer_session = *session;
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
                if !is_initiator(self.id, src) {
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
                    if connection.key().is_some_and(|key| key.index == *index) {
                        debug!("Ignoring stale rekey response from peer {src:X}");
                        return Ok(());
                    }
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
                if connection.key().is_some_and(|key| key.index == *index) {
                    debug!("Ignoring stale rekey confirm from peer {src:X}");
                    return Ok(());
                }
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
            FullMeshPayload::Offer {
                sdp,
                candidates,
                session,
                conn_gen,
            } => {
                let connection = ConnectionRustrtcDataChannel::new(
                    self.ice_servers.clone(),
                    self.candidate_filter_prefixes.clone(),
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
                            session: self.session,
                        }),
                    )
                    .await?;
                self.connections.insert(
                    src,
                    FullMeshConnection {
                        connection: Box::new(connection),
                        last_received: (0, Instant::now()),
                        once_connected: false,
                        peer_session: *session,
                        conn_gen: *conn_gen,
                        last_rekey: Instant::now(),
                        ecdh_key: EphemeralSecret::generate(),
                    },
                );
                self.last_seen_gen.insert(src, *conn_gen);
            }
            #[cfg(not(target_arch = "wasm32"))]
            FullMeshPayload::Answer {
                sdp,
                candidates,
                session,
            } => {
                let Some(connection) = self.connections.get_mut(&src) else {
                    warn!("Received answer from peer {src:X} but no connection exists, ignoring");
                    return Ok(());
                };
                connection.peer_session = *session;
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
            FullMeshPayload::Offer {
                sdp,
                candidates,
                session,
                conn_gen,
            } => {
                let mut connection = ConnectionWasmDataChannel::new(
                    self.ice_servers.clone(),
                    self.candidate_filter_prefixes.clone(),
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
                        peer_session: *session,
                        conn_gen: *conn_gen,
                    },
                );
                self.last_seen_gen.insert(src, *conn_gen);
                self.mesh
                    .send_packet(
                        src,
                        Box::new(FullMeshPayload::Answer {
                            sdp: answer,
                            candidates: local_candidates,
                            session: self.session,
                        }),
                    )
                    .await?;
            }
            #[cfg(target_arch = "wasm32")]
            FullMeshPayload::Answer {
                sdp,
                candidates,
                session,
            } => {
                let Some(conn) = self.connections.get_mut(&src) else {
                    warn!("Received answer from peer {src:X} but no connection exists, ignoring");
                    return Ok(());
                };
                conn.peer_session = *session;
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
            FullMeshPayload::ConnectRequest {
                responder_session,
                last_seen_gen,
                connection_types,
            } => {
                if !is_initiator(self.id, src) {
                    return Ok(());
                }
                let stale = match self.connections.get(&src) {
                    None => true,
                    Some(conn) => {
                        conn.peer_session != *responder_session
                            || last_seen_gen.is_some_and(|last| conn.conn_gen <= last)
                    }
                };
                debug!(
                    "node {:X}: ConnectRequest from {src:X}: responder_session={responder_session:X}, last_seen={last_seen_gen:?}, current={:?}, stale={stale}",
                    self.id,
                    self.connections
                        .get(&src)
                        .map(|conn| (conn.peer_session, conn.conn_gen))
                );
                if stale {
                    if self.connections.remove(&src).is_some() {
                        let mut dm = self.dm.lock().await;
                        dm.disconnected(src).await?;
                    }
                    self.connect_backoff.remove(&src);
                    self.connect_to(src, connection_types.clone()).await?;
                }
                return Ok(());
            }
        }
        Ok(())
    }

    #[skip]
    fn schedule_retry(&mut self, node_id: NodeId) {
        let delay = self
            .connect_backoff
            .get(&node_id)
            .map_or(Duration::from_secs(1), |(_, delay)| {
                delay.saturating_mul(2).min(Duration::from_secs(10))
            });
        self.connect_backoff
            .insert(node_id, (Instant::now() + delay, delay));
    }

    #[skip]
    fn finish_connect(&mut self, node_id: NodeId, result: Result<()>) {
        debug!(
            "node {:X}: finish_connect {:X} ok={}",
            self.id,
            node_id,
            result.is_ok()
        );
        if let Err(err) = result {
            error!("Failed to connect to peer {node_id:X}: {err:?}");
            self.connections.remove(&node_id);
            self.schedule_retry(node_id);
        } else {
            self.connect_backoff.remove(&node_id);
        }
    }

    #[no_reply]
    pub async fn connect_tick(&mut self) -> Result<()> {
        let time = Instant::now();
        // gc
        let mut disconnected = Vec::new();
        self.connections.retain(|node_id, conn| {
            if conn.connection.status() == ConnectionState::Closed {
                disconnected.push(*node_id);
                return false;
            }
            let received = conn.connection.received();
            if received != conn.last_received.0 {
                conn.last_received = (received, time);
            }
            let timeout =
                if conn.once_connected || conn.connection.status() == ConnectionState::Connected {
                    self.connection_timeout
                } else {
                    self.connect_timeout
                };
            let keep = time.duration_since(conn.last_received.1) < timeout;
            if !keep {
                disconnected.push(*node_id);
            }
            keep
        });
        {
            let mut dm = self.dm.lock().await;
            for node_id in &disconnected {
                debug!("node {:X}: connection to peer {node_id:X} is gone", self.id);
                dm.disconnected(*node_id).await?;
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
        for node_id in &disconnected {
            self.schedule_retry(*node_id);
        }
        let nodes = self.registry.get_nodes().await?;
        self.connect_backoff
            .retain(|node_id, _| nodes.contains_key(node_id));
        // rekey
        #[cfg(not(target_arch = "wasm32"))]
        for peer_id in nodes.keys() {
            if !is_initiator(self.id, *peer_id) {
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
            conn.last_rekey = time;
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
        for (&peer_id, node) in &nodes {
            if self.connections.contains_key(&peer_id) {
                continue;
            }
            if let Some((retry_at, _)) = self.connect_backoff.get(&peer_id)
                && time < *retry_at
            {
                continue;
            }
            if !is_initiator(self.id, peer_id) {
                debug!(
                    "node {:X}: responder request to {peer_id:X}: session={:X}, last_seen={:?}",
                    self.id,
                    self.session,
                    self.last_seen_gen.get(&peer_id)
                );
                self.connect_backoff.insert(
                    peer_id,
                    (time + self.connect_interval, self.connect_interval),
                );
                self.mesh
                    .send_packet(
                        peer_id,
                        Box::new(FullMeshPayload::ConnectRequest {
                            responder_session: self.session,
                            last_seen_gen: self.last_seen_gen.get(&peer_id).copied(),
                            connection_types: self.connection_types.clone(),
                        }),
                    )
                    .await?;
                continue;
            }
            self.connect_to(peer_id, node.0.connection_types.clone())
                .await?;
        }
        Ok(())
    }

    async fn connect_to(
        &mut self,
        peer_id: NodeId,
        connection_types: Vec<ConnectionType>,
    ) -> Result<()> {
        debug!("node {:X}: initiating full mesh to {peer_id:X}", self.id);
        #[cfg(not(target_arch = "wasm32"))]
        if connection_types.contains(&ConnectionType::Ice) {
            let result: Result<()> = try {
                let ecdh_key = EphemeralSecret::generate();
                let public_key = ecdh_key.public_key();
                let (connection, parameters, candidates) = ConnectionRustrtcIce::new(
                    self.id,
                    peer_id,
                    self.handle.clone(),
                    self.ice_servers.clone(),
                    self.candidate_filter_prefixes.clone(),
                    self.encrypt_local_packets,
                    true,
                )
                .await?;
                let conn_gen = self.next_conn_gen;
                self.next_conn_gen += 1;
                self.connections.insert(
                    peer_id,
                    FullMeshConnection {
                        connection: Box::new(connection),
                        last_rekey: Instant::now(),
                        last_received: (0, Instant::now()),
                        once_connected: false,
                        peer_session: 0,
                        conn_gen,
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
                            session: self.session,
                            conn_gen,
                        }),
                    )
                    .await?;
            };
            self.finish_connect(peer_id, result);
        } else if connection_types.contains(&ConnectionType::DataChannel) {
            let result: Result<()> = try {
                let connection = ConnectionRustrtcDataChannel::new(
                    self.ice_servers.clone(),
                    self.candidate_filter_prefixes.clone(),
                )
                .await;
                let (offer, candidates) = connection.create_offer().await?;
                let conn_gen = self.next_conn_gen;
                self.next_conn_gen += 1;
                self.connections.insert(
                    peer_id,
                    FullMeshConnection {
                        connection: Box::new(connection),
                        last_received: (0, Instant::now()),
                        once_connected: false,
                        peer_session: 0,
                        conn_gen,
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
                            session: self.session,
                            conn_gen,
                        }),
                    )
                    .await?;
            };
            self.finish_connect(peer_id, result);
        }
        #[cfg(target_arch = "wasm32")]
        if connection_types.contains(&ConnectionType::DataChannel) {
            let result: Result<()> = try {
                let mut connection = ConnectionWasmDataChannel::new(
                    self.ice_servers.clone(),
                    self.candidate_filter_prefixes.clone(),
                )
                .await?;
                let (offer, candidates) = connection.create_offer().await?;
                let conn_gen = self.next_conn_gen;
                self.next_conn_gen += 1;
                self.connections.insert(
                    peer_id,
                    FullMeshConnection {
                        connection: Box::new(connection),
                        last_received: (0, Instant::now()),
                        once_connected: false,
                        peer_session: 0,
                        conn_gen,
                    },
                );
                self.mesh
                    .send_packet(
                        peer_id,
                        Box::new(FullMeshPayload::Offer {
                            sdp: offer,
                            candidates,
                            session: self.session,
                            conn_gen,
                        }),
                    )
                    .await?;
            };
            self.finish_connect(peer_id, result);
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

fn new_session() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos() as u64)
        .unwrap_or(0);
    nanos ^ COUNTER.fetch_add(1, Ordering::Relaxed).rotate_left(32)
}
