#![feature(let_chains)]
#![feature(trait_upcasting)]
#![feature(try_blocks)]
pub(crate) mod errors;
pub(crate) mod mesh;

#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Duration};

    use anyhow::{Result, bail};
    use async_trait::async_trait;
    use futures::{SinkExt, StreamExt};
    use serde::{Deserialize, Serialize};
    use tokio::{io::{AsyncRead, AsyncWrite}, net::TcpListener, sync::Mutex, time::sleep};
    use tokio_tungstenite::{WebSocketStream, accept_async, connect_async, tungstenite::Message};
    use tracing::{Level, debug, info};

    use crate::mesh::{Link, Mesh, igp::IGP, packet::{Packet, Payload}};

    struct L<T>(WebSocketStream<T>);
    #[async_trait]
    impl<T: AsyncRead + AsyncWrite + Unpin + Send + Sync> Link for L<T> {
        async fn recv(&mut self) -> Result<Packet> {
            let packet = match self.0.next().await {
                None => bail!("connection closed"),
                Some(packet) => packet?,
            };
            Ok(serde_json::from_slice(&packet.into_data())?)
        }
        async fn send(&mut self, packet: Packet) -> Result<()> {
            let data = serde_json::to_vec(&packet)?;
            self.0.send(Message::binary(data)).await?;
            Ok(())
        }
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    struct P(String);
    #[typetag::serde]
    impl Payload for P {}

    #[tokio::test]
    async fn node0() -> Result<()> {
        tracing_subscriber::fmt::fmt()
            .with_max_level(Level::DEBUG)
            .init();

        let link1 = {
            let listener = TcpListener::bind("0.0.0.0:2333").await?;
            let (stream, _) = listener.accept().await?;
            let ws_stream = accept_async(stream).await?;
            println!("New WebSocket connection: {}", ws_stream.get_ref().peer_addr()?);
            L(ws_stream)
        };

        let mesh = Arc::new(Mutex::new(Mesh::new(0)));
        let igp = IGP::new(mesh.clone()).await;
        mesh.lock().await.add_link(1, Box::new(link1)).await?;

        loop {
            let _ = &igp;
            info!("sending packet");
            if let Err(err) = mesh.lock().await.send_packet(2, P("Hello, World!".to_string())).await {
                dbg!(err);
            }
            sleep(Duration::from_secs(3)).await;
        }
    }

    #[tokio::test]
    async fn node1() -> Result<()> {
        tracing_subscriber::fmt::fmt()
            .with_max_level(Level::DEBUG)
            .init();

        let link0 = {
            let (ws_stream, _) = connect_async("ws://127.0.0.1:2333").await?;
            println!("WebSocket connection established");
            L(ws_stream)
        };
        let link2 = {
            let listener = TcpListener::bind("0.0.0.0:2334").await?;
            let (stream, _) = listener.accept().await?;
            let ws_stream = accept_async(stream).await?;
            println!("New WebSocket connection: {}", ws_stream.get_ref().peer_addr()?);
            L(ws_stream)
        };

        let mesh = Arc::new(Mutex::new(Mesh::new(1)));
        let igp = IGP::new(mesh.clone()).await;
        mesh.lock().await.add_link(0, Box::new(link0)).await?;
        mesh.lock().await.add_link(2, Box::new(link2)).await?;

        loop {
            let _ = &igp;
            sleep(Duration::from_secs(30)).await;
        }
    }

    #[tokio::test]
    async fn node2() -> Result<()> {
        tracing_subscriber::fmt::fmt()
            .with_max_level(Level::DEBUG)
            .init();

        let link1 = {
            let (ws_stream, _) = connect_async("ws://127.0.0.1:2334").await?;
            println!("WebSocket connection established");
            L(ws_stream)
        };

        let mesh = Arc::new(Mutex::new(Mesh::new(2)));
        let igp = IGP::new(mesh.clone()).await;
        mesh.lock().await.add_link(1, Box::new(link1)).await?;

        let mut recv = mesh.lock().await.add_dispatchee(|packet| {
            (packet.payload.as_ref() as &dyn std::any::Any).is::<P>()
        }).await;
        loop {
            let _ = &igp;
            if let Some(packet) = recv.recv().await {
                info!("p: {:?}", packet);
            }
        }
    }
}
