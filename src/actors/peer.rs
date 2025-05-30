use std::sync::Arc;

use ractor::{async_trait, cast, registry::where_is, Actor, ActorProcessingErr, ActorRef};
use serde::{Deserialize, Serialize};
use tokio_tungstenite::tungstenite::Bytes;
use tracing::error;
use webrtc::{data_channel::RTCDataChannel, peer_connection::RTCPeerConnection};

use crate::actors::net::NetActorMsg;

pub(crate) struct Peer {
    pub(crate) remote_id: String,

    pub(crate) _rtc: RTCPeerConnection,
    pub(crate) signal: Arc<RTCDataChannel>,
    pub(crate) data: Arc<RTCDataChannel>,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) enum Signal {
    Alive(AlivePacket),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct AlivePacket {
    pub(crate) peers: Vec<String>,
}



pub(crate) struct PeerActor;
pub(crate) struct PeerActorState(Peer);
#[derive(Debug)]
pub(crate) enum PeerActorMsg {
    Signal(Bytes),
    SendAlive(AlivePacket),
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
        peer.signal.on_message(Box::new(move |message| {
            if let Err(err) = cast!(myself, PeerActorMsg::Signal(message.data)) {
                error!("failed to send on_channel event: {err}");
            };
            Box::pin(async {})
        }));
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
                match signal {
                    Signal::Alive(alive) => {
                        let net: ActorRef<NetActorMsg> = where_is("net".to_string())
                            .unwrap().into();
                        cast!(net, NetActorMsg::Alive(state.0.remote_id.clone(), alive))?;
                    },
                }
            },
            PeerActorMsg::SendAlive(alive) => {
                let packet = serde_json::to_vec(&Signal::Alive(alive))?;
                state.0.signal.send(&Bytes::from(packet)).await?;
            },
        }
        Ok(())
    }
}
