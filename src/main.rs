#![feature(try_blocks)]
use std::{io::Write, net::SocketAddr, time::{Duration, Instant}};

use anyhow::Result;
use clap::Parser;
use clap_num::maybe_hex;
use rustyline_async::{Readline, ReadlineEvent};
use serde::{Deserialize, Serialize};
use tokio::{select, time::interval};

use crate::{connection::ConnManager, mesh::{Mesh, igp::IGP, packet::{NodeId, Payload}, seq::Seq}};

pub(crate) mod errors;
pub(crate) mod mesh;
pub(crate) mod connection;
pub(crate) mod fullmesh;

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

#[derive(Debug, Parser)]
enum Rl {
    #[clap(alias = "c")]
    Connect { address: String },
    #[clap(alias = "d")]
    Disconnect {
        #[clap(value_parser = maybe_hex::<NodeId>)]
        id: NodeId,
    },
    #[clap(alias = "l")]
    Links,
    #[clap(alias = "r")]
    Routes,
    #[clap(alias = "ir")]
    IGPRoutes,
    Ping {
        #[clap(value_parser = maybe_hex::<NodeId>)]
        dst: NodeId,
    },
    Pong,
}

#[typetag::serde]
impl Payload for PingPayload {}
#[derive(Debug, Clone, Serialize, Deserialize)]
enum PingPayload {
    // TODO: add uuid
    Ping(Seq),
    Pong(Seq),
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let args = Args::parse();

    let mesh = Mesh::new(args.id);
    let igp = IGP::new(mesh.clone()).await;
    let mgr = ConnManager::new(
        mesh.clone(),
        args.token,
        args.servers,
        args.listen,
    );

