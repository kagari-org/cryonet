use std::{
    any::Any,
    collections::{HashMap, HashSet},
    net::IpAddr,
    sync::{Arc, Weak},
    time::Duration,
};

use anyhow::Result;
use cryonet_uapi::{NodeInfo, RegistryStats};
use sactor::sactor;
use serde::{Deserialize, Serialize};
use tokio::sync::{
    Mutex,
    broadcast::{self, error::RecvError},
    mpsc,
};
use tracing::{debug, error, warn};

use crate::{
    fullmesh::{DeviceManager, NotifyFn},
    mesh::{
        MeshEvent, MeshHandle,
        packet::{NodeId, Packet, Payload},
    },
    time::{Instant, Interval, delayed_interval, jitter, random_below},
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConnectionType {
    Ice,
    DataChannel,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Node {
    pub connection_types: Vec<ConnectionType>,
    pub ips: Vec<IpAddr>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
enum RegistryPayload {
    Gossip {
        entries: Vec<(NodeId, Node)>,
        hops: u8,
    },
}

#[typetag::serde]
impl Payload for RegistryPayload {}

pub struct Registry {
    handle: RegistryHandle,

    id: NodeId,
    mesh: MeshHandle,
    dm: Arc<Mutex<Box<dyn DeviceManager>>>,
    connection_types: Vec<ConnectionType>,

    packet_rx: mpsc::Receiver<Packet>,
    gossip_ticker: Interval,
    ip_ticker: Interval,
    last_node: Option<Node>,
    mesh_event_rx: broadcast::Receiver<MeshEvent>,
    node_timeout: Duration,

    fanout: usize,
    gossip_batch: usize,
    gossip_hops: u8,

    nodes: HashMap<NodeId, (Node, Instant)>,
    ips: Arc<Mutex<HashMap<IpAddr, (NodeId, Instant)>>>,
    notifiers: Vec<Weak<NotifyFn>>,
}

#[sactor(pub)]
impl Registry {
    pub async fn new(
        id: NodeId,
        mesh: MeshHandle,
        dm: Arc<Mutex<Box<dyn DeviceManager>>>,
        connection_types: Vec<ConnectionType>,
        ips: Arc<Mutex<HashMap<IpAddr, (NodeId, Instant)>>>,
    ) -> Result<RegistryHandle> {
        Self::new_with_parameters(
            id,
            mesh,
            dm,
            connection_types,
            jitter(Duration::from_secs(5), 0.1),
            Duration::from_secs(120),
            ips,
        )
        .await
    }

    pub async fn new_with_parameters(
        id: NodeId,
        mesh: MeshHandle,
        dm: Arc<Mutex<Box<dyn DeviceManager>>>,
        connection_types: Vec<ConnectionType>,
        gossip_interval: Duration,
        node_timeout: Duration,
        ips: Arc<Mutex<HashMap<IpAddr, (NodeId, Instant)>>>,
    ) -> Result<RegistryHandle> {
        let packet_rx = mesh
            .add_dispatchee(Box::new(|packet| {
                packet
                    .payload
                    .as_payload()
                    .is_some_and(|payload| (payload as &dyn Any).is::<RegistryPayload>())
            }))
            .await?;
        let mesh_event_rx = mesh.subscribe_mesh_events().await?;
        let (future, registry) = Registry::run(move |handle| Registry {
            handle,
            id,
            mesh,
            dm,
            connection_types,
            packet_rx,
            gossip_ticker: delayed_interval(gossip_interval),
            ip_ticker: delayed_interval(Duration::from_secs(1)),
            last_node: None,
            mesh_event_rx,
            node_timeout,
            fanout: 4,
            gossip_batch: 64,
            gossip_hops: 6,
            nodes: HashMap::new(),
            ips,
            notifiers: Vec::new(),
        });
        tokio::task::spawn_local(future);
        Ok(registry)
    }

    #[select]
    fn select(&mut self) -> Vec<Selection<'_>> {
        vec![
            selection!(self.packet_rx.recv().await, handle_packet, it => it),
            selection!(self.mesh_event_rx.recv().await, handle_mesh_event, it => it),
            selection!(self.gossip_ticker.tick().await, gossip_tick),
            selection!(self.ip_ticker.tick().await, ip_tick),
        ]
    }

    #[no_reply]
    async fn handle_packet(&mut self, packet: Option<Packet>) -> Result<()> {
        let Some(packet) = packet else {
            self.handle.stop();
            return Ok(());
        };
        let src = packet.src;
        let payload = packet
            .payload
            .as_payload()
            .and_then(|payload| (payload as &dyn Any).downcast_ref::<RegistryPayload>())
            .unwrap();
        match payload {
            RegistryPayload::Gossip { entries, hops } => {
                let now = Instant::now();
                let mut changed = Vec::new();
                {
                    let mut ips = self.ips.lock().await;
                    for (id, node) in entries {
                        if *id == self.id {
                            continue;
                        }
                        let is_changed = self.nodes.get(id).is_none_or(|(known, _)| known != node);
                        self.nodes.insert(*id, (node.clone(), now));
                        for ip in &node.ips {
                            ips.insert(*ip, (*id, now));
                        }
                        if is_changed {
                            changed.push((*id, node.clone()));
                        }
                    }
                }
                if !changed.is_empty() {
                    self.notify().await;
                    if *hops > 0 {
                        self.send_gossip(changed, *hops - 1, Some(src)).await?;
                    }
                }
            }
        }
        Ok(())
    }

    #[no_reply]
    async fn gossip_tick(&mut self) -> Result<()> {
        self.gc().await?;
        let node = self.local_node().await?;
        self.last_node = Some(node.clone());
        self.send_gossip(vec![(self.id, node)], self.gossip_hops, None)
            .await?;
        Ok(())
    }

    #[no_reply]
    async fn ip_tick(&mut self) -> Result<()> {
        let node = self.local_node().await?;
        if self.last_node.as_ref() == Some(&node) {
            return Ok(());
        }
        self.last_node = Some(node.clone());
        self.send_gossip(vec![(self.id, node)], self.gossip_hops, None)
            .await?;
        Ok(())
    }

    #[no_reply]
    async fn handle_mesh_event(&mut self, event: Result<MeshEvent, RecvError>) -> Result<()> {
        let event = match event {
            Ok(event) => event,
            Err(RecvError::Lagged(_)) => {
                // Push our own record again; the next gossip rounds repair the rest.
                let node = self.local_node().await?;
                self.send_gossip(vec![(self.id, node)], self.gossip_hops, None)
                    .await?;
                return Ok(());
            }
            Err(RecvError::Closed) => {
                self.handle.stop();
                return Ok(());
            }
        };
        if let MeshEvent::LinkUp(dst) = event
            && let Err(err) = self.send_seed(dst).await
        {
            error!("Failed to seed registry to {dst:X}: {err:?}");
        }
        Ok(())
    }

    #[skip]
    async fn local_node(&self) -> Result<Node> {
        let mut ips = self.dm.lock().await.ips().await?;
        ips.sort();
        ips.dedup();
        Ok(Node {
            connection_types: self.connection_types.clone(),
            ips,
        })
    }

    #[skip]
    async fn notify(&mut self) {
        for notify in &self.notifiers {
            if let Some(notify) = notify.upgrade() {
                tokio::task::spawn_local(async move {
                    if let Err(err) = notify().await {
                        error!("Failed to notify registry change: {err:?}");
                    }
                });
            }
        }
        self.notifiers.retain(|notify| notify.strong_count() > 0);
    }

    #[skip]
    async fn gc(&mut self) -> Result<()> {
        let routes = self.mesh.get_routes().await?;
        let now = Instant::now();
        self.nodes.retain(|id, (_, last_seen)| {
            routes.contains_key(id) || now.duration_since(*last_seen) < self.node_timeout
        });
        let live: HashSet<NodeId> = self.nodes.keys().copied().collect();
        self.ips.lock().await.retain(|_, (id, last_seen)| {
            live.contains(id) || now.duration_since(*last_seen) < self.node_timeout
        });
        Ok(())
    }

    #[skip]
    async fn send_seed(&self, dst: NodeId) -> Result<()> {
        let local = self.local_node().await?;
        let mut entries = self
            .nodes
            .iter()
            .filter(|(id, _)| **id != dst)
            .map(|(id, (node, _))| (*id, node.clone()))
            .collect::<Vec<_>>();
        entries.push((self.id, local));
        for chunk in entries.chunks(self.gossip_batch) {
            self.mesh
                .send_packet_reliable(
                    dst,
                    Box::new(RegistryPayload::Gossip {
                        entries: chunk.to_vec(),
                        hops: 0,
                    }),
                )
                .await?;
        }
        Ok(())
    }

    #[skip]
    async fn send_gossip(
        &self,
        entries: Vec<(NodeId, Node)>,
        hops: u8,
        exclude: Option<NodeId>,
    ) -> Result<()> {
        if entries.is_empty() || hops == 0 {
            return Ok(());
        }
        let mut peers = self
            .mesh
            .get_routes()
            .await?
            .into_keys()
            .filter(|id| *id != self.id && Some(*id) != exclude)
            .collect::<Vec<_>>();
        let fanout = peers.len().min(self.fanout);
        for i in 0..fanout {
            let j = i + random_below(peers.len() - i);
            peers.swap(i, j);
        }
        for peer in &peers[..fanout] {
            for chunk in entries.chunks(self.gossip_batch) {
                let payload: Box<dyn Payload> = Box::new(RegistryPayload::Gossip {
                    entries: chunk.to_vec(),
                    hops,
                });
                if let Err(err) = self.mesh.send_packet(*peer, payload).await {
                    debug!("Failed to gossip registry to {peer:X}: {err:?}");
                }
            }
        }
        Ok(())
    }

    pub async fn register_notifier(&mut self, notify: Weak<NotifyFn>) {
        self.notifiers.push(notify);
    }

    pub fn get_node_table(&self) -> HashMap<NodeId, (Node, Instant)> {
        self.nodes.clone()
    }

    pub fn get_nodes(&self) -> Vec<NodeInfo> {
        let now = Instant::now();
        let mut nodes = self
            .nodes
            .iter()
            .map(|(id, (node, last_seen))| NodeInfo {
                id: *id,
                ips: node.ips.iter().map(|ip| ip.to_string()).collect(),
                connection_types: node
                    .connection_types
                    .iter()
                    .map(|connection_type| connection_type_name(connection_type).to_owned())
                    .collect(),
                last_seen_ms: now.duration_since(*last_seen).as_millis() as i64,
            })
            .collect::<Vec<_>>();
        nodes.sort_by_key(|node| node.id);
        nodes
    }

    pub async fn get_stats(&self) -> RegistryStats {
        let nodes = self.nodes.len() as u64;
        let ips = self.ips.lock().await.len() as u64;
        RegistryStats { nodes, ips }
    }

    pub async fn get_local_ips(&self) -> Vec<String> {
        match self.local_node().await {
            Ok(node) => node.ips.iter().map(|ip| ip.to_string()).collect(),
            Err(err) => {
                warn!("Failed to read local IP addresses: {err:?}");
                Vec::new()
            }
        }
    }
}

fn connection_type_name(connection_type: &ConnectionType) -> &'static str {
    match connection_type {
        ConnectionType::Ice => "ice",
        ConnectionType::DataChannel => "datachannel",
    }
}
