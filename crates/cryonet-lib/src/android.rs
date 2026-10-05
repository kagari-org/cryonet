use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr},
    os::fd::{BorrowedFd, IntoRawFd},
    str::FromStr,
    sync::{Arc, Mutex as StdMutex},
};

use crate::{
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
        packet::NodeId,
    },
    time::Instant,
};
use anyhow::Result;
use cidr::AnyIpCidr;
use cryonet_uapi::{Conn, IgpRoute};
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

type Handles = (
    MeshHandle,
    IgpHandle,
    ConnManagerHandle,
    RegistryHandle,
    FullMeshHandle,
);

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
    )
    .await?;
    Ok((mesh, igp, mgr, registry, fm))
}

#[derive(uniffi::Object)]
pub struct Cryonet {
    mesh: MeshHandle,
    igp: IgpHandle,
    _mgr: ConnManagerHandle,
    _registry: RegistryHandle,
    fm: FullMeshHandle,
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
        let (mesh, igp, _mgr, _registry, fm) = ready_rx
            .await
            .map_err(|_| AndroidError::Error {
                reason: "cryonet thread exited".to_owned(),
            })?
            .map_err(|reason| AndroidError::Error { reason })?;
        Ok(Arc::new(Cryonet {
            mesh,
            igp,
            _mgr,
            _registry,
            fm,
            stop: StdMutex::new(Some(stop_tx)),
            thread: StdMutex::new(Some(thread)),
        }))
    }

    // Signals the daemon thread and returns; the join happens in Drop, so this never blocks.
    pub fn stop(&self) {
        self.signal_stop();
    }

    pub async fn get_links(&self) -> std::result::Result<Vec<NodeId>, AndroidError> {
        self.mesh.get_links().await.map_err(android_error)
    }

    pub async fn get_routes(&self) -> std::result::Result<HashMap<NodeId, NodeId>, AndroidError> {
        self.mesh.get_routes().await.map_err(android_error)
    }

    pub async fn get_igp_routes(&self) -> std::result::Result<Vec<IgpRoute>, AndroidError> {
        let routes = self.igp.get_routes().await.map_err(android_error)?;
        let now = Instant::now();
        Ok(routes
            .into_iter()
            .map(|route| IgpRoute {
                seq: route.metric.seq.0,
                metric: route.metric.metric,
                computed_metric: route.computed_metric,
                dst: route.dst,
                from: route.from,
                selected: route.selected,
                timeout_remaining_ms: if route.timeout > now {
                    route.timeout.duration_since(now).as_millis() as i64
                } else {
                    -(now.duration_since(route.timeout).as_millis() as i64)
                },
            })
            .collect())
    }

    pub async fn get_full_mesh_peers(
        &self,
    ) -> std::result::Result<HashMap<NodeId, Conn>, AndroidError> {
        self.fm.get_peers().await.map_err(android_error)
    }
}

impl Drop for Cryonet {
    fn drop(&mut self) {
        self.shutdown();
    }
}
