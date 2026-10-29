#![feature(try_blocks)]
#![allow(clippy::new_ret_no_self)]
use std::{
    collections::HashMap,
    env::var,
    future::pending,
    net::{Ipv6Addr, SocketAddr},
    path::PathBuf,
    str::FromStr,
    sync::Arc,
};

use anyhow::{Result, bail};
use cidr::{AnyIpCidr, Ipv6Cidr};
use clap::Parser;
use cryonet_lib::{
    connection::ConnManager,
    fullmesh::{
        DeviceManager, IceServer,
        fullmesh::FullMesh,
        registry::{ConnectionType, Registry},
        single_tun::SingleTunManager,
        tap::TapManager,
        tun::TunManager,
    },
    mesh::{
        Mesh,
        igp::Igp,
        packet::{random_node_id, random_u64},
    },
};
use cryonet_uapi::DaemonInfo;
use tokio::{sync::Mutex, task::LocalSet};
use tracing_subscriber::EnvFilter;

use crate::uapi::Uapi;

mod uapi;

#[derive(Debug, Parser)]
struct Args {
    #[clap(env = "NODE_NAME", long)]
    name: Option<String>,
    #[clap(env = "NODE_ID", long, value_parser = clap_num::maybe_hex::<u64>)]
    id: Option<u64>,
    #[clap(env, long, short)]
    token: Option<String>,
    #[clap(env, long, short, default_value = "0.0.0.0:2333")]
    listen: SocketAddr,
    #[clap(env, long, short, value_delimiter = ',')]
    servers: Vec<String>,
    #[clap(env, long, short, value_parser = parse_rtc_ice_server, value_delimiter = ',')]
    ice_servers: Vec<IceServer>,
    #[clap(env, long, short, value_parser = AnyIpCidr::from_str, value_delimiter = ',')]
    candidate_filter_prefixes: Vec<AnyIpCidr>,
    #[clap(env, long, default_value_t = false)]
    encrypt_local_packets: bool,
    #[clap(env, long, default_value = "cn")]
    interface_prefix: String,
    #[clap(env, long, default_value_t = false)]
    enable_packet_information: bool,
    #[clap(env, long)]
    ipv6_subnet: Option<Ipv6Cidr>,
    #[clap(env, long)]
    ctl_path: Option<PathBuf>,

    #[clap(env, long, default_value_t = false)]
    tap_mode: bool,
    #[clap(env, long, default_value = "cn0")]
    tap_interface_name: String,
    #[clap(env, long, default_value_t = false, conflicts_with = "tap_mode")]
    single_tun_mode: bool,
    #[clap(env, long, default_value = "cn0")]
    single_tun_interface_name: String,
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
        bail!("Unexpected ice server: {input}");
    }
}

fn default_name() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|name| name.trim().to_owned())
        .unwrap_or_default()
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("info,rustrtc=off")),
        )
        .init();
    let args = Args::parse();
    let ctl_path = match (
        args.ctl_path,
        var("RUNTIME_DIRECTORY"),
        var("XDG_RUNTIME_DIR"),
    ) {
        (Some(path), _, _) => path,
        (None, Ok(dir), _) => PathBuf::from(dir).join("cryonet.ctl"),
        (None, Err(_), Ok(dir)) => PathBuf::from(dir).join("cryonet").join("cryonet.ctl"),
        (None, Err(_), Err(_)) => PathBuf::from("cryonet.ctl"),
    };

    LocalSet::new()
        .run_until(async move {
            let result: Result<()> = try {
                let started_at = std::time::Instant::now();
                let id = args.id.unwrap_or_else(random_node_id);
                let name = args
                    .name
                    .filter(|name| !name.is_empty())
                    .unwrap_or_else(default_name);
                let name: String = name.trim().chars().take(64).collect();
                let ipv6 = args
                    .ipv6_subnet
                    .map(|subnet| -> Result<Ipv6Addr> {
                        if subnet.network_length() != 64 {
                            bail!("IPv6 subnet must be a /64");
                        }
                        let mut octets = subnet.first_address().octets();
                        octets[8..].copy_from_slice(&random_u64().max(1).to_be_bytes());
                        Ok(Ipv6Addr::from(octets))
                    })
                    .transpose()?;
                let (mode, interface) = if args.tap_mode {
                    ("tap".to_owned(), args.tap_interface_name.clone())
                } else if args.single_tun_mode {
                    (
                        "single-tun".to_owned(),
                        args.single_tun_interface_name.clone(),
                    )
                } else {
                    ("tun".to_owned(), format!("{}<node>", args.interface_prefix))
                };
                let info = DaemonInfo {
                    id,
                    name: name.clone(),
                    version: env!("CARGO_PKG_VERSION").to_owned(),
                    uptime_ms: 0,
                    mode,
                    interface,
                    mtu: 1280,
                    listen: args.listen.to_string(),
                    ctl_path: ctl_path.display().to_string(),
                    encrypt_local_packets: args.encrypt_local_packets,
                    packet_information: args.enable_packet_information,
                    ice_servers: args
                        .ice_servers
                        .iter()
                        .map(|server| server.url.clone())
                        .collect(),
                    candidate_filter_prefixes: args
                        .candidate_filter_prefixes
                        .iter()
                        .map(|prefix| prefix.to_string())
                        .collect(),
                    local_ips: Vec::new(),
                };
                let mesh = Mesh::new(id);
                let igp = Igp::new(id, mesh.clone()).await?;
                let mgr = ConnManager::new(id, mesh.clone(), args.token, args.servers, args.listen)
                    .await?;
                let ips = Arc::new(Mutex::new(HashMap::new()));
                let dm: Arc<Mutex<Box<dyn DeviceManager>>> = if args.tap_mode {
                    Arc::new(Mutex::new(Box::new(TapManager::new(
                        args.tap_interface_name,
                        args.enable_packet_information,
                        ips.clone(),
                        ipv6,
                    )?)))
                } else if args.single_tun_mode {
                    Arc::new(Mutex::new(Box::new(SingleTunManager::new(
                        args.single_tun_interface_name,
                        args.enable_packet_information,
                        ips.clone(),
                        ipv6,
                    )?)))
                } else {
                    Arc::new(Mutex::new(Box::new(TunManager::new(
                        args.interface_prefix,
                        args.enable_packet_information,
                        ipv6,
                    ))))
                };
                let registry = Registry::new(
                    id,
                    mesh.clone(),
                    dm.clone(),
                    vec![ConnectionType::Ice, ConnectionType::DataChannel],
                    name,
                    ips,
                )
                .await?;
                let fm = FullMesh::new(
                    id,
                    mesh.clone(),
                    registry.clone(),
                    dm.clone(),
                    args.ice_servers,
                    args.candidate_filter_prefixes,
                    args.encrypt_local_packets,
                    vec![ConnectionType::Ice, ConnectionType::DataChannel],
                )
                .await?;
                let _uapi =
                    Uapi::new(mesh, igp, fm, registry, mgr, info, started_at, ctl_path).await?;

                pending().await
            };
            result
        })
        .await?;
    Ok(())
}
