use std::{collections::HashMap, fmt::Debug, sync::{Arc, Weak}};

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use futures::future::join_all;
use packet::{NodeId, Packet, Payload};
use tokio::{select, sync::{Mutex, broadcast, mpsc}};
use tracing::{debug, error, warn};

use crate::errors::Error;

pub(crate) mod seq;
pub(crate) mod packet;
pub(crate) mod igp;
pub(crate) mod igp_payload;
pub(crate) mod igp_state;

pub(crate) struct Mesh {
    pub(crate) id: NodeId,

    mesh: Weak<Mutex<Mesh>>,

    link_send: HashMap<NodeId, Box<dyn LinkSend>>,
    link_recv_stop: HashMap<NodeId, mpsc::UnboundedSender<()>>,
    routes: HashMap<NodeId, NodeId>,
    dispatchees: Vec<(Box<dyn Fn(&Packet) -> bool + Send + Sync>, mpsc::Sender<Packet>)>,

    handler_event_tx: mpsc::UnboundedSender<HandlerEvent>,
    mesh_event_tx: broadcast::Sender<MeshEvent>,
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
    RouteAdded(NodeId, NodeId),
    RouteRemoved(NodeId, NodeId),
}

enum HandlerEvent {
    ReceivedPacket {
        from: NodeId,
        packet: Result<Packet, LinkError>,
    },
    Stop,
}

impl Mesh {
    pub(crate) fn new(id: NodeId) -> Arc<Mutex<Self>> {
        let (handler_event_tx, mut handler_event_rx) = mpsc::unbounded_channel();
        let mesh = Arc::new_cyclic(|mesh| Mutex::new(Mesh {
            id,
            mesh: mesh.clone(),
            link_send: HashMap::new(),
            link_recv_stop: HashMap::new(),
            routes: HashMap::new(),
            dispatchees: Vec::new(),
            handler_event_tx: handler_event_tx.clone(),
            mesh_event_tx: broadcast::Sender::new(16),
        }));
        let mesh2 = mesh.clone();
        tokio::spawn(async move {
            loop {
                select! {
                    packet = handler_event_rx.recv() => {
                        let event = match packet {
                            Some(event) => event,
                            None => {
                                warn!("handler event channel closed");
                                break;
                            }
                        };
                        match event {
                            HandlerEvent::Stop => break,
                            HandlerEvent::ReceivedPacket { from, packet } => {
                                let mut mesh = mesh.lock().await;
                                if let Err(err) = mesh.handle_packet(from, packet).await {
                                    warn!("Failed to handle packet from {:X}: {}", from, err);
                                }
                            },
                        }
                    }
                }
            }
            warn!("link handler exiting");
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
        debug!("Sending packet {:?} to {:X} via link {:X}", &packet, packet.dst, next_hop);
        match link.send(packet).await {
            res@Err(LinkError::Closed) => {
                self.remove_link(*next_hop);
                res
            },
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
        }).await
    }

    pub(crate) async fn send_packet_link<P: Payload>(&mut self, dst: NodeId, payload: P) -> Result<()> {
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
        let futures = self.link_send.iter_mut().map(|(dst, link)| async {
            (link.send(Packet {
                src: self.id,
                dst: *dst,
                ttl: 16,
                payload: dyn_clone::clone_box(&*payload),
            }).await, *dst)
        }).collect::<Vec<_>>();
        let results = join_all(futures).await;
        for (result, dst) in &results {
            if let Err(LinkError::Closed) = result {
                self.remove_link(*dst);
            }
        }
        results.into_iter()
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

    pub(crate) fn remove_dispatchee(&mut self, rx: &mut mpsc::Receiver<Packet>) {
        rx.close();
        self.dispatchees.retain(|(_, tx)| !tx.is_closed());
    }

    pub(crate) fn add_link(&mut self, dst: NodeId, send: Box<dyn LinkSend>, mut recv: Box<dyn LinkRecv>) {
        self.link_send.insert(dst, send);
        let (tx, mut rx) = mpsc::unbounded_channel();
        self.link_recv_stop.insert(dst, tx);
        let handler_event_tx = self.handler_event_tx.clone();

        let mesh = self.mesh.upgrade().unwrap();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = rx.recv() => {
                        mesh.lock().await.remove_link(dst);
                        break;
                    }
                    packet = recv.recv() => {
                        let result = handler_event_tx.send(HandlerEvent::ReceivedPacket {
                            from: dst,
                            packet,
                        });
                        if let Err(err) = result {
                            warn!("Failed to send received packet to mesh handler: {}", err);
                        }
                    }
                }
            }
        });
        let _ = self.mesh_event_tx.send(MeshEvent::LinkUp(dst));
    }

    pub(crate) fn remove_link(&mut self, dst: NodeId) {
        debug!("Removing link to {:X}", dst);
        self.routes.retain(|_, &mut v| v != dst);
        self.link_send.remove(&dst);
        let ctrl = self.link_recv_stop.remove(&dst);
        if let Some(ctrl) = ctrl {
            let _ = ctrl.send(());
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
            warn!("Tried to set route to {:X} via non-existent link {:X}", dst, next_hop);
            return;
        }
        debug!("Setting route to {:X} via next hop {:X}", dst, next_hop);
        self.routes.insert(dst, next_hop);
        let _ = self.mesh_event_tx.send(MeshEvent::RouteAdded(dst, next_hop));
    }

    pub(crate) fn remove_route(&mut self, dst: NodeId) {
        debug!("Removing route to {:X}", dst);
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

    async fn handle_packet(&mut self, from: NodeId, packet: Result<Packet, LinkError>) -> Result<()> {
        let mut packet = match packet {
            Ok(packet) => packet,
            Err(LinkError::Closed) => {
                self.remove_link(from);
                return Ok(());
            }
            Err(LinkError::Unknown(err)) => {
                warn!("Failed to receive packet: {}", err);
                return Ok(());
            }
        };
        debug!("Received packet: {:?}", packet);
        if packet.dst == self.id {
            for (filter, dispatch) in self.dispatchees.iter() {
                if filter(&packet) {
                    if let Err(err) = dispatch.send(packet).await {
                        warn!("Failed to dispatch packet: {}", err);
                    }
                    break;
                }
            }
        } else {
            if packet.ttl == 0 {
                warn!("Dropping packet to {:X} due to TTL=0", packet.dst);
                return Ok(());
            }
            packet.ttl -= 1;
            self.send_packet_internal(packet).await?;
        }
        Ok(())
    }

    pub(crate) fn stop(&mut self) -> Result<()> {
        self.handler_event_tx.send(HandlerEvent::Stop)?;
        self.link_recv_stop.iter()
            .map(|(_, tx)| tx.send(()))
            .collect::<Result<Vec<_>, _>>()
            .map(|_| ())?;
        Ok(())
    }
}
