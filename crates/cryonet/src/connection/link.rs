use anyhow::anyhow;
use async_trait::async_trait;
use futures::{
    SinkExt, StreamExt,
    stream::{SplitSink, SplitStream},
};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_tungstenite::{WebSocketStream, tungstenite::Message};

use crate::mesh::{LinkError, LinkRecv, LinkSend, packet::Packet};

pub(crate) struct WebSocketLinkSend<T>(SplitSink<WebSocketStream<T>, Message>);
pub(crate) struct WebSocketLinkRecv<T>(SplitStream<WebSocketStream<T>>);

pub(crate) fn new_tungstenite_ws_link<T>(
    ws: WebSocketStream<T>,
) -> (WebSocketLinkSend<T>, WebSocketLinkRecv<T>)
where
    T: AsyncRead + AsyncWrite + Unpin,
{
    let (sink, stream) = ws.split();
    (WebSocketLinkSend(sink), WebSocketLinkRecv(stream))
}

#[async_trait]
impl<T> LinkSend for WebSocketLinkSend<T>
where
    T: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    async fn send(&mut self, packet: Packet) -> Result<(), LinkError> {
        let data = serde_json::to_vec(&packet).map_err(|e| LinkError::Unknown(e.into()))?;
        self.0
            .send(Message::binary(data))
            .await
            .map_err(|e| LinkError::Unknown(e.into()))?;
        Ok(())
    }
}

#[async_trait]
impl<T> LinkRecv for WebSocketLinkRecv<T>
where
    T: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    async fn recv(&mut self) -> Result<Packet, LinkError> {
        let packet = match self.0.next().await {
            Some(Ok(Message::Binary(data))) => data,

            None => Err(LinkError::Closed)?,
            Some(Ok(Message::Close(_))) => Err(LinkError::Closed)?,

            Some(Err(e)) => Err(LinkError::Unknown(e.into()))?,
            Some(Ok(_)) => Err(LinkError::Unknown(anyhow!("Unexpected message type")))?,
        };
        Ok(serde_json::from_slice(&packet).map_err(|e| LinkError::Unknown(e.into()))?)
    }
}
