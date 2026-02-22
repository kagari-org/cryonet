use std::{
    collections::{HashMap, HashSet},
    net::SocketAddr,
    sync::Arc,
    time::Duration,
};

use crate::{
    connection::link::new_reqwest_ws_link,
    mesh::{MeshHandle, packet::NodeId},
};
use anyhow::anyhow;
use bytes::Bytes;
use futures::{SinkExt, StreamExt};
use reqwest_websocket::{CloseCode, Message, Upgrade};
use sactor::{error::{SactorError, SactorResult}, sactor};
use serde::{Deserialize, Serialize};
use tokio::{
    sync::Mutex,
    time::{Interval, interval},
};
use tracing::{error, info, warn};

#[cfg(not(target_arch = "wasm32"))]
use crate::connection::link::new_tungstenite_ws_link;
#[cfg(not(target_arch = "wasm32"))]
use tokio_tungstenite::accept_async;
#[cfg(not(target_arch = "wasm32"))]
use tokio::net::TcpListener;

pub mod link;

#[cfg(target_arch = "wasm32")]
type AcceptParam = ();
#[cfg(not(target_arch = "wasm32"))]
type AcceptParam = tokio::io::Result<(tokio::net::TcpStream, SocketAddr)>;

pub struct ConnManager {
    id: NodeId,
    mesh: MeshHandle,

    token: Option<String>,
    servers: Vec<String>,

    connect_timer: Interval,

    servers_map: Arc<Mutex<HashMap<String, NodeId>>>,
    connecting: Arc<Mutex<HashSet<String>>>,

    #[cfg(not(target_arch = "wasm32"))]
    listener: TcpListener,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AuthPacket {
    token: Option<String>,
    node_id: NodeId,
}

#[sactor(pub)]
impl ConnManager {
    pub async fn new(id: NodeId, mesh: MeshHandle, token: Option<String>, servers: Vec<String>, listen: SocketAddr) -> SactorResult<ConnManagerHandle> {
        Self::new_with_parameters(id, mesh, token, servers, listen, Duration::from_secs(8)).await
    }

    #[allow(unused_variables)]
    pub async fn new_with_parameters(id: NodeId, mesh: MeshHandle, token: Option<String>, servers: Vec<String>, listen: SocketAddr, connect_interval: Duration) -> SactorResult<ConnManagerHandle> {
        #[cfg(not(target_arch = "wasm32"))]
        let listener = {
            let listener = TcpListener::bind(listen).await?;
            info!("Listening on {}", listen);
            listener
        };
        let (future, mgr) = ConnManager::run(move |_| ConnManager {
            id,
            mesh,
            token,
            servers,
            connect_timer: interval(connect_interval),
            servers_map: Arc::new(Mutex::new(HashMap::new())),
            connecting: Arc::new(Mutex::new(HashSet::new())),
            #[cfg(not(target_arch = "wasm32"))]
            listener,
        });
        tokio::task::spawn_local(future);
        Ok(mgr)
    }

    #[select]
    fn select(&mut self) -> Vec<Selection<'_>> {
        vec![
            selection!(self.connect_timer.tick().await, connect),
            #[cfg(not(target_arch = "wasm32"))]
            selection!(self.listener.accept().await, accept, it => it),
        ]
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
            tokio::task::spawn_local(async move {
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
        let client = reqwest::Client::new();
        let mut ws = client.get(&server).upgrade().send().await?.into_websocket().await?;
        ws.send(Message::Binary(Bytes::from(serde_json::to_vec(&AuthPacket {
            token: token.clone(),
            node_id: id,
        })?))).await?;
        let AuthPacket { token: neigh_token, node_id: neigh_id }= match ws.next().await {
            Some(Ok(Message::Binary(bytes))) => serde_json::from_slice(&bytes)?,
            _ => return Err(SactorError::Other(anyhow!("Failed to receive authentication response from server {}", server))),
        };
        if let Some(token) = &token {
            match neigh_token {
                Some(neigh_token) if neigh_token == *token => {},
                _ => {
                    let _ = ws.close(CloseCode::Abnormal, None).await;
                    return Err(SactorError::Other(anyhow!("Server {} provided invalid authentication token", server)))
                },
            }
        }
        servers_map.lock().await.insert(server.clone(), neigh_id);

        let (sink, stream) = new_reqwest_ws_link(ws);
        let added = mesh.add_link(neigh_id, Box::new(sink), Box::new(stream), true).await?;
        if added {
            info!("Connected to server {} (node {:X})", server, neigh_id);
        } else {
            info!("Rejected duplicate connection to {} (node {:X}), keeping existing link", server, neigh_id);
        }
        Ok(())
    }

    #[no_reply]
    #[allow(unused)]
    async fn accept(&mut self, param: AcceptParam) -> SactorResult<()> {
        #[cfg(not(target_arch = "wasm32"))]
        let result = self.accept_internal(param).await;
        #[cfg(target_arch = "wasm32")]
        let result = unreachable!();
        result
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[skip]
    async fn accept_internal(&mut self, param: AcceptParam) -> SactorResult<()> {
        use tokio_tungstenite::tungstenite::Message;
        let (stream, addr) = param?;
        info!("Accepted connection from {}", addr);
        let mut ws = accept_async(stream).await?;
        ws.send(Message::Binary(Bytes::from(serde_json::to_vec(&AuthPacket {
            token: self.token.clone(),
            node_id: self.id,
        })?))).await?;
        let AuthPacket { token: neigh_token, node_id: neigh_id }= match ws.next().await {
            Some(Ok(Message::Binary(bytes))) => serde_json::from_slice(&bytes)?,
            _ => return Err(SactorError::Other(anyhow!("Failed to receive authentication response from connection at {}", addr))),
        };
        if let Some(token) = &self.token {
            match neigh_token {
                Some(neigh_token) if neigh_token == *token => {},
                _ => {
                    let _ = ws.close(None).await;
                    return Err(SactorError::Other(anyhow!("Connection from {} provided invalid authentication token", addr)))
                },
            }
        }
        let (sink, stream) = new_tungstenite_ws_link(ws);
        let added = self.mesh.add_link(neigh_id, Box::new(sink), Box::new(stream), false).await?;
        if added {
            info!("Accepted new connection from node {:X} at {}", neigh_id, addr);
        } else {
            info!("Rejected duplicate connection from node {:X} at {}, keeping existing link", neigh_id, addr);
        }
        Ok(())
    }

    #[allow(dead_code)]
    pub async fn disconnect(&mut self, id: NodeId) -> SactorResult<()> {
        self.mesh.remove_link(id).await
    }

    #[handle_error]
    async fn handle_error(&mut self, err: &SactorError) {
        error!("Error: {:?}", err);
    }
}
