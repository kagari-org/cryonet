use std::{
    any::Any,
    collections::HashMap,
    io,
    path::{Path, PathBuf},
    time::{Duration, Instant as StdInstant},
};

use anyhow::{Error, Result};
use cryonet_lib::{
    collect_stats,
    connection::ConnManagerHandle,
    fullmesh::{fullmesh::FullMeshHandle, registry::RegistryHandle},
    mesh::{
        MeshHandle,
        igp::IgpHandle,
        packet::{Packet, Payload},
    },
};
use cryonet_uapi::{CryonetUapi, DaemonInfo};
use sactor::sactor;
use serde::{Deserialize, Serialize};
use tokio::{
    fs::{create_dir_all, remove_file},
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
    registry: RegistryHandle,
    mgr: ConnManagerHandle,
    info: DaemonInfo,
    started_at: StdInstant,

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
    #[allow(clippy::too_many_arguments)]
    pub async fn new(
        mesh: MeshHandle,
        igp: IgpHandle,
        fm: FullMeshHandle,
        registry: RegistryHandle,
        mgr: ConnManagerHandle,
        info: DaemonInfo,
        started_at: StdInstant,
        path: PathBuf,
    ) -> Result<UapiHandle> {
        Self::new_with_parameters(
            mesh,
            igp,
            fm,
            registry,
            mgr,
            info,
            started_at,
            path,
            Duration::from_secs(30),
            Duration::from_secs(60),
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn new_with_parameters(
        mesh: MeshHandle,
        igp: IgpHandle,
        fm: FullMeshHandle,
        registry: RegistryHandle,
        mgr: ConnManagerHandle,
        info: DaemonInfo,
        started_at: StdInstant,
        path: PathBuf,
        gc_interval: Duration,
        ping_timeout: Duration,
    ) -> Result<UapiHandle> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                create_dir_all(parent).await?;
            }
        }
        let _ = remove_file(&path).await;
        let socket = UnixDatagram::bind(path).unwrap();
        let packet_rx = mesh
            .add_dispatchee(Box::new(|packet| {
                packet
                    .payload
                    .as_payload()
                    .is_some_and(|payload| (payload as &dyn Any).is::<UapiPayload>())
            }))
            .await?;
        let (future, uapi) = Uapi::run(move |handle| Uapi {
            handle,
            mesh,
            igp,
            fm,
            registry,
            mgr,
            info,
            started_at,
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
    async fn handle_packet(&mut self, packet: Option<Packet>) -> Result<()> {
        let Some(packet) = packet else {
            self.handle.stop();
            return Ok(());
        };
        let src = packet.src;
        let uapi_payload = packet
            .payload
            .as_payload()
            .and_then(|payload| (payload as &dyn Any).downcast_ref::<UapiPayload>())
            .unwrap();
        use UapiPayload::*;
        match uapi_payload {
            Ping(uuid) => {
                self.mesh.send_packet(src, Box::new(Pong(*uuid))).await?;
            }
            Pong(uuid) => {
                if let Some((path, _)) = self.ping.remove(uuid) {
                    self.reply(&path, &CryonetUapi::Pong).await?;
                } else {
                    debug!("Received unexpected pong with uuid {uuid}, dropping");
                }
            }
        }
        Ok(())
    }

    #[no_reply]
    async fn handle_message(
        &mut self,
        result: io::Result<(usize, tokio::net::unix::SocketAddr)>,
    ) -> Result<()> {
        let (len, addr) = result?;
        let Some(path) = addr.as_pathname() else {
            error!("Received uapi message from non-path address, dropping");
            return Ok(());
        };
        let path = path.to_owned();
        let cmd: CryonetUapi = match serde_json::from_slice(&self.buf[..len]) {
            Ok(cmd) => cmd,
            Err(err) => {
                self.reply(
                    &path,
                    &CryonetUapi::Error(format!("invalid request: {err}")),
                )
                .await?;
                return Ok(());
            }
        };
        match self.dispatch_message(cmd, &path).await {
            Ok(Some(response)) => self.reply(&path, &response).await?,
            Ok(None) => {}
            Err(err) => {
                self.reply(&path, &CryonetUapi::Error(format!("{err:#}")))
                    .await?;
            }
        }
        Ok(())
    }

    #[skip]
    async fn dispatch_message(
        &mut self,
        cmd: CryonetUapi,
        path: &Path,
    ) -> Result<Option<CryonetUapi>> {
        use CryonetUapi::*;
        let response = match cmd {
            GetLinks => GetLinksResponse(self.mesh.get_links().await?),
            GetRoutes => GetRoutesResponse(self.mesh.get_routes().await?),
            GetIgpRoutes => GetIgpRoutesResponse(self.igp.get_routes().await?),
            GetIgpStatus => GetIgpStatusResponse(self.igp.get_status().await?),
            GetFullMeshPeers => GetFullMeshPeersResponse(self.fm.get_peers().await?),
            GetNodes => GetNodesResponse(self.registry.get_nodes().await?),
            GetServers => GetServersResponse(self.mgr.get_servers().await?),
            GetInfo => {
                let mut info = self.info.clone();
                info.uptime_ms = self.started_at.elapsed().as_millis() as u64;
                info.local_ips = self.registry.get_local_ips().await?;
                GetInfoResponse(info)
            }
            GetStats => GetStatsResponse(
                collect_stats(
                    &self.mesh,
                    &self.igp,
                    &self.fm,
                    &self.registry,
                    self.started_at.elapsed().as_millis() as u64,
                )
                .await?,
            ),
            Ping(dst) => {
                let uuid = Uuid::new_v4();
                self.mesh
                    .send_packet(dst, Box::new(UapiPayload::Ping(uuid)))
                    .await?;
                self.ping.insert(uuid, (path.to_owned(), Instant::now()));
                return Ok(None);
            }
            other => Error(format!("unexpected request: {other:?}")),
        };
        Ok(Some(response))
    }

    #[skip]
    async fn reply(&self, path: &Path, response: &CryonetUapi) -> Result<()> {
        let bytes = serde_json::to_vec(response)?;
        self.socket.send_to(&bytes, path).await?;
        Ok(())
    }

    #[no_reply]
    async fn gc(&mut self) -> Result<()> {
        let now = Instant::now();
        let ping_timeout = self.ping_timeout;
        let mut expired = Vec::new();
        self.ping.retain(|uuid, (path, instant)| {
            if now.duration_since(*instant) > ping_timeout {
                debug!("Removing expired ping with uuid {uuid}, sent at {instant:?}");
                expired.push(path.clone());
                false
            } else {
                true
            }
        });
        for path in expired {
            self.reply(&path, &CryonetUapi::Error("ping timed out".to_owned()))
                .await?;
        }
        Ok(())
    }

    #[handle_error]
    fn handle_error(&mut self, err: &Error) {
        error!("Error: {err:?}");
    }
}
