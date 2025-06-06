#![feature(try_blocks)]
#![feature(let_chains)]
use std::net::SocketAddr;

use actors::{net::NetActor, supervisor::SupervisorActor};
use anyhow::{Ok, Result, anyhow};
use clap::Parser;
use ractor::{Actor, concurrency::Duration};
use tokio::sync::OnceCell;
use tracing::info;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};
use webrtc::ice_transport::ice_server::RTCIceServer;

pub(crate) mod actors;
pub(crate) mod error;
pub(crate) mod utils;

static CONFIG: OnceCell<Config> = OnceCell::const_new();

#[derive(Debug, Clone, Parser)]
pub(crate) struct Config {
    id: String,

    #[clap(long, env, default_value = "127.0.0.1:2333")]
    listen: SocketAddr,
    #[clap(long, env, value_delimiter = ',')]
    ws_servers: Vec<String>,
    #[clap(long, env)]
    token: String,

    #[clap(long, env, value_parser = parse_rtc_ice_server, value_delimiter = ',')]
    ice_servers: Vec<RTCIceServer>,

    #[clap(long, env, value_parser = humantime::parse_duration, default_value = "10s")]
    check_interval: Duration,
    #[clap(long, env, value_parser = humantime::parse_duration, default_value = "1m")]
    check_timeout: Duration,
    #[clap(long, env, value_parser = humantime::parse_duration, default_value = "20s")]
    send_alive_interval: Duration,

    #[clap(long, env, default_value = "cn")]
    interface_prefix: String,
    #[clap(long, env, default_value = "false")]
    auto_interface_name: bool,
    #[clap(long, env, default_value = "true")]
    enable_packet_information: bool,
    #[clap(long, env, default_value = "1504")]
    buf_size: usize,
}

fn parse_rtc_ice_server(input: &str) -> anyhow::Result<RTCIceServer> {
    let splited: Vec<_> = input.split('|').collect();
    if splited.len() == 1 {
        Ok(RTCIceServer {
            urls: vec![splited[0].to_string()],
            ..Default::default()
        })
    } else if splited.len() == 3 {
        Ok(RTCIceServer {
            urls: vec![splited[0].to_string()],
            username: splited[1].to_string(),
            credential: splited[2].to_string(),
        })
    } else {
        return Err(anyhow!("unexpected ice server: {input}"));
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer())
        .with(tracing_subscriber::EnvFilter::new("cryonet=info"))
        .init();

    CONFIG.get_or_init(async || Config::parse()).await;

    info!("spawn SupervisorActor");
    let (_, join) = Actor::spawn(None, SupervisorActor, ()).await?;
    join.await?;
    Ok(())
}
