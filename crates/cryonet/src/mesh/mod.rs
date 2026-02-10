use std::{
    collections::HashMap,
    fmt::Debug,
    sync::{Arc, Weak},
};

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use futures::future::join_all;
use packet::{NodeId, Packet, Payload};
use tokio::{
    select,
    sync::{Mutex, Notify, broadcast, mpsc},
};
use tracing::{debug, error, info, warn};

use crate::errors::Error;

pub(crate) mod igp;
pub(crate) mod packet;
pub(crate) mod seq;

pub(crate) struct Mesh {
    pub(crate) id: NodeId,

    this: Weak<Mutex<Mesh>>,

    link_send: HashMap<NodeId, Box<dyn LinkSend>>,
    link_recv_stop: HashMap<NodeId, Arc<Notify>>,
    routes: HashMap<NodeId, NodeId>,
    #[allow(clippy::type_complexity)]
    dispatchees: Vec<(
        Box<dyn Fn(&Packet) -> bool + Send + Sync>,
        mpsc::Sender<Packet>,
    )>,

    packets_tx: mpsc::Sender<(NodeId, Result<Packet, LinkError>)>,
    mesh_event_tx: broadcast::Sender<MeshEvent>,

    stop: Arc<Notify>,
}

#[async_trait]
pub(crate) trait LinkSend: Send + Sync {
    async fn send(&mut self, packet: Packet) -> std::result::Result<(), LinkError>;
}

#[async_trait]
pub(crate) trait LinkRecv: Send + Sync {
    async fn recv(&mut self) -> std::result::Result<Packet, LinkError>;
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

impl Mesh {
    pub(crate) fn new(id: NodeId) -> Arc<Mutex<Self>> {
        let (packets_tx, mut packets_rx) = mpsc::channel(1024);
        let stop = Arc::new(Notify::new());
        let stop_rx = stop.clone();
        let mesh = Arc::new_cyclic(|this| {
            Mutex::new(Mesh {
                id,
                this: this.clone(),
                link_send: HashMap::new(),
                link_recv_stop: HashMap::new(),
                routes: HashMap::new(),
                dispatchees: Vec::new(),
                packets_tx: packets_tx.clone(),
                mesh_event_tx: broadcast::Sender::new(16),
                stop,
            })
        });
        let mesh2 = mesh.clone();
        tokio::spawn(async move {
            let notified = stop_rx.notified();
            tokio::pin!(notified);
            loop {
                select! {
                    _ = &mut notified => break,
                    packet = packets_rx.recv() => {
                        let (from, packet) = match packet {
                            Some(packet) => packet,
                            None => break,
                        };
                        let mut mesh = mesh.lock().await;
                        if let Err(err) = mesh.handle_packet(from, packet).await {
                            warn!("Failed to handle packet from node {:X}: {}", from, err);
                        }
                    }
                }
            }
            info!("Mesh handler stopped");
        });
        mesh2
    }

    async fn send_packet_internal(&mut self, packet: Packet) -> Result<()> {
        let Some(next_hop) = self.routes.get(&packet.dst) else {
            return Err(anyhow!(Error::Unreachable(packet.dst)));
        };
        let Some(link) = self.link_send.get_mut(next_hop) else {
            return Err(anyhow!(Error::Unreachable(packet.dst)));
        };
        debug!(
            "Sending packet {:?} to {:X} via link {:X}",
            &packet, packet.dst, next_hop
        );
        match link.send(packet).await {
            res @ Err(LinkError::Closed) => {
                self.remove_link(*next_hop);
                res
            }
            res => res,
        }?;
        Ok(())
    }

    pub(crate) async fn send_packet<P: Payload>(&mut self, dst: NodeId, payload: P) -> Result<()> {
        self.send_packet_internal(Packet {
            src: self.id,
            dst,
            ttl: 16,
            payload: Box::new(payload),
        })
        .await
    }

    pub(crate) async fn send_packet_link<P: Payload>(
        &mut self,
        dst: NodeId,
        payload: P,
    ) -> Result<()> {
        let Some(link) = self.link_send.get_mut(&dst) else {
            return Err(anyhow!(Error::NoSuchLink(dst)));
        };
        let packet = Packet {
            src: self.id,
            dst,
            ttl: 16,
            payload: Box::new(payload),
        };
        debug!("Sending packet {:?} to {:X} via direct link", &packet, dst);
        match link.send(packet).await {
            Err(LinkError::Closed) => self.remove_link(dst),
            res => res?,
        };
        Ok(())
    }