    let (mut rl, mut stdout) = Readline::new("> ".to_string())?;
    loop {
        select! {
            line = rl.readline() => {
                let line = match line? {
                    ReadlineEvent::Eof => break,
                    ReadlineEvent::Interrupted => {
                        writeln!(stdout, "")?;
                        continue;
                    },
                    ReadlineEvent::Line(line) => line,
                };
                let line = line.trim().to_string();
                if line.is_empty() {
                    continue;
                }
                rl.add_history_entry(line.clone());
                let line_args = shellwords::split(&line)
                    .map(|mut args| {
                        args.insert(0, "".to_string());
                        Rl::try_parse_from(args)
                    });
                let cmd = match line_args {
                    Err(err) => {
                        writeln!(stdout, "Error parsing command: {}", err)?;
                        continue;
                    },
                    Ok(Err(err)) => {
                        writeln!(stdout, "{}", err)?;
                        continue;
                    },
                    Ok(Ok(cmd)) => cmd,
                };
                match cmd {
                    Rl::Connect { address } => {
                        let mut mgr = mgr.lock().await;
                        if let Err(err) = mgr.connect(address).await {
                            writeln!(stdout, "Failed to connect: {}", err)?;
                        }
                    },
                    Rl::Disconnect { id } => {
                        mgr.lock().await.disconnect(id).await;
                    },
                    Rl::Links => {
                        let result: Result<()> = try {
                            let links = mesh.lock().await.get_links();
                            if links.is_empty() {
                                writeln!(stdout, "No links available")?;
                                continue;
                            }
                            writeln!(stdout, "Links:")?;
                            for link in links {
                                writeln!(stdout, "  - Node ID: {:X}", link)?;
                            }
                        };
                        if let Err(err) = result {
                            writeln!(stdout, "Failed to get links: {}", err)?;
                        }
                    },
                    Rl::Routes => {
                        let result: Result<()> = try {
                            let routes = mesh.lock().await.get_routes();
                            if routes.is_empty() {
                                writeln!(stdout, "No routes available")?;
                                continue;
                            }
                            writeln!(stdout, "Routes:")?;
                            for (dest, next_hop) in routes {
                                writeln!(stdout, "  - Destination: {:X}, Next Hop: {:X}", dest, next_hop)?;
                            }
                        };
                        if let Err(err) = result {
                            writeln!(stdout, "Failed to get routes: {}", err)?;
                        }
                    },
                    Rl::IGPRoutes => {
                        let routes = igp.lock().await.get_routes();
                        if routes.is_empty() {
                            writeln!(stdout, "No IGP routes available")?;
                            continue;
                        }
                        writeln!(stdout, "IGP Routes:")?;
                        for route in routes {
                            writeln!(stdout, "  - {}", route)?;
                        }
                    },
                    Rl::Ping { dst } => {
                        let mut recv = mesh.lock().await.add_dispatchee(|packet|
                            (packet.payload.as_ref() as &dyn std::any::Any).is::<PingPayload>());

                        let mut seq = Seq(0);
                        let mut ticker = interval(Duration::from_secs(1));
                        loop {
                            select! {
                                line = rl.readline() => {
                                    match line? {
                                        ReadlineEvent::Eof | ReadlineEvent::Interrupted => {
                                            writeln!(stdout, "")?;
                                            break
                                        },
                                        ReadlineEvent::Line(_) => {},
                                    };
                                }
                                _ = ticker.tick() => {
                                    let start = Instant::now();
                                    let result = mesh.lock().await.send_packet(dst, PingPayload::Ping(seq)).await;
                                    if let Err(err) = result {
                                        writeln!(stdout, "Failed to send ping to {:X}: {}", dst, err)?;
                                        break;
                                    }
                                    // TODO: add timeout
                                    let Some(packet) = recv.recv().await else {
                                        writeln!(stdout, "Ping receiver closed")?;
                                        break;
                                    };
                                    let payload = (packet.payload.as_ref() as &dyn std::any::Any)
                                        .downcast_ref::<PingPayload>().unwrap();
                                    match payload {
                                        PingPayload::Pong(s) if *s == seq => {
                                            let rtt = Instant::now().duration_since(start);
                                            writeln!(stdout, "Ping {:X}: seq={}, rtt={}ms",
                                                packet.src, s.0, rtt.as_millis())?;
                                        },
                                        // ignore other packets
                                        _ => {},
                                    };
                                    rl.flush()?;
                                    seq += Seq(1);
                                },
                            };
                        }
                        mesh.lock().await.remove_dispatchee(&mut recv);
                    },
                    Rl::Pong => {
                        let mut recv = mesh.lock().await.add_dispatchee(|packet|
                            (packet.payload.as_ref() as &dyn std::any::Any).is::<PingPayload>());
                        loop {
                            select!{
                                line = rl.readline() => {
                                    match line? {
                                        ReadlineEvent::Eof | ReadlineEvent::Interrupted => {
                                            writeln!(stdout, "")?;
                                            break
                                        },
                                        ReadlineEvent::Line(_) => {},
                                    };
                                },
                                packet = recv.recv() => {
                                    let Some(packet) = packet else {
                                        writeln!(stdout, "Pong receiver closed")?;
                                        break;
                                    };
                                    let payload = (packet.payload.as_ref() as &dyn std::any::Any)
                                        .downcast_ref::<PingPayload>().unwrap();
                                    match payload {
                                        PingPayload::Ping(s) => {
                                            let result = mesh.lock().await.send_packet(packet.src, PingPayload::Pong(*s)).await;
                                            if let Err(err) = result {
                                                writeln!(stdout, "Failed to send pong to {:X}: {}", packet.src, err)?;
                                            } else {
                                                writeln!(stdout, "Pong {:X}: seq={}", packet.src, s.0)?;
                                            }
                                            rl.flush()?;
                                        },
                                        // ignore other packets
                                        _ => {},
                                    };
                                },
                            }
                        }
                        mesh.lock().await.remove_dispatchee(&mut recv);
                    },
                }
            }
        }
    }
    igp.lock().await.stop().await;
    mesh.lock().await.stop()?;
    Ok(())
}
