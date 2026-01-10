use anyhow::anyhow;
use async_trait::async_trait;
use futures::{SinkExt, StreamExt};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_tungstenite::{WebSocketStream, tungstenite::Message};

use crate::mesh::{Link, LinkError, packet::Packet};

pub(crate) struct WebSocketLink<T>(WebSocketStream<T>);

impl<T> WebSocketLink<T> {
    pub(crate) fn new(ws_stream: WebSocketStream<T>) -> Self {
        WebSocketLink(ws_stream)
    }
}

#[async_trait]
impl<T: AsyncRead + AsyncWrite + Unpin + Send + Sync> Link for WebSocketLink<T> {
    async fn recv(&mut self) -> Result<Packet, LinkError> {
        let packet = match self.0.next().await {
            None => Err(LinkError::Closed)?,
            Some(Err(e)) => Err(LinkError::Unknown(anyhow!(e)))?,
            Some(Ok(packet)) => packet.into_data(),
        };
        Ok(serde_json::from_slice(&packet).map_err(|e| LinkError::Unknown(anyhow!(e)))?)
    }

    async fn send(&mut self, packet: Packet) -> Result<(), LinkError> {
        let data = serde_json::to_vec(&packet).map_err(|e| LinkError::Unknown(anyhow!(e)))?;
        self.0.send(Message::binary(data)).await.map_err(|e| LinkError::Unknown(anyhow!(e)))?;
        Ok(())
    }
}
