use std::{collections::HashMap, fmt::Debug, sync::Arc};

use futures::future::select_all;
use packet::{NodeId, Packet};
use tokio::{select, sync::{mpsc, oneshot, Mutex}};
use tracing::warn;

use crate::errors::Result;

pub(crate) mod packet;

#[async_trait::async_trait]
pub(crate) trait Link: Debug + Send + Sync {
    async fn send(&self, packet: Packet) -> Result<()>;
    async fn recv(&self) -> Result<Packet>;
}

#[derive(Debug)]
pub(crate) struct Mesh {
    id: NodeId,

    links: Arc<Mutex<HashMap<NodeId, Box<dyn Link>>>>,
    routes: Arc<Mutex<HashMap<NodeId, NodeId>>>,
}

impl Mesh {
    async fn send(&self, packet: Packet) {}
    async fn recv(&self) -> Packet { unimplemented!() }

    async fn add_link(&self, node_id: NodeId, conn: Box<dyn Link>) {
        let mut links = self.links.lock().await;
        links.insert(node_id, conn);
    }

    async fn remove_link(&self, node_id: NodeId) {
        let mut routes = self.routes.lock().await;
        routes.retain(|_, &mut v| v != node_id);
        drop(routes);
        
        let mut links = self.links.lock().await;
        links.remove(&node_id);
        drop(links);
    }

    async fn add_route(&self, dest: NodeId, next_hop: NodeId) {
        let mut routes = self.routes.lock().await;
        routes.insert(dest, next_hop);
    }

    async fn remove_route(&self, dest: NodeId) {
        let mut routes = self.routes.lock().await;
        routes.remove(&dest);
    }

    // forward logic
    async fn handle_links(
        &self,
        mut send_queue: mpsc::Receiver<Packet>,
        queue_to_send: mpsc::Sender<Packet>,
        recv_queue: mpsc::Sender<Packet>,
        mut cont: mpsc::Receiver<()>,
        mut stop: oneshot::Receiver<()>,
    ) {
        let id = self.id;
        let links = self.links.clone();
        let routes = self.routes.clone();
        tokio::spawn(async move {
            loop {
                let links = links.lock().await;
                let futures: Vec<_> = links
                    .values()
                    .map(|conn| conn.recv())
                    .collect();
                if futures.is_empty() {
                    select! {
                        _ = cont.recv() => continue,
                        _ = &mut stop => break,
                        Some(packet) = send_queue.recv() => {
                            warn!("No link available to send packet to {:X}", packet.dst);
                        }
                    }
                }
                select! {
                    _ = cont.recv() => continue,
                    _ = &mut stop => break,
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
                            if let Err(err) = queue_to_send.send(packet).await {
                                warn!("Failed to forward packet to send queue: {}", err);
                            }
                        }
                    }
                    Some(packet) = send_queue.recv() => {
                        let routes = routes.lock().await;
                        let Some(dst) = routes.get(&packet.dst) else {
                            warn!("No route to destination {:X}", packet.dst);
                            continue;
                        };
                        let Some(conn) = links.get(dst) else {
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
}
