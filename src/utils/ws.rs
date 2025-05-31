use std::net::SocketAddr;

use anyhow::{Ok, Result};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::{net::TcpStream, sync::mpsc::channel};
use tokio_tungstenite::{accept_async, connect_async, tungstenite::Message};
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;

use crate::{actors::peer::Peer, error::CryonetError, CONFIG};

use super::rtc::create_rtc_connection;

#[derive(Debug, Serialize, Deserialize)]
enum WSPacket {
    Init { id: String, token: String },
    Offer(RTCSessionDescription),
    Answer(RTCSessionDescription),
}

pub(crate) async fn connect(endpoint: &String) -> Result<Peer> {
    let cfg = CONFIG.get().unwrap();

    let (mut ws, _) = connect_async(endpoint).await?;

    // send init
    let msg = serde_json::to_vec(&WSPacket::Init {
        id: cfg.id.clone(),
        token: cfg.token.clone(),
    })?;
    ws.send(Message::binary(msg)).await?;

    // recv init
    let msg = ws.next().await.ok_or(CryonetError::Connection)??;
    let WSPacket::Init { id: remote_id, token } = serde_json::from_slice(&msg.into_data())? else {
        Err(CryonetError::Connection)?
    };
    if token != cfg.token {
        Err(CryonetError::Token(endpoint.clone()))?
    }

    // send offer
    let rtc = create_rtc_connection().await?;
    let signal = rtc.create_data_channel("signal", None).await?;
    let data = rtc.create_data_channel("data", None).await?;

    let offer = rtc.create_offer(None).await?;
    let mut gather = rtc.gathering_complete_promise().await;
    rtc.set_local_description(offer).await?;
    let _ = gather.recv().await;
    let local_desc = rtc.local_description().await.ok_or(CryonetError::Connection)?;
    let msg = serde_json::to_vec(&WSPacket::Offer(local_desc))?;
    ws.send(Message::binary(msg)).await?;

    // recv answer
    let msg = ws.next().await.ok_or(CryonetError::Connection)??;
    let WSPacket::Answer(remote_desc) = serde_json::from_slice(&msg.into_data())? else {
        Err(CryonetError::Connection)?
    };
    rtc.set_remote_description(remote_desc).await?;

    Ok(Peer { remote_id, rtc, signal, data })
}

pub(crate) async fn accept(stream: TcpStream, addr: SocketAddr) -> Result<Peer> {
    let cfg = CONFIG.get().unwrap();

    let mut ws = accept_async(stream).await?;

    // recv init
    let msg = ws.next().await.ok_or(CryonetError::Connection)??;
    let WSPacket::Init { id: remote_id, token } = serde_json::from_slice(&msg.into_data())? else {
        Err(CryonetError::Connection)?
    };
    if token != cfg.token {
        Err(CryonetError::Token(addr.to_string()))?
    }

    // send init
    let msg = serde_json::to_vec(&WSPacket::Init {
        id: cfg.id.clone(),
        token: cfg.token.clone(),
    })?;
    ws.send(Message::binary(msg)).await?;

    // recv offer
    let rtc = create_rtc_connection().await?;
    let (signal_tx, mut signal) = channel(1);
    let (data_tx, mut data) = channel(1);
    rtc.on_data_channel(Box::new(move |channel| {
        let signal_tx = signal_tx.clone();
        let data_tx = data_tx.clone();
        Box::pin(async move {
            match channel.label() {
                "signal" => signal_tx.send(channel).await.unwrap(),
                "data" => data_tx.send(channel).await.unwrap(),
                _ => unreachable!(),
            }
        })
    }));

    let msg = ws.next().await.ok_or(CryonetError::Connection)??;
    let WSPacket::Offer(remote_desc) = serde_json::from_slice(&msg.into_data())? else {
        Err(CryonetError::Connection)?
    };
    rtc.set_remote_description(remote_desc).await?;

    // send answer
    let answer = rtc.create_answer(None).await?;
    let mut gather = rtc.gathering_complete_promise().await;
    rtc.set_local_description(answer).await?;
    let _ = gather.recv().await;
    let local_desc = rtc.local_description().await.ok_or(CryonetError::Connection)?;
    let msg = serde_json::to_vec(&WSPacket::Answer(local_desc))?;
    ws.send(Message::binary(msg)).await?;


    let signal = signal.recv().await.unwrap();
    let data = data.recv().await.unwrap();
    Ok(Peer { remote_id, rtc, signal, data })
}
