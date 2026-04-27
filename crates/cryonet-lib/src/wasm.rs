use std::{
    collections::HashMap,
    future::pending,
    net::{IpAddr, SocketAddr},
    rc::Rc,
    str::FromStr,
    sync::Arc,
};

use crate::{
    connection::{ConnManager, ConnManagerHandle},
    fullmesh::{
        DeviceManager, IceServer,
        fullmesh::{FullMesh, FullMeshHandle},
        registry::{ConnectionType, Registry, RegistryHandle},
        tap::{TapManager, generate_tap_mac},
    },
    mesh::{
        Mesh, MeshHandle,
        igp::{Igp, IgpHandle},
    },
};
use anyhow::{Result, anyhow, bail};
use cryonet_uapi::NodeId;
use serde::{Deserialize, Serialize};
use tokio::{sync::Mutex, task::LocalSet};
use tracing_subscriber::EnvFilter;
use tracing_web::MakeWebConsoleWriter;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;
use web_sys::js_sys::{Function, Promise, Uint8Array};

thread_local! {
    pub(crate) static LOCAL_SET: Rc<LocalSet> = Rc::new(LocalSet::new());
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Args {
    pub id: NodeId,
    pub token: Option<String>,
    pub servers: Vec<String>,
    pub ice_servers: Vec<IceServer>,
    pub enable_packet_information: bool,
    pub tap_mac_prefix: u16,
    pub addresses: Vec<IpAddr>,
}

pub struct AsyncDevice {
    send: Function,
    recv: Function,
    addresses: Vec<IpAddr>,
}

impl AsyncDevice {
    pub fn addresses(&self) -> Result<Vec<IpAddr>> {
        Ok(self.addresses.clone())
    }

    pub async fn send(&self, buf: &[u8]) -> Result<()> {
        let result = self
            .send
            .call1(&JsValue::NULL, &Uint8Array::from(buf))
            .map_err(|e| anyhow!("{e:?}"))?;
        if !result.is_instance_of::<Promise>() {
            return Ok(());
        }
        JsFuture::from(result.dyn_into::<Promise>().map_err(|e| anyhow!("{e:?}"))?)
            .await
            .map_err(|e| anyhow!("{e:?}"))?;
        Ok(())
    }

    pub async fn recv(&self, buf: &mut [u8]) -> Result<usize> {
        let result = self
            .recv
            .call0(&JsValue::NULL)
            .map_err(|e| anyhow!("{e:?}"))?;
        if !result.is_instance_of::<Promise>() {
            let data = result
                .dyn_into::<Uint8Array>()
                .map_err(|e| anyhow!("{e:?}"))?;
            let len = data.length() as usize;
            if len > buf.len() {
                bail!("buffer too small: {} > {}", len, buf.len());
            }
            data.copy_to(&mut buf[..len]);
            return Ok(len);
        }
        let promise = result.dyn_into::<Promise>().map_err(|e| anyhow!("{e:?}"))?;
        let data = JsFuture::from(promise)
            .await
            .map_err(|e| anyhow!("{e:?}"))?
            .dyn_into::<Uint8Array>()
            .map_err(|e| anyhow!("{e:?}"))?;
        let len = data.length() as usize;
        if len > buf.len() {
            bail!("buffer too small: {} > {}", len, buf.len());
        }
        data.copy_to(&mut buf[..len]);
        Ok(len)
    }
}

#[wasm_bindgen]
pub struct Cryonet {
    mesh: *mut MeshHandle,
    igp: *mut IgpHandle,
    mgr: *mut ConnManagerHandle,
    registry: *mut RegistryHandle,
    fm: *mut FullMeshHandle,

    tap_mac: [u8; 6],
}

#[wasm_bindgen]
impl Cryonet {
    pub async fn init(args: JsValue, send: Function, recv: Function) -> Result<Cryonet, JsValue> {
        let Args {
            id,
            token,
            servers,
            ice_servers,
            enable_packet_information,
            tap_mac_prefix,
            addresses,
        } = serde_wasm_bindgen::from_value(args)?;

        let _guard = LOCAL_SET.with(|local_set| local_set.enter());
        let mesh = Mesh::new(id);
        let igp = Igp::new(id, mesh.clone())
            .await
            .map_err(|e| JsValue::from_str(e.to_string().as_str()))?;
        let mgr = ConnManager::new(
            id,
            mesh.clone(),
            token,
            servers,
            SocketAddr::from_str("0.0.0.0:0").unwrap(),
        )
        .await
        .map_err(|e| JsValue::from_str(e.to_string().as_str()))?;
        let ips = Arc::new(Mutex::new(HashMap::new()));
        let dm = TapManager::new_with_device(
            id,
            tap_mac_prefix,
            enable_packet_information,
            ips.clone(),
            Arc::new(AsyncDevice { send, recv, addresses }),
        )
        .map_err(|e| JsValue::from_str(e.to_string().as_str()))?;
        let dm = Arc::new(Mutex::new(Box::new(dm) as Box<dyn DeviceManager>));
        let registry = Registry::new(
            mesh.clone(),
            dm.clone(),
            vec![ConnectionType::DataChannel],
            ips,
        )
        .await
        .map_err(|e| JsValue::from_str(e.to_string().as_str()))?;
        let fm = FullMesh::new(
            id,
            mesh.clone(),
            registry.clone(),
            dm,
            ice_servers,
            None,
            false,
        )
        .await
        .map_err(|e| JsValue::from_str(e.to_string().as_str()))?;

        Ok(Cryonet {
            mesh: Box::into_raw(Box::new(mesh)),
            igp: Box::into_raw(Box::new(igp)),
            mgr: Box::into_raw(Box::new(mgr)),
            registry: Box::into_raw(Box::new(registry)),
            fm: Box::into_raw(Box::new(fm)),
            tap_mac: generate_tap_mac(id, tap_mac_prefix),
        })
    }

    pub fn tap_mac(&self) -> Vec<u8> {
        self.tap_mac.to_vec()
    }
}

impl Drop for Cryonet {
    fn drop(&mut self) {
        unsafe {
            let _ = Box::from_raw(self.mesh);
            let _ = Box::from_raw(self.igp);
            let _ = Box::from_raw(self.mgr);
            let _ = Box::from_raw(self.registry);
            let _ = Box::from_raw(self.fm);
        }
    }
}

#[wasm_bindgen(start)]
fn main() -> Result<(), JsValue> {
    tracing_subscriber::fmt()
        .with_ansi(false)
        .without_time()
        .with_writer(MakeWebConsoleWriter::new())
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("debug")),
        )
        .init();
    wasm_bindgen_futures::spawn_local(async {
        let set = LOCAL_SET.with(|local_set| local_set.clone());
        set.run_until(pending::<()>()).await;
    });
    Ok(())
}
