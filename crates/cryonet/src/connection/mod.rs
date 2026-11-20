use std::{
    collections::{HashMap, HashSet},
    net::SocketAddr,
    sync::Arc,
    time::Duration,
};

use crate::{
    connection::link::new_tungstenite_ws_link,
    mesh::{MeshHandle, packet::NodeId},
};
use anyhow::{Error, Result, bail};
use bytes::Bytes;
use cryonet_uapi::ServerInfo;
use futures::{SinkExt, StreamExt};
use sactor::sactor;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tracing::{error, info, warn};

use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::TcpListener,
};
use tokio_tungstenite::{WebSocketStream, accept_async, connect_async};

use crate::utils::{Interval, delayed_interval, jitter, timeout};

pub(crate) mod link;

pub(crate) struct ConnManager {
    id: NodeId,
    mesh: MeshHandle,

    token: Option<String>,
    servers: Vec<String>,

    connect_timer: Interval,
    connect_timeout: Duration,

    servers_map: Arc<Mutex<HashMap<String, NodeId>>>,
    connecting: Arc<Mutex<HashSet<String>>>,

    listener: TcpListener,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AuthPacket {
    token: Option<String>,
    node_id: NodeId,
}

async fn handshake<T>(
    ws: &mut WebSocketStream<T>,
    auth: AuthPacket,
    peer: &str,
) -> Result<AuthPacket>
where
    T: AsyncRead + AsyncWrite + Unpin,
{
    use tokio_tungstenite::tungstenite::Message;
    ws.send(Message::Binary(Bytes::from(serde_json::to_vec(&auth)?)))
        .await?;
    match ws.next().await {
        Some(Ok(Message::Binary(bytes))) => Ok(serde_json::from_slice(&bytes)?),
        _ => bail!("Failed to receive authentication response from {peer}"),
    }
}

#[sactor(pub(crate))]
impl ConnManager {
    pub(crate) async fn new(
        id: NodeId,
        mesh: MeshHandle,
        token: Option<String>,
        servers: Vec<String>,
        listen: SocketAddr,
    ) -> Result<ConnManagerHandle> {
        Self::new_with_parameters(
            id,
            mesh,
            token,
            servers,
            listen,
            jitter(Duration::from_secs(2), 0.1),
            Duration::from_secs(10),
        )
        .await
    }

    pub(crate) async fn new_with_parameters(
        id: NodeId,
        mesh: MeshHandle,
        token: Option<String>,
        servers: Vec<String>,
        listen: SocketAddr,
        connect_interval: Duration,
        connect_timeout: Duration,
    ) -> Result<ConnManagerHandle> {
        let listener = {
            let listener = TcpListener::bind(listen).await?;
            info!("Listening on {listen}");
            listener
        };
        let (future, mgr) = ConnManager::run(move |_| ConnManager {
            id,
            mesh,
            token,
            servers,
            connect_timer: delayed_interval(connect_interval),
            connect_timeout,
            servers_map: Arc::new(Mutex::new(HashMap::new())),
            connecting: Arc::new(Mutex::new(HashSet::new())),
            listener,
        });
        tokio::task::spawn_local(future);
        Ok(mgr)
    }

    #[select]
    fn select(&mut self) -> Vec<Selection<'_>> {
        vec![
            selection!(self.connect_timer.tick().await, connect),
            selection!(self.listener.accept().await, accept, it => it),
        ]
    }

