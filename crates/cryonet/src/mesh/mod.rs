use std::{collections::HashMap, fmt::Debug};

use async_trait::async_trait;
use futures::future::join_all;
use packet::{NodeId, Packet, Payload};
use sactor::{error::SactorResult, sactor};
use tokio::{
    select,
    sync::{broadcast, mpsc, watch},
};
use tracing::{debug, error, warn};

use crate::errors::Error;

pub(crate) mod igp;
pub(crate) mod packet;
pub(crate) mod seq;

pub(crate) struct Mesh {
    handle: MeshHandle,

    id: NodeId,

    link_send: HashMap<NodeId, Box<dyn LinkSend>>,
    link_recv_stop: HashMap<NodeId, watch::Sender<bool>>,
    routes: HashMap<NodeId, NodeId>,
    #[allow(clippy::type_complexity)]
    dispatchees: Vec<(Box<dyn Fn(&Packet) -> bool + Send + Sync>, mpsc::Sender<Packet>)>,

    mesh_event_tx: broadcast::Sender<MeshEvent>,
}

#[async_trait]
pub(crate) trait LinkSend: Send + Sync {
    async fn send(&mut self, packet: Packet) -> Result<(), LinkError>;
}

#[async_trait]
pub(crate) trait LinkRecv: Send + Sync {
    async fn recv(&mut self) -> Result<Packet, LinkError>;
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum LinkError {
    #[error("Link closed")]
    Closed,
    #[error("Unknown error: {0}")]
    Unknown(#[from] anyhow::Error),
}

#[allow(dead_code)]
#[derive(Debug, Clone, Copy)]
pub(crate) enum MeshEvent {
    LinkUp(NodeId),
    LinkDown(NodeId),
    RouteSet(NodeId, NodeId),
    RouteRemoved(NodeId, NodeId),
}

#[sactor(pub(crate))]
impl Mesh {
    pub(crate) fn new(id: NodeId) -> MeshHandle {
        let (future, mesh) = Mesh::run(move |handle| Mesh {
            handle,
            id,
            link_send: HashMap::new(),
            link_recv_stop: HashMap::new(),
            routes: HashMap::new(),
            dispatchees: Vec::new(),
            mesh_event_tx: broadcast::Sender::new(16),
        });
        tokio::spawn(future);
        mesh
    }

    #[no_reply]
    async fn handle_packet(&mut self, from: NodeId, packet: Result<Packet, LinkError>) -> SactorResult<()> {
        let mut packet = match packet {
            Ok(packet) => packet,
            Err(LinkError::Closed) => {
                self.remove_link(from);
                return Ok(());
            }
            res => res?,
        };
        debug!("Received packet: {:?}", packet);
        if packet.dst == self.id {
            for (filter, dispatch) in self.dispatchees.iter() {
                if filter(&packet) {
                    if let Err(err) = dispatch.try_send(packet) {
                        warn!("Failed to dispatch packet to handler: {}", err);
                    }
                    break;
                }
            }
        } else {
            // forward
            if packet.ttl == 0 {
                debug!("Dropping packet to node {:X}: TTL expired", packet.dst);
                return Ok(());
            }
            packet.ttl -= 1;
            self.send_packet_internal(packet).await?;
        }
        Ok(())
    }

    async fn send_packet_internal(&mut self, packet: Packet) -> SactorResult<()> {
        let Some(next_hop) = self.routes.get(&packet.dst) else {
            return Err(Error::Unreachable(packet.dst).into());
        };
        let Some(link) = self.link_send.get_mut(next_hop) else {
            return Err(Error::Unreachable(packet.dst).into());
        };
        debug!("Sending packet {:?} to {:X} via link {:X}", &packet, packet.dst, next_hop);
        let res = link.send(packet).await;
        if let Err(LinkError::Closed) = res {
            self.remove_link(*next_hop);
        }
        res?;
        Ok(())
    }

    #[no_reply]
    pub(crate) async fn send_packet(&mut self, dst: NodeId, payload: Box<dyn Payload>) -> SactorResult<()> {
        self.send_packet_internal(Packet { src: self.id, dst, ttl: 16, payload }).await
    }

    #[no_reply]
    pub(crate) async fn send_packet_link(&mut self, dst: NodeId, payload: Box<dyn Payload>) -> SactorResult<()> {
        let Some(link) = self.link_send.get_mut(&dst) else {
            return Err(Error::NoSuchLink(dst).into());
        };
        let packet = Packet { src: self.id, dst, ttl: 16, payload };
        debug!("Sending packet {:?} to {:X} via direct link", &packet, dst);
        let res = link.send(packet).await;
        if let Err(LinkError::Closed) = res {
            self.remove_link(dst);
        }
        res?;
        Ok(())
    }

    #[no_reply]
    pub(crate) async fn broadcast_packet_local(&mut self, payload: Box<dyn Payload>) -> SactorResult<()> {
        debug!("Broadcasting packet {:?} to all links", &payload);
        let futures = self
            .link_send
            .iter_mut()
            .map(|(dst, link)| async {
                (
                    link.send(Packet {
                        src: self.id,
                        dst: *dst,
                        ttl: 16,
                        payload: dyn_clone::clone_box(&*payload),
                    })
                    .await,
                    *dst,
                )
            })
            .collect::<Vec<_>>();
        let results = join_all(futures).await;
        for (result, dst) in &results {
            if let Err(LinkError::Closed) = result {
                self.remove_link(*dst);
            }
        }
        results.into_iter().map(|(res, _)| res).collect::<Result<Vec<_>, _>>().map(|_| ())?;
        Ok(())
    }

    pub(crate) fn add_dispatchee(&mut self, filter: Box<dyn Fn(&Packet) -> bool + Send + Sync>) -> mpsc::Receiver<Packet> {
        let (tx, rx) = mpsc::channel(512);
        self.dispatchees.push((filter, tx));
        rx
    }

    #[allow(dead_code)]
    pub(crate) fn remove_dispatchee(&mut self, mut rx: mpsc::Receiver<Packet>) {
        rx.close();
        self.dispatchees.retain(|(_, tx)| !tx.is_closed());
    }

    pub(crate) fn add_link(&mut self, dst: NodeId, send: Box<dyn LinkSend>, mut recv: Box<dyn LinkRecv>) -> bool {
        if self.link_send.contains_key(&dst) {
            if self.id > dst {
                debug!("Link to node {:X} already exists, keeping existing (our id {:X} > {:X})", dst, self.id, dst);
                return false;
            }
            debug!("Link to node {:X} already exists, replacing (our id {:X} < {:X})", dst, self.id, dst);
            self.remove_link(dst);
        }

        let mesh = self.handle.clone();
        self.link_send.insert(dst, send);
        let (stop, mut stop_rx) = watch::channel(false);
        self.link_recv_stop.insert(dst, stop);

        tokio::spawn(async move {
            loop {
                select! {
                    _ = stop_rx.changed() => break,
                    packet = recv.recv() => {
                        let brk = matches!(&packet, Err(LinkError::Closed));
                        let _ = mesh.handle_packet(dst, packet).await;
                        if brk {
                            break;
                        }
                    }
                }
            }
        });

        let _ = self.mesh_event_tx.send(MeshEvent::LinkUp(dst));
        true
    }

    pub(crate) fn remove_link(&mut self, dst: NodeId) {
        debug!("Removing link to node {:X}", dst);
        self.routes.retain(|_, &mut v| v != dst);
        self.link_send.remove(&dst);
        let ctrl = self.link_recv_stop.remove(&dst);
        if let Some(ctrl) = ctrl {
            let _ = ctrl.send(true);
        }
        let _ = self.mesh_event_tx.send(MeshEvent::LinkDown(dst));
    }

    pub(crate) fn get_links(&self) -> Vec<NodeId> {
        self.link_send.keys().cloned().collect()
    }

    pub(crate) fn set_route(&mut self, dst: NodeId, next_hop: NodeId) {
        if dst == self.id {
            // skip routes to self
            return;
        }
        if !self.link_send.contains_key(&next_hop) {
            warn!("Cannot set route to node {:X}: next hop {:X} does not exist", dst, next_hop);
            return;
        }
        debug!("Setting route: {:X} via {:X}", dst, next_hop);
        self.routes.insert(dst, next_hop);
        let _ = self.mesh_event_tx.send(MeshEvent::RouteSet(dst, next_hop));
    }

    pub(crate) fn remove_route(&mut self, dst: NodeId) {
        debug!("Removing route to node {:X}", dst);
        let route = self.routes.remove(&dst);
        if let Some(next_hop) = route {
            let _ = self.mesh_event_tx.send(MeshEvent::RouteRemoved(dst, next_hop));
        }
    }

    pub(crate) fn get_routes(&self) -> HashMap<NodeId, NodeId> {
        self.routes.clone()
    }

    pub(crate) fn subscribe_mesh_events(&self) -> broadcast::Receiver<MeshEvent> {
        self.mesh_event_tx.subscribe()
    }
}

impl Drop for Mesh {
    fn drop(&mut self) {
        for stop in self.link_recv_stop.values() {
            let _ = stop.send(true);
        }
    }
}
