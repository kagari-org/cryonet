#![feature(let_chains)]
#![feature(trait_upcasting)]
use std::{sync::Arc, time::Duration};

use mesh::{igp::IGP, Mesh};
use tokio::sync::Mutex;

pub(crate) mod errors;
pub(crate) mod mesh;

#[tokio::main]
async fn main() {
    let mesh = Arc::new(Mutex::new(Mesh::new(0)));
    let _igp = IGP::new(
        Duration::from_secs(5),
        Duration::from_secs(5),
        Duration::from_secs(5),
        Duration::from_secs(5),
        64,
        1000,
        mesh,
    ).await;
}
