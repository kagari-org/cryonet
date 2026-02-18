use std::{
    any::Any,
    collections::HashMap,
    time::{Duration, Instant},
};

use cidr::AnyIpCidr;
use rustrtc::{IceCandidate, PeerConnectionState, RtcConfiguration, SdpType, SessionDescription};
use sactor::{
    error::{SactorError, SactorResult},
    sactor,
};
use serde::{Deserialize, Serialize};
use tokio::{
    select,
    sync::{
        broadcast::{self, error::RecvError},
        mpsc,
    },
    time::{Interval, interval},
};
use tracing::{debug, error, warn};
use uuid::Uuid;

use crate::{
    fullmesh::conn::{PeerConn, PeerConnReceiver, PeerConnSender},
    mesh::{
        MeshHandle,
        packet::{NodeId, Packet, Payload},
    },
};

pub(crate) mod conn;
pub(crate) mod tun;

pub(crate) struct FullMesh {
    handle: FullMeshHandle,

    id: NodeId,
    mesh: MeshHandle,

    timeout: Duration,
    discard_timeout: Duration,
    max_connected: usize,
    config: RtcConfiguration,
    candidate_filter_prefix: Option<AnyIpCidr>,

    packet_rx: mpsc::Receiver<Packet>,
    ticker: Interval,

    peers: HashMap<NodeId, HashMap<Uuid, PeerConn>>,
    refresh: broadcast::Sender<()>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
enum FullMeshPayload {
    Offer(Uuid, String),
    Answer(Uuid, String),
    Candidate(Uuid, String),
}

#[typetag::serde]
impl Payload for FullMeshPayload {}

#[sactor(pub(crate))]
impl FullMesh {
    pub(crate) async fn new(id: NodeId, mesh: MeshHandle, config: RtcConfiguration, candidate_filter_prefix: Option<AnyIpCidr>) -> SactorResult<FullMeshHandle> {
        Self::new_with_parameters(id, mesh, Duration::from_secs(30), Duration::from_secs(60), 5, config, candidate_filter_prefix).await
    }

    pub(crate) async fn new_with_parameters(id: NodeId, mesh: MeshHandle, timeout: Duration, discard_timeout: Duration, max_connected: usize, config: RtcConfiguration, candidate_filter_prefix: Option<AnyIpCidr>) -> SactorResult<FullMeshHandle> {
        let packet_rx = mesh.add_dispatchee(Box::new(|packet| (packet.payload.as_ref() as &dyn Any).is::<FullMeshPayload>())).await?;
        let (future, fm) = FullMesh::run(move |handle| FullMesh {
            handle,
            id,
            mesh,
            timeout,
            discard_timeout,
            max_connected,
            config,
            candidate_filter_prefix,
            packet_rx,
            ticker: interval(Duration::from_secs(10)),
            peers: HashMap::new(),
            refresh: broadcast::channel(16).0,
        });
        tokio::spawn(future);
        Ok(fm)
    }

    #[select]
    fn select(&mut self) -> Vec<Selection<'_>> {
        vec![selection!(self.packet_rx.recv().await, handle_packet, it => it), selection!(self.ticker.tick().await, tick)]
    }

