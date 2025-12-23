use std::{collections::HashMap, fmt::Debug, sync::Arc};

use futures::future::select_all;
use packet::{NodeId, Packet};
use tokio::{select, sync::{mpsc, oneshot, Mutex}};
use tracing::warn;

use crate::errors::Result;

pub(crate) mod packet;

#[async_trait::async_trait]
pub(crate) trait Connection: Debug + Send + Sync {
    async fn send(&self, packet: Packet) -> Result<()>;
    async fn recv(&self) -> Result<Packet>;
}

#[derive(Debug)]
pub(crate) struct Mesh {
    id: NodeId,

    connections: Arc<Mutex<HashMap<NodeId, Box<dyn Connection>>>>,
    routes: Arc<Mutex<HashMap<NodeId, NodeId>>>,
}

impl Mesh {
    async fn send(&self, packet: Packet) {}
    async fn recv(&self) -> Packet { unimplemented!() }

    // forward logic
    async fn handle_connections(
        &self,
        mut send_queue: mpsc::Receiver<Packet>,
        queue_to_send: mpsc::Sender<Packet>,
        recv_queue: mpsc::Sender<Packet>,
        mut cont: mpsc::Receiver<()>,
        mut stop: oneshot::Receiver<()>,
    ) {
        let id = self.id;
        let connections = self.connections.clone();
        let routes = self.routes.clone();
        tokio::spawn(async move {
            loop {
                let connections = connections.lock().await;
                let futures: Vec<_> = connections
                    .values()
                    .map(|conn| conn.recv())
                    .collect();
                if futures.is_empty() {
                    select! {
                        _ = cont.recv() => continue,
                        _ = &mut stop => break,
                        Some(packet) = send_queue.recv() => {
                            warn!("No connections available to send packet to {:X}", packet.dst);
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
                        let Some(conn) = connections.get(dst) else {
                            warn!("No connection to next hop {:X}", dst);
                            continue;
                        };
                        if let Err(err) = conn.send(packet).await {
                            warn!("Failed to send packet to {:X}: {}", dst, err);
                        }
                    }
                }
            }
            warn!("connection handler exiting");
        });
    }
}
