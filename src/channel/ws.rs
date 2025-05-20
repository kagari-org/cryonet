use anyhow::Result;
use futures_util::StreamExt;
use tokio_tungstenite::connect_async;

use crate::error::CryonetError;

use super::{BroadcastPacket, Channel};

#[derive(Debug)]
pub(crate)  struct WSChannel {}

impl WSChannel {
    pub(crate) async fn new(endpoint: String) -> Result<(WSChannel, String)> {
        let (mut ws, _) = connect_async(endpoint).await?;
        let first_msg = ws.next().await.ok_or(CryonetError::Connection)??;
        todo!()
    }
}

impl Channel for WSChannel {
    async fn recv(&mut self) -> Result<BroadcastPacket> {
        todo!()
    }

    async fn send(&mut self, packet: &BroadcastPacket) -> Result<()> {
        todo!()
    }
}