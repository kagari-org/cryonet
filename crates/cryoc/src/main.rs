use std::time::Instant;

use anyhow::Result;
use clap::Parser;
use clap_num::maybe_hex;
use cryonet_uapi::{CryonetUapi, NodeId};
use tempfile::tempdir;
use tokio::net::UnixDatagram;

#[derive(Debug, Parser)]
struct Args {
    #[arg(short, default_value = "/run/cryonet/cryonet.ctl")]
    ctl_path: String,
    #[clap(subcommand)]
    subcommand: Subcommand,
}

#[derive(Debug, Parser)]
enum Subcommand {
    #[clap(alias = "l")]
    Links,
    #[clap(alias = "r")]
    Routes,
    #[clap(alias = "ir")]
    IgpRoutes,
    #[clap(alias = "fmp")]
    FullMeshPeers,
    Ping {
        #[clap(value_parser = maybe_hex::<NodeId>)]
        node_id: NodeId,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let dir = tempdir()?;
    let socket = UnixDatagram::bind(dir.path().join("cryonetc"))?;

    let time = Instant::now();
    let request = match args.subcommand {
        Subcommand::Links => CryonetUapi::GetLinks,
        Subcommand::Routes => CryonetUapi::GetRoutes,
        Subcommand::IgpRoutes => CryonetUapi::GetIgpRoutes,
        Subcommand::FullMeshPeers => CryonetUapi::GetFullMeshPeers,
        Subcommand::Ping { node_id } => CryonetUapi::Ping(node_id),
    };
    let request = serde_json::to_vec(&request)?;
    socket.send_to(&request, args.ctl_path).await?;

    let mut buf = [0u8; 16384];
    let len = socket.recv(&mut buf).await?;
    let response: CryonetUapi = serde_json::from_slice(&buf[..len])?;

    match response {
        CryonetUapi::GetLinksResponse(mut items) => {
            items.sort();
            println!("Links:");
            for item in items {
                println!("  {item:X}");
            }
        }
        CryonetUapi::GetRoutesResponse(hash_map) => {
            let mut routes: Vec<_> = hash_map.into_iter().collect();
            routes.sort_by_key(|(dst, _)| *dst);
            println!("Routes:");
            for (dst, next_hop) in routes {
                println!("  {dst:X} -> {next_hop:X}");
            }
        }
        CryonetUapi::GetIgpRoutesResponse(mut igp_routes) => {
            igp_routes.sort_by_key(|r| r.timeout_remaining_ms);
            println!("IGP Routes:");
            for route in igp_routes {
                println!(
                    "  dst: {dst:X}, from: {from:X}, metric: {metric}, computed_metric: {computed_metric}, seq: {seq}, selected: {selected}, timeout_remaining: {remaining}ms",
                    dst = route.dst,
                    from = route.from,
                    metric = route.metric,
                    computed_metric = route.computed_metric,
                    seq = route.seq,
                    selected = route.selected,
                    remaining = route.timeout_remaining_ms,
                );
            }
        }
        CryonetUapi::GetFullMeshPeersResponse(hash_map) => {
            let mut peers: Vec<_> = hash_map.into_iter().collect();
            peers.sort_by_key(|(node_id, _)| *node_id);
            println!("Full Mesh Peers:");
            for (node_id, conns) in peers {
                let mut conns: Vec<_> = conns.into_iter().collect();
                conns.sort_by(|a, b| b.1.elapsed_ms.cmp(&a.1.elapsed_ms));
                println!("  Node {node_id:X}:");
                for (uuid, conn) in conns {
                    println!("    Conn {uuid}: state: {:?}, selected: {}, candidate: {:?}, elapsed: {}ms", conn.state, conn.selected, conn.selected_candidate, conn.elapsed_ms);
                }
            }
        }
        CryonetUapi::Pong => {
            println!("Pong received, latency: {} ms", time.elapsed().as_millis());
        }
        _ => println!("Unexpected response: {response:?}"),
    }

    Ok(())
}
