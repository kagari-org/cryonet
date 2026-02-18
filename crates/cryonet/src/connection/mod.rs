use std::{
    collections::{HashMap, HashSet},
    io,
    net::SocketAddr,
    sync::Arc,
    time::Duration,
};

use sactor::{error::SactorResult, sactor};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::Mutex,
    time::{Interval, interval, timeout},
};
use tokio_tungstenite::{
    accept_hdr_async, connect_async,
    tungstenite::{
        client::IntoClientRequest,
        handshake::server::{Request, Response},
    },
};
use tracing::{info, warn};

use crate::{
    connection::link::new_ws_link,
    errors::Error,
    mesh::{MeshHandle, packet::NodeId},
};

pub(crate) mod link;

pub(crate) struct ConnManager {
    id: NodeId,
    mesh: MeshHandle,

    token: Option<String>,
    servers: Vec<String>,

    listener: TcpListener,
    connect_timer: Interval,

    servers_map: Arc<Mutex<HashMap<String, NodeId>>>,
    connecting: Arc<Mutex<HashSet<String>>>,
}

#[sactor(pub(crate))]
impl ConnManager {
    pub(crate) async fn new(id: NodeId, mesh: MeshHandle, token: Option<String>, servers: Vec<String>, listen: SocketAddr) -> SactorResult<ConnManagerHandle> {
        Self::new_with_parameters(id, mesh, token, servers, listen, Duration::from_secs(8)).await
    }

    pub(crate) async fn new_with_parameters(id: NodeId, mesh: MeshHandle, token: Option<String>, servers: Vec<String>, listen: SocketAddr, connect_interval: Duration) -> SactorResult<ConnManagerHandle> {
        let listener = TcpListener::bind(listen).await?;
        info!("Listening on {}", listen);
        let (future, mgr) = ConnManager::run(move |_| ConnManager {
            id,
            mesh,
            token,
            servers,
            listener,
            connect_timer: interval(connect_interval),
            servers_map: Arc::new(Mutex::new(HashMap::new())),
            connecting: Arc::new(Mutex::new(HashSet::new())),
        });
        tokio::spawn(future);
        Ok(mgr)
    }

    #[select]
    fn select(&mut self) -> Vec<Selection<'_>> {
        vec![selection!(self.listener.accept().await, accept, it => it), selection!(self.connect_timer.tick().await, connect)]
    }

    #[no_reply]
    async fn accept(&mut self, result: io::Result<(TcpStream, SocketAddr)>) -> SactorResult<()> {
        let (stream, addr) = result?;
        let id = self.id;
        let mesh = self.mesh.clone();
        let token = self.token.clone();
        tokio::spawn(async move {
            let result = Self::accept_task(id, mesh, stream, addr, token).await;
            if let Err(err) = result {
                warn!("Failed to accept connection from {}: {}", addr, err);
            }
        });
        Ok(())
    }

    async fn accept_task(id: NodeId, mesh: MeshHandle, stream: TcpStream, addr: SocketAddr, token: Option<String>) -> SactorResult<()> {
        let mut neigh_id = None;
        let result = accept_hdr_async(stream, |req: &Request, mut res: Response| {
            let reject = |reason: String| Err(Response::builder().status(400).body(Some(reason)).unwrap());
            if let Some(token) = &token {
                let Some(auth) = req.headers().get("Authorization") else {
                    return reject("Missing Authorization header".to_string());
                };
                match auth.to_str() {
                    Ok(s) if s == token => {}
                    _ => return reject("Invalid Authorization token".to_string()),
                };
            }
            let peer_id = match req.headers().get("X-NodeId").map(|id| id.to_str().map(|id| id.parse::<NodeId>())) {
                Some(Ok(Ok(id))) => id,
                _ => return reject("Invalid X-Node-Id header".to_string()),
            };
            neigh_id = Some(peer_id);
            res.headers_mut().insert("X-NodeId", id.to_string().parse().unwrap());
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
        let added = mesh.add_link(neigh_id, Box::new(sink), Box::new(stream)).await?;
        if added {
            info!("Accepted connection from {} (node {:X})", addr, neigh_id);
        } else {
            info!("Rejected duplicate connection from {} (node {:X}), keeping existing link", addr, neigh_id);
        }
        Ok(())
    }

    #[no_reply]
    async fn connect(&mut self) -> SactorResult<()> {
        for server in &self.servers {
            if server.is_empty() {
                continue;
            }
            {
                let mut connecting = self.connecting.lock().await;
                if connecting.contains(server) {
                    continue;
                }
                connecting.insert(server.clone());
            }
            let id = self.id;
            let mesh = self.mesh.clone();
            let token = self.token.clone();
            let servers_map = self.servers_map.clone();
            let connecting = self.connecting.clone();
            let server = server.clone();
            tokio::spawn(async move {
                if let Err(err) = Self::connect_task(id, mesh, token, servers_map, server.clone()).await {
                    warn!("Failed to connect to server {}: {}", server, err);
                }
                connecting.lock().await.remove(&server);
            });
        }
        Ok(())
    }

    async fn connect_task(id: NodeId, mesh: MeshHandle, token: Option<String>, servers_map: Arc<Mutex<HashMap<String, NodeId>>>, server: String) -> SactorResult<()> {
        let links = mesh.get_links().await?;
        {
            let servers_map = servers_map.lock().await;
            if let Some(neigh_id) = servers_map.get(&server)
                && links.contains(neigh_id)
            {
                return Ok(());
            }
        }

        info!("Connecting to server {} ...", server);
        let mut req = server.clone().into_client_request()?;
        req.headers_mut().insert("X-NodeId", id.to_string().parse()?);
        if let Some(token) = &token {
            req.headers_mut().insert("Authorization", token.parse()?);
        }
        let (ws_stream, res) = timeout(Duration::from_secs(5), connect_async(req)).await??;
        let Some(neigh_id) = res.headers().get("X-NodeId") else {
            return Err(Error::Unauthorized.into());
        };
        let neigh_id = neigh_id.to_str()?.parse::<NodeId>()?;
        servers_map.lock().await.insert(server.clone(), neigh_id);

        let (sink, stream) = new_ws_link(ws_stream);
        let added = mesh.add_link(neigh_id, Box::new(sink), Box::new(stream)).await?;
        if added {
            info!("Connected to server {} (node {:X})", server, neigh_id);
        } else {
            info!("Rejected duplicate connection to {} (node {:X}), keeping existing link", server, neigh_id);
        }
        Ok(())
    }

    #[allow(dead_code)]
    pub(crate) async fn disconnect(&mut self, id: NodeId) -> SactorResult<()> {
        self.mesh.remove_link(id).await
    }
}
