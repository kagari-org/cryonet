use std::{
    collections::HashMap,
    fmt::Debug,
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use anyhow::{Error, Result};
use async_trait::async_trait;
use futures::future::join_all;
use packet::{NodeId, Packet, PacketPayload, Payload};
use sactor::sactor;
use tokio::{
    select,
    sync::{broadcast, mpsc, watch},
};
use tracing::{debug, error, warn};

use crate::{
    errors::CryonetError,
    time::{Instant, Interval, delayed_interval, jitter_up},
};

pub mod igp;
pub mod packet;
pub mod seq;

pub struct Mesh {
    handle: MeshHandle,

    id: NodeId,
    link_idle_timeout: Duration,

    pending: HashMap<(NodeId, u64), PendingPacket>,
    next_seq: u64,
    received: HashMap<NodeId, ReceivedWindow>,
    retry_ticker: Interval,

    link_send: HashMap<NodeId, Box<dyn LinkSend>>,
    link_recv_stop: HashMap<NodeId, watch::Sender<bool>>,
    link_last_recv: HashMap<NodeId, Instant>,
    routes: HashMap<NodeId, NodeId>,
    #[allow(clippy::type_complexity)]
    dispatchees: Vec<(
        Box<dyn Fn(&Packet) -> bool + Send + Sync>,
        mpsc::Sender<Packet>,
    )>,

    mesh_event_tx: broadcast::Sender<MeshEvent>,
}

macro_rules! define_link_traits {
    ($($supertrait:path),*) => {
        #[cfg_attr(not(target_arch = "wasm32"), async_trait)]
        #[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
        pub trait LinkSend: $($supertrait +)* {
            async fn send(&mut self, packet: Packet) -> Result<(), LinkError>;
        }

        #[cfg_attr(not(target_arch = "wasm32"), async_trait)]
        #[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
        pub trait LinkRecv: $($supertrait +)* {
            async fn recv(&mut self) -> Result<Packet, LinkError>;
        }
    };
}

#[cfg(not(target_arch = "wasm32"))]
define_link_traits!(Send);
#[cfg(target_arch = "wasm32")]
define_link_traits!();

#[derive(Debug, thiserror::Error)]
pub enum LinkError {
    #[error("Link closed")]
    Closed,
    #[error("Unknown error: {0}")]
    Unknown(#[from] anyhow::Error),
}

struct PendingPacket {
    payload: Box<dyn Payload>,
    attempts: u32,
    next_retry: Instant,
}

// bit 0 is highest, bit n is highest - n
struct ReceivedWindow {
    initialized: bool,
    highest: u64,
    bits: u64,
}

impl ReceivedWindow {
    fn new() -> Self {
        Self {
            initialized: false,
            highest: 0,
            bits: 0,
        }
    }

    fn accept(&mut self, seq: u64) -> bool {
        if !self.initialized {
            self.initialized = true;
            self.highest = seq;
            self.bits = 1;
            return true;
        }

        let ahead = seq.wrapping_sub(self.highest);
        if ahead == 0 {
            if self.bits & 1 != 0 {
                return false;
            }
            self.bits |= 1;
            return true;
        }
        if ahead < 1 << 63 {
            self.bits = if ahead >= 64 {
                1
            } else {
                (self.bits << ahead) | 1
            };
            self.highest = seq;
            return true;
        }

        let behind = self.highest.wrapping_sub(seq);
        if behind >= 64 {
            // too old to be tracked, accept it instead of silently dropping it
            return true;
        }
        let mask = 1 << behind;
        if self.bits & mask != 0 {
            return false;
        }
        self.bits |= mask;
        true
    }
}

const RELIABLE_RETRY_INTERVAL: Duration = Duration::from_secs(1);
const RELIABLE_RETRY_JITTER: f64 = 0.2;
const RELIABLE_RETRY_TICK: Duration = Duration::from_millis(250);
const RELIABLE_MAX_ATTEMPTS: u32 = 10;

// Peers keep a receive window per source; starting at zero makes a restarted
// process look like a duplicate of its previous run and its packets get dropped.
fn initial_seq() -> u64 {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos() as u64)
        .unwrap_or(0);
    nanos ^ COUNTER.fetch_add(1, Ordering::Relaxed).rotate_left(32)
}

#[allow(dead_code)]
#[derive(Debug, Clone, Copy)]
pub enum MeshEvent {
    LinkUp(NodeId),
    LinkDown(NodeId),
    RouteSet(NodeId, NodeId),
    RouteRemoved(NodeId, NodeId),
}

#[sactor(pub)]
impl Mesh {
    pub fn new(id: NodeId) -> MeshHandle {
        Self::new_with_parameters(id, Duration::from_secs(20))
    }

    pub fn new_with_parameters(id: NodeId, link_idle_timeout: Duration) -> MeshHandle {
        let (future, mesh) = Mesh::run(move |handle| Mesh {
            handle,
            id,
            link_idle_timeout,
            pending: HashMap::new(),
            next_seq: initial_seq(),
            received: HashMap::new(),
            retry_ticker: delayed_interval(RELIABLE_RETRY_TICK),
            link_send: HashMap::new(),
            link_recv_stop: HashMap::new(),
            link_last_recv: HashMap::new(),
            routes: HashMap::new(),
            dispatchees: Vec::new(),
            mesh_event_tx: broadcast::Sender::new(64),
        });
        tokio::task::spawn_local(future);
        mesh
    }

    #[no_reply]
    async fn handle_packet(
        &mut self,
        from: NodeId,
        packet: Result<Packet, LinkError>,
    ) -> Result<()> {
        if self.link_send.contains_key(&from) {
            self.link_last_recv.insert(from, Instant::now());
        }
        let mut packet = match packet {
            Ok(packet) => packet,
            Err(LinkError::Closed) => {
                self.remove_link(from);
                return Ok(());
            }
            res => res?,
        };
        debug!("Received packet: {packet:?}");
        if packet.dst == self.id {
            let Packet {
                src,
                dst,
                ttl,
                payload,
            } = packet;
            let payload = match payload {
                PacketPayload::Ack { seq } => {
                    debug!("Received ack {seq} from node {src:X}");
                    self.pending.remove(&(src, seq));
                    return Ok(());
                }
                PacketPayload::Payload(payload) => payload,
                PacketPayload::Reliable { seq, payload } => {
                    if let Err(err) = self.send_ack(from, src, seq).await {
                        debug!("Failed to acknowledge packet {seq} from node {src:X}: {err:?}");
                    }
                    let first = self
                        .received
                        .entry(src)
                        .or_insert_with(ReceivedWindow::new)
                        .accept(seq);
                    if !first {
                        debug!("Dropping duplicate reliable packet {seq} from node {src:X}");
                        return Ok(());
                    }
                    payload
                }
            };
            let packet = Packet {
                src,
                dst,
                ttl,
                payload: PacketPayload::Payload(payload),
            };
            for (filter, dispatch) in self.dispatchees.iter() {
                if filter(&packet) {
                    if let Err(err) = dispatch.try_send(packet) {
                        warn!("Failed to dispatch packet to handler: {err}");
                    }
                    break;
                }
            }
        } else {
            // forward
            if packet.ttl == 0 {
                debug!("Dropping packet to node {:X}: TTL expired", packet.dst);
                return Ok(());
            }
            packet.ttl -= 1;
            self.send_packet_internal(packet).await?;
        }
        Ok(())
    }

    async fn send_packet_internal(&mut self, packet: Packet) -> Result<()> {
        let Some(next_hop) = self.routes.get(&packet.dst) else {
            return Err(CryonetError::Unreachable(packet.dst).into());
        };
        let Some(link) = self.link_send.get_mut(next_hop) else {
            return Err(CryonetError::Unreachable(packet.dst).into());
        };
        debug!(
            "Sending packet {packet:?} to {:X} via link {next_hop:X}",
            packet.dst,
        );
        let res = link.send(packet).await;
        if let Err(LinkError::Closed) = res {
            self.remove_link(*next_hop);
        }
        res?;
        Ok(())
    }

    #[no_reply]
    pub async fn send_packet(&mut self, dst: NodeId, payload: Box<dyn Payload>) -> Result<()> {
        self.send_packet_internal(Packet {
            src: self.id,
            dst,
            ttl: 16,
            payload: PacketPayload::Payload(payload),
        })
        .await
    }

    #[no_reply]
    pub async fn send_packet_link(&mut self, dst: NodeId, payload: Box<dyn Payload>) -> Result<()> {
        let Some(link) = self.link_send.get_mut(&dst) else {
            return Err(CryonetError::NoSuchLink(dst).into());
        };
        let packet = Packet {
            src: self.id,
            dst,
            ttl: 16,
            payload: PacketPayload::Payload(payload),
        };
        debug!("Sending packet {packet:?} to {dst:X} via direct link");
        let res = link.send(packet).await;
        if let Err(LinkError::Closed) = res {
            self.remove_link(dst);
        }
        res?;
        Ok(())
    }

    #[no_reply]
    pub async fn send_packet_reliable(
        &mut self,
        dst: NodeId,
        payload: Box<dyn Payload>,
    ) -> Result<()> {
        let seq = self.next_seq;
        self.next_seq = self.next_seq.wrapping_add(1);
        self.pending.insert(
            (dst, seq),
            PendingPacket {
                payload: dyn_clone::clone_box(&*payload),
                attempts: 1,
                next_retry: Instant::now()
                    + jitter_up(RELIABLE_RETRY_INTERVAL, RELIABLE_RETRY_JITTER),
            },
        );
        if let Err(err) = self.send_reliable_packet(seq, dst, payload).await {
            // the destination may become routable later
            debug!("Reliable packet {seq} to {dst:X} is not routable yet: {err:?}");
        }
        Ok(())
    }

    async fn send_reliable_packet(
        &mut self,
        seq: u64,
        dst: NodeId,
        payload: Box<dyn Payload>,
    ) -> Result<()> {
        let packet = Packet {
            src: self.id,
            dst,
            ttl: 16,
            payload: PacketPayload::Reliable { seq, payload },
        };
        if let Some(link) = self.link_send.get_mut(&dst) {
            debug!("Sending reliable packet {seq} to {dst:X} via direct link");
            let res = link.send(packet).await;
            if let Err(LinkError::Closed) = res {
                self.remove_link(dst);
            }
            res?;
        } else {
            self.send_packet_internal(packet).await?;
        }
        Ok(())
    }

    async fn send_ack(&mut self, from: NodeId, dst: NodeId, seq: u64) -> Result<()> {
        let ack = Packet {
            src: self.id,
            dst,
            ttl: 16,
            payload: PacketPayload::Ack { seq },
        };
        // the reverse path is valid for multi-hop packets and works before a
        // route back to the source is installed
        if let Some(link) = self.link_send.get_mut(&from) {
            let res = link.send(ack).await;
            if let Err(LinkError::Closed) = res {
                self.remove_link(from);
            }
            res?;
        } else {
            self.send_packet_internal(ack).await?;
        }
        Ok(())
    }

    #[select]
    fn select(&mut self) -> Vec<Selection<'_>> {
        vec![selection!(self.retry_ticker.tick().await, retry)]
    }

    #[no_reply]
    async fn retry(&mut self) -> Result<()> {
        let now = Instant::now();
        let mut resend = Vec::new();
        self.pending.retain(|(dst, seq), pending| {
            if now < pending.next_retry {
                return true;
            }
            if pending.attempts >= RELIABLE_MAX_ATTEMPTS {
                warn!(
                    "Giving up reliable packet {seq} to node {dst:X} after {} attempts",
                    pending.attempts
                );
                return false;
            }
            pending.attempts += 1;
            pending.next_retry = now + jitter_up(RELIABLE_RETRY_INTERVAL, RELIABLE_RETRY_JITTER);
            resend.push((*seq, *dst, dyn_clone::clone_box(&*pending.payload)));
            true
        });
        for (seq, dst, payload) in resend {
            if let Err(err) = self.send_reliable_packet(seq, dst, payload).await {
                debug!("Reliable packet {seq} to {dst:X} is still unreachable: {err:?}");
            }
        }
        Ok(())
    }

    #[no_reply]
    pub async fn broadcast_packet_local(&mut self, payload: Box<dyn Payload>) -> Result<()> {
        debug!("Broadcasting packet {payload:?} to all links");
        let futures = self
            .link_send
            .iter_mut()
            .map(|(dst, link)| async {
                (
                    link.send(Packet {
                        src: self.id,
                        dst: *dst,
                        ttl: 16,
                        payload: PacketPayload::Payload(dyn_clone::clone_box(&*payload)),
                    })
                    .await,
                    *dst,
                )
            })
            .collect::<Vec<_>>();
        let results = join_all(futures).await;
        for (result, dst) in &results {
            if let Err(LinkError::Closed) = result {
                self.remove_link(*dst);
            }
        }
        results
            .into_iter()
            .map(|(res, _)| res)
            .collect::<Result<Vec<_>, _>>()
            .map(|_| ())?;
        Ok(())
    }

    pub fn add_dispatchee(
        &mut self,
        filter: Box<dyn Fn(&Packet) -> bool + Send + Sync>,
    ) -> mpsc::Receiver<Packet> {
        let (tx, rx) = mpsc::channel(512);
        self.dispatchees.push((filter, tx));
        rx
    }

    #[allow(dead_code)]
    pub fn remove_dispatchee(&mut self, mut rx: mpsc::Receiver<Packet>) {
        rx.close();
        self.dispatchees.retain(|(_, tx)| !tx.is_closed());
    }

    pub fn add_link(
        &mut self,
        dst: NodeId,
        send: Box<dyn LinkSend>,
        mut recv: Box<dyn LinkRecv>,
        is_initiator: bool,
    ) -> bool {
        if self.link_send.contains_key(&dst) {
            // ensure keeping the same link for both sides to avoid duplicate links,
            // unless the existing link is dead
            let keep_new = is_initiator == (self.id < dst) || !self.link_is_alive(dst);
            if !keep_new {
                debug!(
                    "Link to node {dst:X} already exists, keeping existing (is_initiator={is_initiator}, our id {:X}, dst {dst:X})",
                    self.id,
                );
                return false;
            }
            debug!(
                "Link to node {dst:X} already exists, replacing (is_initiator={is_initiator}, our id {:X}, dst {dst:X})",
                self.id,
            );
            self.remove_link(dst);
        }

        let mesh = self.handle.clone();
        self.link_send.insert(dst, send);
        self.link_last_recv.insert(dst, Instant::now());
        let (stop, mut stop_rx) = watch::channel(false);
        self.link_recv_stop.insert(dst, stop);

        tokio::task::spawn_local(async move {
            loop {
                select! {
                    _ = stop_rx.changed() => break,
                    packet = recv.recv() => {
                        let brk = matches!(&packet, Err(LinkError::Closed));
                        let _ = mesh.handle_packet(dst, packet).await;
                        if brk {
                            break;
                        }
                    }
                }
            }
        });

        let _ = self.mesh_event_tx.send(MeshEvent::LinkUp(dst));
        true
    }

    pub fn remove_link(&mut self, dst: NodeId) {
        debug!("Removing link to node {dst:X}");
        self.routes.retain(|_, &mut v| v != dst);
        self.received.remove(&dst);
        self.link_send.remove(&dst);
        self.link_last_recv.remove(&dst);
        let ctrl = self.link_recv_stop.remove(&dst);
        if let Some(ctrl) = ctrl {
            let _ = ctrl.send(true);
        }
        let _ = self.mesh_event_tx.send(MeshEvent::LinkDown(dst));
    }

    pub fn link_is_alive(&self, dst: NodeId) -> bool {
        self.link_last_recv
            .get(&dst)
            .is_some_and(|last| Instant::now().duration_since(*last) < self.link_idle_timeout)
    }

    // drop links that stopped receiving, a half-open one would keep rejecting
    // the peer's re-dials forever
    pub fn reap_stale_links(&mut self) {
        let stale: Vec<NodeId> = self
            .link_send
            .keys()
            .filter(|dst| !self.link_is_alive(**dst))
            .copied()
            .collect();
        for dst in stale {
            warn!("Link to node {dst:X} is silent, removing");
            self.remove_link(dst);
        }
    }

    pub fn get_links(&self) -> Vec<NodeId> {
        self.link_send.keys().cloned().collect()
    }

    pub fn set_route(&mut self, dst: NodeId, next_hop: NodeId) {
        if dst == self.id {
            // skip routes to self
            return;
        }
        if !self.link_send.contains_key(&next_hop) {
            warn!("Cannot set route to node {dst:X}: next hop {next_hop:X} does not exist");
            return;
        }
        debug!("Setting route: {dst:X} via {next_hop:X}");
        let previous = self.routes.insert(dst, next_hop);
        if previous != Some(next_hop) {
            let _ = self.mesh_event_tx.send(MeshEvent::RouteSet(dst, next_hop));
        }
    }

    pub fn remove_route(&mut self, dst: NodeId) {
        debug!("Removing route to node {dst:X}");
        let route = self.routes.remove(&dst);
        if let Some(next_hop) = route {
            let _ = self
                .mesh_event_tx
                .send(MeshEvent::RouteRemoved(dst, next_hop));
        }
    }

    pub fn get_routes(&self) -> HashMap<NodeId, NodeId> {
        self.routes.clone()
    }

    pub fn subscribe_mesh_events(&self) -> broadcast::Receiver<MeshEvent> {
        self.mesh_event_tx.subscribe()
    }

    #[handle_error]
    fn handle_error(&mut self, err: &Error) {
        error!("Error: {err:?}");
    }
}

