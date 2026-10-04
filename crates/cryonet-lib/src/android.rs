use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr},
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
};
use anyhow::Result;
use cidr::AnyIpCidr;
use tokio::{sync::Mutex, task::LocalSet};

#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum AndroidError {
    #[error("{message}")]
    Error { message: String },
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
    // Safety: Android passes a valid, open tun fd and transfers its ownership.
    let dm = unsafe {
        SingleTunManager::new_from_fd(
            args.tun_fd,
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
    stop: StdMutex<Option<tokio::sync::oneshot::Sender<()>>>,
    thread: StdMutex<Option<std::thread::JoinHandle<()>>>,
}

impl Cryonet {
    fn shutdown(&self) {
        if let Ok(mut stop) = self.stop.lock()
            && let Some(stop) = stop.take()
        {
            let _ = stop.send(());
        }
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
    pub fn init(args: Args) -> std::result::Result<Arc<Self>, AndroidError> {
        let (ready_tx, ready_rx) =
            std::sync::mpsc::sync_channel::<std::result::Result<(), String>>(1);
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
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
                        Ok(_handles) => {
                            let _ = ready_tx.send(Ok(()));
                            let _ = stop_rx.await;
                        }
                        Err(err) => {
                            let _ = ready_tx.send(Err(format!("{err:?}")));
                        }
                    }
                });
            })
            .map_err(|err| AndroidError::Error {
                message: err.to_string(),
            })?;
        ready_rx
            .recv()
            .map_err(|err| AndroidError::Error {
                message: err.to_string(),
            })?
            .map_err(|message| AndroidError::Error { message })?;
        Ok(Arc::new(Cryonet {
            stop: StdMutex::new(Some(stop_tx)),
            thread: StdMutex::new(Some(thread)),
        }))
    }

    pub fn stop(&self) {
        self.shutdown();
    }
}

impl Drop for Cryonet {
    fn drop(&mut self) {
        self.shutdown();
    }
}
