use std::{future::pending, net::SocketAddr, rc::Rc, str::FromStr};

use crate::{
    connection::{ConnManager, ConnManagerHandle},
    fullmesh::{FullMesh, FullMeshHandle, IceServer},
    mesh::{
        Mesh, MeshHandle,
        igp::{Igp, IgpHandle},
    },
};
use anyhow::Result;
use cryonet_uapi::NodeId;
use serde::{Deserialize, Serialize};
use tokio::task::LocalSet;
use tracing_subscriber::EnvFilter;
use tracing_web::MakeWebConsoleWriter;
use wasm_bindgen::prelude::*;
use web_sys::{Event, EventTarget, js_sys::Function};

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
    fm: *mut FullMeshHandle,

    on_refresh: EventTarget,
}

#[wasm_bindgen]
impl Cryonet {
    pub async fn init(args: JsValue) -> Result<Cryonet, JsValue> {
        let Args { id, token, servers, ice_servers } = serde_wasm_bindgen::from_value(args)?;

        let _guard = LOCAL_SET.with(|local_set| local_set.enter());
        let mesh = Mesh::new(id);
        let igp = Igp::new(id, mesh.clone()).await.map_err(|e| JsValue::from_str(e.to_string().as_str()))?;
        let mgr = ConnManager::new(id, mesh.clone(), token, servers, SocketAddr::from_str("0.0.0.0:0").unwrap()).await.map_err(|e| JsValue::from_str(e.to_string().as_str()))?;
        let fm = FullMesh::new(id, mesh.clone(), ice_servers, None).await.map_err(|e| JsValue::from_str(e.to_string().as_str()))?;
        let mut refresh = fm.subscribe_refresh().await.map_err(|e| JsValue::from_str(e.to_string().as_str()))?;
        let refresh_et = EventTarget::new()?;
        let refresh2_et = refresh_et.clone();
        tokio::task::spawn_local(async move {
            while refresh.recv().await.is_ok() {
                let _ = refresh2_et.dispatch_event(&Event::new("refresh").unwrap());
            }
        });

        Ok(Cryonet {
            mesh: Box::into_raw(Box::new(mesh)),
            igp: Box::into_raw(Box::new(igp)),
            mgr: Box::into_raw(Box::new(mgr)),
            fm: Box::into_raw(Box::new(fm)),
            on_refresh: refresh_et,
        })
    }

    pub fn on_refresh(&self, callback: &Function) -> Result<(), JsValue> {
        self.on_refresh.add_event_listener_with_callback("refresh", callback)
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
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("debug")))
        .init();
    wasm_bindgen_futures::spawn_local(async {
        let set = LOCAL_SET.with(|local_set| local_set.clone());
        set.run_until(pending::<()>()).await;
    });
    Ok(())
}
