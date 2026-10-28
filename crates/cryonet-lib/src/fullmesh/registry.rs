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
    fullmesh::{DeviceManager, IpEntry, IpMap, NotifyFn},
    mesh::{
        MeshEvent, MeshHandle,
        packet::{NodeId, Packet, Payload},
    },
    utils::{Instant, Interval, delayed_interval, jitter, now_ms, random_below},
};

const MAX_CLAIM_SKEW_MS: u64 = 5 * 60 * 1000;

fn claim_wins(current: &IpEntry, node: NodeId, claimed_at: u64) -> bool {
    claimed_at > current.claimed_at || (claimed_at == current.claimed_at && node < current.node)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConnectionType {
    Ice,
    DataChannel,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Node {
    pub connection_types: Vec<ConnectionType>,
    pub ips: Vec<IpAddr>,
    pub claimed_at: u64,
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
    sync_ticker: Interval,
    last_node: Option<Node>,
    mesh_event_rx: broadcast::Receiver<MeshEvent>,
    node_timeout: Duration,

    fanout: usize,
    gossip_batch: usize,
    gossip_hops: u8,

    nodes: HashMap<NodeId, (Node, Instant)>,
    ips: Arc<Mutex<IpMap>>,
    claimed_at: u64,
    last_ips: Option<Vec<IpAddr>>,
    conflicts: HashSet<(IpAddr, NodeId)>,
    notifiers: Vec<Weak<NotifyFn>>,
}

#[sactor(pub)]
impl Registry {
    pub async fn new(
        id: NodeId,
        mesh: MeshHandle,
        dm: Arc<Mutex<Box<dyn DeviceManager>>>,
        connection_types: Vec<ConnectionType>,
        ips: Arc<Mutex<IpMap>>,
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
        ips: Arc<Mutex<IpMap>>,
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
            sync_ticker: delayed_interval(jitter(gossip_interval * 6, 0.1)),
            last_node: None,
            mesh_event_rx,
            node_timeout,
            fanout: 4,
            gossip_batch: 64,
            gossip_hops: 6,
            nodes: HashMap::new(),
            ips,
            claimed_at: 0,
            last_ips: None,
            conflicts: HashSet::new(),
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
            selection!(self.sync_ticker.tick().await, sync_tick),
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
                let mut moved = Vec::new();
                {
                    let mut ips = self.ips.lock().await;
                    let wall_now = now_ms();
                    for (id, node) in entries {
                        if *id == self.id {
                            continue;
                        }
                        if let Some((known, _)) = self.nodes.get(id)
                            && node.claimed_at < known.claimed_at
                        {
                            debug!(
                                "Ignoring stale registry entry for {id:X}: claim {} < {}",
                                node.claimed_at, known.claimed_at
                            );
                            continue;
                        }
                        let is_changed = self.nodes.get(id).is_none_or(|(known, _)| known != node);
                        let last_seen = if is_changed || *hops > 0 {
                            now
                        } else {
                            self.nodes.get(id).map_or(now, |(_, last_seen)| *last_seen)
                        };
                        let previous = self.nodes.insert(*id, (node.clone(), last_seen));
                        if let Some((previous, _)) = previous {
                            for ip in &previous.ips {
                                if !node.ips.contains(ip)
                                    && ips.get(ip).is_some_and(|entry| entry.node == *id)
                                {
                                    ips.remove(ip);
                                }
                            }
                        }
                        for ip in &node.ips {
                            let claim = node.claimed_at;
                            if claim > wall_now.saturating_add(MAX_CLAIM_SKEW_MS) {
                                if self.conflicts.insert((*ip, *id)) {
                                    warn!(
                                        "Ignoring claim for {ip} from {id:X}: {claim} is too far in the future"
                                    );
                                }
                                continue;
                            }
                            let current = ips.get(ip).copied();
                            let wins = match current {
                                Some(current) => claim_wins(&current, *id, claim),
                                None => true,
                            };
                            if !wins {
                                let current = current.unwrap();
                                if current.node != *id && self.conflicts.insert((*ip, *id)) {
                                    warn!(
                                        "Ignoring claim for {ip} from {id:X}: owned by {:X} (claim {}, ours {claim})",
                                        current.node, current.claimed_at
                                    );
                                }
                                if current.node == *id
                                    && current.claimed_at == claim
                                    && *hops > 0
                                    && let Some(current) = ips.get_mut(ip)
                                {
                                    current.seen = now;
                                }
                                continue;
                            }
                            let previous = ips.insert(
                                *ip,
                                IpEntry {
                                    node: *id,
                                    claimed_at: claim,
                                    seen: now,
                                },
                            );
                            if previous.is_none_or(|previous| previous.node != *id) {
                                moved.push(*ip);
                            }
                            if let Some(previous) = previous
                                && previous.node != *id
                            {
                                warn!(
                                    "{ip} moved from {:X} to {id:X} (claim {claim})",
                                    previous.node
                                );
                            }
                            self.conflicts.remove(&(*ip, *id));
                        }
                        if is_changed {
                            changed.push((*id, node.clone()));
                        }
                    }
                }
                if !moved.is_empty() {
                    moved.sort();
                    moved.dedup();
                    let mut dm = self.dm.lock().await;
                    for ip in moved {
                        if let Err(err) = dm.neighbor_changed(ip).await {
                            warn!("Failed to invalidate neighbor for {ip}: {err:?}");
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
        let node = self.refresh_local_node().await?;
        self.last_node = Some(node.clone());
        self.send_gossip(vec![(self.id, node)], self.gossip_hops, None)
            .await?;
        Ok(())
    }

    #[no_reply]
    async fn ip_tick(&mut self) -> Result<()> {
        let node = self.refresh_local_node().await?;
        if self.last_node.as_ref() == Some(&node) {
            return Ok(());
        }
        self.last_node = Some(node.clone());
        self.send_gossip(vec![(self.id, node)], self.gossip_hops, None)
            .await?;
        Ok(())
    }

    #[no_reply]
    async fn sync_tick(&mut self) -> Result<()> {
        let peers = self
            .mesh
            .get_routes()
            .await?
            .into_keys()
            .filter(|id| *id != self.id)
            .collect::<Vec<_>>();
        let Some(dst) = peers.get(random_below(peers.len())).copied() else {
            return Ok(());
        };
        if let Err(err) = self.send_seed(dst).await {
            debug!("Failed to sync registry with {dst:X}: {err:?}");
        } else {
            debug!("Synced registry with {dst:X}");
        }
        Ok(())
    }

    #[no_reply]
    async fn handle_mesh_event(&mut self, event: Result<MeshEvent, RecvError>) -> Result<()> {
        let event = match event {
            Ok(event) => event,
            Err(RecvError::Lagged(_)) => {
                // Push our own record again; the next gossip rounds repair the rest.
                let node = self.refresh_local_node().await?;
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
    async fn device_ips(&self) -> Result<Vec<IpAddr>> {
        let mut ips = self.dm.lock().await.ips().await?;
        ips.sort();
        ips.dedup();
        Ok(ips)
    }

    #[skip]
    async fn refresh_local_node(&mut self) -> Result<Node> {
        let ips = self.device_ips().await?;
        if self.last_ips.as_ref() != Some(&ips) {
            self.last_ips = Some(ips.clone());
            self.claimed_at = now_ms().max(self.claimed_at.saturating_add(1));
        }
        Ok(Node {
            connection_types: self.connection_types.clone(),
            ips,
            claimed_at: self.claimed_at,
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
        let mut ips = self.ips.lock().await;
        ips.retain(|_, entry| {
            live.contains(&entry.node) || now.duration_since(entry.seen) < self.node_timeout
        });
        self.conflicts.retain(|(ip, _)| ips.contains_key(ip));
        Ok(())
    }

    #[skip]
    async fn send_seed(&mut self, dst: NodeId) -> Result<()> {
        let local = self.refresh_local_node().await?;
        let routes = self.mesh.get_routes().await?;
        let mut entries = self
            .nodes
            .iter()
            .filter(|(id, _)| **id != dst && routes.contains_key(*id))
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
        match self.device_ips().await {
            Ok(ips) => ips.iter().map(|ip| ip.to_string()).collect(),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(node: NodeId, claimed_at: u64) -> IpEntry {
        IpEntry {
            node,
            claimed_at,
            seen: Instant::now(),
        }
    }

    #[test]
    fn claim_wins_prefers_newer_claim() {
        assert!(claim_wins(&entry(2, 100), 1, 101));
        assert!(!claim_wins(&entry(2, 100), 1, 99));
    }

    #[test]
    fn claim_wins_breaks_ties_by_node_id() {
        assert!(claim_wins(&entry(2, 100), 1, 100));
        assert!(!claim_wins(&entry(1, 100), 2, 100));
        assert!(!claim_wins(&entry(1, 100), 1, 100));
    }
}
