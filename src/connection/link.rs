use anyhow::anyhow;
use async_trait::async_trait;
use futures::{SinkExt, StreamExt, stream::{SplitSink, SplitStream}};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_tungstenite::{WebSocketStream, tungstenite::Message};

use crate::mesh::{LinkError, LinkRecv, LinkSend, packet::Packet};

pub(crate) struct WebSocketLinkSend<T>(SplitSink<WebSocketStream<T>, Message>);
pub(crate) struct WebSocketLinkRecv<T>(SplitStream<WebSocketStream<T>>);

pub(crate) fn new_ws_link<T>(ws_stream: WebSocketStream<T>) -> (WebSocketLinkSend<T>, WebSocketLinkRecv<T>)
where T: AsyncRead + AsyncWrite + Unpin + Send + Sync
{
    let (sink, stream) = ws_stream.split();
    (WebSocketLinkSend(sink), WebSocketLinkRecv(stream))
}

#[async_trait]
impl<T: AsyncRead + AsyncWrite + Unpin + Send + Sync> LinkSend for WebSocketLinkSend<T> {
    async fn send(&mut self, packet: Packet) -> Result<(), LinkError> {
        let data = serde_json::to_vec(&packet).map_err(|e| LinkError::Unknown(anyhow!(e)))?;
        self.0.send(Message::binary(data)).await.map_err(|e| LinkError::Unknown(anyhow!(e)))?;
        Ok(())
    }
}

#[async_trait]
impl<T: AsyncRead + AsyncWrite + Unpin + Send + Sync> LinkRecv for WebSocketLinkRecv<T> {
    async fn recv(&mut self) -> Result<Packet, LinkError> {
        let packet = match self.0.next().await {
            None => Err(LinkError::Closed)?,
            Some(Err(e)) => Err(LinkError::Unknown(anyhow!(e)))?,
            Some(Ok(packet)) => packet.into_data(),
        };
        Ok(serde_json::from_slice(&packet).map_err(|e| LinkError::Unknown(anyhow!(e)))?)
    }
}
