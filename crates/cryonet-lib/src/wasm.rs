use std::{net::SocketAddr, str::FromStr};

use anyhow::Result;
use cidr::AnyIpCidr;
use cryonet_uapi::NodeId;
use serde::{Deserialize, Serialize};
use wasm_bindgen::prelude::*;
use web_sys::{Event, EventTarget, js_sys::Function};
use crate::{connection::{ConnManager, ConnManagerHandle}, fullmesh::{FullMesh, FullMeshHandle, IceServer}, mesh::{Mesh, MeshHandle, igp::{Igp, IgpHandle}}};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Args {
    pub id: NodeId,
    pub token: Option<String>,
    pub servers: Vec<String>,
    pub ice_servers: Vec<IceServer>,
    pub candidate_filter_prefix: Option<String>,
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
        let Args { id, token, servers, ice_servers, candidate_filter_prefix } = serde_wasm_bindgen::from_value(args)?;
        let candidate_filter_prefix = match candidate_filter_prefix.map(|c| AnyIpCidr::from_str(&c)) {
            Some(Ok(cidr)) => Some(cidr),
            Some(Err(e)) => return Err(JsValue::from_str(e.to_string().as_str())),
            None => None,
        };

        let mesh = Mesh::new(id);
        let igp = Igp::new(id, mesh.clone()).await
            .map_err(|e| JsValue::from_str(e.to_string().as_str()))?;
        let mgr = ConnManager::new(id, mesh.clone(), token, servers, SocketAddr::from_str("0.0.0.0:0").unwrap()).await
            .map_err(|e| JsValue::from_str(e.to_string().as_str()))?;
        let fm = FullMesh::new(id, mesh.clone(), ice_servers, candidate_filter_prefix).await
            .map_err(|e| JsValue::from_str(e.to_string().as_str()))?;
        let mut refresh = fm.subscribe_refresh().await
            .map_err(|e| JsValue::from_str(e.to_string().as_str()))?;
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
