use std::{path::Path, sync::Arc};

use anyhow::Result;
use cryonet_uapi::{Conn, ConnState, CryonetUapi, IgpRoute};
use rustrtc::PeerConnectionState;
use tokio::{
    fs::remove_file,
    net::UnixDatagram,
    select,
    sync::{Mutex, Notify},
};
use tracing::error;

use crate::{
    fullmesh::FullMesh,
    mesh::{Mesh, igp::Igp},
};

pub(crate) struct Uapi {
    mesh: Arc<Mutex<Mesh>>,
    igp: Arc<Mutex<Igp>>,
    fm: Arc<Mutex<FullMesh>>,
    stop: Arc<Notify>,
}

impl Uapi {
    pub(crate) async fn new(
        mesh: Arc<Mutex<Mesh>>,
        igp: Arc<Mutex<Igp>>,
        fm: Arc<Mutex<FullMesh>>,
        path: String,
    ) -> Arc<Mutex<Self>> {
        remove_file(&path).await.unwrap();
        let socket = UnixDatagram::bind(path).unwrap();
        let stop = Arc::new(Notify::new());
        let uapi = Arc::new(Mutex::new(Self {
            mesh,
            igp,
            fm,
            stop: stop.clone(),
        }));
        let uapi2 = uapi.clone();
        tokio::spawn(async move {
            let mut buf = [0u8; 1024];
            let notified = stop.notified();
            tokio::pin!(notified);
            loop {
                select! {
                    _ = &mut notified => break,
                    res = socket.recv_from(&mut buf) => {
                        let (len, addr) = match res {
                            Ok(res) => res,
                            Err(err) => {
                                error!("Failed to receive uapi message: {err}");
                                continue;
                            },
                        };
                        let Some(path) = addr.as_pathname() else {
                            error!("Received uapi message from non-path address, dropping");
                            continue;
                        };
                        let result = uapi.lock().await.handle_message(
                            &socket,
                            &buf[..len],
                            path,
                        ).await;
                        if let Err(err) = result {
                            error!("Failed to handle uapi message: {err}");
                        }
                    },
                }
            }
        });
        uapi2
    }

    async fn handle_message(
        &mut self,
        socket: &UnixDatagram,
        msg: &[u8],
        path: &Path,
    ) -> Result<()> {
        let cmd: CryonetUapi = serde_json::from_slice(msg)?;
        use CryonetUapi::*;
        match cmd {
            GetLinks => {
                let links = self.mesh.lock().await.get_links();
                let response = GetLinksResponse(links);
                let bytes = serde_json::to_vec(&response)?;
                socket.send_to(&bytes, path).await?;
            }
            GetRoutes => {
                let routes = self.mesh.lock().await.get_routes();
                let response = GetRoutesResponse(routes);
                let bytes = serde_json::to_vec(&response)?;
                socket.send_to(&bytes, path).await?;
            }
            GetIgpRoutes => {
                let routes = self.igp.lock().await.get_routes();
                let routes = routes
                    .into_iter()
                    .map(|route| IgpRoute {
                        seq: route.metric.seq.0,
                        metric: route.metric.metric,
                        computed_metric: route.computed_metric,
                        dst: route.dst,
                        from: route.from,
                        selected: route.selected,
                    })
                    .collect();
                let response = GetIgpRoutesResponse(routes);
                let bytes = serde_json::to_vec(&response)?;
                socket.send_to(&bytes, path).await?;
            }
            GetFullMeshPeers => {
                let fm = self.fm.lock().await;
                let peers = fm
                    .get_peers()
                    .iter()
                    .map(|(node_id, conns)| {
                        let conns = conns
                            .iter()
                            .map(|(uuid, conn)| {
                                use PeerConnectionState::*;
                                (
                                    *uuid,
                                    Conn {
                                        selected: conn.selected,
                                        state: match *conn.conn.state_watcher.borrow() {
                                            New => ConnState::New,
                                            Connecting => ConnState::Connecting,
                                            Connected => ConnState::Connected,
                                            Disconnected => ConnState::Disconnected,
                                            Failed => ConnState::Failed,
                                            Closed => ConnState::Closed,
                                        },
                                    },
                                )
                            })
                            .collect();
                        (*node_id, conns)
                    })
                    .collect();
                let response = GetFullMeshPeersResponse(peers);
                let bytes = serde_json::to_vec(&response)?;
                socket.send_to(&bytes, path).await?;
            }
            _ => error!("Unexpected uapi command: {:?}, dropping", cmd),
        };
        Ok(())
    }

    pub(crate) fn stop(&self) {
        self.stop.notify_waiters();
    }
}

impl Drop for Uapi {
    fn drop(&mut self) {
        self.stop();
    }
}
