use std::{array::from_fn, collections::HashMap, fmt::Debug, sync::Arc};

use anyhow::Result;
use futures::future::select_all;
use packet::{NodeId, Packet};
use tokio::{select, sync::{mpsc, oneshot, Mutex}};
use tracing::warn;

pub(crate) mod packet;

#[async_trait::async_trait]
pub(crate) trait Link: Debug + Send + Sync {
    async fn send(&self, packet: Packet) -> Result<()>;
    async fn recv(&self) -> Result<Packet>;
}

pub(crate) struct Mesh {
    id: NodeId,

    links: Arc<Mutex<HashMap<NodeId, Box<dyn Link>>>>,
    routes: Arc<Mutex<HashMap<NodeId, NodeId>>>,

    send_queue: mpsc::Sender<Packet>,
    dispatchees: Arc<Mutex<Vec<(Box<dyn Fn(&Packet) -> bool + Send + Sync>, mpsc::Sender<Packet>)>>>,

    stop: Option<[oneshot::Sender<()>; 2]>,
    cont: mpsc::Sender<()>,
}

impl Mesh {
    pub(crate) fn new(id: NodeId) -> Self {
        let stop_channels: [_; 2] = from_fn(|_| oneshot::channel());
        let (stop_tx, mut stop_rx): (Vec<_>, Vec<_>) = stop_channels.into_iter().unzip();
        let (cont_tx, cont_rx) = mpsc::channel(8);

        let (send_queue_tx, send_queue_rx) = mpsc::channel(64);
        let (recv_queue_tx, recv_queue_rx) = mpsc::channel(64);

        let mesh = Mesh {
            id,
            links: Arc::new(Mutex::new(HashMap::new())),
            routes: Arc::new(Mutex::new(HashMap::new())),
            send_queue: send_queue_tx.clone(),
            dispatchees: Arc::new(Mutex::new(Vec::new())),
            stop: Some(stop_tx.try_into().unwrap()),
            cont: cont_tx,
        };
        mesh.handle_links(
            send_queue_rx,
            send_queue_tx,
            recv_queue_tx,
            cont_rx,
            stop_rx.pop().unwrap(),
        );
        mesh.dispatch(
            recv_queue_rx,
            stop_rx.pop().unwrap(),
        );

        mesh
    }

    pub(crate) fn stop(&mut self) {
        if let Some([tx_links, tx_dispatch]) = self.stop.take() {
            let _ = tx_links.send(());
            let _ = tx_dispatch.send(());
        }
    }

    pub(crate) async fn send_packet(&self, packet: Packet) -> Result<()> {
        self.send_queue.send(packet).await?;
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

    pub(crate) async fn add_link(&self, node_id: NodeId, conn: Box<dyn Link>) -> Result<()> {
        let mut links = self.links.lock().await;
        links.insert(node_id, conn);
        self.cont.send(()).await?;
        Ok(())
    }

    pub(crate) async fn remove_link(&self, node_id: NodeId) -> Result<()> {
        let mut routes = self.routes.lock().await;
        routes.retain(|_, &mut v| v != node_id);
        drop(routes);
        
        let mut links = self.links.lock().await;
        links.remove(&node_id);
        drop(links);

        self.cont.send(()).await?;
        Ok(())
    }

    pub(crate) async fn add_route(&self, dest: NodeId, next_hop: NodeId) {
        let mut routes = self.routes.lock().await;
        routes.insert(dest, next_hop);
    }

    pub(crate) async fn remove_route(&self, dest: NodeId) {
        let mut routes = self.routes.lock().await;
        routes.remove(&dest);
    }

    // forward logic
    fn handle_links(
        &self,
        mut send_queue: mpsc::Receiver<Packet>,
        send_queue_tx: mpsc::Sender<Packet>,
        recv_queue: mpsc::Sender<Packet>,
        // when adding or removing links, we need to notify the handler to refresh the link list
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
                            if let Err(err) = send_queue_tx.send(packet).await {
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

    fn dispatch(&self,
        mut recv_queue: mpsc::Receiver<Packet>,
        mut stop: oneshot::Receiver<()>,
    ) {
        let dispatchees = self.dispatchees.clone();
        tokio::spawn(async move {
            loop {
                select! {
                    _ = &mut stop => break,
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
