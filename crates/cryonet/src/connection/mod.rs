use std::{
    collections::{HashMap, HashSet},
    net::SocketAddr,
    pin::Pin,
    sync::Arc,
    time::Duration,
};

use crate::{
    connection::link::new_reqwest_ws_link,
    errors::Error,
    mesh::{MeshHandle, packet::NodeId},
};
use futures::future::select_all;
use reqwest_websocket::Upgrade;
use sactor::{error::{SactorError, SactorResult}, sactor};
use tokio::{
    sync::Mutex,
    time::{Interval, interval},
};
use tracing::{error, info, warn};

#[cfg(not(feature = "webrtc"))]
use crate::connection::link::new_axum_ws_link;
#[cfg(not(feature = "webrtc"))]
use axum::{
    Router,
    extract::{State, ws::WebSocketUpgrade},
    http::HeaderMap,
    response::IntoResponse,
    routing::any,
    serve,
};
#[cfg(not(feature = "webrtc"))]
use reqwest::StatusCode;
#[cfg(not(feature = "webrtc"))]
use tokio::net::TcpListener;

pub(crate) mod link;

pub(crate) struct ConnManager {
    id: NodeId,
    mesh: MeshHandle,

    token: Option<String>,
    servers: Vec<String>,

    connect_timer: Interval,

    servers_map: Arc<Mutex<HashMap<String, NodeId>>>,
    connecting: Arc<Mutex<HashSet<String>>>,
}

#[sactor(pub(crate))]
impl ConnManager {
    pub(crate) async fn new(id: NodeId, mesh: MeshHandle, token: Option<String>, servers: Vec<String>, listen: SocketAddr) -> SactorResult<ConnManagerHandle> {
        Self::new_with_parameters(id, mesh, token, servers, listen, Duration::from_secs(8)).await
    }

    #[allow(unused_variables)]
    pub(crate) async fn new_with_parameters(id: NodeId, mesh: MeshHandle, token: Option<String>, servers: Vec<String>, listen: SocketAddr, connect_interval: Duration) -> SactorResult<ConnManagerHandle> {
        #[cfg(not(feature = "webrtc"))]
        let serve = {
            let listener = TcpListener::bind(listen).await?;
            info!("Listening on {}", listen);
            let token = token.clone();
            let app = Router::new().route("/", any(handle_request)).with_state((id, mesh.clone(), token.clone()));
            Box::pin(serve(listener, app).into_future())
        };
        let (future, mgr) = ConnManager::run(move |_| ConnManager {
            id,
            mesh,
            token,
            servers,
            connect_timer: interval(connect_interval),
            servers_map: Arc::new(Mutex::new(HashMap::new())),
            connecting: Arc::new(Mutex::new(HashSet::new())),
        });
        tokio::spawn(async move {
            let futures: Vec<Pin<Box<dyn Future<Output = ()> + Send>>> = vec![
                Box::pin(future),
                #[cfg(not(feature = "webrtc"))]
                Box::pin(async move {
                    let _ = serve.await;
                }),
            ];
            select_all(futures).await;
        });
        Ok(mgr)
    }

    #[select]
    fn select(&mut self) -> Vec<Selection<'_>> {
        vec![selection!(self.connect_timer.tick().await, connect)]
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
        let client = reqwest::Client::new();
        let mut request = client.get(&server);
        request = request.header("X-NodeId", id.to_string());
        if let Some(token) = &token {
            request = request.header("Authorization", token);
        }
        let response = request.upgrade().send().await?;
        if let Some(token) = &token {
            match response.headers().get("Authorization") {
                Some(auth) if auth.to_str()? == token => {}
                _ => return Err(Error::Unauthorized.into()),
            }
        }
        let neigh_id = response.headers().get("X-NodeId");
        let Some(neigh_id_val) = neigh_id else {
            return Err(Error::Unauthorized.into());
        };
        let neigh_id = neigh_id_val.to_str()?.parse::<NodeId>()?;
        servers_map.lock().await.insert(server.clone(), neigh_id);

        let ws_stream = response.into_websocket().await?;
        let (sink, stream) = new_reqwest_ws_link(ws_stream);
        let added = mesh.add_link(neigh_id, Box::new(sink), Box::new(stream), true).await?;
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

    #[handle_error]
    async fn handle_error(&mut self, err: &SactorError) {
        error!("Error: {:?}", err);
    }
}

#[cfg(not(feature = "webrtc"))]
async fn handle_request(ws: WebSocketUpgrade, headers: HeaderMap, State((id, mesh, token)): State<(NodeId, MeshHandle, Option<String>)>) -> impl IntoResponse {
    if let Some(token) = &token {
        if let Some(auth) = headers.get("Authorization")
            && let Ok(auth) = auth.to_str()
            && auth == token
        {
            // Authorized
        } else {
            return StatusCode::UNAUTHORIZED.into_response();
        }
    };
    let Some(Ok(Ok(node_id))) = headers.get("X-NodeId").map(|id| id.to_str().map(|id| id.parse::<NodeId>())) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let mut response = ws.on_upgrade(async move |ws| {
        let (sink, stream) = new_axum_ws_link(ws);
        match mesh.add_link(node_id, Box::new(sink), Box::new(stream), false).await {
            Ok(true) => info!("Accepted new connection from node {:X}", node_id),
            Ok(false) => info!("Rejected duplicate connection from node {:X}, keeping existing link", node_id),
            Err(e) => warn!("Failed to accept connection from node {:X}: {}", node_id, e),
        }
    });
    if let Some(token) = &token {
        response.headers_mut().insert("Authorization", token.parse().unwrap());
    }
    response.headers_mut().insert("X-NodeId", id.to_string().parse().unwrap());
    response
}
