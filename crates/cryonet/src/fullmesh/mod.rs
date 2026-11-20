use std::{collections::HashMap, future::Future, net::IpAddr, pin::Pin};

use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::{
    fullmesh::ice::{IceReceiver, IceSender},
    mesh::packet::NodeId,
    utils::Instant,
};

pub(crate) mod ice;
pub(crate) mod manager;
pub(crate) mod registry;
pub(crate) mod tap;
pub(crate) mod tun;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct IceServer {
    pub(crate) url: String,
    pub(crate) username: Option<String>,
    pub(crate) credential: Option<String>,
}

#[derive(Clone, Copy)]
pub(crate) struct IpEntry {
    pub(crate) node: NodeId,
    pub(crate) claim: u64,
    pub(crate) seen: Instant,
}

pub(crate) type IpMap = HashMap<IpAddr, IpEntry>;

#[async_trait(?Send)]
pub(crate) trait DeviceManager {
    async fn connected(
        &mut self,
        node_id: NodeId,
        sender: IceSender,
        receiver: IceReceiver,
    ) -> Result<()>;
    async fn disconnected(&mut self, node_id: NodeId) -> Result<()>;
    async fn ips(&self) -> Result<Vec<IpAddr>>;
    async fn neighbor_changed(&mut self, _ip: IpAddr) -> Result<()> {
        Ok(())
    }
}

pub(crate) type NotifyFn =
    Box<dyn Fn() -> Pin<Box<dyn Future<Output = Result<()>> + Send>> + Send + Sync>;

// Pick one side of the pair deterministically so both peers agree.
fn pair_initiator(a: NodeId, b: NodeId) -> NodeId {
    let (lo, hi) = if a < b { (a, b) } else { (b, a) };
    let mut hash = lo ^ hi.rotate_left(32);
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
    fn pair_initiator_handles_wide_ids() {
        let ids = [
            1u64,
            0x1234_5678_9abc_def0,
            0x8000_0000_0000_0001,
            0xffff_ffff_ffff_fffe,
            0x0000_0001_0000_0000,
        ];
        for &a in &ids {
            for &b in &ids {
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
