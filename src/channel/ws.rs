use anyhow::{Ok, Result};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use tokio::net::TcpStream;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

use crate::error::CryonetError;

use super::{BroadcastPacket, Channel};

#[derive(Debug, Serialize, Deserialize)]
enum WSPacket {
    Init { id: String },
    Broadcast,
}

#[derive(Debug)]
pub(crate) struct WSChannel {
    ws: WebSocketStream<MaybeTlsStream<TcpStream>>,
}

impl WSChannel {
    pub(crate) async fn new(endpoint: String) -> Result<(WSChannel, String)> {
        let (mut ws, _) = connect_async(endpoint).await?;
        let first_msg = ws.next().await.ok_or(CryonetError::Connection)??;
        let init: WSPacket = serde_json::from_slice(&first_msg.into_data())?;
        let WSPacket::Init { id } = init else { Err(CryonetError::BadPacket)? };
        Ok((WSChannel { ws }, id))
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