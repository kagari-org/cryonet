use std::{collections::HashMap, fmt::Debug, pin::Pin, sync::Arc};

use anyhow::{bail, Result};
use futures::future::select_all;
use packet::{NodeId, Packet, Payload};
use tokio::{select, sync::{broadcast, mpsc, Mutex}};
use tracing::warn;

use crate::errors::Error;

pub(crate) mod seq;
pub(crate) mod packet;
pub(crate) mod igp;
pub(crate) mod igp_payload;
pub(crate) mod igp_state;

pub(crate) trait Link: Debug + Send + Sync {
    fn send(&self, packet: Packet) -> Pin<Box<dyn Future<Output = Result<()>> + Send>>;
    fn recv(&self) -> Pin<Box<dyn Future<Output = Result<Packet>> + Send>>;
}

type Links = HashMap<NodeId, Box<dyn Link>>;
type Routes = HashMap<NodeId, NodeId>;
pub(crate) struct Mesh {
    id: NodeId,

    links_routes: Arc<Mutex<(Links, Routes)>>,
    link_event_tx: broadcast::Sender<LinkEvent>,
    route_event_tx: broadcast::Sender<RouteEvent>,

    send_queue: mpsc::Sender<Packet>,
    dispatchees: Arc<Mutex<Vec<(Box<dyn Fn(&Packet) -> bool + Send + Sync>, mpsc::Sender<Packet>)>>>,

    stop: broadcast::Sender<()>,
    cont: mpsc::Sender<()>,
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

impl Mesh {
    pub(crate) fn new(id: NodeId) -> Self {
        let stop_tx = broadcast::Sender::new(1);
        let stop_rx1 = stop_tx.subscribe();
        let stop_rx2 = stop_tx.subscribe();
        let (cont_tx, cont_rx) = mpsc::channel(8);

        let (send_queue_tx, send_queue_rx) = mpsc::channel(64);
        let (recv_queue_tx, recv_queue_rx) = mpsc::channel(64);

        let mesh = Mesh {
            id,
            links_routes: Arc::new(Mutex::new((HashMap::new(), HashMap::new()))),
            link_event_tx: broadcast::Sender::new(16),
            route_event_tx: broadcast::Sender::new(16),
            send_queue: send_queue_tx.clone(),
            dispatchees: Arc::new(Mutex::new(Vec::new())),
            stop: stop_tx,
            cont: cont_tx,
        };
        mesh.handle_links(
            send_queue_rx,
            send_queue_tx,
            recv_queue_tx,
            cont_rx,
            stop_rx1,
        );
        mesh.dispatch(
            recv_queue_rx,
            stop_rx2,
        );

        mesh
    }

    pub(crate) fn stop(&mut self) {
        if let Err(err) = self.stop.send(()) {
            warn!("Failed to send stop signal to mesh: {}", err);
        }
    }

    pub(crate) async fn send_packet<P: Payload>(&self, dst: NodeId, payload: P) -> Result<()> {
        self.send_queue.send(Packet {
            src: self.id,
            dst,
            ttl: 16,
            payload: Box::new(payload),
        }).await?;
        Ok(())
    }

    pub(crate) async fn send_packet_link<P: Payload>(&self, dst: NodeId, payload: P) -> Result<()> {
        let links_routes = self.links_routes.lock().await;
        let Some(link) = links_routes.0.get(&dst) else {
            bail!(Error::NoSuchLink(dst));
        };
        link.send(Packet {
            src: self.id,
            dst,
            ttl: 16,
            payload: Box::new(payload),
        }).await?;
        Ok(())
    }

