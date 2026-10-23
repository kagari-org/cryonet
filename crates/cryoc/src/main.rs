use std::{
    env::var,
    path::PathBuf,
    time::{Duration, Instant},
};

use anyhow::{Result, bail};
use clap::Parser;
use clap_num::maybe_hex;
use cryonet_uapi::{ConnState, CryonetUapi, NodeId};
use tempfile::tempdir;
use tokio::net::UnixDatagram;

fn format_state(state: ConnState) -> &'static str {
    match state {
        ConnState::Connecting => "connecting",
        ConnState::Connected => "connected",
        ConnState::Closed => "closed",
    }
}

fn format_bytes(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    for unit in UNITS {
        if value < 1024.0 {
            return format!("{value:.2} {unit}");
        }
        value /= 1024.0;
    }
    format!("{value:.2} PiB")
}

fn format_ms(ms: i64) -> String {
    let (sign, ms) = if ms < 0 {
        ("-", ms.unsigned_abs())
    } else {
        ("", ms as u64)
    };
    let body = if ms < 1_000 {
        format!("{ms}ms")
    } else if ms < 60_000 {
        format!("{:.1}s", ms as f64 / 1_000.0)
    } else if ms < 3_600_000 {
        let secs = ms / 1_000;
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else {
        let secs = ms / 1_000;
        format!("{}h{:02}m", secs / 3_600, (secs % 3_600) / 60)
    };
    format!("{sign}{body}")
}

fn format_metric(metric: u32) -> String {
    if metric == u32::MAX {
        "inf".to_owned()
    } else {
        metric.to_string()
    }
}

fn format_cost(cost: u32) -> String {
    if cost == u32::MAX {
        "unknown".to_owned()
    } else {
        cost.to_string()
    }
}

fn table(headers: &[&str], rows: &[Vec<String>]) {
    let widths = headers
        .iter()
        .enumerate()
        .map(|(i, header)| {
            rows.iter()
                .map(|row| row[i].chars().count())
                .chain(std::iter::once(header.len()))
                .max()
                .unwrap_or(0)
        })
        .collect::<Vec<_>>();
    let render = |cells: &[String]| {
        let mut line = String::from("  ");
        for (i, cell) in cells.iter().enumerate() {
            if i > 0 {
                line.push_str("  ");
            }
            line.push_str(cell);
            if i + 1 < cells.len() {
                for _ in 0..widths[i].saturating_sub(cell.chars().count()) {
                    line.push(' ');
                }
            }
        }
        line
    };
    let header_cells = headers
        .iter()
        .map(|header| header.to_uppercase())
        .collect::<Vec<_>>();
    println!("{}", render(&header_cells));
    for row in rows {
        println!("{}", render(row));
    }
}

fn kv(label: &str, value: impl std::fmt::Display) {
    println!("  {label:<12}{value}");
}

fn join_or_dash(values: &[String]) -> String {
    if values.is_empty() {
        "-".to_owned()
    } else {
        values.join(", ")
    }
}

#[derive(Debug, Parser)]
struct Args {
    #[arg(short)]
    ctl_path: Option<PathBuf>,
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
    #[clap(alias = "is")]
    IgpStatus,
    #[clap(alias = "fmp")]
    FullMeshPeers,
    #[clap(alias = "i")]
    Info,
    #[clap(alias = "s")]
    Stats,
    #[clap(alias = "n")]
    Nodes,
    #[clap(alias = "sv")]
    Servers,
    Ping {
        #[clap(value_parser = maybe_hex::<NodeId>)]
        node_id: NodeId,
    },
}

fn resolve_ctl_path(explicit: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(path) = explicit {
        return Ok(path);
    }
    let mut candidates = vec![PathBuf::from("/run/cryonet/cryonet.ctl")];
    if let Ok(dir) = var("XDG_RUNTIME_DIR") {
        candidates.push(PathBuf::from(dir).join("cryonet").join("cryonet.ctl"));
    }
    if let Some(path) = candidates.iter().find(|path| path.exists()) {
        return Ok(path.clone());
    }
    bail!(
        "no Cryonet control socket found, tried: {}",
        candidates
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    )
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let ctl_path = resolve_ctl_path(args.ctl_path)?;
    let dir = tempdir()?;
    let socket = UnixDatagram::bind(dir.path().join("cryonetc"))?;

    let time = Instant::now();
    let request = match args.subcommand {
        Subcommand::Links => CryonetUapi::GetLinks,
        Subcommand::Routes => CryonetUapi::GetRoutes,
        Subcommand::IgpRoutes => CryonetUapi::GetIgpRoutes,
        Subcommand::IgpStatus => CryonetUapi::GetIgpStatus,
        Subcommand::FullMeshPeers => CryonetUapi::GetFullMeshPeers,
        Subcommand::Info => CryonetUapi::GetInfo,
        Subcommand::Stats => CryonetUapi::GetStats,
        Subcommand::Nodes => CryonetUapi::GetNodes,
        Subcommand::Servers => CryonetUapi::GetServers,
        Subcommand::Ping { node_id } => CryonetUapi::Ping(node_id),
    };
    let timeout = match request {
        CryonetUapi::Ping(_) => Duration::from_secs(65),
        _ => Duration::from_secs(10),
    };
    let request = serde_json::to_vec(&request)?;
    socket.send_to(&request, &ctl_path).await?;

    let mut buf = vec![0u8; 1 << 20];
    let len = tokio::time::timeout(timeout, socket.recv(&mut buf))
        .await
        .map_err(|_| anyhow::anyhow!("timed out waiting for a response from the daemon"))??;
    let response: CryonetUapi = serde_json::from_slice(&buf[..len])?;

    match response {
        CryonetUapi::GetLinksResponse(mut links) => {
            links.sort_by_key(|link| link.node);
            println!("LINKS ({})", links.len());
            if links.is_empty() {
                println!("  (none)");
            } else {
                let rows = links
                    .iter()
                    .map(|link| {
                        vec![
                            format!("{:X}", link.node),
                            if link.alive { "up" } else { "down" }.to_owned(),
                            if link.last_recv_ms < 0 {
                                "-".to_owned()
                            } else {
                                format_ms(link.last_recv_ms)
                            },
                            format_ms(link.idle_timeout_ms),
                        ]
                    })
                    .collect::<Vec<_>>();
                table(&["NODE", "STATUS", "LAST RX", "IDLE"], &rows);
            }
        }
        CryonetUapi::GetRoutesResponse(hash_map) => {
            let mut routes: Vec<_> = hash_map.into_iter().collect();
            routes.sort_by_key(|(dst, _)| *dst);
            println!("ROUTES ({})", routes.len());
            if routes.is_empty() {
                println!("  (none)");
            } else {
                let rows = routes
                    .iter()
                    .map(|(dst, next_hop)| vec![format!("{dst:X}"), format!("{next_hop:X}")])
                    .collect::<Vec<_>>();
                table(&["DST", "NEXT HOP"], &rows);
            }
        }
        CryonetUapi::GetIgpRoutesResponse(mut igp_routes) => {
            igp_routes.sort_by_key(|route| (route.dst, route.from));
            println!("IGP ROUTES ({})", igp_routes.len());
            if igp_routes.is_empty() {
                println!("  (none)");
            } else {
                let rows = igp_routes
                    .iter()
                    .map(|route| {
                        vec![
                            format!("{:X}", route.dst),
                            format!("{:X}", route.from),
                            route.seq.to_string(),
                            format_metric(route.metric),
                            format_metric(route.computed_metric),
                            if route.selected { "yes" } else { "no" }.to_owned(),
                            format_ms(route.timeout_remaining_ms),
                        ]
                    })
                    .collect::<Vec<_>>();
                table(
                    &[
                        "DST", "FROM", "SEQ", "METRIC", "COMPUTED", "SELECTED", "EXPIRES",
                    ],
                    &rows,
                );
            }
        }
        CryonetUapi::GetIgpStatusResponse(status) => {
            println!("IGP STATUS");
            kv("SELF SEQ", status.self_seq);
            kv(
                "TIMERS",
                format!(
                    "hello {} · dump {} · gc {} · route {} · source {} · request {}",
                    format_ms(status.hello_interval_ms as i64),
                    format_ms(status.dump_interval_ms as i64),
                    format_ms(status.gc_interval_ms as i64),
                    format_ms(status.route_timeout_ms as i64),
                    format_ms(status.source_timeout_ms as i64),
                    format_ms(status.seqno_request_timeout_ms as i64),
                ),
            );
            kv(
                "PARAMETERS",
                format!(
                    "diameter {} · update threshold {} · dump batch {}",
                    status.diameter, status.update_threshold, status.dump_batch_size
                ),
            );
            println!();
            println!("NEIGHBORS ({})", status.neighbors.len());
            if status.neighbors.is_empty() {
                println!("  (none)");
            } else {
                let rows = status
                    .neighbors
                    .iter()
                    .map(|neighbor| {
                        vec![
                            format!("{:X}", neighbor.node),
                            format_cost(neighbor.cost),
                            format_ms(neighbor.last_hello_ms),
                        ]
                    })
                    .collect::<Vec<_>>();
                table(&["NODE", "COST", "LAST HELLO"], &rows);
            }
            println!();
            println!("FEASIBILITY DISTANCES ({})", status.sources.len());
            if status.sources.is_empty() {
                println!("  (none)");
            } else {
                let rows = status
                    .sources
                    .iter()
                    .map(|source| {
                        vec![
                            format!("{:X}", source.dst),
                            source.seq.to_string(),
                            format_metric(source.metric),
                            format_ms(source.expires_in_ms),
                        ]
                    })
                    .collect::<Vec<_>>();
                table(&["DST", "SEQ", "METRIC", "EXPIRES"], &rows);
            }
            println!();
            println!("PENDING SEQNO REQUESTS ({})", status.requests.len());
            if status.requests.is_empty() {
                println!("  (none)");
            } else {
                let rows = status
                    .requests
                    .iter()
                    .map(|request| {
                        vec![
                            format!("{:X}", request.dst),
                            request.seq.to_string(),
                            request.retries.to_string(),
                            format_ms(request.expires_in_ms),
                        ]
                    })
                    .collect::<Vec<_>>();
                table(&["DST", "SEQ", "RETRIES", "EXPIRES"], &rows);
            }
        }
        CryonetUapi::GetFullMeshPeersResponse(status) => {
            println!(
                "FULL MESH PEERS ({}, session 0x{:X})",
                status.peers.len(),
                status.session
            );
            if status.peers.is_empty() {
                println!("  (none)");
            } else {
                let rows = status
                    .peers
                    .iter()
                    .map(|peer| {
                        let rekey = if peer.connection_type == "ice" && peer.last_rekey_ms >= 0 {
                            format_ms(peer.last_rekey_ms)
                        } else {
                            "-".to_owned()
                        };
                        vec![
                            format!("{:X}", peer.node),
                            if peer.initiator {
                                "initiator"
                            } else {
                                "responder"
                            }
                            .to_owned(),
                            peer.conn_gen.to_string(),
                            format_state(peer.state).to_owned(),
                            format_bytes(peer.sent),
                            format_bytes(peer.received),
                            format_ms(peer.last_recv_ms),
                            rekey,
                            peer.key_index
                                .map_or("-".to_owned(), |index| u8::from(index).to_string()),
                            peer.selected_candidate.as_deref().unwrap_or("-").to_owned(),
                        ]
                    })
                    .collect::<Vec<_>>();
                table(
                    &[
                        "NODE",
                        "ROLE",
                        "GEN",
                        "STATE",
                        "TX",
                        "RX",
                        "LAST RX",
                        "REKEY",
                        "KEY",
                        "CANDIDATE",
                    ],
                    &rows,
                );
            }
            if !status.pending.is_empty() {
                println!();
                println!("PENDING ({})", status.pending.len());
                let rows = status
                    .pending
                    .iter()
                    .map(|pending| {
                        vec![
                            format!("{:X}", pending.node),
                            format_ms(pending.retry_in_ms),
                            format_ms(pending.backoff_ms as i64),
                        ]
                    })
                    .collect::<Vec<_>>();
                table(&["NODE", "RETRY IN", "BACKOFF"], &rows);
            }
        }
        CryonetUapi::GetNodesResponse(mut nodes) => {
            nodes.sort_by_key(|node| node.id);
            println!("REGISTRY NODES ({})", nodes.len());
            if nodes.is_empty() {
                println!("  (none)");
            } else {
                let rows = nodes
                    .iter()
                    .map(|node| {
                        vec![
                            format!("{:X}", node.id),
                            format_ms(node.last_seen_ms),
                            join_or_dash(&node.connection_types),
                            join_or_dash(&node.ips),
                        ]
                    })
                    .collect::<Vec<_>>();
                table(&["NODE", "LAST SEEN", "TRANSPORTS", "IPS"], &rows);
            }
        }
        CryonetUapi::GetServersResponse(servers) => {
            println!("SERVERS ({})", servers.len());
            if servers.is_empty() {
                println!("  (none)");
            } else {
                let rows = servers
                    .iter()
                    .map(|server| {
                        let state = if server.connecting {
                            "connecting"
                        } else if server.connected {
                            "connected"
                        } else if server.node.is_some() {
                            "resolved"
                        } else {
                            "pending"
                        };
                        vec![
                            server.url.clone(),
                            server
                                .node
                                .map_or("-".to_owned(), |node| format!("{node:X}")),
                            state.to_owned(),
                        ]
                    })
                    .collect::<Vec<_>>();
                table(&["URL", "NODE", "STATE"], &rows);
            }
        }
        CryonetUapi::GetInfoResponse(info) => {
            println!("CRYONET {}", info.version);
            kv("NODE", format!("{} (0x{:X})", info.id, info.id));
            kv("UPTIME", format_ms(info.uptime_ms as i64));
            kv("MODE", info.mode);
            kv("INTERFACE", info.interface);
            kv("MTU", info.mtu);
            kv("LISTEN", info.listen);
            kv(
                "ENCRYPT",
                if info.encrypt_local_packets {
                    "on"
                } else {
                    "off"
                },
            );
            kv(
                "PACKET INFO",
                if info.packet_information { "on" } else { "off" },
            );
            kv("CONTROL", info.ctl_path);
            kv("LOCAL IPS", join_or_dash(&info.local_ips));
            kv("ICE SERVERS", join_or_dash(&info.ice_servers));
            kv("FILTERS", join_or_dash(&info.candidate_filter_prefixes));
        }
        CryonetUapi::GetStatsResponse(stats) => {
            println!("STATS (uptime {})", format_ms(stats.uptime_ms as i64));
            let mesh = &stats.mesh;
            kv(
                "MESH",
                format!(
                    "links {} · routes {} · rx {} · tx {} · forwarded {}",
                    mesh.links, mesh.routes, mesh.received, mesh.sent, mesh.forwarded
                ),
            );
            kv(
                "DROPS",
                format!(
                    "ttl {} · duplicate {} · unreachable {} · dispatch full {}",
                    mesh.dropped_ttl,
                    mesh.dropped_duplicate,
                    mesh.dropped_unreachable,
                    mesh.dropped_dispatch_full
                ),
            );
            kv(
                "RELIABLE",
                format!(
                    "pending {} · retries {} · given up {} · acked {}",
                    mesh.reliable_pending,
                    mesh.reliable_retries,
                    mesh.reliable_given_up,
                    mesh.reliable_acked
                ),
            );
            let igp = &stats.igp;
            kv(
                "IGP",
                format!(
                    "routes {} · selected {} · neighbors {} · sources {} · requests {}",
                    igp.routes, igp.selected, igp.neighbors, igp.sources, igp.requests
                ),
            );
            kv(
                "IGP TX",
                format!(
                    "hello {} · update {} · route request {} · seqno request {}",
                    igp.hellos_sent, igp.updates_sent, igp.route_requests, igp.seqno_requests
                ),
            );
            kv(
                "IGP RX",
                format!(
                    "hello {} · update {}",
                    igp.hellos_received, igp.updates_received
                ),
            );
            let fullmesh = &stats.fullmesh;
            kv(
                "FULL MESH",
                format!(
                    "peers {} · connected {} · connecting {} · pending {} · tx {} · rx {}",
                    fullmesh.peers,
                    fullmesh.connected,
                    fullmesh.connecting,
                    fullmesh.pending,
                    format_bytes(fullmesh.sent),
                    format_bytes(fullmesh.received)
                ),
            );
            let registry = &stats.registry;
            kv(
                "REGISTRY",
                format!("nodes {} · ips {}", registry.nodes, registry.ips),
            );
        }
        CryonetUapi::Pong => {
            println!("PONG  {} ms", time.elapsed().as_millis());
        }
        CryonetUapi::Error(message) => {
            eprintln!("error: {message}");
            std::process::exit(1);
        }
        _ => println!("Unexpected response: {response:?}"),
    }

    Ok(())
}
