use std::{collections::HashMap, fmt::Debug, sync::Arc};

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use futures::future::{join_all, pending, select_all};
use packet::{NodeId, Packet, Payload};
use tokio::{select, sync::{Mutex, broadcast, mpsc, oneshot}};
use tracing::warn;

use crate::errors::Error;

pub(crate) mod seq;
pub(crate) mod packet;
pub(crate) mod igp;
pub(crate) mod igp_payload;
pub(crate) mod igp_state;

pub(crate) struct Mesh {
    id: NodeId,

    routes: Arc<Mutex<HashMap<NodeId, NodeId>>>,

    handler_event_tx: mpsc::Sender<HandlerEvent>,
    dispatcher_event_tx: mpsc::Sender<DispatcherEvent>,

    dispatchees: Arc<Mutex<Vec<(Box<dyn Fn(&Packet) -> bool + Send + Sync>, mpsc::Sender<Packet>)>>>,

    link_event_tx: broadcast::Sender<LinkEvent>,
    route_event_tx: broadcast::Sender<RouteEvent>,
}

#[async_trait]
pub(crate) trait Link: Debug + Send + Sync {
    async fn recv(&mut self) -> Result<Packet>;
    async fn send(&self, packet: Packet) -> Result<()>;
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum LinkEvent {
    Up(NodeId),
    Down(NodeId),
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum RouteEvent {
    Added(NodeId, NodeId),
    Removed(NodeId, NodeId),
}

enum HandlerEvent {
    AddLink(NodeId, Box<dyn Link>, oneshot::Sender<Result<()>>),
    RemoveLink(NodeId, oneshot::Sender<Result<()>>),
    ListLinks(oneshot::Sender<Result<Vec<NodeId>>>),
    SendPacket(Packet, oneshot::Sender<Result<()>>),
    SendPacketLink(Packet, oneshot::Sender<Result<()>>),
    BroadcastPacket(Box<dyn Payload>, oneshot::Sender<Result<()>>),
    Stop,
}

enum DispatcherEvent {
    Stop,
}

impl Mesh {
    pub(crate) fn new(id: NodeId) -> Self {
        let (handler_event_tx, handler_event_rx) = mpsc::channel(64);
        let (dispatcher_event_tx, dispatcher_event_rx) = mpsc::channel(8);

        let (dispatch_tx, dispatch_rx) = mpsc::channel(64);

        let mesh = Mesh {
            id,
            routes: Arc::new(Mutex::new(HashMap::new())),
            handler_event_tx: handler_event_tx.clone(),
            dispatcher_event_tx,
            dispatchees: Arc::new(Mutex::new(Vec::new())),
            link_event_tx: broadcast::Sender::new(16),
            route_event_tx: broadcast::Sender::new(16),
        };
        mesh.handle_links(
            dispatch_tx,
            handler_event_tx,
            handler_event_rx,
        );
        mesh.dispatch(
            dispatch_rx,
            dispatcher_event_rx,
        );

        mesh
    }

    pub(crate) async fn send_packet<P: Payload>(&self, dst: NodeId, payload: P) -> Result<()> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.handler_event_tx.send(HandlerEvent::SendPacket(Packet {
            src: self.id,
            dst,
            ttl: 16,
            payload: Box::new(payload),
        }, reply_tx)).await?;
        reply_rx.await?
    }

    pub(crate) async fn send_packet_link<P: Payload>(&self, dst: NodeId, payload: P) -> Result<()> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.handler_event_tx.send(HandlerEvent::SendPacketLink(Packet {
            src: self.id,
            dst,
            ttl: 16,
            payload: Box::new(payload),
        }, reply_tx)).await?;
        reply_rx.await?
    }

