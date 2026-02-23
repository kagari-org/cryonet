#![feature(try_blocks)]
use std::{env::var, net::SocketAddr, path::PathBuf, str::FromStr};

use anyhow::{Result, anyhow};
use cidr::AnyIpCidr;
use clap::Parser;
use clap_num::maybe_hex;
use cryonet_lib::{connection::ConnManager, fullmesh::{FullMesh, IceServer, tun::TunManager}, mesh::{Mesh, igp::Igp}};
use cryonet_uapi::NodeId;
use sactor::error::SactorResult;
use tokio::{signal::ctrl_c, task::LocalSet};
use tracing_subscriber::EnvFilter;

use crate::uapi::Uapi;

mod uapi;

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
            url: parts[0].to_string(),
            ..Default::default()
        })
    } else if parts.len() == 3 {
        Ok(IceServer {
            url: parts[0].to_string(),
            username: Some(parts[1].to_string()),
            credential: Some(parts[2].to_string()),
        })
    } else {
        Err(anyhow!("Unexpected ice server: {input}"))
    }
}

#[tokio::main]
async fn main() -> SactorResult<()> {
    tracing_subscriber::fmt().with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info,rustrtc=off"))).init();
    let args = Args::parse();
    let runtime_directory = var("RUNTIME_DIRECTORY");
    let ctl_path = match (args.ctl_path, runtime_directory) {
        (Some(path), _) => path,
        (None, Ok(dir)) => PathBuf::from(dir).join("cryonet.ctl"),
        (None, Err(_)) => PathBuf::from("cryonet.ctl"),
    };

    LocalSet::new().run_until(async move {
        let result: SactorResult<()> = try {
            let mesh = Mesh::new(args.id);
            let igp = Igp::new(args.id, mesh.clone()).await?;
            let _mgr = ConnManager::new(args.id, mesh.clone(), args.token, args.servers, args.listen).await?;
            let fm = FullMesh::new(args.id, mesh.clone(), args.ice_servers, args.candidate_filter_prefix).await?;
            let _tm = TunManager::new(fm.clone(), args.interface_prefix, args.enable_packet_information).await?;
            let _uapi = Uapi::new(mesh.clone(), igp.clone(), fm.clone(), ctl_path).await?;

            ctrl_c().await?;
        };
        result
    }).await?;
    Ok(())
}