    pub(crate) async fn broadcast_packet_local<P: Payload>(&mut self, payload: P) -> Result<()> {
        debug!("Broadcasting packet {:?} to all links", &payload);
        let payload = Box::new(payload);
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
        results
            .into_iter()
            .map(|(res, _)| res)
            .collect::<Result<Vec<_>, _>>()
            .map(|_| ())?;
        Ok(())
    }

    pub(crate) fn add_dispatchee<F>(&mut self, filter: F) -> mpsc::Receiver<Packet>
    where
        F: Fn(&Packet) -> bool + Send + Sync + 'static,
    {
        let (tx, rx) = mpsc::channel(64);
        self.dispatchees.push((Box::new(filter), tx));
        rx
    }

    #[allow(dead_code)]
    pub(crate) fn remove_dispatchee(&mut self, rx: &mut mpsc::Receiver<Packet>) {
        rx.close();
        self.dispatchees.retain(|(_, tx)| !tx.is_closed());
    }

    pub(crate) fn add_link(
        &mut self,
        dst: NodeId,
        send: Box<dyn LinkSend>,
        mut recv: Box<dyn LinkRecv>,
    ) {
        if self.link_send.contains_key(&dst) {
            debug!("Link to node {:X} already exists, removing old link", dst);
            self.remove_link(dst);
        }

        self.link_send.insert(dst, send);
        let stop = Arc::new(Notify::new());
        let stop_rx = stop.clone();
        self.link_recv_stop.insert(dst, stop);
        let packets_tx = self.packets_tx.clone();

        let mesh = self.this.clone();
        tokio::spawn(async move {
            let notified = stop_rx.notified();
            tokio::pin!(notified);
            loop {
                select! {
                    _ = &mut notified => {
                        let Some(mesh) = mesh.upgrade() else {
                            break;
                        };
                        mesh.lock().await.remove_link(dst);
                        break;
                    }
                    packet = recv.recv() => {
                        let mut brk = false;
                        if let Err(LinkError::Closed) = &packet {
                            brk = true;
                        }
                        if let Err(err) = packets_tx.send((dst, packet)).await {
                            error!("Failed to send packet to mesh handler: {}", err);
                            break;
                        }
                        if brk {
                            break;
                        }
                    }
                }
            }
        });
        let _ = self.mesh_event_tx.send(MeshEvent::LinkUp(dst));
    }

    pub(crate) fn remove_link(&mut self, dst: NodeId) {
        debug!("Removing link to node {:X}", dst);
        self.routes.retain(|_, &mut v| v != dst);
        self.link_send.remove(&dst);
        let ctrl = self.link_recv_stop.remove(&dst);
        if let Some(ctrl) = ctrl {
            ctrl.notify_waiters();
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
            warn!(
                "Cannot set route to node {:X}: next hop {:X} does not exist",
                dst, next_hop
            );
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
            let _ = self
                .mesh_event_tx
                .send(MeshEvent::RouteRemoved(dst, next_hop));
        }
    }

    pub(crate) fn get_routes(&self) -> HashMap<NodeId, NodeId> {
        self.routes.clone()
    }

    pub(crate) fn subscribe_mesh_events(&self) -> broadcast::Receiver<MeshEvent> {
        self.mesh_event_tx.subscribe()
    }

    async fn handle_packet(
        &mut self,
        from: NodeId,
        packet: Result<Packet, LinkError>,
    ) -> Result<()> {
        let mut packet = match packet {
            Ok(packet) => packet,
            Err(LinkError::Closed) => {
                self.remove_link(from);
                return Ok(());
            }
            Err(LinkError::Unknown(err)) => {
                warn!("Error receiving packet: {}", err);
                return Ok(());
            }
        };
        debug!("Received packet: {:?}", packet);
        if packet.dst == self.id {
            for (filter, dispatch) in self.dispatchees.iter() {
                if filter(&packet) {
                    if let Err(err) = dispatch.send(packet).await {
                        warn!("Failed to dispatch packet to handler: {}", err);
                    }
                    break;
                }
            }
        } else {
            if packet.ttl == 0 {
                debug!("Dropping packet to node {:X}: TTL expired", packet.dst);
                return Ok(());
            }
            packet.ttl -= 1;
            self.send_packet_internal(packet).await?;
        }
        Ok(())
    }

    pub(crate) fn stop(&self) {
        self.stop.notify_waiters();
        for stop in self.link_recv_stop.values() {
            stop.notify_waiters();
        }
    }
}

impl Drop for Mesh {
    fn drop(&mut self) {
        self.stop();
    }
}
