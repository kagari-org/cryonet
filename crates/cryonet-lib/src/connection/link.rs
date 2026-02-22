use anyhow::anyhow;
use async_trait::async_trait;
use futures::{
    SinkExt, StreamExt,
    stream::{SplitSink, SplitStream},
};

use crate::mesh::{LinkError, LinkRecv, LinkSend, packet::Packet};

pub struct WebSocketLinkSend<WebSocket, Message>(SplitSink<WebSocket, Message>);
pub struct WebSocketLinkRecv<WebSocket>(SplitStream<WebSocket>);

#[cfg(not(target_arch = "wasm32"))]
type TungsteniteWebSocket = tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>;
#[cfg(not(target_arch = "wasm32"))]
type TungsteniteMessage = tokio_tungstenite::tungstenite::Message;
type ReqwestWebSocket = reqwest_websocket::WebSocket;
type ReqwestMessage = reqwest_websocket::Message;

#[cfg(not(target_arch = "wasm32"))]
pub fn new_tungstenite_ws_link(ws: TungsteniteWebSocket) -> (WebSocketLinkSend<TungsteniteWebSocket, TungsteniteMessage>, WebSocketLinkRecv<TungsteniteWebSocket>) {
    let (sink, stream) = ws.split();
    (WebSocketLinkSend(sink), WebSocketLinkRecv(stream))
}

pub fn new_reqwest_ws_link(ws: ReqwestWebSocket) -> (WebSocketLinkSend<ReqwestWebSocket, ReqwestMessage>, WebSocketLinkRecv<ReqwestWebSocket>) {
    let (sink, stream) = ws.split();
    (WebSocketLinkSend(sink), WebSocketLinkRecv(stream))
}

#[cfg(not(target_arch = "wasm32"))]
#[async_trait(?Send)]
impl LinkSend for WebSocketLinkSend<TungsteniteWebSocket, TungsteniteMessage> {
    async fn send(&mut self, packet: Packet) -> Result<(), LinkError> {
        let data = serde_json::to_vec(&packet).map_err(|e| LinkError::Unknown(anyhow!(e)))?;
        self.0.send(TungsteniteMessage::binary(data)).await.map_err(|e| LinkError::Unknown(anyhow!(e)))?;
        Ok(())
    }
}

#[cfg(not(target_arch = "wasm32"))]
#[async_trait(?Send)]
impl LinkRecv for WebSocketLinkRecv<TungsteniteWebSocket> {
    async fn recv(&mut self) -> Result<Packet, LinkError> {
        let packet = match self.0.next().await {
            Some(Ok(TungsteniteMessage::Binary(data))) => data,

            None => Err(LinkError::Closed)?,
            Some(Ok(TungsteniteMessage::Close(_))) => Err(LinkError::Closed)?,

            Some(Err(e)) => Err(LinkError::Unknown(anyhow!(e)))?,
            Some(Ok(_)) => Err(LinkError::Unknown(anyhow!("Unexpected message type")))?,
        };
        Ok(serde_json::from_slice(&packet).map_err(|e| LinkError::Unknown(anyhow!(e)))?)
    }
}

#[async_trait(?Send)]
impl LinkSend for WebSocketLinkSend<ReqwestWebSocket, ReqwestMessage> {
    async fn send(&mut self, packet: Packet) -> Result<(), LinkError> {
        let data = serde_json::to_vec(&packet).map_err(|e| LinkError::Unknown(anyhow!(e)))?;
        self.0.send(ReqwestMessage::Binary(data.into())).await.map_err(|e| LinkError::Unknown(anyhow!(e)))?;
        Ok(())
    }
}

#[async_trait(?Send)]
impl LinkRecv for WebSocketLinkRecv<ReqwestWebSocket> {
    async fn recv(&mut self) -> Result<Packet, LinkError> {
        let packet = match self.0.next().await {
            Some(Ok(ReqwestMessage::Binary(data))) => data,

            None => Err(LinkError::Closed)?,
            Some(Ok(ReqwestMessage::Close { .. })) => Err(LinkError::Closed)?,

            Some(Err(e)) => Err(LinkError::Unknown(anyhow!(e)))?,
            Some(Ok(_)) => Err(LinkError::Unknown(anyhow!("Unexpected message type")))?,
        };
        Ok(serde_json::from_slice(&packet).map_err(|e| LinkError::Unknown(anyhow!(e)))?)
    }
}
