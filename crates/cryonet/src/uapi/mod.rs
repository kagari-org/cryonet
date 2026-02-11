use std::{any::Any, collections::HashMap, path::{Path, PathBuf}, sync::Arc, time::{Duration, Instant}};

use anyhow::Result;
use cryonet_uapi::{Conn, ConnState, CryonetUapi, IgpRoute};
use rustrtc::PeerConnectionState;
use serde::{Deserialize, Serialize};
use tokio::{
    fs::remove_file,
    net::UnixDatagram,
    select,
    sync::{Mutex, Notify}, time::interval,
};
use tracing::{debug, error};
use uuid::Uuid;

use crate::{
    fullmesh::FullMesh,
    mesh::{Mesh, igp::Igp, packet::{NodeId, Payload}},
};

pub(crate) struct Uapi {
    mesh: Arc<Mutex<Mesh>>,
    igp: Arc<Mutex<Igp>>,
    fm: Arc<Mutex<FullMesh>>,
    ping: HashMap<Uuid, (PathBuf, Instant)>,
    stop: Arc<Notify>,
}

#[typetag::serde]
impl Payload for UapiPayload {}
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
enum UapiPayload {
    Ping(Uuid), Pong(Uuid),
}

impl Uapi {
    pub(crate) async fn new(
        mesh: Arc<Mutex<Mesh>>,
        igp: Arc<Mutex<Igp>>,
        fm: Arc<Mutex<FullMesh>>,
        path: String,
    ) -> Arc<Mutex<Self>> {
        Self::new_with_parameters(
            mesh,
            igp,
            fm,
            path,
            Duration::from_secs(30),
            Duration::from_secs(60),
        ).await
    }

    pub(crate) async fn new_with_parameters(
        mesh: Arc<Mutex<Mesh>>,
        igp: Arc<Mutex<Igp>>,
        fm: Arc<Mutex<FullMesh>>,
        path: String,
        gc_interval: Duration,
        ping_timeout: Duration,
    ) -> Arc<Mutex<Self>> {
        let _ = remove_file(&path).await;
        let socket = UnixDatagram::bind(path).unwrap();
        let stop = Arc::new(Notify::new());
        let mut packet_rx = mesh.lock().await
            .add_dispatchee(|packet| (packet.payload.as_ref() as &dyn Any).is::<UapiPayload>());
        let uapi = Arc::new(Mutex::new(Self {
            mesh,
            igp,
            fm,
            ping: HashMap::new(),
            stop: stop.clone(),
        }));
        let uapi2 = uapi.clone();
        tokio::spawn(async move {
            let mut buf = [0u8; 1024];
            let notified = stop.notified();
            tokio::pin!(notified);
            let mut gc_ticker = interval(gc_interval);
            loop {
                select! {
                    _ = &mut notified => break,
                    packet = packet_rx.recv() => {
                        let Some(packet) = packet else {
                            error!("Uapi packet receiver closed unexpectedly");
                            break;
                        };
                        let uapi_payload = (packet.payload.as_ref() as &dyn Any)
                            .downcast_ref::<UapiPayload>().unwrap();
                        let result = uapi.lock().await
                            .handle_packet(&socket, packet.src, uapi_payload)
                            .await;
                        if let Err(err) = result {
                            error!("Failed to handle uapi packet: {err}");
                        }
                    },
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
                    _ = gc_ticker.tick() => {
                        let now = Instant::now();
                        uapi.lock().await.ping.retain(|uuid, (_, instant)| {
                            if now.duration_since(*instant) > ping_timeout {
                                debug!("Removing expired ping with uuid {}, sent at {:?}", uuid, instant);
                                false
                            } else {
                                true
                            }
                        });
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
            Ping(dst) => {
                let uuid = Uuid::new_v4();
                let instant = Instant::now();
                self.ping.insert(uuid, (path.to_owned(), instant));
                self.mesh.lock().await.send_packet(dst, UapiPayload::Ping(uuid)).await?;
                self.ping.insert(uuid, (path.to_owned(), instant));
            }
            _ => error!("Unexpected uapi command: {:?}, dropping", cmd),
        };
        Ok(())
    }

    async fn handle_packet(
        &mut self,
        socket: &UnixDatagram,
        src: NodeId,
        payload: &UapiPayload,
    ) -> Result<()> {
        use UapiPayload::*;
        match payload {
            Ping(uuid) => {
                self.mesh.lock().await.send_packet(src, Pong(*uuid)).await?;
            }
            Pong(uuid) => {
                if let Some((path, _)) = self.ping.remove(uuid) {
                    let response = CryonetUapi::Pong;
                    let bytes = serde_json::to_vec(&response)?;
                    socket.send_to(&bytes, path).await?;
                } else {
                    debug!("Received unexpected pong with uuid {uuid}, dropping");
                }
            }
        }
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
