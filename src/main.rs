#![feature(try_blocks)]
#![feature(let_chains)]
use std::{collections::HashMap, net::SocketAddr};

use actors::net::NetActor;
use anyhow::Result;
use clap::Parser;
use fluent_uri::{encoding::EStr, Uri};
use ractor::{concurrency::Duration, Actor};
use tokio::sync::OnceCell;
use tracing::info;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};
use webrtc::ice_transport::ice_server::RTCIceServer;

pub(crate) mod error;
pub(crate) mod actors;
pub(crate) mod models;

static CONFIG: OnceCell<Config> = OnceCell::const_new();

#[derive(Debug, Clone, Parser)]
pub(crate) struct Config {
    id: String,

    #[clap(long, default_value = "127.0.0.1:2333")]
    listen: SocketAddr,

    #[clap(long)]
    ws_servers: Vec<String>,

    #[clap(long, value_parser = parse_rtc_ice_server)]
    ice_servers: Vec<RTCIceServer>,

    #[clap(long, value_parser = humantime::parse_duration)]
    check_interval: Duration,
    #[clap(long, value_parser = humantime::parse_duration)]
    check_timeout: Duration,
    #[clap(long, value_parser = humantime::parse_duration)]
    send_alive_interval: Duration,
}

fn parse_rtc_ice_server(input: &str) -> anyhow::Result<RTCIceServer> {
    let (username, credential) = match Uri::parse(input)?.query() {
        None => ("".to_string(), "".to_string()),
        Some(query) => {
            let mut map: HashMap<_, _> = query
                .split('&')
                .map(|s| s.split_once('=').unwrap_or((s, EStr::EMPTY)))
                .map(|(k, v)| (
                    k.decode().into_string_lossy(),
                    v.decode().into_string_lossy(),
                ))
                .collect();
            let username = map.remove("username")
                .map(|x| x.into_owned())
                .unwrap_or("".to_string());
            let credential = map.remove("credential")
                .map(|x| x.into_owned())
                .unwrap_or("".to_string());
            (username, credential)
        },
    };
    Ok(RTCIceServer {
        urls: vec![input.to_string()],
        username,
        credential,
    })
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer())
        .with(tracing_subscriber::EnvFilter::new("cryonet=trace"))
        .init();

    CONFIG.get_or_init(async || Config::parse()).await;

    info!("spawn NetActor");
    let (_, join) = Actor::spawn(Some("net".to_string()), NetActor, ()).await?;

    join.await?;
    Ok(())
}
