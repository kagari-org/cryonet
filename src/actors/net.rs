use std::{collections::HashMap, time::SystemTime};

use ractor::{async_trait, call, cast, registry::where_is, Actor, ActorProcessingErr, ActorRef, SupervisionEvent};
use tracing::{debug, error, info};

use crate::{actors::{peer::{DescPacket, PeerActor}, ws_connect::WSConnectActorMsg}, CONFIG};

use super::{peer::{AlivePacket, Peer, PeerActorMsg}, rtc_shake::{RTCShakeActor, RTCShakeActorArgs, RTCShakeActorMsg}, ws_connect::WSConnectActor, ws_listen::WSListenActor};

#[derive(Debug)]
pub(crate) enum NetPeerRef {
    Actor(ActorRef<PeerActorMsg>),
    Added,
    Pending(ActorRef<RTCShakeActorMsg>),
}

#[derive(Debug)]
pub(crate) struct NetPeer {
    #[allow(dead_code)]
    remote_id: String,
    last_shake: SystemTime,
    actor: NetPeerRef,
}

#[derive(derive_more::Debug)]
pub(crate) enum NetActorMsg {
    // events from other actors
    #[debug("NewPeer")]
    NewPeer(Peer),
    Alive(String, AlivePacket),
    RemoteDesc(DescPacket),
    PeerDisconnected(String),

    // events from self
    SendAlive,
    Check,
}

#[derive(Debug)]
pub(crate) struct NetActorState {
    peers: HashMap<String, NetPeer>,
}

pub(crate) struct NetActor;

#[async_trait]
impl Actor for NetActor {
    type Msg = NetActorMsg;
    type State = NetActorState;
    type Arguments = ();

    async fn pre_start(
        &self,
        myself: ActorRef<Self::Msg>,
        _: Self::Arguments,
    ) -> Result<Self::State, ActorProcessingErr> {
        info!("spawn WSListenActor");
        Actor::spawn_linked(Some("ws_listen".to_string()), WSListenActor, (), myself.get_cell()).await?;
        info!("spawn WSConnectActor");
        Actor::spawn_linked(Some("ws_connect".to_string()), WSConnectActor, (), myself.get_cell()).await?;

        let cfg = CONFIG.get().unwrap();
        myself.send_interval(cfg.check_interval, || NetActorMsg::Check);
        cast!(myself, NetActorMsg::Check)?;
        myself.send_interval(cfg.send_alive_interval, || NetActorMsg::SendAlive);

        Ok(NetActorState { peers: HashMap::new() })
    }

