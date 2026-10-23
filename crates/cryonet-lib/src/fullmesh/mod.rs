use std::{
    any::Any,
    future::Future,
    net::{IpAddr, SocketAddr},
    pin::Pin,
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
#[cfg(not(target_arch = "wasm32"))]
pub mod single_tun;
#[cfg(not(target_os = "android"))]
pub mod tap;
#[cfg(all(not(target_arch = "wasm32"), not(target_os = "android")))]
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

#[cfg(not(target_arch = "wasm32"))]
pub type NotifyFn = Box<dyn Fn() -> Pin<Box<dyn Future<Output = Result<()>> + Send>> + Send + Sync>;
#[cfg(target_arch = "wasm32")]
pub type NotifyFn = Box<dyn Fn() -> Pin<Box<dyn Future<Output = Result<()>>>>>;

// Pick one side of the pair deterministically so both peers agree.
fn pair_initiator(a: NodeId, b: NodeId) -> NodeId {
    let (lo, hi) = if a < b { (a, b) } else { (b, a) };
    let mut hash = ((lo as u64) << 32) | hi as u64;
    // splitmix64 finalizer, stable across runs and versions
    hash ^= hash >> 30;
    hash = hash.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    hash ^= hash >> 27;
    hash = hash.wrapping_mul(0x94d0_49bb_1331_11eb);
    hash ^= hash >> 31;
    if hash & 1 == 0 { lo } else { hi }
}

fn is_initiator(me: NodeId, peer: NodeId) -> bool {
    pair_initiator(me, peer) == me
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pair_initiator_is_symmetric() {
        for a in 0..256 {
            for b in 0..256 {
                let initiator = pair_initiator(a, b);
                assert_eq!(initiator, pair_initiator(b, a));
                assert!(initiator == a || initiator == b);
                if a != b {
                    assert_ne!(is_initiator(a, b), is_initiator(b, a));
                }
            }
        }
    }

    #[test]
    fn pair_initiator_is_balanced() {
        let n = 64;
        let mut counts = vec![0usize; n as usize];
        for a in 0..n {
            for b in (a + 1)..n {
                counts[pair_initiator(a, b) as usize] += 1;
            }
        }
        let average = (n - 1) as f64 / 2.0;
        for count in counts {
            let ratio = count as f64 / average;
            assert!(
                (0.5..=1.5).contains(&ratio),
                "unbalanced: {count} vs {average}"
            );
        }
    }
}