    pub(crate) async fn broadcast_packet_local<P: Payload>(&self, payload: P) -> Result<()> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.handler_event_tx.send(HandlerEvent::BroadcastPacket(Box::new(payload), reply_tx)).await?;
        reply_rx.await?
    }

    pub(crate) async fn add_dispatchee<F>(&self, filter: F) -> mpsc::Receiver<Packet>
    where
        F: Fn(&Packet) -> bool + Send + Sync + 'static,
    {
        let (tx, rx) = mpsc::channel(64);
        let mut dispatchees = self.dispatchees.lock().await;
        dispatchees.push((Box::new(filter), tx));
        rx
    }

    pub(crate) async fn remove_dispatchee(&self, rx: &mut mpsc::Receiver<Packet>) {
        let mut dispatchees = self.dispatchees.lock().await;
        rx.close();
        dispatchees.retain(|(_, tx)| !tx.is_closed());
    }

    // link api
    pub(crate) async fn add_link(&self, dst: NodeId, link: Box<dyn Link>) -> Result<()> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.handler_event_tx.send(HandlerEvent::AddLink(dst, link, reply_tx)).await?;
        reply_rx.await??;
        self.link_event_tx.send(LinkEvent::Up(dst))?;
        Ok(())
    }

    pub(crate) async fn remove_link(&self, dst: NodeId) -> Result<()> {
        let mut routes = self.routes.lock().await;
        routes.retain(|_, &mut v| v != dst);
        let (reply_tx, reply_rx) = oneshot::channel();
        self.handler_event_tx.send(HandlerEvent::RemoveLink(dst, reply_tx)).await?;
        reply_rx.await??;
        self.link_event_tx.send(LinkEvent::Down(dst))?;
        Ok(())
    }

    pub(crate) async fn get_links(&self) -> Result<Vec<NodeId>> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.handler_event_tx.send(HandlerEvent::ListLinks(reply_tx)).await?;
        reply_rx.await?
    }

    pub(crate) async fn subscribe_link_events(&self) -> broadcast::Receiver<LinkEvent> {
        self.link_event_tx.subscribe()
    }

    // route api
    pub(crate) async fn set_route(&self, dest: NodeId, next_hop: NodeId) {
        let mut routes = self.routes.lock().await;
        if !routes.contains_key(&next_hop) {
            warn!("Tried to set route to {:X} via non-existent link {:X}", dest, next_hop);
            return;
        }
        routes.insert(dest, next_hop);
        let result = self.route_event_tx.send(RouteEvent::Added(dest, next_hop));
        if let Err(err) = result {
            warn!("Failed to send route added event: {}", err);
        }
    }

    pub(crate) async fn remove_route(&self, dest: NodeId) {
        let mut routes = self.routes.lock().await;
        let route = routes.remove(&dest);
        if let Some(next_hop) = route {
            let result = self.route_event_tx.send(RouteEvent::Removed(dest, next_hop));
            if let Err(err) = result {
                warn!("Failed to send route removed event: {}", err);
            }
        }
    }

    pub(crate) async fn get_routes(&self) -> HashMap<NodeId, NodeId> {
        let routes = self.routes.lock().await;
        routes.clone()
    }

    pub(crate) async fn subscribe_route_events(&self) -> broadcast::Receiver<RouteEvent> {
        self.route_event_tx.subscribe()
    }

    // forward logic
    fn handle_links(
        &self,
        dispatch: mpsc::Sender<Packet>,
        handler_event_tx: mpsc::Sender<HandlerEvent>,
        mut handler_event_rx: mpsc::Receiver<HandlerEvent>,
    ) {
        let id = self.id;
        let routes = self.routes.clone();
        tokio::spawn(async move {
            let mut links: HashMap<NodeId, Box<dyn Link>> = HashMap::new();
            loop {
                select! {
                    packet = async {
                        let mut futures: Vec<_> = links.values_mut().map(|link| link.recv()).collect();
                        futures.push(Box::pin(pending())); // to avoid empty select_all
                        let (packet, _, _) = select_all(futures).await;
                        packet
                     } => {
                        let mut packet = match packet {
                            Ok(packet) => packet,
                            Err(err) => {
                                warn!("Failed to receive packet: {}", err);
                                continue;
                            }
                        };
                        if packet.dst == id {
                            if let Err(err) = dispatch.send(packet).await {
                                warn!("Failed to forward packet to recv queue: {}", err);
                            }
                        } else {
                            if packet.ttl == 0 {
                                warn!("Dropping packet to {:X} due to TTL=0", packet.dst);
                                continue;
                            }
                            packet.ttl -= 1;
                            let result: Result<()> = try {
                                let (reply_tx, reply_rx) = oneshot::channel();
                                handler_event_tx.send(HandlerEvent::SendPacket(packet, reply_tx)).await?;
                                reply_rx.await??;
                            };
                            if let Err(err) = result {
                                warn!("Failed to forward packet to {}", err);
                            }
                        }
                    }
                    packet = handler_event_rx.recv() => {
                        let event = match packet {
                            Some(event) => event,
                            None => {
                                warn!("handler event channel closed");
                                break;
                            }
                        };
                        match event {
                            HandlerEvent::AddLink(dst, link, reply_tx) => {
                                links.insert(dst, link);
                                let _ = reply_tx.send(Ok(()));
                            }
                            HandlerEvent::RemoveLink(dst, reply_tx) => {
                                links.remove(&dst);
                                let _ = reply_tx.send(Ok(()));
                            }
                            HandlerEvent::ListLinks(reply_tx) => {
                                let link_list: Vec<NodeId> = links.keys().cloned().collect();
                                let _ = reply_tx.send(Ok(link_list));
                            }
                            HandlerEvent::SendPacket(packet, reply_tx) => {
                                let routes = routes.lock().await;
                                let Some(route) = routes.get(&packet.dst) else {
                                    let _ = reply_tx.send(Err(anyhow!(Error::Unreachable(packet.dst))));
                                    continue;
                                };
                                let Some(link) = links.get(route) else {
                                    let _ = reply_tx.send(Err(anyhow!(Error::NoSuchLink(*route))));
                                    continue;
                                };
                                let _ = reply_tx.send(link.send(packet).await);
                            }
                            HandlerEvent::SendPacketLink(packet, reply_tx) => {
                                let Some(link) = links.get(&packet.dst) else {
                                    let _ = reply_tx.send(Err(anyhow!(Error::NoSuchLink(packet.dst))));
                                    continue;
                                };
                                let _ = reply_tx.send(link.send(packet).await);
                            }
                            HandlerEvent::BroadcastPacket(payload, reply_tx) => {
                                let futures = links.iter().map(|(dst, link)| link.send(Packet {
                                    src: id,
                                    dst: *dst,
                                    ttl: 16,
                                    payload: dyn_clone::clone_box(&*payload),
                                })).collect::<Vec<_>>();
                                let results = join_all(futures).await
                                    .into_iter()
                                    .collect::<Result<Vec<_>, _>>()
                                    .map(|_| ());
                                let _ = reply_tx.send(results);
                            }
                            HandlerEvent::Stop => {
                                break;
                            }
                        }
                    }
                }
            }
            warn!("link handler exiting");
        });
    }

    fn dispatch(&self,
        mut recv_queue: mpsc::Receiver<Packet>,
        mut dispatch_event_rx: mpsc::Receiver<DispatcherEvent>,
    ) {
        let dispatchees = self.dispatchees.clone();
        tokio::spawn(async move {
            loop {
                select! {
                    evt = dispatch_event_rx.recv() => {
                        match evt {
                            None => {
                                warn!("Dispatcher event channel closed");
                                break;
                            }
                            Some(DispatcherEvent::Stop) => {
                                break;
                            }
                        }
                    },
                    packet = recv_queue.recv() => {
                        let packet = match packet {
                            Some(packet) => packet,
                            None => {
                                warn!("Dispatcher recv channel closed");
                                break;
                            }
                        };
                        let dispatchees = dispatchees.lock().await;
                        for (filter, dispatch) in dispatchees.iter() {
                            if filter(&packet) {
                                if let Err(err) = dispatch.send(packet).await {
                                    warn!("Failed to dispatch packet: {}", err);
                                }
                                break;
                            }
                        }
                    }
                }
            }
        });
    }
}

impl Drop for Mesh {
    fn drop(&mut self) {
        let result = self.handler_event_tx.try_send(HandlerEvent::Stop);
        if let Err(err) = result {
            warn!("Failed to send stop signal to mesh handler: {}", err);
        }
        let result = self.dispatcher_event_tx.try_send(DispatcherEvent::Stop);
        if let Err(err) = result {
            warn!("Failed to send stop signal to mesh dispatcher: {}", err);
        }
    }
}
