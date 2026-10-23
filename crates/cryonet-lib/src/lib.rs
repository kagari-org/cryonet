#![feature(try_blocks)]
#![allow(clippy::new_ret_no_self)]

use anyhow::Result;
use cryonet_uapi::{ConnState, FullMeshStats, Stats};
use fullmesh::{fullmesh::FullMeshHandle, registry::RegistryHandle};
use mesh::{MeshHandle, igp::IgpHandle};

pub mod connection;
pub mod errors;
pub mod fullmesh;
pub mod mesh;

#[cfg(target_os = "android")]
uniffi::setup_scaffolding!();

#[cfg(target_os = "android")]
pub mod android;

#[cfg(target_arch = "wasm32")]
pub mod wasm;

mod time;

pub async fn collect_stats(
    mesh: &MeshHandle,
    igp: &IgpHandle,
    fm: &FullMeshHandle,
    registry: &RegistryHandle,
    uptime_ms: u64,
) -> Result<Stats> {
    let status = fm.get_peers().await?;
    let fullmesh = FullMeshStats {
        peers: status.peers.len() as u64,
        connected: status
            .peers
            .iter()
            .filter(|peer| peer.state == ConnState::Connected)
            .count() as u64,
        connecting: status
            .peers
            .iter()
            .filter(|peer| peer.state == ConnState::Connecting)
            .count() as u64,
        pending: status.pending.len() as u64,
        sent: status.peers.iter().map(|peer| peer.sent).sum(),
        received: status.peers.iter().map(|peer| peer.received).sum(),
    };
    Ok(Stats {
        uptime_ms,
        mesh: mesh.get_stats().await?,
        igp: igp.get_stats().await?,
        fullmesh,
        registry: registry.get_stats().await?,
    })
}