    #[no_reply]
    async fn handle_packet(&mut self, packet: Option<Packet>) -> SactorResult<()> {
        let Some(packet) = packet else {
            self.handle.stop();
            return Ok(());
        };
        let src = packet.src;
        let payload = (packet.payload.as_ref() as &dyn Any).downcast_ref::<FullMeshPayload>().unwrap();
        if src == self.id {
            error!("Received packet from self (node {:X}), ignoring", self.id);
            return Ok(());
        }
        match payload {
            FullMeshPayload::Offer(id, offer) if self.id > src => {
                let mut conn = PeerConn::new(self.config.clone()).await?;
                start_peer_loop(self.handle.clone(), self.mesh.clone(), src, *id, &conn);
                let mut offer = SessionDescription::parse(SdpType::Offer, offer)?;
                filter_candidate(&mut offer, &self.candidate_filter_prefix);
                let answer = conn.answer(offer).await?;
                self.mesh.send_packet(src, Box::new(FullMeshPayload::Answer(*id, answer.to_string()))).await??;
                self.peers.entry(src).or_default().insert(*id, conn);
                let _ = self.refresh.send(());
            }
            FullMeshPayload::Answer(id, answer) if self.id < src => {
                let conn = self.peers.get(&src).and_then(|conns| conns.get(id));
                let Some(conn) = conn else {
                    warn!("No PeerConn found for node {:X} id {}, ignoring", src, id);
                    return Ok(());
                };
                let mut answer = SessionDescription::parse(SdpType::Answer, answer)?;
                filter_candidate(&mut answer, &self.candidate_filter_prefix);
                conn.answered(answer).await?;
                let _ = self.refresh.send(());
            }
            FullMeshPayload::Candidate(id, candidate) => {
                let conn = self.peers.get(&src).and_then(|conns| conns.get(id));
                let Some(conn) = conn else {
                    warn!("No PeerConn found for node {:X} id {}, ignoring", src, id);
                    return Ok(());
                };
                let candidate = IceCandidate::from_sdp(candidate).map_err(|e| SactorError::Other(e))?;
                if !check_candidate(&candidate, &self.candidate_filter_prefix) {
                    return Ok(());
                }
                conn.add_ice_candidate(candidate).await?;
            }
            _ => {
                warn!("Unexpected packet type from node {:X}, ignoring", src);
                return Ok(());
            }
        };
        Ok(())
    }

