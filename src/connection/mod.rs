use std::{collections::HashMap, net::SocketAddr, sync::Arc, time::Duration};

use anyhow::Result;
use tokio::{net::{TcpListener, TcpStream}, select, sync::{Mutex, mpsc}, time::interval};
use tokio_tungstenite::{accept_hdr_async, connect_async, tungstenite::{client::IntoClientRequest, handshake::server::{Request, Response}}};
use tracing::{info, warn};

use crate::{connection::link::new_ws_link, errors::Error, mesh::{Mesh, packet::NodeId}};

pub(crate) mod link;

pub(crate) struct ConnManager {
    mesh: Arc<Mutex<Mesh>>,
    token: Option<String>,
    servers: HashMap<String, NodeId>,
    stop: mpsc::UnboundedSender<()>,
}

impl ConnManager {
    pub(crate) fn new(
        mesh: Arc<Mutex<Mesh>>,
        token: Option<String>,
        servers: Vec<String>,
        listen: SocketAddr,
    ) -> Arc<Mutex<Self>> {
        Self::new_with_parameters(
            mesh,
            token,
            servers,
            listen,
            Duration::from_secs(8),
        )
    }

    pub(crate) fn new_with_parameters(
        mesh: Arc<Mutex<Mesh>>,
        token: Option<String>,
        servers: Vec<String>,
        listen: SocketAddr,
        connect_interval: Duration,
    ) -> Arc<Mutex<Self>> {
        let (stop_tx, mut stop_rx) = mpsc::unbounded_channel();
        let mgr = Arc::new(Mutex::new(ConnManager {
            mesh: mesh.clone(),
            token,
            servers: HashMap::new(),
            stop: stop_tx,
        }));
        let mgr2 = mgr.clone();
        tokio::spawn(async move {
            let listener = TcpListener::bind(listen).await.unwrap();
            let mut connect_timer = interval(connect_interval);
            loop {
                select! {
                    _ = stop_rx.recv() => break,
                    _ = connect_timer.tick() => {
                        let mut mgr = mgr.lock().await;
                        for server in &servers {
                            if let Err(err) = mgr.connect(server.clone()).await {
                                warn!("Failed to connect to server {}: {}", server, err);
                            }
                        }
                    }
                    Ok((stream, addr)) = listener.accept() => {
                        let mut mgr = mgr.lock().await;
                        if let Err(err) = mgr.accept((stream, addr)).await {
                            warn!("Failed to accept connection from {}: {}", addr, err);
                        }
                    }
                }
            }
        });
        mgr2
    }

    async fn accept(&mut self, (stream, addr): (TcpStream, SocketAddr)) -> Result<()> {
        let mut mesh = self.mesh.lock().await;
        let mut neigh_id = None;
        let Ok(ws) = accept_hdr_async(stream, |req: &Request, mut res: Response| {
            let reject = |reason: String| Err(Response::builder()
                .status(400)
                .body(Some(reason))
                .unwrap());
            if let Some(token) = &self.token {
                let Some(auth) = req.headers().get("Authorization") else {
                    return reject("Missing Authorization header".to_string());
                };
                match auth.to_str() {
                    Ok(s) if s == token => {},
                    _ => return reject("Invalid Authorization token".to_string()),
                };
            }
            let peer_id = match req.headers()
                .get("X-NodeId")
                .map(|id| id.to_str().map(|id| id.parse::<NodeId>()))
            {
                Some(Ok(Ok(id))) => id,
                _ => return reject("Invalid X-Node-Id header".to_string()),
            };
            if mesh.get_links().contains(&peer_id) {
                return reject("Link to this node already exists".to_string());
            }
            neigh_id = Some(peer_id);
            res.headers_mut().insert("X-NodeId", mesh.id.to_string().parse().unwrap());
            Ok(res)
        }).await else {
            info!("Failed to accept WebSocket connection from {}", addr);
            return Ok(());
        };
        let (sink, stream) = new_ws_link(ws);
        mesh.add_link(neigh_id.unwrap(), Box::new(sink), Box::new(stream));
        info!("Accepted connection from {}", addr);
        Ok(())
    }

    pub(crate) async fn connect(&mut self, server: String) -> Result<()> {
        // lock here to avoid interleaving with incoming connections
        let mut mesh = self.mesh.lock().await;
        if let Some(neigh_id) = self.servers.get(&server) {
            if mesh.get_links().contains(neigh_id) {
                return Ok(());
            }
        }

        let mut req = server.clone().into_client_request()?;
        req.headers_mut().insert("X-NodeId", mesh.id.to_string().parse()?);
        if let Some(token) = &self.token {
            req.headers_mut().insert("Authorization", token.parse()?);
        }
        let (mut ws_stream, res) = connect_async(req).await?;
        let Some(neigh_id) = res.headers().get("X-NodeId") else {
            return Err(anyhow::anyhow!(Error::Unauthorized));
        };
        let neigh_id = neigh_id.to_str()?.parse::<NodeId>()?;
        self.servers.insert(server.clone(), neigh_id);

        if mesh.get_links().contains(&neigh_id) {
            ws_stream.close(None).await?;
            return Ok(());
        }
        
        let (sink, stream) = new_ws_link(ws_stream);
        mesh.add_link(neigh_id, Box::new(sink), Box::new(stream));
        info!("Connected to server {}", server);
        Ok(())
    }

    pub(crate) async fn disconnect(&mut self, id: NodeId) {
        self.mesh.lock().await.remove_link(id);
    }

    pub(crate) fn stop(&self) -> Result<()> {
        self.stop.send(())?;
        Ok(())
    }
}
