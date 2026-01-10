#![feature(try_blocks)]
use std::{io::Write, net::SocketAddr, sync::Arc};

use anyhow::Result;
use clap::Parser;
use clap_num::maybe_hex;
use rustyline_async::{Readline, ReadlineEvent};
use tokio::{net::TcpListener, select, sync::Mutex};
use tokio_tungstenite::{accept_hdr_async, connect_async, tungstenite::{client::IntoClientRequest, handshake::server}};

use crate::{link::WebSocketLink, mesh::{Mesh, igp::IGP, packet::NodeId}};

pub(crate) mod errors;
pub(crate) mod mesh;
pub(crate) mod link;

#[derive(Debug, Parser)]
struct Args {
    #[clap(value_parser = maybe_hex::<NodeId>)]
    id: NodeId,
    #[clap(long, short, default_value = "0.0.0.0:2333")]
    listen: SocketAddr
}

#[derive(Debug, Parser)]
enum Rl {
    #[clap(alias = "c")]
    Connect { address: String },
    #[clap(alias = "l")]
    Links,
    #[clap(alias = "r")]
    Routes,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let args = Args::parse();

    let mesh = Arc::new(Mutex::new(Mesh::new(args.id)));
    let igp = IGP::new(mesh.clone()).await;

    let listener = TcpListener::bind(args.listen).await?;
    let (mut rl, mut stdout) = Readline::new("> ".to_string())?;

    loop {
        select! {
            Ok((stream, addr)) = listener.accept() => {
                let mut neigh_id = None;
                let ws = accept_hdr_async(stream, |request: &server::Request, mut response: server::Response| {
                    // TODO: check Authentication here
                    let id = match request.headers()
                        .get("X-NodeId")
                        .map(|id| id.to_str().map(|id| id.parse::<NodeId>()))
                    {
                        Some(Ok(Ok(id))) => id,
                        None | Some(Err(_)) | Some(Ok(Err(_))) => return Err(server::Response::builder()
                            .status(400)
                            .body(Some("Missing or invalid X-NodeId header".into()))
                            .unwrap()),
                    };
                    neigh_id = Some(id);
                    response.headers_mut().insert("X-NodeId", args.id.to_string().parse().unwrap());
                    Ok(response)
                }).await;
                let ws = match ws {
                    Ok(ws) => ws,
                    Err(err) => {
                        writeln!(stdout, "Failed to accept connection from {}: {}", addr, err)?;
                        continue;
                    },
                };
                let neigh_id = neigh_id.unwrap();
                writeln!(stdout, "Accepted connection from {} (Node ID: {})", addr, neigh_id)?;
                let link = WebSocketLink::new(ws);
                let result = mesh.lock().await.add_link(neigh_id, Box::new(link)).await;
                if let Err(err) = result {
                    writeln!(stdout, "Failed to add link for Node ID {}: {}", neigh_id, err)?;
                }
            }
            line = rl.readline() => {
                let line = line?;
                match line {
                    ReadlineEvent::Eof => break,
                    ReadlineEvent::Interrupted => writeln!(stdout, "")?,
                    ReadlineEvent::Line(line) => {
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
                                let result: Result<()> = try {
                                    let mut request = address.clone().into_client_request()?;
                                    request.headers_mut().insert("X-NodeId", args.id.to_string().parse()?);
                                    let (ws, response) = connect_async(request).await?;
                                    let neigh_id = response.headers()
                                        .get("X-NodeId")
                                        .ok_or_else(|| anyhow::anyhow!("Missing X-NodeId header"))
                                        .map(|id| id.to_str().map(|id| id.parse::<NodeId>()))???;
                                    let link = WebSocketLink::new(ws);
                                    mesh.lock().await.add_link(neigh_id, Box::new(link)).await?;
                                    writeln!(stdout, "Connected to {} (Node ID: {})", address, neigh_id)?;
                                };
                                if let Err(err) = result {
                                    writeln!(stdout, "Failed to connect to {}: {}", address, err)?;
                                }
                            },
                            Rl::Links => {
                                let result: Result<()> = try {
                                    let mesh = mesh.lock().await;
                                    let links = mesh.get_links().await?;
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
                                    let mesh = mesh.lock().await;
                                    let routes = mesh.get_routes().await;
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
                        }
                    },
                };
            }
        }
    }
    drop(igp);
    Ok(())
}
