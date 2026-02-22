use anyhow::anyhow;
use async_trait::async_trait;
use futures::{
    SinkExt, StreamExt,
    stream::{SplitSink, SplitStream},
};

use crate::mesh::{LinkError, LinkRecv, LinkSend, packet::Packet};

pub(crate) struct WebSocketLinkSend<WebSocket, Message>(SplitSink<WebSocket, Message>);
pub(crate) struct WebSocketLinkRecv<WebSocket>(SplitStream<WebSocket>);

#[cfg(not(target_arch = "wasm32"))]
type AxumWebSocket = axum::extract::ws::WebSocket;
#[cfg(not(target_arch = "wasm32"))]
type AxumMessage = axum::extract::ws::Message;
type ReqwestWebSocket = reqwest_websocket::WebSocket;
type ReqwestMessage = reqwest_websocket::Message;

#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn new_axum_ws_link(ws: AxumWebSocket) -> (WebSocketLinkSend<AxumWebSocket, AxumMessage>, WebSocketLinkRecv<AxumWebSocket>) {
    let (sink, stream) = ws.split();
    (WebSocketLinkSend(sink), WebSocketLinkRecv(stream))
}

pub(crate) fn new_reqwest_ws_link(ws: ReqwestWebSocket) -> (WebSocketLinkSend<ReqwestWebSocket, ReqwestMessage>, WebSocketLinkRecv<ReqwestWebSocket>) {
    let (sink, stream) = ws.split();
    (WebSocketLinkSend(sink), WebSocketLinkRecv(stream))
}

#[cfg(not(target_arch = "wasm32"))]
#[async_trait(?Send)]
impl LinkSend for WebSocketLinkSend<AxumWebSocket, AxumMessage> {
    async fn send(&mut self, packet: Packet) -> Result<(), LinkError> {
        let data = serde_json::to_vec(&packet).map_err(|e| LinkError::Unknown(anyhow!(e)))?;
        self.0.send(AxumMessage::binary(data)).await.map_err(|e| LinkError::Unknown(anyhow!(e)))?;
        Ok(())
    }
}

#[cfg(not(target_arch = "wasm32"))]
#[async_trait(?Send)]
impl LinkRecv for WebSocketLinkRecv<AxumWebSocket> {
    async fn recv(&mut self) -> Result<Packet, LinkError> {
        let packet = match self.0.next().await {
            Some(Ok(AxumMessage::Binary(data))) => data,

            None => Err(LinkError::Closed)?,
            Some(Ok(AxumMessage::Close(_))) => Err(LinkError::Closed)?,

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
