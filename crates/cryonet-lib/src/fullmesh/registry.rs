use std::{
    any::Any,
    collections::HashMap,
    net::IpAddr,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::Result;
use sactor::sactor;
use serde::{Deserialize, Serialize};
use tokio::{
    sync::{Mutex, mpsc},
    time::{Interval, interval},
};
use tracing::error;

use crate::{
    fullmesh::DeviceManager,
    mesh::{
        MeshHandle,
        packet::{NodeId, Packet, Payload},
    },
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConnectionType {
    Ice,
    DataChannel,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Node {
    pub connection_types: Vec<ConnectionType>,
    pub ips: Vec<IpAddr>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
enum RegistryPayload {
    Node(Node),
}

#[typetag::serde]
impl Payload for RegistryPayload {}

pub struct Registry {
    handle: RegistryHandle,

    mesh: MeshHandle,
    dm: Arc<Mutex<Box<dyn DeviceManager>>>,
    connection_types: Vec<ConnectionType>,

    packet_rx: mpsc::Receiver<Packet>,
    announce_ticker: Interval,
    node_timeout: Duration,

    nodes: HashMap<NodeId, (Node, Instant)>,
    ips: Arc<Mutex<HashMap<IpAddr, (NodeId, Instant)>>>,
}

#[sactor(pub)]
impl Registry {
    pub async fn new(
        mesh: MeshHandle,
        dm: Arc<Mutex<Box<dyn DeviceManager>>>,
        connection_types: Vec<ConnectionType>,
        ips: Arc<Mutex<HashMap<IpAddr, (NodeId, Instant)>>>,
    ) -> Result<RegistryHandle> {
        Self::new_with_parameters(
            mesh,
            dm,
            connection_types,
            Duration::from_secs(30),
            Duration::from_secs(120),
            ips,
        )
        .await
    }

    pub async fn new_with_parameters(
        mesh: MeshHandle,
        dm: Arc<Mutex<Box<dyn DeviceManager>>>,
        connection_types: Vec<ConnectionType>,
        announce_interval: Duration,
        node_timeout: Duration,
        ips: Arc<Mutex<HashMap<IpAddr, (NodeId, Instant)>>>,
    ) -> Result<RegistryHandle> {
        let packet_rx = mesh
            .add_dispatchee(Box::new(|packet| {
                (packet.payload.as_ref() as &dyn Any).is::<RegistryPayload>()
            }))
            .await?;
        let (future, registry) = Registry::run(move |handle| Registry {
            handle,
            mesh,
            dm,
            connection_types,
            packet_rx,
            announce_ticker: interval(announce_interval),
            node_timeout,
            nodes: HashMap::new(),
            ips,
        });
        tokio::task::spawn_local(future);
        Ok(registry)
    }

    #[select]
    fn select(&mut self) -> Vec<Selection<'_>> {
        vec![
            selection!(self.packet_rx.recv().await, handle_packet, it => it),
            selection!(self.announce_ticker.tick().await, announce_tick),
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
            .downcast_ref::<RegistryPayload>()
            .unwrap();
        match payload {
            RegistryPayload::Node(node) => {
                let now = Instant::now();
                self.nodes.insert(src, (node.clone(), now));
                let mut ips = self.ips.lock().await;
                for ip in &node.ips {
                    ips.insert(*ip, (src, now));
                }
            }
        }
        Ok(())
    }

    #[no_reply]
    async fn announce_tick(&mut self) -> Result<()> {
        // gc
        let now = Instant::now();
        self.nodes
            .retain(|_, (_, last_seen)| now.duration_since(*last_seen) < self.node_timeout);
        self.ips
            .lock()
            .await
            .retain(|_, (_, last_seen)| now.duration_since(*last_seen) < self.node_timeout);
        // annonce
        let node = Node {
            connection_types: self.connection_types.clone(),
            ips: self.dm.lock().await.ips().await?,
        };
        let peers = self
            .mesh
            .get_routes()
            .await?
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        for peer in peers {
            let result = self
                .mesh
                .send_packet(peer, Box::new(RegistryPayload::Node(node.clone())))
                .await;
            if let Err(e) = result {
                error!("Failed to send registry packet to {}: {:?}", peer, e);
            }
        }
        Ok(())
    }

    pub fn get_nodes(&self) -> HashMap<NodeId, (Node, Instant)> {
        self.nodes.clone()
    }
}
