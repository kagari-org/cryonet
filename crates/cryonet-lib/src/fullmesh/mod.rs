use std::{
    any::Any,
    net::{IpAddr, SocketAddr},
};

use anyhow::Result;
use async_trait::async_trait;
use bytes::Bytes;
use serde::{Deserialize, Serialize};

use crate::mesh::packet::NodeId;

#[cfg(not(target_arch = "wasm32"))]
pub mod conn_rustrtc_dc;
#[cfg(not(target_arch = "wasm32"))]
pub mod conn_rustrtc_ice;
#[cfg(not(target_arch = "wasm32"))]
pub mod fm_rustrtc_ice;
pub mod registry;
pub mod tap;
#[cfg(not(target_arch = "wasm32"))]
pub mod tun;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IceServer {
    pub url: String,
    pub username: Option<String>,
    pub credential: Option<String>,
}

#[async_trait]
pub trait DeviceManager: Send + Sync {
    async fn connected(
        &mut self,
        node_id: NodeId,
        sender: Box<dyn ConnectionSender>,
        receiver: Box<dyn ConnectionReceiver>,
    ) -> Result<()>;
    async fn disconnected(&mut self, node_id: NodeId) -> Result<()>;
    async fn ips(&self) -> Result<Vec<IpAddr>>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionState {
    Connecting,
    Connected,
    Closed,
}

#[async_trait]
pub trait Connection: Send + Sync + Any {
    async fn sender(&self) -> Result<Box<dyn ConnectionSender>>;
    async fn receiver(&self) -> Result<Box<dyn ConnectionReceiver>>;
    fn sent(&self) -> u64;
    fn received(&self) -> u64;
    fn status(&self) -> ConnectionState;
    async fn selected_candidate(&self) -> Option<String>;

    fn as_any(&self) -> &dyn Any;
    fn as_any_mut(&mut self) -> &mut dyn Any;
}

#[async_trait]
pub trait ConnectionSender: Send + Sync {
    async fn send(&mut self, data: Bytes) -> Result<usize>;
}

#[async_trait]
pub trait ConnectionReceiver: Send + Sync {
    async fn recv(&mut self) -> Result<(Bytes, SocketAddr)>;
}
