#![feature(try_blocks)]
use std::{env::var, net::SocketAddr, path::PathBuf, str::FromStr};

use anyhow::{Result, anyhow};
use cidr::AnyIpCidr;
use clap::Parser;
use clap_num::maybe_hex;
use rustrtc::{IceCredentialType, IceServer, RtcConfiguration};
use tokio::signal::ctrl_c;
use tracing_subscriber::EnvFilter;

use crate::{
    connection::ConnManager,
    fullmesh::{FullMesh, tun::TunManager},
    mesh::{Mesh, igp::Igp, packet::NodeId},
    uapi::Uapi,
};

pub(crate) mod connection;
pub(crate) mod errors;
pub(crate) mod fullmesh;
pub(crate) mod mesh;
pub(crate) mod uapi;

#[derive(Debug, Parser)]
struct Args {
    #[clap(env, value_parser = maybe_hex::<NodeId>)]
    id: NodeId,
    #[clap(env, long, short)]
    token: Option<String>,
    #[clap(env, long, short, default_value = "0.0.0.0:2333")]
    listen: SocketAddr,
    #[clap(env, long, short, value_delimiter = ',')]
    servers: Vec<String>,
    #[clap(env, long, short, value_parser = parse_rtc_ice_server, value_delimiter = ',')]
    ice_servers: Vec<IceServer>,
    #[clap(env, long, short, value_parser = AnyIpCidr::from_str)]
    candidate_filter_prefix: Option<AnyIpCidr>,
    #[clap(env, long, default_value = "cn")]
    interface_prefix: String,
    #[clap(env, long, default_value_t = false)]
    enable_packet_information: bool,
    #[clap(env, long)]
    ctl_path: Option<PathBuf>,
}

fn parse_rtc_ice_server(input: &str) -> Result<IceServer> {
    let parts: Vec<_> = input.split('|').collect();
    if parts.len() == 1 {
        Ok(IceServer {
            urls: vec![parts[0].to_string()],
            ..Default::default()
        })
    } else if parts.len() == 3 {
        Ok(IceServer {
            urls: vec![parts[0].to_string()],
            credential_type: IceCredentialType::Password,
            username: Some(parts[1].to_string()),
            credential: Some(parts[2].to_string()),
        })
    } else {
        Err(anyhow!("Unexpected ice server: {input}"))
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt().with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info,rustrtc=off"))).init();
    let args = Args::parse();
    let rtc_configuration = RtcConfiguration {
        ice_servers: args.ice_servers.clone(),
        ..Default::default()
    };
    let runtime_directory = var("RUNTIME_DIRECTORY");
    let ctl_path = match (args.ctl_path, runtime_directory) {
        (Some(path), _) => path,
        (None, Ok(dir)) => PathBuf::from(dir).join("cryonet.ctl"),
        (None, Err(_)) => PathBuf::from("cryonet.ctl"),
    };

    let mesh = Mesh::new(args.id);
    let igp = Igp::new(args.id, mesh.clone()).await.map_err(|e| anyhow!("{e}"))?;
    let _mgr = ConnManager::new(args.id, mesh.clone(), args.token, args.servers, args.listen).await.map_err(|e| anyhow!("{e}"))?;
    let fm = FullMesh::new(args.id, mesh.clone(), rtc_configuration, args.candidate_filter_prefix).await.map_err(|e| anyhow!("{e}"))?;
    let _tm = TunManager::new(fm.clone(), args.interface_prefix, args.enable_packet_information).await.map_err(|e| anyhow!("{e}"))?;
    let _uapi = Uapi::new(mesh.clone(), igp.clone(), fm.clone(), ctl_path).await.map_err(|e| anyhow!("{e}"))?;

    ctrl_c().await?;
    Ok(())
}
