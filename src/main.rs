#![feature(try_blocks)]
use std::{future::pending, net::SocketAddr};

use anyhow::Result;
use clap::Parser;
use clap_num::maybe_hex;
use rustrtc::RtcConfiguration;

use crate::{
    connection::ConnManager,
    fullmesh::{FullMesh, tun::TunManager},
    mesh::{Mesh, igp::Igp, packet::NodeId},
};

pub(crate) mod connection;
pub(crate) mod errors;
pub(crate) mod fullmesh;
pub(crate) mod mesh;

#[derive(Debug, Parser)]
struct Args {
    #[clap(value_parser = maybe_hex::<NodeId>)]
    id: NodeId,
    #[clap(long, short)]
    token: Option<String>,
    #[clap(long, short, default_value = "0.0.0.0:2333")]
    listen: SocketAddr,
    #[clap(long, short, value_delimiter = ',')]
    servers: Vec<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let args = Args::parse();

    let mesh = Mesh::new(args.id);
    let igp = Igp::new(mesh.clone()).await;
    let mgr = ConnManager::new(mesh.clone(), args.token, args.servers, args.listen);
    let fm = FullMesh::new(mesh.clone(), RtcConfiguration::default()).await;
    let tm = TunManager::new(fm.clone()).await;

    pending::<()>().await;
    tm.lock().await.stop();
    fm.lock().await.stop();
    mgr.lock().await.stop();
    igp.lock().await.stop();
    mesh.lock().await.stop();
    Ok(())
}
