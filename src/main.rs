#![feature(let_chains)]
#![feature(trait_upcasting)]
#![feature(try_blocks)]
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use futures::pending;
use mesh::{igp::IGP, Mesh};
use tokio::sync::{Mutex, mpsc::{Receiver, Sender, channel}};

use crate::mesh::{Link, packet::Packet};

pub(crate) mod errors;
pub(crate) mod mesh;

#[derive(Debug)]
struct L(Receiver<Packet>, Sender<Packet>);

impl L {
    async fn new() -> (Self, Self) {
        let (a_to_b_tx, a_to_b_rx) = channel(1024);
        let (b_to_a_tx, b_to_a_rx) = channel(1024);
        (L(a_to_b_rx, b_to_a_tx), L(b_to_a_rx, a_to_b_tx))
    }
}

#[async_trait]
impl Link for L {
    async fn send(&self, packet: Packet) -> Result<()> {
        self.1.send(packet).await?;
        Ok(())
    }
    async fn recv(&mut self) -> Result<Packet> {
        let Some(packet) = self.0.recv().await else {
            anyhow::bail!("Link closed");
        };
        Ok(packet)
    }
}

#[tokio::main]
async fn main() -> Result<()>{
    tracing_subscriber::fmt::init();

    let (link1_a, link1_b) = L::new().await;
    let (link2_a, link2_b) = L::new().await;

    let mesh1 = Arc::new(Mutex::new(Mesh::new(1)));
    let igp = IGP::new(mesh1.clone()).await;
    mesh1.lock().await.add_link(2, Box::new(link1_a)).await?;

    let mesh2 = Arc::new(Mutex::new(Mesh::new(2)));
    let igp2 = IGP::new(mesh2.clone()).await;
    mesh2.lock().await.add_link(1, Box::new(link1_b)).await?;
    mesh2.lock().await.add_link(3, Box::new(link2_a)).await?;

    let mesh3 = Arc::new(Mutex::new(Mesh::new(3)));
    let igp3 = IGP::new(mesh3.clone()).await;
    mesh3.lock().await.add_link(2, Box::new(link2_b)).await?;

    pending!();
    Ok(())
}
