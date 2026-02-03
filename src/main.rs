#![feature(try_blocks)]
use std::{net::SocketAddr, str::FromStr};

use anyhow::{Result, anyhow};
use cidr::AnyIpCidr;
use clap::Parser;
use clap_num::maybe_hex;
use rustrtc::{IceCredentialType, IceServer, RtcConfiguration};
use tokio::signal::ctrl_c;

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
    #[clap(long, short, value_parser = parse_rtc_ice_server, value_delimiter = ',')]
    ice_servers: Vec<IceServer>,
    #[clap(long, short, value_parser = AnyIpCidr::from_str)]
    candidate_filter_prefix: Option<AnyIpCidr>,
    #[clap(long, default_value = "cn")]
    interface_prefix: String,
    #[clap(long, default_value_t = false)]
    enable_packet_information: bool,
}

fn parse_rtc_ice_server(input: &str) -> Result<IceServer> {
    let splited: Vec<_> = input.split('|').collect();
    if splited.len() == 1 {
        Ok(IceServer {
            urls: vec![splited[0].to_string()],
            ..Default::default()
        })
    } else if splited.len() == 3 {
        Ok(IceServer {
            urls: vec![splited[0].to_string()],
            credential_type: IceCredentialType::Password,
            username: Some(splited[1].to_string()),
            credential: Some(splited[2].to_string()),
        })
    } else {
        Err(anyhow!("Unexpected ice server: {input}"))
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let args = Args::parse();
    let rtc_configuration = RtcConfiguration {
        ice_servers: args.ice_servers.clone(),
        ..Default::default()
    };

    let mesh = Mesh::new(args.id);
    let igp = Igp::new(mesh.clone()).await;
    let mgr = ConnManager::new(mesh.clone(), args.token, args.servers, args.listen);
    let fm = FullMesh::new(
        mesh.clone(),
        rtc_configuration,
        args.candidate_filter_prefix,
    )
    .await;
    let tm = TunManager::new(
        fm.clone(),
        args.interface_prefix,
        args.enable_packet_information,
    )
    .await;

    ctrl_c().await?;

    tm.lock().await.stop();
    fm.lock().await.stop();
    mgr.lock().await.stop();
    igp.lock().await.stop();
    mesh.lock().await.stop();
    Ok(())
}
