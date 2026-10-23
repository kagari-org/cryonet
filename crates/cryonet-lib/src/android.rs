use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr},
    os::fd::{BorrowedFd, IntoRawFd},
    str::FromStr,
    sync::{Arc, Mutex as StdMutex},
    time::Instant,
};

use crate::{
    collect_stats,
    connection::{ConnManager, ConnManagerHandle},
    fullmesh::{
        DeviceManager, IceServer as FullMeshIceServer,
        fullmesh::{FullMesh, FullMeshHandle},
        registry::{ConnectionType, Registry, RegistryHandle},
        single_tun::SingleTunManager,
    },
    mesh::{
        Mesh, MeshHandle,
        igp::{Igp, IgpHandle},
    },
};
use anyhow::Result;
use cidr::AnyIpCidr;
use cryonet_uapi::{
    DaemonInfo, FullMeshStatus, IgpRoute, IgpStatus, LinkInfo, NodeId, NodeInfo, ServerInfo, Stats,
};
use tokio::{
    sync::{Mutex, oneshot},
    task::LocalSet,
};

#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum AndroidError {
    #[error("{reason}")]
    Error { reason: String },
}

fn android_error(err: anyhow::Error) -> AndroidError {
    AndroidError::Error {
        reason: format!("{err:?}"),
    }
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct IceServer {
    pub url: String,
    pub username: Option<String>,
    pub credential: Option<String>,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct Args {
    pub id: NodeId,
    pub token: Option<String>,
    pub servers: Vec<String>,
    pub ice_servers: Vec<IceServer>,
    pub enable_packet_information: bool,
    pub encrypt_local_packets: bool,
    pub candidate_filter_prefixes: Vec<String>,
    pub addresses: Vec<String>,
    pub tun_fd: i32,
}

struct Handles {
    mesh: MeshHandle,
    igp: IgpHandle,
    mgr: ConnManagerHandle,
    registry: RegistryHandle,
    fm: FullMeshHandle,
    info: DaemonInfo,
}

async fn run(args: Args) -> Result<Handles> {
    let candidate_filter_prefixes = args
        .candidate_filter_prefixes
        .iter()
        .map(|prefix| AnyIpCidr::from_str(prefix))
        .collect::<Result<Vec<_>, _>>()?;
    let addresses = args
        .addresses
        .iter()
        .map(|address| IpAddr::from_str(address))
        .collect::<Result<Vec<_>, _>>()?;
    let info = DaemonInfo {
        id: args.id,
        version: env!("CARGO_PKG_VERSION").to_owned(),
        uptime_ms: 0,
        mode: "single-tun".to_owned(),
        interface: "tun".to_owned(),
        mtu: 1280,
        listen: "-".to_owned(),
        ctl_path: "-".to_owned(),
        encrypt_local_packets: args.encrypt_local_packets,
        packet_information: args.enable_packet_information,
        ice_servers: args
            .ice_servers
            .iter()
            .map(|server| server.url.clone())
            .collect(),
        candidate_filter_prefixes: args.candidate_filter_prefixes.clone(),
        local_ips: Vec::new(),
    };
    let ice_servers = args
        .ice_servers
        .into_iter()
        .map(|server| FullMeshIceServer {
            url: server.url,
            username: server.username,
            credential: server.credential,
        })
        .collect();

    let mesh = Mesh::new(args.id);
    let igp = Igp::new(args.id, mesh.clone()).await?;
    let mgr = ConnManager::new(
        args.id,
        mesh.clone(),
        args.token,
        args.servers,
        SocketAddr::from_str("0.0.0.0:0").unwrap(),
    )
    .await?;
    let ips = Arc::new(Mutex::new(HashMap::new()));
    // Safety: dup the fd so the tun owns a copy, the original stays with Android.
    let tun_fd = unsafe { BorrowedFd::borrow_raw(args.tun_fd) }
        .try_clone_to_owned()?
        .into_raw_fd();
    let dm = unsafe {
        SingleTunManager::new_from_fd(
            tun_fd,
            args.enable_packet_information,
            ips.clone(),
            addresses,
        )
    }?;
    let dm = Arc::new(Mutex::new(Box::new(dm) as Box<dyn DeviceManager>));
    let registry = Registry::new(
        args.id,
        mesh.clone(),
        dm.clone(),
        vec![ConnectionType::Ice, ConnectionType::DataChannel],
        ips,
    )
    .await?;
    let fm = FullMesh::new(
        args.id,
        mesh.clone(),
        registry.clone(),
        dm,
        ice_servers,
        candidate_filter_prefixes,
        args.encrypt_local_packets,
        vec![ConnectionType::Ice, ConnectionType::DataChannel],
    )
    .await?;
    Ok(Handles {
        mesh,
        igp,
        mgr,
        registry,
        fm,
        info,
    })
}

#[derive(uniffi::Object)]
pub struct Cryonet {
    mesh: MeshHandle,
    igp: IgpHandle,
    mgr: ConnManagerHandle,
    registry: RegistryHandle,
    fm: FullMeshHandle,
    info: DaemonInfo,
    started_at: Instant,
    stop: StdMutex<Option<oneshot::Sender<()>>>,
    thread: StdMutex<Option<std::thread::JoinHandle<()>>>,
}

impl Cryonet {
    fn signal_stop(&self) {
        if let Ok(mut stop) = self.stop.lock()
            && let Some(stop) = stop.take()
        {
            let _ = stop.send(());
        }
    }

    fn shutdown(&self) {
        self.signal_stop();
        if let Ok(mut thread) = self.thread.lock()
            && let Some(thread) = thread.take()
        {
            let _ = thread.join();
        }
    }
}

#[uniffi::export]
impl Cryonet {
    #[uniffi::constructor]
    pub async fn init(args: Args) -> std::result::Result<Arc<Self>, AndroidError> {
        let (ready_tx, ready_rx) = oneshot::channel::<std::result::Result<Handles, String>>();
        let (stop_tx, stop_rx) = oneshot::channel::<()>();
        let started_at = Instant::now();
        let thread = std::thread::Builder::new()
            .name("cryonet".to_owned())
            .spawn(move || {
                let rt = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(err) => {
                        let _ = ready_tx.send(Err(err.to_string()));
                        return;
                    }
                };
                let local = LocalSet::new();
                local.block_on(&rt, async move {
                    match run(args).await {
                        Ok(handles) => {
                            // The actor handles are `Send` on this target, so hand them back to the
                            // FFI thread. The `LocalSet` stays alive here until `stop` is called,
                            // which keeps the actors running.
                            let _ = ready_tx.send(Ok(handles));
                            let _ = stop_rx.await;
                        }
                        Err(err) => {
                            let _ = ready_tx.send(Err(format!("{err:?}")));
                        }
                    }
                });
            })
            .map_err(|err| AndroidError::Error {
                reason: err.to_string(),
            })?;
        let Handles {
            mesh,
            igp,
            mgr,
            registry,
            fm,
            info,
        } = ready_rx
            .await
            .map_err(|_| AndroidError::Error {
                reason: "cryonet thread exited".to_owned(),
            })?
            .map_err(|reason| AndroidError::Error { reason })?;
        Ok(Arc::new(Cryonet {
            mesh,
            igp,
            mgr,
            registry,
            fm,
            info,
            started_at,
            stop: StdMutex::new(Some(stop_tx)),
            thread: StdMutex::new(Some(thread)),
        }))
    }

    // Signals the daemon thread and returns; the join happens in Drop, so this never blocks.
    pub fn stop(&self) {
        self.signal_stop();
    }

    pub async fn get_info(&self) -> std::result::Result<DaemonInfo, AndroidError> {
        let mut info = self.info.clone();
        info.uptime_ms = self.started_at.elapsed().as_millis() as u64;
        info.local_ips = self.registry.get_local_ips().await.map_err(android_error)?;
        Ok(info)
    }

    pub async fn get_stats(&self) -> std::result::Result<Stats, AndroidError> {
        let uptime_ms = self.started_at.elapsed().as_millis() as u64;
        collect_stats(&self.mesh, &self.igp, &self.fm, &self.registry, uptime_ms)
            .await
            .map_err(android_error)
    }

    pub async fn get_links(&self) -> std::result::Result<Vec<LinkInfo>, AndroidError> {
        self.mesh.get_links().await.map_err(android_error)
    }

    pub async fn get_routes(&self) -> std::result::Result<HashMap<NodeId, NodeId>, AndroidError> {
        self.mesh.get_routes().await.map_err(android_error)
    }

    pub async fn get_igp_routes(&self) -> std::result::Result<Vec<IgpRoute>, AndroidError> {
        self.igp.get_routes().await.map_err(android_error)
    }

    pub async fn get_igp_status(&self) -> std::result::Result<IgpStatus, AndroidError> {
        self.igp.get_status().await.map_err(android_error)
    }

    pub async fn get_full_mesh_peers(&self) -> std::result::Result<FullMeshStatus, AndroidError> {
        self.fm.get_peers().await.map_err(android_error)
    }

    pub async fn get_nodes(&self) -> std::result::Result<Vec<NodeInfo>, AndroidError> {
        self.registry.get_nodes().await.map_err(android_error)
    }

    pub async fn get_servers(&self) -> std::result::Result<Vec<ServerInfo>, AndroidError> {
        self.mgr.get_servers().await.map_err(android_error)
    }
}

impl Drop for Cryonet {
    fn drop(&mut self) {
        self.shutdown();
    }
}