    #[no_reply]
    async fn connect(&mut self) -> Result<()> {
        // drop half-open links, they would keep rejecting the peer's re-dials
        if let Err(err) = self.mesh.reap_stale_links().await {
            warn!("Failed to reap stale mesh links: {err}");
        }
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
            let connect_timeout = self.connect_timeout;
            let server = server.clone();
            tokio::task::spawn_local(async move {
                let result = match timeout(
                    connect_timeout,
                    Self::connect_task(id, mesh, token, servers_map, server.clone()),
                )
                .await
                {
                    Ok(result) => result,
                    Err(_) => Err(Error::msg("timed out")),
                };
                if let Err(err) = result {
                    warn!("Failed to connect to server {server}: {err}");
                }
                connecting.lock().await.remove(&server);
            });
        }
        Ok(())
    }

    async fn connect_task(
        id: NodeId,
        mesh: MeshHandle,
        token: Option<String>,
        servers_map: Arc<Mutex<HashMap<String, NodeId>>>,
        server: String,
    ) -> Result<()> {
        let links = mesh.get_link_ids().await?;
        {
            let servers_map = servers_map.lock().await;
            if let Some(neigh_id) = servers_map.get(&server)
                && links.contains(neigh_id)
            {
                return Ok(());
            }
        }

        info!("Connecting to server {server} ...");
        let (mut ws, _) = connect_async(server.as_str()).await?;
        let AuthPacket {
            node_id: neigh_id, ..
        } = handshake(
            &mut ws,
            AuthPacket {
                token: token.clone(),
                node_id: id,
            },
            &format!("server {server}"),
        )
        .await?;
        servers_map.lock().await.insert(server.clone(), neigh_id);

        let (sink, stream) = new_tungstenite_ws_link(ws);
        let added = mesh
            .add_link(neigh_id, Box::new(sink), Box::new(stream), true)
            .await?;
        if added {
            info!("Connected to server {server} (node {neigh_id:X})");
        } else {
            info!(
                "Rejected duplicate connection to {server} (node {neigh_id:X}), keeping existing link"
            );
        }
        Ok(())
    }

    #[no_reply]
    async fn accept(
        &mut self,
        param: tokio::io::Result<(tokio::net::TcpStream, SocketAddr)>,
    ) -> Result<()> {
        let (stream, addr) = param?;
        stream.set_nodelay(true)?;
        info!("Accepted connection from {addr}");
        let id = self.id;
        let connect_timeout = self.connect_timeout;
        let mut ws = match timeout(connect_timeout, accept_async(stream)).await {
            Ok(ws) => ws?,
            Err(_) => bail!("Timed out during WebSocket handshake with {addr}"),
        };
        let auth = timeout(
            connect_timeout,
            handshake(
                &mut ws,
                AuthPacket {
                    token: None,
                    node_id: id,
                },
                &format!("connection at {addr}"),
            ),
        )
        .await;
        let AuthPacket {
            token: neigh_token,
            node_id: neigh_id,
        } = match auth {
            Ok(auth) => auth?,
            Err(_) => bail!("Timed out waiting for authentication from {addr}"),
        };
        if let Some(token) = &self.token {
            match neigh_token {
                Some(neigh_token) if neigh_token == *token => {}
                _ => {
                    let _ = ws.close(None).await;
                    bail!("Connection from {addr} provided invalid authentication token",);
                }
            }
        }
        let (sink, stream) = new_tungstenite_ws_link(ws);
        let added = self
            .mesh
            .add_link(neigh_id, Box::new(sink), Box::new(stream), false)
            .await?;
        if added {
            info!("Accepted new connection from node {neigh_id:X} at {addr}");
        } else {
            info!(
                "Rejected duplicate connection from node {neigh_id:X} at {addr}, keeping existing link",
            );
        }
        Ok(())
    }

    pub(crate) async fn get_servers(&self) -> Vec<ServerInfo> {
        let links = self.mesh.get_link_ids().await.unwrap_or_default();
        let map = self.servers_map.lock().await;
        let connecting = self.connecting.lock().await;
        self.servers
            .iter()
            .filter(|server| !server.is_empty())
            .map(|url| {
                let node = map.get(url).copied();
                ServerInfo {
                    url: url.clone(),
                    node,
                    connecting: connecting.contains(url),
                    connected: node.is_some_and(|node| links.contains(&node)),
                }
            })
            .collect()
    }

    #[handle_error]
    async fn handle_error(&mut self, err: &Error) {
        error!("Error: {err:?}");
    }
}