impl Drop for Mesh {
    fn drop(&mut self) {
        for stop in self.link_recv_stop.values() {
            let _ = stop.send(true);
        }
    }
}
#[cfg(test)]
mod tests {
    use std::{
        any::Any,
        future::Future,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use async_trait::async_trait;
    use serde::{Deserialize, Serialize};
    use tokio::sync::mpsc;

    use super::*;
    use crate::mesh::igp::{Igp, Route};

    struct ChannelSend(mpsc::Sender<Packet>);
    struct ChannelRecv(mpsc::Receiver<Packet>);

    #[async_trait]
    impl LinkSend for ChannelSend {
        async fn send(&mut self, packet: Packet) -> Result<(), LinkError> {
            self.0.send(packet).await.map_err(|_| LinkError::Closed)
        }
    }

    #[async_trait]
    impl LinkRecv for ChannelRecv {
        async fn recv(&mut self) -> Result<Packet, LinkError> {
            self.0.recv().await.ok_or(LinkError::Closed)
        }
    }

    #[typetag::serde]
    impl Payload for MeshTestPayload {}

    #[derive(Debug, Clone, Serialize, Deserialize)]
    struct MeshTestPayload {
        value: u32,
    }

    fn is_test_payload(packet: &Packet) -> bool {
        packet
            .payload
            .as_payload()
            .is_some_and(|payload| (payload as &dyn Any).is::<MeshTestPayload>())
    }

    struct TestSend {
        tx: mpsc::Sender<Packet>,
        sent: Arc<AtomicUsize>,
        drop_budget: usize,
        matches: Arc<dyn Fn(&Packet) -> bool + Send + Sync>,
    }

    fn counted_send(tx: mpsc::Sender<Packet>) -> (TestSend, Arc<AtomicUsize>) {
        let sent = Arc::new(AtomicUsize::new(0));
        (
            TestSend {
                tx,
                sent: sent.clone(),
                drop_budget: 0,
                matches: Arc::new(|_| false),
            },
            sent,
        )
    }

    fn dropping_send(
        tx: mpsc::Sender<Packet>,
        drop_budget: usize,
        matches: Arc<dyn Fn(&Packet) -> bool + Send + Sync>,
    ) -> (TestSend, Arc<AtomicUsize>) {
        let sent = Arc::new(AtomicUsize::new(0));
        (
            TestSend {
                tx,
                sent: sent.clone(),
                drop_budget,
                matches,
            },
            sent,
        )
    }

    #[async_trait]
    impl LinkSend for TestSend {
        async fn send(&mut self, packet: Packet) -> Result<(), LinkError> {
            self.sent.fetch_add(1, Ordering::SeqCst);
            if self.drop_budget > 0 && (self.matches)(&packet) {
                self.drop_budget -= 1;
                return Ok(());
            }
            self.tx.send(packet).await.map_err(|_| LinkError::Closed)
        }
    }

    async fn connect(a: &MeshHandle, b: &MeshHandle, a_id: NodeId, b_id: NodeId) {
        let (a_tx, b_rx) = mpsc::channel(64);
        let (b_tx, a_rx) = mpsc::channel(64);
        assert!(
            a.add_link(
                b_id,
                Box::new(ChannelSend(a_tx)),
                Box::new(ChannelRecv(a_rx)),
                true
            )
            .await
            .unwrap()
        );
        assert!(
            b.add_link(
                a_id,
                Box::new(ChannelSend(b_tx)),
                Box::new(ChannelRecv(b_rx)),
                true
            )
            .await
            .unwrap()
        );
    }

    async fn wait_until<F, Fut>(mut f: F) -> bool
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = bool>,
    {
        for _ in 0..200 {
            if f().await {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        false
    }

    #[test]
    fn simulated_three_nodes_propagate_and_fail_over() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let local = tokio::task::LocalSet::new();
        local.block_on(&rt, async {
            let a = 1;
            let b = 2;
            let c = 3;
            let mesh_a = Mesh::new_with_parameters(a, Duration::from_secs(2));
            let mesh_b = Mesh::new_with_parameters(b, Duration::from_secs(2));
            let mesh_c = Mesh::new_with_parameters(c, Duration::from_secs(2));

            let igp = |id, mesh| {
                Igp::new_with_parameters(
                    Duration::from_millis(50),
                    Duration::from_millis(100),
                    Duration::from_millis(500),
                    Duration::from_secs(2),
                    Duration::from_secs(5),
                    Duration::from_millis(100),
                    64,
                    1000,
                    id,
                    mesh,
                )
            };
            let igp_a = igp(a, mesh_a.clone()).await.unwrap();
            let igp_b = igp(b, mesh_b.clone()).await.unwrap();
            let igp_c = igp(c, mesh_c.clone()).await.unwrap();

            connect(&mesh_a, &mesh_b, a, b).await;
            connect(&mesh_b, &mesh_c, b, c).await;

            assert!(
                wait_until(|| async {
                    let ar = igp_a.get_routes().await.unwrap();
                    let br = igp_b.get_routes().await.unwrap();
                    let cr = igp_c.get_routes().await.unwrap();
                    ar.iter().any(|r| r.dst == c && r.from == b && r.selected)
                        && br.iter().any(|r| r.dst == a && r.from == a && r.selected)
                        && br.iter().any(|r| r.dst == c && r.from == c && r.selected)
                        && cr.iter().any(|r| r.dst == a && r.from == b && r.selected)
                })
                .await,
                "initial IGP routes"
            );

            assert_eq!(mesh_a.get_routes().await.unwrap().get(&c), Some(&b));
            assert_eq!(mesh_b.get_routes().await.unwrap().get(&a), Some(&a));
            assert_eq!(mesh_b.get_routes().await.unwrap().get(&c), Some(&c));
            assert_eq!(mesh_c.get_routes().await.unwrap().get(&a), Some(&b));

            mesh_b.remove_link(c).await.unwrap();
            mesh_c.remove_link(b).await.unwrap();

            assert!(
                wait_until(|| async {
                    !igp_a
                        .get_routes()
                        .await
                        .unwrap()
                        .iter()
                        .any(|r| r.dst == c && r.selected)
                        && !igp_b
                            .get_routes()
                            .await
                            .unwrap()
                            .iter()
                            .any(|r| r.dst == c && r.selected)
                        && !igp_c
                            .get_routes()
                            .await
                            .unwrap()
                            .iter()
                            .any(|r| r.dst == a && r.selected)
                        && !mesh_a.get_routes().await.unwrap().contains_key(&c)
                        && !mesh_c.get_routes().await.unwrap().contains_key(&a)
                })
                .await,
                "routes removed after link failure"
            );

            connect(&mesh_b, &mesh_c, b, c).await;

            assert!(
                wait_until(|| async {
                    igp_a
                        .get_routes()
                        .await
                        .unwrap()
                        .iter()
                        .any(|r| r.dst == c && r.from == b && r.selected)
                        && igp_c
                            .get_routes()
                            .await
                            .unwrap()
                            .iter()
                            .any(|r| r.dst == a && r.from == b && r.selected)
                        && mesh_a.get_routes().await.unwrap().get(&c) == Some(&b)
                })
                .await,
                "routes re-learned after link recovery"
            );
        });
    }

    fn has_route(routes: &[Route], dst: NodeId, from: NodeId) -> bool {
        routes
            .iter()
            .any(|route| route.dst == dst && route.from == from && route.selected)
    }

    fn no_route(routes: &[Route], dst: NodeId) -> bool {
        !routes
            .iter()
            .any(|route| route.dst == dst && route.selected)
    }

    async fn disconnect(a: &MeshHandle, b: &MeshHandle, a_id: NodeId, b_id: NodeId) {
        a.remove_link(b_id).await.unwrap();
        b.remove_link(a_id).await.unwrap();
    }

    #[test]
    fn simulated_five_nodes_reachability_and_reconnect() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let local = tokio::task::LocalSet::new();
        local.block_on(&rt, async {
            let a = 1;
            let b = 2;
            let c = 3;
            let d = 4;
            let e = 5;
            let mesh_a = Mesh::new_with_parameters(a, Duration::from_secs(2));
            let mesh_b = Mesh::new_with_parameters(b, Duration::from_secs(2));
            let mesh_c = Mesh::new_with_parameters(c, Duration::from_secs(2));
            let mesh_d = Mesh::new_with_parameters(d, Duration::from_secs(2));
            let mesh_e = Mesh::new_with_parameters(e, Duration::from_secs(2));

            let igp = |id, mesh| {
                Igp::new_with_parameters(
                    Duration::from_millis(50),
                    Duration::from_millis(100),
                    Duration::from_millis(500),
                    Duration::from_secs(2),
                    Duration::from_secs(5),
                    Duration::from_millis(100),
                    64,
                    1000,
                    id,
                    mesh,
                )
            };
            let igp_a = igp(a, mesh_a.clone()).await.unwrap();
            let igp_b = igp(b, mesh_b.clone()).await.unwrap();
            let igp_c = igp(c, mesh_c.clone()).await.unwrap();
            let igp_d = igp(d, mesh_d.clone()).await.unwrap();
            let igp_e = igp(e, mesh_e.clone()).await.unwrap();

            connect(&mesh_a, &mesh_b, a, b).await;
            connect(&mesh_b, &mesh_c, b, c).await;
            connect(&mesh_c, &mesh_d, c, d).await;
            connect(&mesh_d, &mesh_e, d, e).await;

            assert!(
                wait_until(|| async {
                    let ar = igp_a.get_routes().await.unwrap();
                    let br = igp_b.get_routes().await.unwrap();
                    let cr = igp_c.get_routes().await.unwrap();
                    let dr = igp_d.get_routes().await.unwrap();
                    let er = igp_e.get_routes().await.unwrap();
                    has_route(&ar, b, b)
                        && has_route(&ar, c, b)
                        && has_route(&ar, d, b)
                        && has_route(&ar, e, b)
                        && has_route(&br, a, a)
                        && has_route(&br, c, c)
                        && has_route(&br, d, c)
                        && has_route(&br, e, c)
                        && has_route(&cr, a, b)
                        && has_route(&cr, b, b)
                        && has_route(&cr, d, d)
                        && has_route(&cr, e, d)
                        && has_route(&dr, a, c)
                        && has_route(&dr, b, c)
                        && has_route(&dr, c, c)
                        && has_route(&dr, e, e)
                        && has_route(&er, a, d)
                        && has_route(&er, b, d)
                        && has_route(&er, c, d)
                        && has_route(&er, d, d)
                })
                .await,
                "initial five-node reachability"
            );
            assert_eq!(mesh_a.get_routes().await.unwrap().get(&e), Some(&b));
            assert_eq!(mesh_e.get_routes().await.unwrap().get(&a), Some(&d));

            disconnect(&mesh_c, &mesh_d, c, d).await;

            assert!(
                wait_until(|| async {
                    let ar = igp_a.get_routes().await.unwrap();
                    let br = igp_b.get_routes().await.unwrap();
                    let cr = igp_c.get_routes().await.unwrap();
                    let dr = igp_d.get_routes().await.unwrap();
                    let er = igp_e.get_routes().await.unwrap();
                    has_route(&ar, b, b)
                        && has_route(&ar, c, b)
                        && no_route(&ar, d)
                        && no_route(&ar, e)
                        && has_route(&br, a, a)
                        && has_route(&br, c, c)
                        && no_route(&br, d)
                        && no_route(&br, e)
                        && has_route(&cr, a, b)
                        && has_route(&cr, b, b)
                        && no_route(&cr, d)
                        && no_route(&cr, e)
                        && no_route(&dr, a)
                        && no_route(&dr, b)
                        && no_route(&dr, c)
                        && has_route(&dr, e, e)
                        && no_route(&er, a)
                        && no_route(&er, b)
                        && no_route(&er, c)
                        && has_route(&er, d, d)
                })
                .await,
                "C-D split reachability"
            );
            assert!(!mesh_a.get_routes().await.unwrap().contains_key(&e));
            assert!(!mesh_e.get_routes().await.unwrap().contains_key(&a));

            connect(&mesh_c, &mesh_d, c, d).await;

            assert!(
                wait_until(|| async {
                    has_route(&igp_a.get_routes().await.unwrap(), e, b)
                        && has_route(&igp_c.get_routes().await.unwrap(), e, d)
                        && has_route(&igp_e.get_routes().await.unwrap(), a, d)
                        && mesh_a.get_routes().await.unwrap().get(&e) == Some(&b)
                })
                .await,
                "C-D reconnect reachability"
            );

            disconnect(&mesh_a, &mesh_b, a, b).await;

            let isolated = wait_until(|| async {
                let ar = igp_a.get_routes().await.unwrap();
                let br = igp_b.get_routes().await.unwrap();
                let cr = igp_c.get_routes().await.unwrap();
                let dr = igp_d.get_routes().await.unwrap();
                let er = igp_e.get_routes().await.unwrap();
                no_route(&ar, b)
                    && no_route(&ar, c)
                    && no_route(&ar, d)
                    && no_route(&ar, e)
                    && no_route(&br, a)
                    && no_route(&cr, a)
                    && no_route(&dr, a)
                    && no_route(&er, a)
                    && has_route(&br, c, c)
                    && has_route(&cr, d, d)
                    && has_route(&dr, e, e)
            })
            .await;
            assert!(isolated, "A isolated");
            assert!(!mesh_a.get_routes().await.unwrap().contains_key(&b));
            assert!(!mesh_b.get_routes().await.unwrap().contains_key(&a));

            connect(&mesh_a, &mesh_b, a, b).await;

            assert!(
                wait_until(|| async {
                    has_route(&igp_a.get_routes().await.unwrap(), e, b)
                        && has_route(&igp_e.get_routes().await.unwrap(), a, d)
                        && mesh_a.get_routes().await.unwrap().get(&e) == Some(&b)
                        && mesh_e.get_routes().await.unwrap().get(&a) == Some(&d)
                })
                .await,
                "A-B reconnect reachability"
            );
        });
    }

    #[test]
    fn reliable_packet_is_delivered_and_acked() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let local = tokio::task::LocalSet::new();
        local.block_on(&rt, async {
            let a = 1;
            let b = 2;
            let mesh_a = Mesh::new(a);
            let mesh_b = Mesh::new(b);
            let mut packets = mesh_b
                .add_dispatchee(Box::new(is_test_payload))
                .await
                .unwrap();

            let (a_tx, b_rx) = mpsc::channel(64);
            let (b_tx, a_rx) = mpsc::channel(64);
            let (a_send, a_sent) = counted_send(a_tx);
            let (b_send, b_sent) = counted_send(b_tx);
            assert!(
                mesh_a
                    .add_link(b, Box::new(a_send), Box::new(ChannelRecv(a_rx)), true)
                    .await
                    .unwrap()
            );
            assert!(
                mesh_b
                    .add_link(a, Box::new(b_send), Box::new(ChannelRecv(b_rx)), true)
                    .await
                    .unwrap()
            );

            mesh_a
                .send_packet_reliable(b, Box::new(MeshTestPayload { value: 7 }))
                .await
                .unwrap();
            let packet = tokio::time::timeout(Duration::from_secs(2), packets.recv())
                .await
                .unwrap()
                .unwrap();
            let payload = (packet.payload.as_payload().unwrap() as &dyn Any)
                .downcast_ref::<MeshTestPayload>()
                .unwrap();
            assert_eq!(payload.value, 7);

            tokio::time::sleep(Duration::from_millis(1500)).await;
            assert_eq!(a_sent.load(Ordering::SeqCst), 1);
            assert_eq!(b_sent.load(Ordering::SeqCst), 1);
        });
    }

