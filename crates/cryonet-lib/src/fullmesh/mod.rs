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
#[cfg(target_arch = "wasm32")]
pub mod conn_wasm;

pub mod fullmesh;
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

#[async_trait(?Send)]
pub trait DeviceManager {
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

#[async_trait(?Send)]
pub trait Connection: Any {
    async fn sender(&self) -> Result<Box<dyn ConnectionSender>>;
    async fn receiver(&self) -> Result<Box<dyn ConnectionReceiver>>;
    fn sent(&self) -> u64;
    fn received(&self) -> u64;
    fn status(&self) -> ConnectionState;
    async fn selected_candidate(&self) -> Option<String>;

    fn as_any(&self) -> &dyn Any;
    fn as_any_mut(&mut self) -> &mut dyn Any;
}

macro_rules! define_sender_receiver {
    ($($supertrait:path),*) => {
        #[cfg_attr(not(target_arch = "wasm32"), async_trait)]
        #[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
        pub trait ConnectionSender: $($supertrait +)* {
            async fn send(&mut self, data: Bytes) -> Result<usize>;
        }

        #[cfg_attr(not(target_arch = "wasm32"), async_trait)]
        #[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
        pub trait ConnectionReceiver: $($supertrait +)* {
            async fn recv(&mut self) -> Result<(Bytes, SocketAddr)>;
        }
    };
}

#[cfg(not(target_arch = "wasm32"))]
define_sender_receiver!(Send, Sync);
#[cfg(target_arch = "wasm32")]
define_sender_receiver!();
