use std::{
    collections::HashMap, future::pending, net::SocketAddr, rc::Rc, str::FromStr, sync::Arc,
};

use crate::{
    connection::{ConnManager, ConnManagerHandle},
    fullmesh::{
        IceServer,
        fullmesh::{FullMesh, FullMeshHandle},
        registry::{ConnectionType, Registry},
    },
    mesh::{
        Mesh, MeshHandle,
        igp::{Igp, IgpHandle},
    },
};
use anyhow::Result;
use cryonet_uapi::NodeId;
use serde::{Deserialize, Serialize};
use tokio::{sync::Mutex, task::LocalSet};
use tracing_subscriber::EnvFilter;
use tracing_web::MakeWebConsoleWriter;
use wasm_bindgen::prelude::*;
use web_sys::{
    Event, EventTarget,
    js_sys::{Array, Function, Map},
};

thread_local! {
    pub(crate) static LOCAL_SET: Rc<LocalSet> = Rc::new(LocalSet::new());
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Args {
    pub id: NodeId,
    pub token: Option<String>,
    pub servers: Vec<String>,
    pub ice_servers: Vec<IceServer>,
}

#[wasm_bindgen]
pub struct Cryonet {
    mesh: *mut MeshHandle,
    igp: *mut IgpHandle,
    mgr: *mut ConnManagerHandle,
    registry: *mut Registry,
    fm: *mut FullMeshHandle,
}

#[wasm_bindgen]
impl Cryonet {
    pub async fn init(args: JsValue) -> Result<Cryonet, JsValue> {
        let Args {
            id,
            token,
            servers,
            ice_servers,
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
        let dm = todo!();
        let registry = Registry::new(
            mesh.clone(),
            dm.clone(),
            vec![ConnectionType::DataChannel],
            ips.clone(),
        )
        .await
        .map_err(|e| JsValue::from_str(e.to_string().as_str()))?;
        let fm = FullMesh::new(
            id,
            mesh.clone(),
            registry.clone(),
            dm.clone(),
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
        })
    }
}

impl Drop for Cryonet {
    fn drop(&mut self) {
        unsafe {
            let _ = Box::from_raw(self.mesh);
            let _ = Box::from_raw(self.igp);
            let _ = Box::from_raw(self.mgr);
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
