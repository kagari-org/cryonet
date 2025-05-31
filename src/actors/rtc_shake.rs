use std::sync::Arc;

use ractor::{
    Actor, ActorProcessingErr, ActorRef, RpcReplyPort, async_trait, cast, registry::where_is,
};
use tracing::error;
use webrtc::{
    data_channel::RTCDataChannel,
    peer_connection::{
        RTCPeerConnection, peer_connection_state::RTCPeerConnectionState,
        sdp::session_description::RTCSessionDescription,
    },
};

use crate::{
    CONFIG,
    error::CryonetError,
    utils::rtc::{create_rtc_connection, is_master},
};

use super::{net::NetActorMsg, peer::Peer};

pub(crate) enum RTCShakeActorMsg {
    GetDesc(RpcReplyPort<Option<Box<RTCSessionDescription>>>),
    PutDesc(Box<RTCSessionDescription>),
    Connected,
    OnChannel(Arc<RTCDataChannel>),
}

pub(crate) struct RTCShakeActorState {
    remote_id: String,

    master: bool,
    rtc: Option<RTCPeerConnection>,
    local_desc: Option<Box<RTCSessionDescription>>,
    remote_desc_set: bool,

    signal_channel: Option<Arc<RTCDataChannel>>,
    data_channel: Option<Arc<RTCDataChannel>>,
}

#[derive(Debug)]
pub(crate) struct RTCShakeActorArgs {
    pub(crate) remote_id: String,
}

// this actor is for hand shaking.
// will send NewPeer Msg to NetActor.
// will be killed once shaked.
pub(crate) struct RTCShakeActor;

#[async_trait]
impl Actor for RTCShakeActor {
    type Msg = RTCShakeActorMsg;
    type State = RTCShakeActorState;
    type Arguments = RTCShakeActorArgs;

    async fn pre_start(
        &self,
        myself: ActorRef<Self::Msg>,
        args: Self::Arguments,
    ) -> Result<Self::State, ActorProcessingErr> {
        let cfg = CONFIG.get().unwrap();

        let master = is_master(&cfg.id, &args.remote_id)?;
        let rtc = create_rtc_connection().await?;

        let myself1 = myself.clone();
        rtc.on_data_channel(Box::new(move |channel| {
            if let Err(err) = cast!(myself1, RTCShakeActorMsg::OnChannel(channel)) {
                error!("failed to send on_channel event: {err}");
            };
            Box::pin(async {})
        }));

        let myself2 = myself;
        rtc.on_peer_connection_state_change(Box::new(move |state| {
            if let RTCPeerConnectionState::Connected = state {
                if let Err(err) = cast!(myself2, RTCShakeActorMsg::Connected) {
                    error!("failed to send connected event: {err}");
                };
            }
            Box::pin(async {})
        }));

        let (signal_channel, data_channel) = if master {
            (
                Some(rtc.create_data_channel("shake", None).await?),
                Some(rtc.create_data_channel("data", None).await?),
            )
        } else {
            (None, None)
        };

        let local_desc = if master {
            let offer = rtc.create_offer(None).await?;
            let mut gather = rtc.gathering_complete_promise().await;
            rtc.set_local_description(offer).await?;
            let _ = gather.recv().await;
            Some(
                rtc.local_description()
                    .await
                    .ok_or(CryonetError::Connection)?,
            )
        } else {
            None
        };

        Ok(RTCShakeActorState {
            remote_id: args.remote_id.clone(),
            master,
            rtc: Some(rtc),
            local_desc: local_desc.map(Box::new),
            remote_desc_set: false,
            signal_channel,
            data_channel,
        })
    }

    async fn handle(
        &self,
        myself: ActorRef<Self::Msg>,
        msg: Self::Msg,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        match msg {
            RTCShakeActorMsg::GetDesc(reply) => {
                reply.send(state.local_desc.clone())?;
            }
            RTCShakeActorMsg::PutDesc(desc) => {
                if !state.remote_desc_set {
                    let rtc = state.rtc.as_ref().unwrap();
                    rtc.set_remote_description(*desc).await?;
                    if !state.master {
                        let answer = rtc.create_answer(None).await?;
                        let mut gather = rtc.gathering_complete_promise().await;
                        rtc.set_local_description(answer).await?;
                        let _ = gather.recv().await;
                        let local_desc = rtc
                            .local_description()
                            .await
                            .ok_or(CryonetError::Connection)?;
                        state.local_desc = Some(Box::new(local_desc));
                    }
                    state.remote_desc_set = true;
                }
            }
            RTCShakeActorMsg::Connected => {
                if state.master {
                    let net: ActorRef<NetActorMsg> = where_is("net".to_string()).unwrap().into();
                    let peer = Peer {
                        remote_id: state.remote_id.clone(),
                        rtc: state.rtc.take().unwrap(),
                        signal: state.signal_channel.take().unwrap(),
                        data: state.data_channel.take().unwrap(),
                    };
                    cast!(net, NetActorMsg::NewPeer(peer))?;
                    myself.stop(Some("created new peer".to_string()));
                }
            }
            RTCShakeActorMsg::OnChannel(channel) => {
                match channel.label() {
                    "shake" => state.signal_channel = Some(channel),
                    "data" => state.data_channel = Some(channel),
                    _ => Err(CryonetError::Connection)?,
                }
                if state.signal_channel.is_some() && state.data_channel.is_some() {
                    let net: ActorRef<NetActorMsg> = where_is("net".to_string()).unwrap().into();
                    let peer = Peer {
                        remote_id: state.remote_id.clone(),
                        rtc: state.rtc.take().unwrap(),
                        signal: state.signal_channel.take().unwrap(),
                        data: state.data_channel.take().unwrap(),
                    };
                    cast!(net, NetActorMsg::NewPeer(peer))?;
                    myself.stop(Some("created new peer".to_string()));
                }
            }
        }
        Ok(())
    }
}
