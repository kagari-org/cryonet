use std::{collections::{HashMap, HashSet}, net::SocketAddr, sync::Arc, time::Duration};

use anyhow::Result;
use tokio::{
    net::{TcpListener, TcpStream},
    select,
    sync::{Mutex, Notify},
    time::{interval, timeout},
};
use tokio_tungstenite::{
    accept_hdr_async, connect_async,
    tungstenite::{
        client::IntoClientRequest,
        handshake::server::{Request, Response},
    },
};
use tracing::{debug, info, warn};

use crate::{
    connection::link::new_ws_link,
    errors::Error,
    mesh::{Mesh, packet::NodeId},
};

pub(crate) mod link;

pub(crate) struct ConnManager {
    mesh: Arc<Mutex<Mesh>>,
    stop: Arc<Notify>,
}

impl ConnManager {
    pub(crate) fn new(
        mesh: Arc<Mutex<Mesh>>,
        token: Option<String>,
        servers: Vec<String>,
        listen: SocketAddr,
    ) -> Arc<Mutex<Self>> {
        Self::new_with_parameters(mesh, token, servers, listen, Duration::from_secs(8))
    }

    pub(crate) fn new_with_parameters(
        mesh: Arc<Mutex<Mesh>>,
        token: Option<String>,
        servers: Vec<String>,
        listen: SocketAddr,
        connect_interval: Duration,
    ) -> Arc<Mutex<Self>> {
        let stop = Arc::new(Notify::new());
        let stop_rx = stop.clone();
        let servers_map = Arc::new(Mutex::new(HashMap::<String, NodeId>::new()));
        let connecting = Arc::new(Mutex::new(HashSet::<String>::new()));
        let mgr = Arc::new(Mutex::new(ConnManager {
            mesh: mesh.clone(),
            stop,
        }));
        let mesh_for_accept = mesh.clone();
        let mesh_for_connect = mesh.clone();
        let token_for_accept = token.clone();
        let token_for_connect = token;
        tokio::spawn(async move {
            let listener = TcpListener::bind(listen).await.unwrap();
            info!("Listening on {}", listen);
            let mut connect_timer = interval(connect_interval);
            let notified = stop_rx.notified();
            tokio::pin!(notified);
            loop {
                select! {
                    _ = &mut notified => break,
                    _ = connect_timer.tick() => {
                        for server in &servers {
                            if server.is_empty() {
                                continue;
                            }
                            {
                                let mut connecting = connecting.lock().await;
                                if connecting.contains(server) {
                                    continue;
                                }
                                connecting.insert(server.clone());
                            }
                            let mesh = mesh_for_connect.clone();
                            let token = token_for_connect.clone();
                            let servers_map = servers_map.clone();
                            let connecting = connecting.clone();
                            let server = server.clone();
                            tokio::spawn(async move {
                                if let Err(err) = Self::connect(
                                    mesh, token, servers_map, server.clone(),
                                ).await {
                                    warn!("Failed to connect to server {}: {}", server, err);
                                }
                                connecting.lock().await.remove(&server);
                            });
                        }
                    }
                    Ok((stream, addr)) = listener.accept() => {
                        let mesh = mesh_for_accept.clone();
                        let token = token_for_accept.clone();
                        tokio::spawn(async move {
                            if let Err(err) = Self::accept(mesh, token, stream, addr).await {
                                warn!("Failed to accept connection from {}: {}", addr, err);
                            }
                        });
                    }
                }
            }
        });
        mgr
    }

    async fn accept(
        mesh: Arc<Mutex<Mesh>>,
        token: Option<String>,
        stream: TcpStream,
        addr: SocketAddr,
    ) -> Result<()> {
        let id = mesh.lock().await.id;
        let mut neigh_id = None;
        let result = accept_hdr_async(stream, |req: &Request, mut res: Response| {
            let reject =
                |reason: String| Err(Response::builder().status(400).body(Some(reason)).unwrap());
            if let Some(token) = &token {
                let Some(auth) = req.headers().get("Authorization") else {
                    return reject("Missing Authorization header".to_string());
                };
                match auth.to_str() {
                    Ok(s) if s == token => {}
                    _ => return reject("Invalid Authorization token".to_string()),
                };
            }
            let peer_id = match req
                .headers()
                .get("X-NodeId")
                .map(|id| id.to_str().map(|id| id.parse::<NodeId>()))
            {
                Some(Ok(Ok(id))) => id,
                _ => return reject("Invalid X-Node-Id header".to_string()),
            };
            neigh_id = Some(peer_id);
            res.headers_mut()
                .insert("X-NodeId", id.to_string().parse().unwrap());
            Ok(res)
        })
        .await;
        let ws = match result {
            Ok(ws) => ws,
            Err(e) => {
                warn!("WebSocket handshake failed from {}: {}", addr, e);
                return Ok(());
            }
        };
        let neigh_id = neigh_id.unwrap();
        let (sink, stream) = new_ws_link(ws);
        let added = mesh
            .lock()
            .await
            .add_link(neigh_id, Box::new(sink), Box::new(stream));
        if added {
            info!("Accepted connection from {} (node {:X})", addr, neigh_id);
        } else {
            info!(
                "Rejected duplicate connection from {} (node {:X}), keeping existing link",
                addr, neigh_id
            );
        }
        Ok(())
    }

    async fn connect(
        mesh: Arc<Mutex<Mesh>>,
        token: Option<String>,
        servers: Arc<Mutex<HashMap<String, NodeId>>>,
        server: String,
    ) -> Result<()> {
        let (links, id) = {
            let mesh = mesh.lock().await;
            (mesh.get_links(), mesh.id)
        };
        {
            let servers = servers.lock().await;
            if let Some(neigh_id) = servers.get(&server)
                && links.contains(neigh_id)
            {
                return Ok(());
            }
        }

        debug!("Connecting to server {} ...", server);
        let mut req = server.clone().into_client_request()?;
        req.headers_mut()
            .insert("X-NodeId", id.to_string().parse()?);
        if let Some(token) = &token {
            req.headers_mut().insert("Authorization", token.parse()?);
        }
        let (ws_stream, res) = timeout(Duration::from_secs(5), connect_async(req)).await??;
        let Some(neigh_id) = res.headers().get("X-NodeId") else {
            return Err(anyhow::anyhow!(Error::Unauthorized));
        };
        let neigh_id = neigh_id.to_str()?.parse::<NodeId>()?;
        servers.lock().await.insert(server.clone(), neigh_id);

        let (sink, stream) = new_ws_link(ws_stream);
        let added = mesh
            .lock()
            .await
            .add_link(neigh_id, Box::new(sink), Box::new(stream));
        if added {
            info!("Connected to server {} (node {:X})", server, neigh_id);
        } else {
            info!(
                "Rejected duplicate connection to {} (node {:X}), keeping existing link",
                server, neigh_id
            );
        }
        Ok(())
    }

    #[allow(dead_code)]
    pub(crate) async fn disconnect(&mut self, id: NodeId) {
        self.mesh.lock().await.remove_link(id);
    }

    pub(crate) fn stop(&self) {
        self.stop.notify_waiters();
    }
}

impl Drop for ConnManager {
    fn drop(&mut self) {
        self.stop();
    }
}