    #[no_reply]
    async fn tick(&mut self) -> SactorResult<()> {
        // gc
        let time = Instant::now();
        for conns in self.peers.values_mut() {
            conns.retain(|_, conn| {
                let timeouted = time.duration_since(conn.time) > self.timeout;
                !timeouted || conn.connected()
            });
        }
        // check connected
        let peers = self.mesh.get_routes().await?.keys().cloned().collect::<Vec<_>>();
        for node_id in peers {
            let result: SactorResult<()> = try {
                let conns = self.peers.entry(node_id).or_default();
                let connected = conns.values().filter(|conn| conn.connected()).count();
                if connected >= self.max_connected {
                    let newest = conns.values().max_by(|x, y| x.time.cmp(&y.time)).unwrap();
                    if newest.time.elapsed() > self.discard_timeout {
                        let mut entries = conns.iter().collect::<Vec<_>>();
                        entries.sort_by(|x, y| x.1.time.cmp(&y.1.time));
                        let sorted = entries.iter().map(|(id, _)| **id).collect::<Vec<_>>();
                        for id in sorted {
                            if conns.len() < self.max_connected {
                                break;
                            }
                            if conns.get(&id).unwrap().selected {
                                continue;
                            }
                            conns.remove(&id);
                            let _ = self.refresh.send(());
                        }
                    } else {
                        continue;
                    }
                }
                if self.id > node_id {
                    continue;
                }
                let mut conn = PeerConn::new(self.config.clone()).await?;
                let id = Uuid::new_v4();
                start_peer_loop(self.handle.clone(), self.mesh.clone(), node_id, id, &conn);
                let offer = conn.offer().await?;
                self.mesh.send_packet(node_id, Box::new(FullMeshPayload::Offer(id, offer))).await??;
                conns.insert(id, conn);
            };
            if let Err(err) = result {
                error!("Failed to connect to node {:X}: {}", node_id, err);
            }
        }
        // select
        for conns in self.peers.values_mut() {
            if conns.is_empty() {
                continue;
            }
            let old = conns.iter().find_map(|(id, peer)| if peer.selected { Some(*id) } else { None });
            for conn in conns.values_mut() {
                conn.selected = false;
            }
            let mut connected = conns.iter_mut().filter(|conn| conn.1.connected()).collect::<Vec<_>>();
            connected.sort_by(|x, y| y.1.time.cmp(&x.1.time));
            let best = match connected.len() {
                0 => None,
                1 => Some(&mut connected[0]),
                // delay connection selection
                _ => Some(&mut connected[1]),
            };
            match (old, best) {
                (None, Some(select)) => {
                    select.1.selected = true;
                    let _ = self.refresh.send(());
                }
                (Some(_), None) => {
                    let _ = self.refresh.send(());
                }
                (Some(old), Some(select)) => {
                    select.1.selected = true;
                    if old != *select.0 {
                        let _ = self.refresh.send(());
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn remove_conn(&mut self, node_id: NodeId, id: Uuid) {
        if let Some(conns) = self.peers.get_mut(&node_id) {
            conns.remove(&id);
            let _ = self.refresh.send(());
        }
    }

    pub(crate) fn subscribe_refresh(&self) -> broadcast::Receiver<()> {
        self.refresh.subscribe()
    }

    pub(crate) fn get_senders(&self) -> HashMap<NodeId, PeerConnSender> {
        let mut senders = HashMap::new();
        for (node_id, conns) in &self.peers {
            for conn in conns.values() {
                if conn.selected {
                    senders.insert(*node_id, conn.sender());
                    break;
                }
            }
        }
        senders
    }

    pub(crate) fn get_receivers(&self) -> HashMap<NodeId, Vec<PeerConnReceiver>> {
        let mut receivers = HashMap::new();
        for (node_id, conns) in &self.peers {
            let mut entry = Vec::new();
            for conn in conns.values() {
                if !conn.connected() {
                    continue;
                }
                let receiver = match conn.receiver() {
                    Ok(receiver) => receiver,
                    Err(err) => {
                        warn!("Failed to get receiver from node {:X}: {}", node_id, err);
                        continue;
                    }
                };
                entry.push(receiver);
            }
            receivers.insert(*node_id, entry);
        }
        receivers
    }

    pub(crate) fn get_peers(&self) -> HashMap<NodeId, HashMap<Uuid, cryonet_uapi::Conn>> {
        self.peers
            .iter()
            .map(|(node_id, conns)| {
                let conns = conns
                    .iter()
                    .map(|(uuid, conn)| {
                        use PeerConnectionState::*;
                        (
                            *uuid,
                            cryonet_uapi::Conn {
                                selected: conn.selected,
                                state: match *conn.state_watcher.borrow() {
                                    New => cryonet_uapi::ConnState::New,
                                    Connecting => cryonet_uapi::ConnState::Connecting,
                                    Connected => cryonet_uapi::ConnState::Connected,
                                    Disconnected => cryonet_uapi::ConnState::Disconnected,
                                    Failed => cryonet_uapi::ConnState::Failed,
                                    Closed => cryonet_uapi::ConnState::Closed,
                                },
                            },
                        )
                    })
                    .collect();
                (*node_id, conns)
            })
            .collect()
    }
}

fn start_peer_loop(fm: FullMeshHandle, mesh: MeshHandle, id: NodeId, uuid: Uuid, conn: &PeerConn) {
    let mut candidate = conn.subscribe_candidates();
    let mut state = conn.subscribe_state();
    tokio::spawn(async move {
        loop {
            select! {
                candidate = candidate.recv() => {
                    let candidate = match candidate {
                        Ok(candidate) => candidate,
                        Err(RecvError::Lagged(_)) => continue,
                        Err(err) => {
                            debug!("Candidate channel closed for node {:X}: {}", id, err);
                            break;
                        }
                    };
                    debug!("Sending candidate to node {:X}: {:?}", id, candidate);
                    let _ = mesh.send_packet(id, Box::new(FullMeshPayload::Candidate(uuid, candidate.to_sdp()))).await;
                }
                s = state.changed() => {
                    let Ok(_) = s else {
                        debug!("State channel closed for node {:X}", id);
                        break;
                    };
                    let s = *state.borrow_and_update();
                    use PeerConnectionState::*;
                    if s == Disconnected || s == Failed || s == Closed {
                        let _ = fm.remove_conn(id, uuid).await;
                        break;
                    }
                }
            }
        }
    });
}

fn filter_candidate(sdp: &mut SessionDescription, cidr: &Option<AnyIpCidr>) {
    for sections in &mut sdp.media_sections {
        sections.attributes.retain(|attr| {
            if attr.key != "candidate" {
                return true;
            }
            if let Some(val) = &attr.value
                && let Ok(candidate) = IceCandidate::from_sdp(val)
                && !check_candidate(&candidate, cidr)
            {
                return false;
            }
            true
        });
    }
}

fn check_candidate(candidate: &IceCandidate, cidr: &Option<AnyIpCidr>) -> bool {
    let Some(cidr) = cidr else {
        return true;
    };
    !cidr.contains(&candidate.address.ip())
}