    pub(crate) async fn broadcast_packet_local<P: Payload>(&self, payload: P) -> Result<()> {
        let links_routes = self.links_routes.lock().await;
        for (dst, link) in links_routes.0.iter() {
            link.send(Packet {
                src: self.id,
                dst: *dst,
                ttl: 16,
                payload: dyn_clone::clone_box(&payload),
            }).await?;
        }
        Ok(())
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
    pub(crate) async fn add_link(&self, dst: NodeId, conn: Box<dyn Link>) -> Result<()> {
        let mut links_routes = self.links_routes.lock().await;
        links_routes.0.insert(dst, conn);
        self.cont.send(()).await?;
        self.link_event_tx.send(LinkEvent::Up(dst))?;
        Ok(())
    }

    pub(crate) async fn remove_link(&self, dst: NodeId) -> Result<()> {
        let mut links_routes = self.links_routes.lock().await;
        links_routes.0.remove(&dst);
        links_routes.1.retain(|_, &mut v| v != dst);
        self.cont.send(()).await?;
        self.link_event_tx.send(LinkEvent::Down(dst))?;
        Ok(())
    }

    pub(crate) async fn get_links(&self) -> Vec<NodeId> {
        let links_routes = self.links_routes.lock().await;
        links_routes.0.keys().cloned().collect()
    }

    pub(crate) async fn subscribe_link_events(&self) -> broadcast::Receiver<LinkEvent> {
        self.link_event_tx.subscribe()
    }

    // route api
    pub(crate) async fn set_route(&self, dest: NodeId, next_hop: NodeId) {
        let mut links_routes = self.links_routes.lock().await;
        if !links_routes.0.contains_key(&next_hop) {
            warn!("Tried to set route to {:X} via non-existent link {:X}", dest, next_hop);
            return;
        }
        links_routes.1.insert(dest, next_hop);
        let result = self.route_event_tx.send(RouteEvent::Added(dest, next_hop));
        if let Err(err) = result {
            warn!("Failed to send route added event: {}", err);
        }
    }

    pub(crate) async fn remove_route(&self, dest: NodeId) {
        let mut links_routes = self.links_routes.lock().await;
        let route = links_routes.1.remove(&dest);
        if let Some(next_hop) = route {
            let result = self.route_event_tx.send(RouteEvent::Removed(dest, next_hop));
            if let Err(err) = result {
                warn!("Failed to send route removed event: {}", err);
            }
        }
    }

    pub(crate) async fn get_routes(&self) -> HashMap<NodeId, NodeId> {
        let links_routes = self.links_routes.lock().await;
        links_routes.1.clone()
    }

    pub(crate) async fn subscribe_route_events(&self) -> broadcast::Receiver<RouteEvent> {
        self.route_event_tx.subscribe()
    }

    // forward logic
    fn handle_links(
        &self,
        mut send_queue: mpsc::Receiver<Packet>,
        send_queue_tx: mpsc::Sender<Packet>,
        recv_queue: mpsc::Sender<Packet>,
        // when adding or removing links, we need to notify the handler to refresh the link list
        mut cont: mpsc::Receiver<()>,
        mut stop: broadcast::Receiver<()>,
    ) {
        let id = self.id;
        let links_routes = self.links_routes.clone();
        tokio::spawn(async move {
            loop {
                let lr = links_routes.lock().await;
                let futures: Vec<_> = lr.0
                    .values()
                    .map(|conn| conn.recv())
                    .collect();
                drop(lr);
                if futures.is_empty() {
                    select! {
                        _ = cont.recv() => continue,
                        _ = stop.recv() => break,
                        Some(packet) = send_queue.recv() => {
                            warn!("No link available to send packet to {:X}", packet.dst);
                        }
                    }
                }
                select! {
                    _ = cont.recv() => continue,
                    _ = stop.recv() => break,
                    (packet, _, _) = select_all(futures) => {
                        let mut packet = match packet {
                            Ok(packet) => packet,
                            Err(err) => {
                                warn!("Failed to receive packet: {}", err);
                                continue;
                            }
                        };
                        if packet.dst == id {
                            if let Err(err) = recv_queue.send(packet).await {
                                warn!("Failed to forward packet to recv queue: {}", err);
                            }
                        } else {
                            if packet.ttl == 0 {
                                warn!("Dropping packet to {:X} due to TTL=0", packet.dst);
                                continue;
                            }
                            packet.ttl -= 1;
                            if let Err(err) = send_queue_tx.send(packet).await {
                                warn!("Failed to forward packet to send queue: {}", err);
                            }
                        }
                    }
                    Some(packet) = send_queue.recv() => {
                        let links_routes = links_routes.lock().await;
                        let Some(dst) = links_routes.1.get(&packet.dst) else {
                            warn!("No route to destination {:X}", packet.dst);
                            continue;
                        };
                        let Some(conn) = links_routes.0.get(dst) else {
                            warn!("No link to next hop {:X}", dst);
                            continue;
                        };
                        if let Err(err) = conn.send(packet).await {
                            warn!("Failed to send packet to {:X}: {}", dst, err);
                        }
                    }
                }
            }
            warn!("link handler exiting");
        });
    }

    fn dispatch(&self,
        mut recv_queue: mpsc::Receiver<Packet>,
        mut stop: broadcast::Receiver<()>,
    ) {
        let dispatchees = self.dispatchees.clone();
        tokio::spawn(async move {
            loop {
                select! {
                    _ = stop.recv() => break,
                    Some(packet) = recv_queue.recv() => {
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
        self.stop();
    }
}
