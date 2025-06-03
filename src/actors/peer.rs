use std::{os::fd::AsRawFd, sync::Arc};

use ractor::{
    async_trait, cast, registry::where_is, Actor, ActorProcessingErr, ActorRef, ActorStatus, RpcReplyPort
};
use serde::{Deserialize, Serialize};
use tokio_tungstenite::tungstenite::Bytes;
use tracing::{error, info, warn};
use tun_rs::{AsyncDevice, DeviceBuilder};
use webrtc::{
    data_channel::RTCDataChannel,
    peer_connection::{
        RTCPeerConnection, peer_connection_state::RTCPeerConnectionState,
        sdp::session_description::RTCSessionDescription,
    },
};

use crate::{CONFIG, actors::net::NetActorMsg};

pub(crate) struct Peer {
    pub(crate) remote_id: String,

    pub(crate) rtc: RTCPeerConnection,
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
    pub(crate) desc: Box<RTCSessionDescription>,
}

pub(crate) struct PeerActor;
pub(crate) struct PeerActorState(Peer, Arc<AsyncDevice>);
#[derive(Debug)]
pub(crate) enum PeerActorMsg {
    Signal(Bytes),
    SendAlive(AlivePacket),
    SendDesc(DescPacket),
    Stop(RpcReplyPort<()>),
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
        let cfg = CONFIG.get().unwrap();
        // send disconnect event to net
        let myself1 = myself.clone();
        let remote_id = peer.remote_id.clone();
        peer.rtc
            .on_peer_connection_state_change(Box::new(move |state| {
                if let RTCPeerConnectionState::Disconnected = state {
                    let net: ActorRef<NetActorMsg> = where_is("net".to_string()).unwrap().into();
                    if let Err(err) = cast!(net, NetActorMsg::PeerDisconnected(remote_id.clone(), myself1.get_id())) {
                        error!("failed to send disconnected event: {err}");
                    };
                    // stop by NetActor
                }
                Box::pin(async {})
            }));

        // receive messages
        let myself2 = myself.clone();
        peer.signal.on_message(Box::new(move |message| {
            if let Err(err) = cast!(myself2, PeerActorMsg::Signal(message.data)) {
                error!("failed to send on_channel event: {err}");
            };
            Box::pin(async {})
        }));

        // setup tun
        info!("setup tun for {}", peer.remote_id);
        let mut tun = DeviceBuilder::new();
        if !cfg.auto_interface_name {
            tun = tun.name(format!("{}{}", cfg.interface_prefix, peer.remote_id));
        }
        if cfg.enable_packet_information {
            tun = tun.packet_information(true);
        }
        let tun = Arc::new(tun.build_async()?);
        let send = tun.clone();
        let recv = send.clone();

        peer.data.on_message(Box::new(move |message| {
            let tun = send.clone();
            Box::pin(async move {
                if let Err(err) = tun.send(&message.data).await {
                    warn!("failed to send data to tun: {err}");
                }
            })
        }));

        let data_channel = peer.data.clone();
        tokio::spawn(async move {
            let cfg = CONFIG.get().unwrap();
            loop {
                if let ActorStatus::Stopped = myself.get_status() {
                    break;
                }
                let mut buf = vec![0u8; cfg.buf_size];
                let len = match recv.recv(&mut buf).await {
                    Ok(len) => len,
                    Err(err) => {
                        warn!("failed to recv data from tun: {err}");
                        continue;
                    }
                };
                let result = data_channel.send(&Bytes::from(buf[0..len].to_vec())).await;
                if let Err(err) = result {
                    error!("failed to send data to peer: {err}");
                }
            }
        });

        Ok(PeerActorState(peer, tun))
    }

    async fn handle(
        &self,
        myself: ActorRef<Self::Msg>,
        message: Self::Msg,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        match message {
            PeerActorMsg::Signal(signal) => {
                let signal: Signal = serde_json::from_slice(&signal)?;
                let net: ActorRef<NetActorMsg> = where_is("net".to_string()).unwrap().into();
                match signal {
                    Signal::Alive(alive) => {
                        cast!(net, NetActorMsg::Alive(state.0.remote_id.clone(), alive))?;
                    }
                    Signal::Desc(desc) => {
                        cast!(net, NetActorMsg::RemoteDesc(desc))?;
                    }
                }
            }
            PeerActorMsg::SendAlive(alive) => {
                let packet = serde_json::to_vec(&Signal::Alive(alive))?;
                if let Err(err) = state.0.signal.send(&Bytes::from(packet)).await {
                    error!("failed to send alive: {err}");
                }
            }
            PeerActorMsg::SendDesc(desc) => {
                let packet = serde_json::to_vec(&Signal::Desc(desc))?;
                if let Err(err) = state.0.signal.send(&Bytes::from(packet)).await {
                    error!("failed to send desc: {err}");
                }
            }
            PeerActorMsg::Stop(reply) => {
                // tun devices are still existing after dropping them, so we manually close it
                // very dirty
                unsafe { libc::close(state.1.as_raw_fd()) };
                reply.send(())?;
                myself.stop(Some("closing".to_string()));
            }
        }
        Ok(())
    }
}
