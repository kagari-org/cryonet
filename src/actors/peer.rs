use std::{sync::Arc, time::Duration};

use ractor::{async_trait, cast, registry::where_is, Actor, ActorProcessingErr, ActorRef};
use serde::{Deserialize, Serialize};
use tokio_tungstenite::tungstenite::Bytes;
use tracing::error;
use webrtc::{data_channel::RTCDataChannel, peer_connection::{sdp::session_description::RTCSessionDescription, RTCPeerConnection}};

use crate::{actors::net::NetActorMsg, CONFIG};

pub(crate) struct Peer {
    pub(crate) remote_id: String,

    pub(crate) _rtc: RTCPeerConnection,
    pub(crate) signal: Arc<RTCDataChannel>,
    pub(crate) data: Arc<RTCDataChannel>,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) enum Signal {
    Alive(AlivePacket),
    Desc(DescPacket),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct AlivePacket {
    pub(crate) peers: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct DescPacket {
    pub(crate) from_id: String,
    pub(crate) target_id: String,
    pub(crate) desc: RTCSessionDescription,
}



pub(crate) struct PeerActor;
pub(crate) struct PeerActorState(Peer);
#[derive(Debug)]
pub(crate) enum PeerActorMsg {
    Signal(Bytes),
    SendAlive(AlivePacket),
    SendDesc(DescPacket),

    Data(Bytes),
    Send,
}

#[async_trait]
impl Actor for PeerActor {
    type Msg = PeerActorMsg;
    type State = PeerActorState;
    type Arguments = Peer;

    async fn pre_start(
        &self,
        myself: ActorRef<Self::Msg>,
        peer: Self::Arguments,
    ) -> Result<Self::State, ActorProcessingErr> {
        // TODO: setup tun here
        // TODO: disconnect event
        let myself1 = myself.clone();
        peer.signal.on_message(Box::new(move |message| {
            if let Err(err) = cast!(myself1, PeerActorMsg::Signal(message.data)) {
                error!("failed to send on_channel event: {err}");
            };
            Box::pin(async {})
        }));
        let myself2 = myself.clone();
        peer.data.on_message(Box::new(move |message| {
            // TODO: may send data to tun directly.
            if let Err(err) = cast!(myself2, PeerActorMsg::Data(message.data)) {
                error!("failed to send on_channel event: {err}");
            };
            Box::pin(async {})
        }));
        myself.send_interval(Duration::from_secs(3), || PeerActorMsg::Send);
        Ok(PeerActorState(peer))
    }

    async fn handle(
        &self,
        _: ActorRef<Self::Msg>,
        message: Self::Msg,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        match message {
            PeerActorMsg::Signal(signal) => {
                let signal: Signal = serde_json::from_slice(&signal)?;
                let net: ActorRef<NetActorMsg> = where_is("net".to_string())
                    .unwrap().into();
                match signal {
                    Signal::Alive(alive) => {
                        cast!(net, NetActorMsg::Alive(state.0.remote_id.clone(), alive))?;
                    },
                    Signal::Desc(desc) => {
                        cast!(net, NetActorMsg::RemoteDesc(desc))?;
                    },
                }
            },
            PeerActorMsg::SendAlive(alive) => {
                let packet = serde_json::to_vec(&Signal::Alive(alive))?;
                state.0.signal.send(&Bytes::from(packet)).await?;
            },
            PeerActorMsg::SendDesc(desc) => {
                let packet = serde_json::to_vec(&Signal::Desc(desc))?;
                state.0.signal.send(&Bytes::from(packet)).await?;
            },
            PeerActorMsg::Data(data) => {
                let x = String::from_utf8_lossy(&data);
                println!("{x}");
            },
            PeerActorMsg::Send => {
                let id = &CONFIG.get().unwrap().id;
                state.0.data.send_text(format!("test from {id}")).await?;
            },
        }
        Ok(())
    }
}