    async fn handle(
        &self,
        myself: ActorRef<Self::Msg>,
        message: Self::Msg,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        debug!("received event: {message:?}");
        let cfg = CONFIG.get().unwrap();
        match message {
            // new peer from ws, ws_listen or rtc_shake
            NetActorMsg::NewPeer(peer) => {
                if let Some(NetPeer { actor, .. }) = state.peers.remove(&peer.remote_id) {
                    match actor {
                        NetPeerRef::Pending(actor_ref) => {
                            actor_ref.stop(Some("established connection with peer".to_string()));
                        },
                        NetPeerRef::Actor(actor_ref) => {
                            actor_ref.stop(Some("replaced by new connection".to_string()));
                        },
                        NetPeerRef::Added => {},
                    }
                }
                let remote_id = peer.remote_id.clone();
                info!("spawn PeerActor for {remote_id}");
                let (actor, _) = Actor::spawn_linked(None, PeerActor, peer, myself.get_cell()).await?;
                let net_peer = NetPeer {
                    remote_id: remote_id.clone(),
                    last_shake: SystemTime::now(),
                    actor: NetPeerRef::Actor(actor),
                };
                state.peers.insert(remote_id, net_peer);
            },
            // received alive packet
            NetActorMsg::Alive(remote_id, alive_packet) => {
                let entry = state.peers.entry(remote_id.clone());
                let now = SystemTime::now();
                let net_peer = entry.or_insert(NetPeer {
                    remote_id: remote_id.clone(),
                    last_shake: now,
                    actor: NetPeerRef::Added,
                });
                net_peer.last_shake = now;
                // new peer ids from peer
                for peer in alive_packet.peers {
                    if peer == cfg.id || peer == remote_id { continue; }
                    let entry = state.peers.entry(peer.clone());
                    entry.or_insert(NetPeer {
                        remote_id: peer,
                        last_shake: now,
                        actor: NetPeerRef::Added,
                    });
                }
            },
            NetActorMsg::RemoteDesc(packet) => {
                if packet.target_id == cfg.id {
                    // desc arrived
                    if let Some(NetPeer {
                        actor: NetPeerRef::Pending(actor_ref),
                        ..
                    }) = state.peers.get(&packet.from_id) {
                        cast!(actor_ref, RTCShakeActorMsg::PutDesc(packet.desc))?;
                    }
                } else {
                    // forward desc
                    // only forward once
                    if let Some(NetPeer {
                        actor: NetPeerRef::Actor(actor_ref),
                        ..
                    }) = state.peers.get(&packet.target_id) {
                        cast!(actor_ref, PeerActorMsg::SendDesc(packet))?;
                    }
                }
            },
            NetActorMsg::PeerDisconnected(remote_id) => {
                let peer = state.peers.get_mut(&remote_id);
                if let Some(NetPeer { remote_id, last_shake, actor }) = peer {
                    if let NetPeerRef::Actor(actor_ref) = actor {
                        error!("peer `{remote_id}` disconnected, reconnecting...");
                        actor_ref.stop(Some("peer disconnected".to_string()));
                        *actor = NetPeerRef::Added;
                        *last_shake = SystemTime::now();
                    }
                }
            },
            // send alive at intervals
            NetActorMsg::SendAlive => {
                let peers: Vec<String> = state.peers.keys()
                    .map(|id| id.to_string())
                    .collect();
                let alive = AlivePacket { peers };
                for peer in state.peers.values() {
                    if let NetPeerRef::Actor(peer) = &peer.actor {
                        cast!(peer, PeerActorMsg::SendAlive(alive.clone()))?;
                    }
                }
            },
            // run check at intervals
            NetActorMsg::Check => {
                debug!("starting check");
                // ws connect
                if state.peers.is_empty() {
                    let ws_connect: ActorRef<WSConnectActorMsg> = where_is("ws_connect".to_string())
                        .unwrap().into();
                    cast!(ws_connect, WSConnectActorMsg::Connect)?;
                }
                // check alive
                state.peers.retain(|remote_id, peer| {
                    let now = SystemTime::now();
                    let dur = now.duration_since(peer.last_shake).unwrap();
                    let timeout = dur > cfg.check_timeout;
                    match (&mut peer.actor, timeout) {
                        (_, false) => true,
                        (NetPeerRef::Added, true) => false,
                        (NetPeerRef::Pending(actor_ref), true) => {
                            error!("peer `{remote_id}` has benn dropped");
                            actor_ref.stop(Some("shake timeout".to_string()));
                            false
                        }
                        // reconnect
                        (actor@NetPeerRef::Actor(_), true) => {
                            error!("actor `{remote_id}` disconnected, reconnecting...");
                            let NetPeerRef::Actor(actor_ref) = &actor else { unreachable!() };
                            actor_ref.stop(Some("alive timeout".to_string()));
                            *actor = NetPeerRef::Added;
                            peer.last_shake = SystemTime::now();
                            true
                        }
                    }
                });
                // spawn rtc_shake
                for (remote_id, peer) in &mut state.peers {
                    if !matches!(peer.actor, NetPeerRef::Added) { continue; }
                    info!("spawn RTCShakeActor for {remote_id}");
                    let (rtc_shake, _) = Actor::spawn_linked(None, RTCShakeActor, RTCShakeActorArgs {
                        remote_id: remote_id.clone(),
                    }, myself.get_cell()).await?;
                    peer.actor = NetPeerRef::Pending(rtc_shake);
                }
                // collect desc
                let mut descs = Vec::<DescPacket>::with_capacity(state.peers.len());
                for (remote_id, peer) in &state.peers {
                    if let NetPeerRef::Pending(actor_ref) = &peer.actor {
                        let desc = call!(actor_ref, RTCShakeActorMsg::GetDesc)?;
                        if let Some(desc) = desc {
                            descs.push(DescPacket {
                                from_id: cfg.id.clone(),
                                target_id: remote_id.clone(),
                                desc,
                            });
                        }
                    }
                }
                for desc in descs {
                    for (remote_id, peer) in &state.peers {
                        if let NetPeerRef::Actor(actor_ref) = &peer.actor && desc.target_id != *remote_id {
                            cast!(actor_ref, PeerActorMsg::SendDesc(desc.clone()))?;
                        }
                    }
                }
            },
        };
        Ok(())
    }

    async fn handle_supervisor_evt(
        &self,
        myself: ActorRef<Self::Msg>,
        message: SupervisionEvent,
        _: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        match message {
            // ignore ActorTerminated
            SupervisionEvent::ActorFailed(_, err) => {
                error!("actor failed: {err}");
                myself.stop(None);
            },
            _ => {},
        }
        Ok(())
    }
}
