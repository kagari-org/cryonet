use std::{
    any::Any,
    collections::HashMap,
    io,
    path::PathBuf,
    time::Duration,
};

use cryonet_lib::{fullmesh::FullMeshHandle, mesh::{MeshHandle, igp::IgpHandle, packet::{Packet, Payload}}};
use cryonet_uapi::{CryonetUapi, IgpRoute};
use sactor::{error::{SactorError, SactorResult}, sactor};
use serde::{Deserialize, Serialize};
use tokio::{
    fs::remove_file,
    net::UnixDatagram,
    sync::mpsc,
    time::{Instant, Interval, interval},
};
use tracing::{debug, error};
use uuid::Uuid;

pub struct Uapi {
    handle: UapiHandle,

    mesh: MeshHandle,
    igp: IgpHandle,
    fm: FullMeshHandle,

    socket: UnixDatagram,
    buf: [u8; 1024],
    packet_rx: mpsc::Receiver<Packet>,
    gc_ticker: Interval,
    ping_timeout: Duration,

    ping: HashMap<Uuid, (PathBuf, Instant)>,
}

#[typetag::serde]
impl Payload for UapiPayload {}
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
enum UapiPayload {
    Ping(Uuid),
    Pong(Uuid),
}

#[sactor(pub)]
impl Uapi {
    pub async fn new(mesh: MeshHandle, igp: IgpHandle, fm: FullMeshHandle, path: PathBuf) -> SactorResult<UapiHandle> {
        Self::new_with_parameters(mesh, igp, fm, path, Duration::from_secs(30), Duration::from_secs(60)).await
    }

    pub async fn new_with_parameters(mesh: MeshHandle, igp: IgpHandle, fm: FullMeshHandle, path: PathBuf, gc_interval: Duration, ping_timeout: Duration) -> SactorResult<UapiHandle> {
        let _ = remove_file(&path).await;
        let socket = UnixDatagram::bind(path).unwrap();
        let packet_rx = mesh.add_dispatchee(Box::new(|packet| (packet.payload.as_ref() as &dyn Any).is::<UapiPayload>())).await?;
        let (future, uapi) = Uapi::run(move |handle| Uapi {
            handle,
            mesh,
            igp,
            fm,
            socket,
            buf: [0u8; 1024],
            packet_rx,
            gc_ticker: interval(gc_interval),
            ping_timeout,
            ping: HashMap::new(),
        });
        tokio::task::spawn_local(future);
        Ok(uapi)
    }

    #[select]
    fn select(&mut self) -> Vec<Selection<'_>> {
        vec![
            selection!(self.packet_rx.recv().await, handle_packet, it => it),
            selection!(self.socket.recv_from(&mut self.buf).await, handle_message, it => it),
            selection!(self.gc_ticker.tick().await, gc),
        ]
    }

    #[no_reply]
    async fn handle_packet(&mut self, packet: Option<Packet>) -> SactorResult<()> {
        let Some(packet) = packet else {
            self.handle.stop();
            return Ok(());
        };
        let src = packet.src;
        let uapi_payload = (packet.payload.as_ref() as &dyn Any).downcast_ref::<UapiPayload>().unwrap();
        use UapiPayload::*;
        match uapi_payload {
            Ping(uuid) => {
                self.mesh.send_packet(src, Box::new(Pong(*uuid))).await?;
            }
            Pong(uuid) => {
                if let Some((path, _)) = self.ping.remove(uuid) {
                    let response = CryonetUapi::Pong;
                    let bytes = serde_json::to_vec(&response)?;
                    self.socket.send_to(&bytes, path).await?;
                } else {
                    debug!("Received unexpected pong with uuid {uuid}, dropping");
                }
            }
        }
        Ok(())
    }

    #[no_reply]
    async fn handle_message(&mut self, result: io::Result<(usize, tokio::net::unix::SocketAddr)>) -> SactorResult<()> {
        let (len, addr) = result?;
        let Some(path) = addr.as_pathname() else {
            error!("Received uapi message from non-path address, dropping");
            return Ok(());
        };
        let path = path.to_owned();
        let cmd: CryonetUapi = serde_json::from_slice(&self.buf[..len])?;
        self.dispatch_message(cmd, path).await?;
        Ok(())
    }

    async fn dispatch_message(&mut self, cmd: CryonetUapi, path: PathBuf) -> SactorResult<()> {
        use CryonetUapi::*;
        match cmd {
            GetLinks => {
                let links = self.mesh.get_links().await?;
                let response = GetLinksResponse(links);
                let bytes = serde_json::to_vec(&response)?;
                self.socket.send_to(&bytes, &path).await?;
            }
            GetRoutes => {
                let routes = self.mesh.get_routes().await?;
                let response = GetRoutesResponse(routes);
                let bytes = serde_json::to_vec(&response)?;
                self.socket.send_to(&bytes, &path).await?;
            }
            GetIgpRoutes => {
                let now = Instant::now();
                let routes = self.igp.get_routes().await?;
                let routes = routes
                    .into_iter()
                    .map(|route| {
                        let timeout_remaining_ms = if route.timeout > now {
                            route.timeout.duration_since(now).as_millis() as i64
                        } else {
                            -(now.duration_since(route.timeout).as_millis() as i64)
                        };
                        IgpRoute {
                            seq: route.metric.seq.0,
                            metric: route.metric.metric,
                            computed_metric: route.computed_metric,
                            dst: route.dst,
                            from: route.from,
                            selected: route.selected,
                            timeout_remaining_ms,
                        }
                    })
                    .collect();
                let response = GetIgpRoutesResponse(routes);
                let bytes = serde_json::to_vec(&response)?;
                self.socket.send_to(&bytes, &path).await?;
            }
            GetFullMeshPeers => {
                let peers = self.fm.get_peers().await?;
                let response = GetFullMeshPeersResponse(peers);
                let bytes = serde_json::to_vec(&response)?;
                self.socket.send_to(&bytes, &path).await?;
            }
            Ping(dst) => {
                let uuid = Uuid::new_v4();
                let instant = Instant::now();
                self.mesh.send_packet(dst, Box::new(UapiPayload::Ping(uuid))).await?;
                self.ping.insert(uuid, (path, instant));
            }
            _ => error!("Unexpected uapi command: {:?}, dropping", cmd),
        };
        Ok(())
    }

    #[no_reply]
    async fn gc(&mut self) -> SactorResult<()> {
        let now = Instant::now();
        let ping_timeout = self.ping_timeout;
        self.ping.retain(|uuid, (_, instant)| {
            if now.duration_since(*instant) > ping_timeout {
                debug!("Removing expired ping with uuid {}, sent at {:?}", uuid, instant);
                false
            } else {
                true
            }
        });
        Ok(())
    }

    #[handle_error]
    fn handle_error(&mut self, err: &SactorError) {
        error!("Error: {:?}", err);
    }
}
