use std::{any::Any, collections::HashMap, sync::{Arc, Weak}, time::{Duration, Instant}};

use anyhow::Result;
use rustrtc::{IceCandidate, PeerConnectionState, RtcConfiguration};
use serde::{Deserialize, Serialize};
use tokio::{select, sync::{Mutex, broadcast::{self, error::RecvError}, mpsc}, time::interval};
use tracing::{debug, error, warn};
use uuid::Uuid;

use crate::{fullmesh::conn::{PeerConn, PeerConnReceiver, PeerConnSender}, mesh::{Mesh, packet::{NodeId, Payload}}};

pub(crate) mod conn;
pub(crate) mod tun;

pub(crate) struct FullMesh {
    id: NodeId,
    timeout: Duration,
    discard_timeout: Duration,
    max_connected: usize,
    config: RtcConfiguration,
    mesh: Arc<Mutex<Mesh>>,
    peers: HashMap<NodeId, HashMap<Uuid, Conn>>,
    stop: mpsc::UnboundedSender<()>,
    refresh: broadcast::Sender<()>,

    this: Weak<Mutex<FullMesh>>,
}

struct Conn {
    selected: bool,
    time: Instant,
    conn: PeerConn,
}

impl Drop for Conn {
    fn drop(&mut self) {
        self.conn.close();
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
enum FullMeshPayload {
    Offer(Uuid, String),
    Answer(Uuid, String),
    Candidate(Uuid, String),
}

#[typetag::serde]
impl Payload for FullMeshPayload {}

impl FullMesh {
    pub(crate) async fn new(
        mesh: Arc<Mutex<Mesh>>,
        config: RtcConfiguration,
    ) -> Arc<Mutex<FullMesh>> {
        Self::new_with_parameters(
            mesh,
            Duration::from_secs(30),
            Duration::from_secs(60),
            5,
            config,
        ).await
    }

    pub(crate) async fn new_with_parameters(
        mesh: Arc<Mutex<Mesh>>,
        timeout: Duration,
        discard_timeout: Duration,
        max_connected: usize,
        config: RtcConfiguration,
    ) -> Arc<Mutex<FullMesh>> {
        let (stop_tx, mut stop_rx) = mpsc::unbounded_channel();
        let id = mesh.lock().await.id;
        let fm = Arc::new_cyclic(|this| Mutex::new(FullMesh {
            id,
            timeout,
            discard_timeout,
            max_connected,
            config,
            mesh: mesh.clone(),
            peers: HashMap::new(),
            stop: stop_tx,
            refresh: broadcast::channel(1).0,
            this: this.clone(),
        }));
        let fm2 = fm.clone();
        tokio::spawn(async move {
            let mut packets = fm.lock().await.mesh.lock().await.add_dispatchee(|packet|
                    (packet.payload.as_ref() as &dyn Any).is::<FullMeshPayload>());
            let mut ticker = interval(Duration::from_secs(10));
            loop {
                select! {
                    _ = stop_rx.recv() => break,
                    _ = ticker.tick() => {
                        fm.lock().await.tick().await;
                    },
                    packet = packets.recv() => {
                        let Some(packet) = packet else {
                            error!("FullMesh packet channel closed unexpectedly");
                            break;
                        };
                        let payload = (packet.payload.as_ref() as &dyn Any)
                            .downcast_ref::<FullMeshPayload>().unwrap();
                        let result = fm.lock().await.handle_packet(packet.src, payload).await;
                        if let Err(err) = result {
                            warn!("Failed to handle packet from node {:X}: {}", packet.src, err);
                        }
                    }
                }
            }
        });
        fm2
    }

    async fn handle_packet(
        &mut self,
        src: NodeId,
        payload: &FullMeshPayload,
    ) -> Result<()> {
        if src == self.id {
            error!("Received packet from self (node {:X}), ignoring", self.id);
            return Ok(());
        }
        match payload {
            FullMeshPayload::Offer(id, offer) if self.id > src => {
                let mut conn = PeerConn::new(self.config.clone()).await?;
                let answer = conn.answer(offer).await?;
                self.mesh.lock().await.send_packet(
                    src,
                    FullMeshPayload::Answer(*id, answer.to_string()),
                ).await?;
                self.start_peer_loop(src, *id, &conn);
                let origin = self.peers.entry(src)
                    .or_insert_with(|| HashMap::new())
                    .insert(*id, create_peer(src, *id, conn, self.this.clone()));
                if origin.is_some() {
                    warn!("Overwriting existing PeerConn for node {:X} id {}", src, id);
                }
                let _ = self.refresh.send(());
            },
            FullMeshPayload::Answer(id, answer) if self.id < src => {
                let conn = self.peers.get(&src).and_then(|conns| conns.get(id));
                let Some(conn) = conn else {
                    warn!("No PeerConn found for node {:X} id {}, ignoring", src, id);
                    return Ok(());
                };
                conn.conn.answered(answer).await?;
                self.start_peer_loop(src, *id, &conn.conn);
                let _ = self.refresh.send(());
            },
            FullMeshPayload::Candidate(id, candidate) => {
                let conn = self.peers.get(&src).and_then(|conns| conns.get(id));
                let Some(conn) = conn else {
                    warn!("No PeerConn found for node {:X} id {}, ignoring", src, id);
                    return Ok(());
                };
                conn.conn.add_ice_candidate(IceCandidate::from_sdp(candidate)?).await?;
            },
            _ => {
                warn!("Unexpected packet type from node {:X}, ignoring", src);
                return Ok(());
            },
        };
        Ok(())
    }

    async fn tick(&mut self) {
        // gc
        let time = Instant::now();
        for (_, conns) in &mut self.peers {
            conns.retain(|_, conn| {
                let timeouted = time.duration_since(conn.time) > self.timeout;
                !timeouted || conn.conn.connected()
            });
        }
        // check connected
        let peers = self.mesh.lock().await.get_routes().keys().cloned().collect::<Vec<_>>();
        for node_id in peers {
            let conns = self.peers.entry(node_id)
                .or_insert_with(|| HashMap::new());
            let connected = conns
                .values()
                .filter(|conn| conn.conn.connected())
                .count();
            if connected >= self.max_connected {
                let newest = conns.values()
                    .max_by(|x, y| x.time.cmp(&y.time))
                    .unwrap();
                if newest.time.elapsed() > self.discard_timeout {
                    let mut entries = conns.iter().collect::<Vec<_>>();
                    entries.sort_by(|x, y| x.1.time.cmp(&y.1.time));
                    let sorted = entries
                        .iter()
                        .map(|(id, _)| (**id).clone())
                        .collect::<Vec<_>>();
                    for id in sorted {
                        if conns.len() <= self.max_connected - 1 {
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
            let conn = PeerConn::new(self.config.clone()).await;
            let mut conn = match conn {
                Ok(conn) => conn,
                Err(err) => {
                    error!("Failed to create PeerConn to node {:X}: {}", node_id, err);
                    continue;
                },
            };
            let id = Uuid::new_v4();
            let offer = match conn.offer().await {
                Ok(offer) => offer,
                Err(err) => {
                    error!("Failed to create offer to node {:X}: {}", node_id, err);
                    continue;
                },
            };
            let result = self.mesh.lock().await
                .send_packet(node_id, FullMeshPayload::Offer(id, offer)).await;
            if let Err(err) = result {
                error!("Failed to send offer to node {:X}: {}", node_id, err);
                continue;
            }
            conns.insert(id, create_peer(node_id, id, conn, self.this.clone()));
        }
        // select
        for (_, conns) in &mut self.peers {
            if conns.is_empty() {
                continue;
            }
            let old = conns.iter().find_map(|(id, peer)| if peer.selected {
                Some(*id)
            } else {
                None
            });
            for conn in conns.values_mut() {
                conn.selected = false;
            }
            let mut connected = conns.iter_mut()
                .filter(|conn| conn.1.conn.connected())
                .collect::<Vec<_>>();
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
                },
                (Some(_), None) => {
                    let _ = self.refresh.send(());
                },
                (Some(old), Some(select)) => {
                    select.1.selected = true;
                    if old != *select.0 {
                        let _ = self.refresh.send(());
                    }
                },
                _ => {}
            }
        }
    }

    fn start_peer_loop(&self, id: NodeId, uuid: Uuid, peer: &PeerConn) {
        let mesh = self.mesh.clone();
        let mut candidate = peer.subscribe_candidates();
        tokio::spawn(async move {
            loop {
                let candidate = candidate.recv().await;
                let candidate = match candidate {
                    Ok(candidate) => candidate,
                    Err(RecvError::Lagged(_)) => continue,
                    Err(err) => {
                        debug!("Candidate channel closed for node {:X}: {}", id, err);
                        break;
                    },
                };
                debug!("Sending candidate to node {:X}: {:?}", id, candidate);
                let result = mesh.lock().await.send_packet(
                    id,
                    FullMeshPayload::Candidate(uuid, candidate.to_sdp()),
                ).await;
                if let Err(err) = result {
                    warn!("Failed to send candidate to node {:X}: {}", id, err);
                }
            }
        });
    }

    pub(crate) fn subscribe_refresh(&self) -> broadcast::Receiver<()> {
        self.refresh.subscribe()
    }

    pub(crate) fn get_senders(&self) -> HashMap<NodeId, PeerConnSender> {
        let mut senders = HashMap::new();
        for (node_id, conns) in &self.peers {
            for conn in conns.values() {
                if conn.selected {
                    senders.insert(*node_id, conn.conn.sender());
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
                if !conn.conn.connected() {
                    continue;
                }
                let receiver = match conn.conn.receiver() {
                    Ok(receiver) => receiver,
                    Err(err) => {
                        warn!("Failed to get receiver from node {:X}: {}", node_id, err);
                        continue;
                    },
                };
                entry.push(receiver);
            }
            receivers.insert(*node_id, entry);
        }
        receivers
    }

    pub(crate) fn stop(&mut self) -> Result<()> {
        self.stop.send(())?;
        Ok(())
    }
}

fn create_peer(
    node_id: NodeId,
    id: Uuid,
    conn: PeerConn,
    fm: Weak<Mutex<FullMesh>>,
) -> Conn {
    let mut state = conn.subscribe_state();
    tokio::spawn(async move {
        loop {
            let Ok(_) = state.changed().await else {
                break;
            };
            let s = *state.borrow_and_update();
            use PeerConnectionState::*;
            match s {
                Disconnected | Failed | Closed => {
                    let Some(fm) = fm.upgrade() else {
                        break;
                    };
                    let mut fm = fm.lock().await;
                    let conn = fm.peers
                        .get_mut(&node_id)
                        .and_then(|conns| conns.remove(&id));
                    if let Some(_) = conn {
                        let _ = fm.refresh.send(());
                    }
                },
                _ => {}
            }
        }
    });
    Conn {
        selected: false,
        time: Instant::now(),
        conn,
    }
}