    #[test]
    fn reliable_packet_retries_until_acknowledged() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let local = tokio::task::LocalSet::new();
        local.block_on(&rt, async {
            let a = 1;
            let b = 2;
            let mesh_a = Mesh::new(a);
            let mesh_b = Mesh::new(b);
            let mut packets = mesh_b
                .add_dispatchee(Box::new(is_test_payload))
                .await
                .unwrap();

            let (a_tx, b_rx) = mpsc::channel(64);
            let (b_tx, a_rx) = mpsc::channel(64);
            let (a_send, a_sent) = dropping_send(
                a_tx,
                1,
                Arc::new(|packet: &Packet| {
                    matches!(&packet.payload, PacketPayload::Reliable { .. })
                }),
            );
            let (b_send, _) = counted_send(b_tx);
            assert!(
                mesh_a
                    .add_link(b, Box::new(a_send), Box::new(ChannelRecv(a_rx)), true)
                    .await
                    .unwrap()
            );
            assert!(
                mesh_b
                    .add_link(a, Box::new(b_send), Box::new(ChannelRecv(b_rx)), true)
                    .await
                    .unwrap()
            );

            mesh_a
                .send_packet_reliable(b, Box::new(MeshTestPayload { value: 8 }))
                .await
                .unwrap();
            let packet = tokio::time::timeout(Duration::from_secs(3), packets.recv())
                .await
                .unwrap()
                .unwrap();
            let payload = (packet.payload.as_payload().unwrap() as &dyn Any)
                .downcast_ref::<MeshTestPayload>()
                .unwrap();
            assert_eq!(payload.value, 8);
            assert_eq!(a_sent.load(Ordering::SeqCst), 2);

            tokio::time::sleep(Duration::from_millis(1500)).await;
            assert_eq!(a_sent.load(Ordering::SeqCst), 2);
        });
    }

    #[test]
    fn lost_ack_retransmits_without_duplicate_delivery() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let local = tokio::task::LocalSet::new();
        local.block_on(&rt, async {
            let a = 1;
            let b = 2;
            let mesh_a = Mesh::new(a);
            let mesh_b = Mesh::new(b);
            let mut packets = mesh_b
                .add_dispatchee(Box::new(is_test_payload))
                .await
                .unwrap();

            let (a_tx, b_rx) = mpsc::channel(64);
            let (b_tx, a_rx) = mpsc::channel(64);
            let (a_send, a_sent) = counted_send(a_tx);
            let (b_send, b_sent) = dropping_send(
                b_tx,
                1,
                Arc::new(|packet: &Packet| matches!(&packet.payload, PacketPayload::Ack { .. })),
            );
            assert!(
                mesh_a
                    .add_link(b, Box::new(a_send), Box::new(ChannelRecv(a_rx)), true)
                    .await
                    .unwrap()
            );
            assert!(
                mesh_b
                    .add_link(a, Box::new(b_send), Box::new(ChannelRecv(b_rx)), true)
                    .await
                    .unwrap()
            );

            mesh_a
                .send_packet_reliable(b, Box::new(MeshTestPayload { value: 9 }))
                .await
                .unwrap();
            let packet = tokio::time::timeout(Duration::from_secs(2), packets.recv())
                .await
                .unwrap()
                .unwrap();
            let payload = (packet.payload.as_payload().unwrap() as &dyn Any)
                .downcast_ref::<MeshTestPayload>()
                .unwrap();
            assert_eq!(payload.value, 9);

            assert!(
                wait_until(|| async { a_sent.load(Ordering::SeqCst) == 2 }).await,
                "retransmit after the lost ack"
            );
            tokio::time::sleep(Duration::from_millis(1500)).await;
            assert_eq!(a_sent.load(Ordering::SeqCst), 2);
            assert_eq!(b_sent.load(Ordering::SeqCst), 2);
            assert!(packets.try_recv().is_err(), "duplicate was delivered");
        });
    }

    #[test]
    fn reliable_ack_routes_back_across_multiple_hops() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let local = tokio::task::LocalSet::new();
        local.block_on(&rt, async {
            let a = 1;
            let b = 2;
            let c = 3;
            let mesh_a = Mesh::new(a);
            let mesh_b = Mesh::new(b);
            let mesh_c = Mesh::new(c);
            let mut packets = mesh_c
                .add_dispatchee(Box::new(is_test_payload))
                .await
                .unwrap();

            let (a_tx, b_rx) = mpsc::channel(64);
            let (b_tx, a_rx) = mpsc::channel(64);
            let (b2_tx, c_rx) = mpsc::channel(64);
            let (c_tx, b2_rx) = mpsc::channel(64);
            let (a_send, a_sent) = counted_send(a_tx);
            let (b_send, _) = counted_send(b_tx);
            let (b2_send, _) = counted_send(b2_tx);
            let (c_send, _) = counted_send(c_tx);
            assert!(
                mesh_a
                    .add_link(b, Box::new(a_send), Box::new(ChannelRecv(a_rx)), true)
                    .await
                    .unwrap()
            );
            assert!(
                mesh_b
                    .add_link(a, Box::new(b_send), Box::new(ChannelRecv(b_rx)), true)
                    .await
                    .unwrap()
            );
            assert!(
                mesh_b
                    .add_link(c, Box::new(b2_send), Box::new(ChannelRecv(b2_rx)), true)
                    .await
                    .unwrap()
            );
            assert!(
                mesh_c
                    .add_link(b, Box::new(c_send), Box::new(ChannelRecv(c_rx)), true)
                    .await
                    .unwrap()
            );

            mesh_a.set_route(c, b).await.unwrap();
            mesh_b.set_route(a, a).await.unwrap();
            mesh_b.set_route(c, c).await.unwrap();
            mesh_c.set_route(a, b).await.unwrap();

            mesh_a
                .send_packet_reliable(c, Box::new(MeshTestPayload { value: 10 }))
                .await
                .unwrap();
            let packet = tokio::time::timeout(Duration::from_secs(2), packets.recv())
                .await
                .unwrap()
                .unwrap();
            let payload = (packet.payload.as_payload().unwrap() as &dyn Any)
                .downcast_ref::<MeshTestPayload>()
                .unwrap();
            assert_eq!(payload.value, 10);

            tokio::time::sleep(Duration::from_millis(1500)).await;
            assert_eq!(a_sent.load(Ordering::SeqCst), 1);
        });
    }
}
