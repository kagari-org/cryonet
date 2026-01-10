use std::{collections::HashMap, fmt::Debug, sync::Arc};

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use futures::future::{join_all, pending, select_all};
use packet::{NodeId, Packet, Payload};
use tokio::{select, sync::{Mutex, broadcast, mpsc, oneshot}};
use tracing::{debug, warn};

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
pub(crate) trait Link: Send + Sync {
    async fn recv(&mut self) -> Result<Packet>;
    async fn send(&mut self, packet: Packet) -> Result<()>;
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
        debug!("Adding link to {:X}", dst);
        self.handler_event_tx.send(HandlerEvent::AddLink(dst, link, reply_tx)).await?;
        reply_rx.await??;
        let _ = self.link_event_tx.send(LinkEvent::Up(dst));
        Ok(())
    }

    pub(crate) async fn remove_link(&self, dst: NodeId) -> Result<()> {
        let mut routes = self.routes.lock().await;
        routes.retain(|_, &mut v| v != dst);
        let (reply_tx, reply_rx) = oneshot::channel();
        debug!("Removing link to {:X}", dst);
        self.handler_event_tx.send(HandlerEvent::RemoveLink(dst, reply_tx)).await?;
        reply_rx.await??;
        let _ = self.link_event_tx.send(LinkEvent::Down(dst));
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
    pub(crate) async fn set_route(&self, dst: NodeId, next_hop: NodeId) {
        if dst == self.id {
            // skip routes to self
            return;
        }
        let Ok(links) = self.get_links().await else {
            warn!("Failed to get links when setting route to {:X} via {:X}", dst, next_hop);
            return;
        };
        let mut routes = self.routes.lock().await;
        if !links.contains(&next_hop) {
            warn!("Tried to set route to {:X} via non-existent link {:X}", dst, next_hop);
            return;
        }
        debug!("Setting route to {:X} via next hop {:X}", dst, next_hop);
        routes.insert(dst, next_hop);
        let _ = self.route_event_tx.send(RouteEvent::Added(dst, next_hop));
    }

    pub(crate) async fn remove_route(&self, dst: NodeId) {
        let mut routes = self.routes.lock().await;
        debug!("Removing route to {:X}", dst);
        let route = routes.remove(&dst);
        if let Some(next_hop) = route {
            let _ = self.route_event_tx.send(RouteEvent::Removed(dst, next_hop));
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
                                // TODO: may remove link on closure
                                warn!("Failed to receive packet: {}", err);
                                continue;
                            }
                        };
                        debug!("Received packet: {:?}", packet);
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
                                let (reply_tx, _reply_rx) = oneshot::channel();
                                // ignore reply_rx to avoid deadlock
                                handler_event_tx.send(HandlerEvent::SendPacket(packet, reply_tx)).await?;
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
                                let Some(link) = links.get_mut(route) else {
                                    let _ = reply_tx.send(Err(anyhow!(Error::NoSuchLink(*route))));
                                    continue;
                                };
                                debug!("Sending packet to {:X} via link {:X}, {:?}", packet.dst, route, packet);
                                let _ = reply_tx.send(link.send(packet).await);
                            }
                            HandlerEvent::SendPacketLink(packet, reply_tx) => {
                                let Some(link) = links.get_mut(&packet.dst) else {
                                    let _ = reply_tx.send(Err(anyhow!(Error::NoSuchLink(packet.dst))));
                                    continue;
                                };
                                debug!("Sending packet to {:X} via direct link, {:?}", packet.dst, packet);
                                let _ = reply_tx.send(link.send(packet).await);
                            }
                            HandlerEvent::BroadcastPacket(payload, reply_tx) => {
                                debug!("Broadcasting packet to all links, {:?}", payload);
                                let futures = links.iter_mut().map(|(dst, link)| link.send(Packet {
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
        mut dispatch_rx: mpsc::Receiver<Packet>,
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
                    packet = dispatch_rx.recv() => {
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

#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Duration};

    use anyhow::{Result, bail};
    use async_trait::async_trait;
    use futures::{SinkExt, StreamExt};
    use serde::{Deserialize, Serialize};
    use tokio::{io::{AsyncRead, AsyncWrite}, net::TcpListener, sync::Mutex, time::sleep};
    use tokio_tungstenite::{WebSocketStream, accept_async, connect_async, tungstenite::Message};
    use tracing::{Level, info};

    use crate::mesh::{Link, Mesh, igp::IGP, packet::{Packet, Payload}};

    struct L<T>(WebSocketStream<T>);
    #[async_trait]
    impl<T: AsyncRead + AsyncWrite + Unpin + Send + Sync> Link for L<T> {
        async fn recv(&mut self) -> Result<Packet> {
            let packet = match self.0.next().await {
                None => bail!("connection closed"),
                Some(packet) => packet?,
            };
            Ok(serde_json::from_slice(&packet.into_data())?)
        }
        async fn send(&mut self, packet: Packet) -> Result<()> {
            let data = serde_json::to_vec(&packet)?;
            self.0.send(Message::binary(data)).await?;
            Ok(())
        }
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    struct P(String);
    #[typetag::serde]
    impl Payload for P {}

    #[tokio::test]
    async fn node0() -> Result<()> {
        tracing_subscriber::fmt::fmt()
            .with_max_level(Level::DEBUG)
            .init();

        let link1 = {
            let listener = TcpListener::bind("0.0.0.0:2333").await?;
            let (stream, _) = listener.accept().await?;
            let ws_stream = accept_async(stream).await?;
            println!("New WebSocket connection: {}", ws_stream.get_ref().peer_addr()?);
            L(ws_stream)
        };

        let mesh = Arc::new(Mutex::new(Mesh::new(0)));
        let igp = IGP::new(mesh.clone()).await;
        mesh.lock().await.add_link(1, Box::new(link1)).await?;

        loop {
            let _ = &igp;
            info!("sending packet");
            if let Err(err) = mesh.lock().await.send_packet(2, P("Hello, World!".to_string())).await {
                dbg!(err);
            }
            sleep(Duration::from_secs(3)).await;
        }
    }

    #[tokio::test]
    async fn node1() -> Result<()> {
        tracing_subscriber::fmt::fmt()
            .with_max_level(Level::DEBUG)
            .init();

        let link0 = {
            let (ws_stream, _) = connect_async("ws://127.0.0.1:2333").await?;
            println!("WebSocket connection established");
            L(ws_stream)
        };
        let link2 = {
            let listener = TcpListener::bind("0.0.0.0:2334").await?;
            let (stream, _) = listener.accept().await?;
            let ws_stream = accept_async(stream).await?;
            println!("New WebSocket connection: {}", ws_stream.get_ref().peer_addr()?);
            L(ws_stream)
        };

        let mesh = Arc::new(Mutex::new(Mesh::new(1)));
        let igp = IGP::new(mesh.clone()).await;
        mesh.lock().await.add_link(0, Box::new(link0)).await?;
        mesh.lock().await.add_link(2, Box::new(link2)).await?;

        loop {
            let _ = &igp;
            sleep(Duration::from_secs(30)).await;
        }
    }

    #[tokio::test]
    async fn node2() -> Result<()> {
        tracing_subscriber::fmt::fmt()
            .with_max_level(Level::DEBUG)
            .init();

        let link1 = {
            let (ws_stream, _) = connect_async("ws://127.0.0.1:2334").await?;
            println!("WebSocket connection established");
            L(ws_stream)
        };

        let mesh = Arc::new(Mutex::new(Mesh::new(2)));
        let igp = IGP::new(mesh.clone()).await;
        mesh.lock().await.add_link(1, Box::new(link1)).await?;

        let mut recv = mesh.lock().await.add_dispatchee(|packet| {
            (packet.payload.as_ref() as &dyn std::any::Any).is::<P>()
        }).await;
        loop {
            let _ = &igp;
            if let Some(packet) = recv.recv().await {
                info!("p: {:?}", packet);
            }
        }
    }
}
