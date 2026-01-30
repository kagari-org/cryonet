use std::{any::Any, collections::HashMap, sync::{Arc, Weak}, time::{Duration, Instant}};

use anyhow::Result;
use rustrtc::{IceCandidate, PeerConnectionState, RtcConfiguration};
use serde::{Deserialize, Serialize};
use tokio::{select, sync::{Mutex, Notify, futures::Notified, mpsc}, time::interval};
use tracing::{debug, error, warn};
use uuid::Uuid;

use crate::{fullmesh::conn::PeerConn, mesh::{Mesh, packet::{NodeId, Payload}}};

pub(crate) mod conn;

pub(crate) struct FullMesh {
    id: NodeId,
    timeout: Duration,
    discard_timeout: Duration,
    max_connected: usize,
    config: RtcConfiguration,
    mesh: Arc<Mutex<Mesh>>,
    peers: HashMap<NodeId, HashMap<Uuid, Conn>>,
    stop: mpsc::UnboundedSender<()>,
    refresh: Notify,

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
            refresh: Notify::new(),
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
                            warn!("packet channel closed");
                            break;
                        };
                        let payload = (packet.payload.as_ref() as &dyn Any)
                            .downcast_ref::<FullMeshPayload>().unwrap();
                        let result = fm.lock().await.handle_packet(packet.src, payload).await;
                        if let Err(err) = result {
                            warn!("error handling packet from {}: {}", packet.src, err);
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
            error!("unexpected packet from self, ignoring");
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
                    error!("overwriting existing PeerConn for node {} id {}", src, id);
                }
            },
            FullMeshPayload::Answer(id, answer) if self.id < src => {
                let conn = self.peers.get(&src).and_then(|conns| conns.get(id));
                let Some(conn) = conn else {
                    error!("no PeerConn found for node {} id {}, ignoring", src, id);
                    return Ok(());
                };
                conn.conn.answered(answer).await?;
                self.start_peer_loop(src, *id, &conn.conn);
            },
            FullMeshPayload::Candidate(id, candidate) => {
                let conn = self.peers.get(&src).and_then(|conns| conns.get(id));
                let Some(conn) = conn else {
                    error!("no PeerConn found for node {} id {}, ignoring", src, id);
                    return Ok(());
                };
                conn.conn.add_ice_candidate(IceCandidate::from_sdp(candidate)?).await?;
            },
            _ => {
                warn!("unexpected packet type from node {}, ignoring", src);
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
        self.peers.retain(|_, conns| !conns.is_empty());
        // check connected
        let peers = self.mesh.lock().await.get_routes().keys().cloned().collect::<Vec<_>>();
        for peer in peers {
            let conns = self.peers.entry(peer)
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
                    for id in sorted.iter() {
                        if conns.len() <= self.max_connected - 1 {
                            break;
                        }
                        if conns.get(id).unwrap().selected {
                            continue;
                        }
                        conns.remove(id);
                    }
                } else {
                    continue;
                }
            }
            if self.id > peer {
                continue;
            }
            let conn = PeerConn::new(self.config.clone()).await;
            let mut conn = match conn {
                Ok(conn) => conn,
                Err(err) => {
                    error!("error creating PeerConn to {}: {}", peer, err);
                    continue;
                },
            };
            let id = Uuid::new_v4();
            let offer = match conn.offer().await {
                Ok(offer) => offer,
                Err(err) => {
                    error!("error creating offer to {}: {}", peer, err);
                    continue;
                },
            };
            let result = self.mesh.lock().await
                .send_packet(peer, FullMeshPayload::Offer(id, offer)).await;
            if let Err(err) = result {
                error!("error sending offer to {}: {}", peer, err);
                continue;
            }
            conns.insert(id, create_peer(peer, id, conn, self.this.clone()));
        }
        // select
        for (_, conns) in &mut self.peers {
            if conns.is_empty() {
                // unreachable
                continue;
            }
            let old = conns.iter().find_map(|(id, peer)| if peer.selected {
                Some(*id)
            } else {
                None
            });
            let mut newest: Option<(&Uuid, &mut Conn)> = None;
            for conn in conns {
                conn.1.selected = false;
                if !conn.1.conn.connected() {
                    continue;
                }
                if let Some(ref n) = newest {
                    if conn.1.time > n.1.time {
                        newest = Some(conn);
                    }
                } else {
                    newest = Some(conn);
                }
            }
            match (old, newest) {
                (None, Some(select)) => {
                    select.1.selected = true;
                    self.refresh.notify_waiters();
                },
                (Some(_), None) => {
                    self.refresh.notify_waiters();
                },
                (Some(old), Some(select)) => {
                    select.1.selected = true;
                    if old != *select.0 {
                        self.refresh.notify_waiters();
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
                    Err(err) => {
                        warn!("candidate channel closed: {}", err);
                        break;
                    },
                };
                debug!("sending candidate to {}: {:?}", id, candidate);
                let result = mesh.lock().await.send_packet(
                    id,
                    FullMeshPayload::Candidate(uuid, candidate.to_sdp()),
                ).await;
                if let Err(err) = result {
                    warn!("error sending candidate to {}: {}", id, err);
                }
            }
        });
    }

    pub(crate) fn refresh_notified(&self) -> Notified<'_> {
        self.refresh.notified()
    }

    pub(crate) fn stop(&mut self) -> Result<()> {
        self.stop.send(())?;
        Ok(())
    }
}

fn create_peer(
    peer: NodeId,
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
                        .get_mut(&peer)
                        .and_then(|conns| conns.remove(&id));
                    if let Some(_) = conn {
                        fm.refresh.notify_waiters();
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
