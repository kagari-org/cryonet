// inspired by RFC 8966

use std::{
    any::Any,
    collections::{HashMap, hash_map::Entry},
    time::Duration,
};

use anyhow::{Error, Result};
use cryonet_uapi::{IgpNeighbor, IgpRequest, IgpRoute, IgpSource, IgpStats, IgpStatus};
use sactor::sactor;
use serde::{Deserialize, Serialize};
use tokio::sync::{
    broadcast::{self, error::RecvError},
    mpsc,
};
use tracing::{debug, error, warn};

use crate::mesh::{
    MeshEvent, MeshHandle,
    packet::{NodeId, Packet, Payload},
    seq::{Seq, SeqMetric},
};
use crate::time::{Instant, Interval, interval, jitter_down};

pub struct Igp {
    handle: IgpHandle,

    id: NodeId,
    mesh: MeshHandle,

    costs: HashMap<NodeId, Cost>,
    // feasibility distance: the best distance (higher seqno, lower metric) we have sent
    sources: HashMap<NodeId, (SeqMetric, Instant)>,
    requests: HashMap<NodeId, RouteRequest>,
    routes: HashMap<(NodeId, NodeId), Route>, // (dst, neigh) -> Route

    hello_ticker: Interval,
    dump_ticker: Interval,
    gc_ticker: Interval,
    hello_interval: Duration,
    dump_interval: Duration,
    gc_interval: Duration,
    route_timeout: Duration,
    source_timeout: Duration,
    seqno_request_timeout: Duration,
    diameter: u16,
    update_threshold: u32,
    dump_batch_size: usize,
    stats: IgpCounters,

    packet_rx: mpsc::Receiver<Packet>,
    mesh_event_rx: broadcast::Receiver<MeshEvent>,
}

#[typetag::serde]
impl Payload for IgpPayload {}
#[derive(Debug, Clone, Serialize, Deserialize)]
enum IgpPayload {
    Hello { seq: Seq },
    HelloReply { seq: Seq },
    RouteRequest { dst: NodeId }, // only used when route is about to expire
    RouteDump,
    SequenceRequest { seq: Seq, dst: NodeId, ttl: u16 },
    Update { metric: SeqMetric, dst: NodeId },
    Updates(Vec<(SeqMetric, NodeId)>),
}

struct Cost {
    seq: Seq,
    start: Instant,
    cost: u32,
}

struct RouteRequest {
    seq: Seq,
    expiry: Instant,
    retries: u8,
}

#[derive(Default)]
struct IgpCounters {
    hellos_sent: u64,
    hellos_received: u64,
    updates_sent: u64,
    updates_received: u64,
    route_requests: u64,
    seqno_requests: u64,
}

// RFC 8966 3.2.7: retry pending seqno requests a small number of times
const MAX_SEQNO_REQUEST_RETRIES: u8 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Route {
    pub metric: SeqMetric,
    pub computed_metric: u32,
    pub dst: NodeId,
    pub from: NodeId,
    pub timeout: Instant,
    pub selected: bool,
}

#[sactor(pub)]
impl Igp {
    pub async fn new(id: NodeId, mesh: MeshHandle) -> Result<IgpHandle> {
        // RFC 8966 3.4.1 allows shortening the Hello interval at any time but
        // increasing it only right before sending a Hello, and 3.7.1 requires
        // each route to be advertised at least once per Update interval.
        // Shortening keeps both upper bounds valid while still desynchronising
        // independently started nodes.
        Self::new_with_parameters(
            jitter_down(Duration::from_secs(4), 0.2),
            jitter_down(Duration::from_secs(16), 0.2),
            Duration::from_secs(60),
            Duration::from_secs(56),
            Duration::from_secs(3 * 60),
            Duration::from_secs(8),
            64,
            1000,
            64,
            id,
            mesh,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn new_with_parameters(
        hello_interval: Duration,
        dump_interval: Duration,
        gc_interval: Duration,
        route_timeout: Duration,
        source_timeout: Duration,
        seqno_request_timeout: Duration,
        diameter: u16,
        update_threshold: u32,
        dump_batch_size: usize,
        id: NodeId,
        mesh: MeshHandle,
    ) -> Result<IgpHandle> {
        let packet_rx = mesh
            .add_dispatchee(Box::new(|packet| {
                packet
                    .payload
                    .as_payload()
                    .is_some_and(|payload| (payload as &dyn Any).is::<IgpPayload>())
            }))
            .await?;
        let mesh_event_rx = mesh.subscribe_mesh_events().await?;
        let now = Instant::now();
        let self_metric = SeqMetric {
            seq: Seq(0),
            metric: 0,
        };
        let mut routes = HashMap::new();
        routes.insert(
            (id, id),
            Route {
                metric: self_metric,
                computed_metric: 0,
                dst: id,
                from: id,
                timeout: now + route_timeout,
                selected: true,
            },
        );
        let mut sources = HashMap::new();
        sources.insert(id, (self_metric, now + source_timeout));
        let (future, igp) = Igp::run(move |handle| Igp {
            handle,
            id,
            mesh: mesh.clone(),
            costs: HashMap::new(),
            sources,
            requests: HashMap::new(),
            routes,
            hello_ticker: interval(hello_interval),
            dump_ticker: interval(dump_interval),
            gc_ticker: interval(gc_interval),
            hello_interval,
            dump_interval,
            gc_interval,
            route_timeout,
            source_timeout,
            seqno_request_timeout,
            diameter,
            update_threshold,
            dump_batch_size: dump_batch_size.max(1),
            stats: IgpCounters::default(),
            packet_rx,
            mesh_event_rx,
        });
        tokio::task::spawn_local(future);
        Ok(igp)
    }

    #[select]
    fn select(&mut self) -> Vec<Selection<'_>> {
        vec![
            selection!(self.hello_ticker.tick().await, hello),
            selection!(self.dump_ticker.tick().await, dump, _ => None),
            selection!(self.gc_ticker.tick().await, gc),
            selection!(self.packet_rx.recv().await, handle_packet, it => it),
            selection!(self.mesh_event_rx.recv().await, handle_mesh_event, it => it),
        ]
    }

    #[skip]
    async fn hello_link(&mut self, link: NodeId) -> Result<()> {
        let cost = self
            .costs
            .entry(link)
            .and_modify(|cost| {
                cost.seq += Seq(1);
                cost.start = Instant::now();
            })
            .or_insert(Cost {
                seq: Seq(0),
                start: Instant::now(),
                cost: u32::MAX,
            });
        self.mesh
            .send_packet_link(link, Box::new(IgpPayload::Hello { seq: cost.seq }))
            .await?;
        self.stats.hellos_sent += 1;
        Ok(())
    }

    #[no_reply]
    async fn hello(&mut self) -> Result<()> {
        for link in self.mesh.get_link_ids().await? {
            self.hello_link(link).await?;
        }
        Ok(())
    }

    #[no_reply]
    async fn dump(&mut self, neigh: Option<NodeId>) -> Result<()> {
        // add self route
        self.routes
            .entry((self.id, self.id))
            .and_modify(|route| route.timeout = Instant::now() + self.route_timeout)
            .or_insert_with(|| Route {
                metric: SeqMetric {
                    seq: Seq(0),
                    metric: 0,
                },
                computed_metric: 0,
                dst: self.id,
                from: self.id,
                timeout: Instant::now() + self.route_timeout,
                selected: true,
            });
        self.sources
            .entry(self.id)
            .and_modify(|source| source.1 = Instant::now() + self.source_timeout)
            .or_insert_with(|| {
                (
                    SeqMetric {
                        seq: Seq(0),
                        metric: 0,
                    },
                    Instant::now() + self.source_timeout,
                )
            });

        self.select_route().await?;

        let mut updates: Vec<(SeqMetric, NodeId)> = Vec::with_capacity(self.routes.len());
        for ((dst, _), route) in &self.routes {
            if !route.selected {
                continue;
            }
            // update source timeout
            let distance = route_distance(route);
            match self.sources.entry(*dst) {
                Entry::Occupied(mut occupied_entry) => {
                    let source = occupied_entry.get_mut();
                    if distance.better(&source.0) {
                        source.0 = distance;
                    }
                    source.1 = Instant::now() + self.source_timeout;
                }
                Entry::Vacant(vacant_entry) => {
                    vacant_entry.insert((distance, Instant::now() + self.source_timeout));
                    warn!("Missing source for node {dst:X} during RouteDump, inserting");
                }
            }
            updates.push((distance, route.dst));
        }

        for chunk in updates.chunks(self.dump_batch_size) {
            let payload: Box<dyn Payload> = Box::new(IgpPayload::Updates(chunk.to_vec()));
            match neigh {
                Some(neigh) => self.mesh.send_packet_link(neigh, payload).await?,
                None => self.mesh.broadcast_packet_local(payload).await?,
            }
        }
        self.stats.updates_sent += updates.len() as u64;

        Ok(())
    }

    #[no_reply]
    async fn gc(&mut self) -> Result<()> {
        let now = Instant::now();
        self.sources.retain(|_, (_, expiry)| *expiry > now);
        self.requests.retain(|_, request| request.expiry > now);
        self.routes
            .retain(|_, route| !(route.timeout <= now && route.metric.metric == u32::MAX));

        for ((dst, neigh), route) in &mut self.routes {
            if route.timeout <= now {
                route.metric.metric = u32::MAX;
                route.computed_metric = u32::MAX;
                route.timeout = now + self.route_timeout;
                route.selected = false;
                self.mesh
                    .send_packet_link(*neigh, Box::new(IgpPayload::RouteRequest { dst: *dst }))
                    .await?;
            }
        }
        self.select_route().await?;

        Ok(())
    }

    #[no_reply]
    async fn handle_packet(&mut self, packet: Option<Packet>) -> Result<()> {
        let packet = match packet {
            Some(packet) => packet,
            None => {
                self.handle.stop();
                return Ok(());
            }
        };
        let src = packet.src;
        let payload = packet
            .payload
            .as_payload()
            .and_then(|payload| (payload as &dyn Any).downcast_ref::<IgpPayload>())
            .unwrap();
        match payload {
            IgpPayload::Hello { seq } => {
                self.stats.hellos_received += 1;
                self.mesh
                    .send_packet_link(src, Box::new(IgpPayload::HelloReply { seq: *seq }))
                    .await?;
            }

            IgpPayload::HelloReply { seq } => {
                let time = Instant::now();
                let Entry::Occupied(mut cost) = self.costs.entry(src) else {
                    warn!("Received unexpected HelloReply from node {src:X}");
                    return Ok(());
                };
                let cost = cost.get_mut();
                if cost.seq != *seq {
                    debug!("Ignoring HelloReply with mismatched seq {seq:?} from node {src:X}");
                    return Ok(());
                }
                let rtt = time.duration_since(cost.start).as_millis() + 100;
                let origin = cost.cost;
                if cost.cost == u32::MAX {
                    cost.cost = rtt as u32;
                    // first hello reply received, request route dump
                    self.mesh
                        .send_packet_link(src, Box::new(IgpPayload::RouteDump))
                        .await?;
                } else {
                    cost.cost = ((836 * cost.cost as u128 + 164 * rtt) / 1000) as u32;
                }
                if cost.cost != origin {
                    let cost = cost.cost;
                    self.update_route_costs(src, cost).await?;
                }
            }

            IgpPayload::RouteRequest { dst } => {
                self.stats.route_requests += 1;
                let route = self
                    .routes
                    .iter()
                    .find(|((d, _), r)| d == dst && r.selected);
                match route {
                    Some((_, route)) => {
                        self.stats.updates_sent += 1;
                        self.mesh
                            .broadcast_packet_local(generate_update(route))
                            .await?;
                    }
                    None => {
                        self.stats.updates_sent += 1;
                        self.mesh
                            .broadcast_packet_local(generate_retraction(*dst))
                            .await?;
                    }
                }
            }

            IgpPayload::RouteDump => self.dump(Some(src)).await?,

            IgpPayload::SequenceRequest { seq, dst, ttl } => {
                let route = find_route_for_request(&mut self.routes, *dst, self.id);
                let route = match route {
                    Some(route) => {
                        if route.metric.metric == u32::MAX {
                            debug!(
                                "Route to node {dst:X} is unreachable, dropping SequenceRequest"
                            );
                            return Ok(());
                        }
                        route
                    }
                    None => {
                        debug!("No route to node {dst:X}, dropping SequenceRequest");
                        return Ok(());
                    }
                };
                if *seq <= route.metric.seq {
                    self.stats.updates_sent += 1;
                    self.mesh
                        .broadcast_packet_local(generate_update(route))
                        .await?;
                    return Ok(());
                }
                if *dst == self.id {
                    // RFC 8966 3.2.2 says the source increments its seqno on every
                    // request, while 4.6.11 only requires the response to be no older
                    // than the requested seqno. After a restart our self seqno is back
                    // at 0 while neighbours still hold the previous value in their
                    // feasibility distance. Incrementing by one would need one request
                    // per missing seqno and usually falls back to the source GC timer;
                    // jump straight to the requested value instead. The check above
                    // guarantees the request is ahead, so this never moves the seqno
                    // backwards. This is a deliberate local extension to the literal
                    // +1 behaviour, not a spontaneous increment.
                    route.metric.seq = self_seq_after_request(route.metric.seq, *seq);

                    self.stats.updates_sent += 1;
                    self.mesh
                        .broadcast_packet_local(generate_update(route))
                        .await?;
                    return Ok(());
                }
                // forward the request
                if *ttl == 0 {
                    debug!("SequenceRequest to node {dst:X} expired: TTL=0");
                    return Ok(());
                }
                let source = self.sources.get(dst);
                let existing_request = self.requests.get(dst);
                if let Some(existing_request) = existing_request
                    && *seq <= existing_request.seq
                    && Instant::now() < existing_request.expiry
                {
                    debug!(
                        "Suppressing duplicate SequenceRequest for node {dst:X} with seq {seq:?}"
                    );
                    return Ok(());
                }
                self.requests.insert(
                    *dst,
                    RouteRequest {
                        seq: *seq,
                        expiry: Instant::now() + self.seqno_request_timeout,
                        retries: 0,
                    },
                );
                let mut route_feasible: Option<&Route> = None;
                let mut route_any: Option<&Route> = None;
                for ((_, neigh), route) in &self.routes {
                    if *neigh == src || *dst != route.dst {
                        continue;
                    }
                    if let Some(source) = source
                        && route.metric.feasible(&source.0)
                    {
                        match route_feasible {
                            None => route_feasible = Some(route),
                            Some(rf) => {
                                if route.computed_metric < rf.computed_metric {
                                    route_feasible = Some(route);
                                }
                            }
                        }
                    }
                    match route_any {
                        None => route_any = Some(route),
                        Some(ra) => {
                            if route.computed_metric < ra.computed_metric {
                                route_any = Some(route);
                            }
                        }
                    }
                }
                // ttl decrement
                let mut payload = Box::new(payload.clone());
                match payload.as_mut() {
                    IgpPayload::SequenceRequest { ttl, .. } => *ttl = ttl.saturating_sub(1),
                    _ => unreachable!(),
                }
                // send
                if let Some(route) = route_feasible {
                    self.mesh.send_packet_link(route.from, payload).await?;
                    return Ok(());
                }
                if let Some(route) = route_any {
                    self.mesh.send_packet_link(route.from, payload).await?;
                    return Ok(());
                }
                debug!("No alternative route to node {dst:X}, dropping SequenceRequest");
            }

            IgpPayload::Update { metric, dst } => {
                self.apply_update(src, *metric, *dst).await?;
            }

            IgpPayload::Updates(updates) => {
                for (metric, dst) in updates {
                    self.apply_update(src, *metric, *dst).await?;
                }
            }
        }
        Ok(())
    }

    #[skip]
    async fn apply_update(&mut self, src: NodeId, metric: SeqMetric, dst: NodeId) -> Result<()> {
        self.stats.updates_received += 1;
        if dst == self.id {
            // ignore updates about ourself
            return Ok(());
        }
        let computed_metric = match (self.costs.get(&src), metric.metric) {
            (_, u32::MAX) => u32::MAX,
            (None, _) => u32::MAX - 1,
            (Some(cost), metric) => metric.saturating_add(cost.cost),
        };

        let source = self.sources.get(&dst);
        let entry = self.routes.entry((dst, src));
        match entry {
            Entry::Vacant(entry) => {
                if metric.metric == u32::MAX {
                    // ignore
                    return Ok(());
                }
                if let Some(source) = source
                    && !metric.feasible(&source.0)
                {
                    // ignore
                    return Ok(());
                }
                entry.insert(Route {
                    metric,
                    computed_metric,
                    dst,
                    from: src,
                    timeout: Instant::now() + self.route_timeout,
                    selected: false,
                });
            }
            Entry::Occupied(mut entry) => {
                let route = entry.get_mut();
                // no FD left (e.g. source GC): accept and let select_route recreate it
                if let Some(source) = source
                    && route.selected
                    && !metric.feasible(&source.0)
                {
                    // ignore
                    return Ok(());
                }
                // ignore seq of retraction update
                if metric.metric == u32::MAX {
                    route.metric.metric = u32::MAX;
                } else {
                    route.metric = metric;
                }
                route.computed_metric = computed_metric;
                if metric.metric != u32::MAX {
                    route.timeout = Instant::now() + self.route_timeout;
                }
                if let Some(source) = source
                    && !route.metric.feasible(&source.0)
                {
                    route.selected = false;
                }
            }
        }

        // check seqno requests
        let route = &self.routes[&(dst, src)];
        if let Some(seqno_request) = self.requests.get(&dst)
            && metric.seq >= seqno_request.seq
        {
            self.requests.remove(&dst);
            // trigger update
            self.stats.updates_sent += 1;
            self.mesh
                .broadcast_packet_local(generate_update(route))
                .await?;
        }

        self.select_route().await?;
        Ok(())
    }

    #[no_reply]
    async fn handle_mesh_event(
        &mut self,
        event: Result<MeshEvent, broadcast::error::RecvError>,
    ) -> Result<()> {
        let event = match event {
            Ok(event) => event,
            Err(RecvError::Lagged(_)) => return Ok(()),
            Err(RecvError::Closed) => {
                self.handle.stop();
                return Ok(());
            }
        };
        match event {
            MeshEvent::LinkUp(node_id) => {
                self.hello_link(node_id).await?;
                // A restarted peer keeps its old source FD here but has lost its
                // self seqno. Probe it immediately instead of waiting for the
                // selected route to expire; the peer jumps to the requested value.
                if let Some(source) = self.sources.get(&node_id).copied() {
                    self.send_seqno_request(node_id, source.0.seq + Seq(1), true)
                        .await?;
                }
                // re-export routes after unexpected link disconnection
                let _ = self.export().await;
            }
            MeshEvent::LinkDown(node_id) => {
                if let Some(cost) = self.costs.get_mut(&node_id) {
                    cost.cost = u32::MAX;
                }
                self.update_route_costs(node_id, u32::MAX).await?;
            }
            _ => {}
        };
        Ok(())
    }

    async fn select_route(&mut self) -> Result<()> {
        let now = Instant::now();
        let mut routes: HashMap<NodeId, Vec<(&mut Route, &NodeId)>> = HashMap::new();
        let mut previous = HashMap::new();
        for ((dst, neigh), route) in &mut self.routes {
            if route.selected {
                previous.insert(
                    *dst,
                    SeqMetric {
                        seq: route.metric.seq,
                        metric: route.computed_metric,
                    },
                );
            }
            route.selected = false;
            routes.entry(*dst).or_default().push((route, neigh));
        }

        let mut pending_requests = Vec::new();
        for (dst, routes) in routes {
            let source = self.sources.get(&dst).copied();
            let best = select_best_route(dst, routes, source.map(|source| source.0), self.id);

            let Some(best) = best else {
                self.mesh.remove_route(dst).await?;
                if let Some(source) = source {
                    pending_requests.push((dst, source.0.seq + Seq(1)));
                }
                continue;
            };
            best.selected = true;
            self.mesh.set_route(dst, best.from).await?;

            let distance = route_distance(best);

            // advertise when the distance we send changes, not only when the FD changes
            let significant = should_advertise(
                previous.get(&dst).copied(),
                distance,
                source.is_none(),
                self.update_threshold,
            );
            match self.sources.entry(dst) {
                Entry::Occupied(mut entry) => {
                    let source = entry.get_mut();
                    if distance.better(&source.0) {
                        source.0 = distance;
                    }
                    if significant {
                        source.1 = now + self.source_timeout;
                    }
                }
                Entry::Vacant(entry) => {
                    entry.insert((distance, now + self.source_timeout));
                }
            }
            if significant {
                self.stats.updates_sent += 1;
                self.mesh
                    .broadcast_packet_local(generate_update(best))
                    .await?;
            }
        }
        for (dst, seq) in pending_requests {
            self.send_seqno_request(dst, seq, false).await?;
        }
        Ok(())
    }

    #[skip]
    async fn send_seqno_request(&mut self, dst: NodeId, seq: Seq, force: bool) -> Result<()> {
        let now = Instant::now();
        if !force && let Some(existing) = self.requests.get(&dst) {
            if seq <= existing.seq && now < existing.expiry {
                debug!("Suppressing duplicate SequenceRequest for node {dst:X} with seq {seq:?}");
                return Ok(());
            }
            if existing.seq == seq && existing.retries >= MAX_SEQNO_REQUEST_RETRIES {
                return Ok(());
            }
        }
        let retries = self
            .requests
            .get(&dst)
            .filter(|request| request.seq == seq)
            .map_or(0, |request| request.retries + 1);
        self.requests.insert(
            dst,
            RouteRequest {
                seq,
                expiry: now + self.seqno_request_timeout,
                retries,
            },
        );
        self.stats.seqno_requests += 1;
        self.mesh
            .broadcast_packet_local(Box::new(IgpPayload::SequenceRequest {
                seq,
                dst,
                ttl: self.diameter,
            }))
            .await?;
        Ok(())
    }

    async fn export(&self) -> Result<()> {
        for ((dst, neigh), route) in &self.routes {
            if route.selected {
                self.mesh.set_route(*dst, *neigh).await?;
            }
        }
        Ok(())
    }

    async fn update_route_costs(&mut self, node_id: NodeId, cost: u32) -> Result<()> {
        let mut changed = false;
        for ((_, neigh), route) in &mut self.routes {
            if *neigh != node_id {
                continue;
            }
            route.computed_metric = route.metric.metric.saturating_add(cost);
            changed = true;
        }
        if changed {
            self.select_route().await?;
        }
        Ok(())
    }

    pub fn get_routes(&self) -> Vec<IgpRoute> {
        let now = Instant::now();
        self.routes
            .values()
            .map(|route| IgpRoute {
                seq: route.metric.seq.0,
                metric: route.metric.metric,
                computed_metric: route.computed_metric,
                dst: route.dst,
                from: route.from,
                selected: route.selected,
                timeout_remaining_ms: remaining_ms(now, route.timeout),
            })
            .collect()
    }

    pub fn get_status(&self) -> IgpStatus {
        let now = Instant::now();
        let mut neighbors = self
            .costs
            .iter()
            .map(|(node, cost)| IgpNeighbor {
                node: *node,
                cost: cost.cost,
                last_hello_ms: now.duration_since(cost.start).as_millis() as i64,
            })
            .collect::<Vec<_>>();
        neighbors.sort_by_key(|neighbor| neighbor.node);
        let mut sources = self
            .sources
            .iter()
            .map(|(dst, (metric, expiry))| IgpSource {
                dst: *dst,
                seq: metric.seq.0,
                metric: metric.metric,
                expires_in_ms: remaining_ms(now, *expiry),
            })
            .collect::<Vec<_>>();
        sources.sort_by_key(|source| source.dst);
        let mut requests = self
            .requests
            .iter()
            .map(|(dst, request)| IgpRequest {
                dst: *dst,
                seq: request.seq.0,
                retries: request.retries,
                expires_in_ms: remaining_ms(now, request.expiry),
            })
            .collect::<Vec<_>>();
        requests.sort_by_key(|request| request.dst);
        IgpStatus {
            self_seq: self
                .routes
                .get(&(self.id, self.id))
                .map_or(0, |route| route.metric.seq.0),
            neighbors,
            sources,
            requests,
            hello_interval_ms: self.hello_interval.as_millis() as u64,
            dump_interval_ms: self.dump_interval.as_millis() as u64,
            gc_interval_ms: self.gc_interval.as_millis() as u64,
            route_timeout_ms: self.route_timeout.as_millis() as u64,
            source_timeout_ms: self.source_timeout.as_millis() as u64,
            seqno_request_timeout_ms: self.seqno_request_timeout.as_millis() as u64,
            diameter: self.diameter,
            update_threshold: self.update_threshold,
            dump_batch_size: self.dump_batch_size as u64,
        }
    }

    pub fn get_stats(&self) -> IgpStats {
        IgpStats {
            routes: self.routes.len() as u64,
            selected: self.routes.values().filter(|route| route.selected).count() as u64,
            neighbors: self.costs.len() as u64,
            sources: self.sources.len() as u64,
            requests: self.requests.len() as u64,
            hellos_sent: self.stats.hellos_sent,
            hellos_received: self.stats.hellos_received,
            updates_sent: self.stats.updates_sent,
            updates_received: self.stats.updates_received,
            route_requests: self.stats.route_requests,
            seqno_requests: self.stats.seqno_requests,
        }
    }

    #[handle_error]
    fn handle_error(&mut self, err: &Error) {
        error!("Error: {err:?}");
    }
}

fn route_distance(route: &Route) -> SeqMetric {
    SeqMetric {
        seq: route.metric.seq,
        metric: route.computed_metric,
    }
}

fn remaining_ms(now: Instant, instant: Instant) -> i64 {
    if instant > now {
        instant.duration_since(now).as_millis() as i64
    } else {
        -(now.duration_since(instant).as_millis() as i64)
    }
}

fn select_best_route<'a>(
    dst: NodeId,
    routes: Vec<(&'a mut Route, &'a NodeId)>,
    source: Option<SeqMetric>,
    self_id: NodeId,
) -> Option<&'a mut Route> {
    routes
        .into_iter()
        .filter(|(route, _)| {
            // the local route is always selectable; it has no next-hop cost
            if dst == self_id {
                return route.metric.metric == 0 && route.computed_metric == 0;
            }
            // feasibility uses the metric advertised by the neighbour, not our
            // computed metric; retractions and dead next hops are not selectable
            route.metric.metric != u32::MAX
                && route.computed_metric != u32::MAX
                && source.is_none_or(|source| route.metric.feasible(&source))
        })
        .min_by_key(|(route, _)| route.computed_metric)
        .map(|(route, _)| route)
}

fn should_advertise(
    previous: Option<SeqMetric>,
    distance: SeqMetric,
    source_missing: bool,
    update_threshold: u32,
) -> bool {
    source_missing
        || previous.is_none_or(|previous| {
            previous.seq != distance.seq
                || previous.metric.abs_diff(distance.metric) > update_threshold
        })
}

fn self_seq_after_request(current: Seq, requested: Seq) -> Seq {
    if requested > current {
        requested
    } else {
        current
    }
}

fn find_route_for_request(
    routes: &mut HashMap<(NodeId, NodeId), Route>,
    dst: NodeId,
    self_id: NodeId,
) -> Option<&mut Route> {
    routes
        .iter_mut()
        .find(|((d, n), r)| {
            *d == dst
                && if dst == self_id {
                    *n == self_id
                } else {
                    r.selected
                }
        })
        .map(|(_, route)| route)
}

fn generate_update(route: &Route) -> Box<dyn Payload> {
    Box::new(IgpPayload::Update {
        dst: route.dst,
        metric: route_distance(route),
    })
}

fn generate_retraction(dst: NodeId) -> Box<dyn Payload> {
    Box::new(IgpPayload::Update {
        dst,
        metric: SeqMetric {
            seq: Seq(0),
            metric: u32::MAX,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn route(dst: NodeId, from: NodeId, seq: u16, metric: u32, computed: u32) -> Route {
        Route {
            metric: SeqMetric {
                seq: Seq(seq),
                metric,
            },
            computed_metric: computed,
            dst,
            from,
            timeout: Instant::now(),
            selected: false,
        }
    }

    #[test]
    fn route_distance_tracks_computed_metric() {
        let route = route(1, 2, 7, 0, 123);
        assert_eq!(
            route_distance(&route),
            SeqMetric {
                seq: Seq(7),
                metric: 123
            }
        );
    }

    #[test]
    fn best_route_ignores_seqno() {
        let mut low = route(1, 2, 5, 90, 500);
        let mut high = route(1, 3, 6, 1000, 600);
        let n2 = 2;
        let n3 = 3;
        let source = SeqMetric {
            seq: Seq(5),
            metric: 100,
        };
        let best = select_best_route(1, vec![(&mut high, &n3), (&mut low, &n2)], Some(source), 9);
        assert_eq!(best.unwrap().from, 2);
    }

    #[test]
    fn best_route_skips_infeasible_dead_and_retracted() {
        let mut infeasible = route(1, 2, 5, 100, 100);
        let mut dead = route(1, 3, 6, 1000, u32::MAX);
        let mut retracted = route(1, 4, 6, u32::MAX, 50);
        let mut feasible = route(1, 5, 5, 10, 500);
        let n2 = 2;
        let n3 = 3;
        let n4 = 4;
        let n5 = 5;
        let source = SeqMetric {
            seq: Seq(5),
            metric: 100,
        };
        let best = select_best_route(
            1,
            vec![
                (&mut infeasible, &n2),
                (&mut dead, &n3),
                (&mut retracted, &n4),
                (&mut feasible, &n5),
            ],
            Some(source),
            9,
        );
        assert_eq!(best.unwrap().from, 5);
    }

    #[test]
    fn best_route_keeps_local_route() {
        let self_id = 7;
        let mut local = route(self_id, self_id, 3, 0, 0);
        let mut via = route(self_id, 2, 0, 0, 10);
        let n_local = self_id;
        let n_via = 2;
        let source = SeqMetric {
            seq: Seq(10),
            metric: 100,
        };
        let best = select_best_route(
            self_id,
            vec![(&mut via, &n_via), (&mut local, &n_local)],
            Some(source),
            self_id,
        );
        assert_eq!(best.unwrap().from, self_id);
    }

    #[test]
    fn best_route_none_without_feasible() {
        let source = SeqMetric {
            seq: Seq(5),
            metric: 100,
        };
        let mut a = route(1, 2, 5, 100, 100);
        let mut b = route(1, 3, 5, 200, 50);
        let n2 = 2;
        let n3 = 3;
        let best = select_best_route(1, vec![(&mut a, &n2), (&mut b, &n3)], Some(source), 9);
        assert!(best.is_none());
    }

    #[test]
    fn advertise_on_seqno_or_large_metric_change() {
        let previous = SeqMetric {
            seq: Seq(5),
            metric: 100,
        };
        let same = previous;
        let newer = SeqMetric {
            seq: Seq(6),
            metric: 100,
        };
        let small = SeqMetric {
            seq: Seq(5),
            metric: 110,
        };
        let large = SeqMetric {
            seq: Seq(5),
            metric: 111,
        };
        assert!(!should_advertise(Some(previous), same, false, 10));
        assert!(should_advertise(Some(previous), newer, false, 10));
        assert!(!should_advertise(Some(previous), small, false, 10));
        assert!(should_advertise(Some(previous), large, false, 10));
        assert!(should_advertise(None, same, false, 10));
        assert!(should_advertise(Some(previous), same, true, 10));
    }

    #[test]
    fn self_seq_jumps_forward_only() {
        assert_eq!(self_seq_after_request(Seq(0), Seq(5)), Seq(5));
        assert_eq!(self_seq_after_request(Seq(10), Seq(3)), Seq(10));
        assert_eq!(self_seq_after_request(Seq(65535), Seq(0)), Seq(0));
        assert_eq!(self_seq_after_request(Seq(0), Seq(0)), Seq(0));
    }

    #[test]
    fn find_route_for_request_prefers_local_route() {
        let self_id = 7;
        let mut routes = HashMap::new();
        let local = route(self_id, self_id, 3, 0, 0);
        let mut via = route(self_id, 2, 5, 0, 100);
        via.selected = true;
        routes.insert((self_id, self_id), local);
        routes.insert((self_id, 2), via);

        let found = find_route_for_request(&mut routes, self_id, self_id).unwrap();
        assert_eq!(found.from, self_id);
        assert_eq!(found.metric.seq, Seq(3));

        let mut routes = HashMap::new();
        let stale = route(8, 2, 1, 0, 100);
        let mut current = route(8, 3, 2, 0, 100);
        current.selected = true;
        routes.insert((8, 2), stale);
        routes.insert((8, 3), current);

        let found = find_route_for_request(&mut routes, 8, self_id).unwrap();
        assert_eq!(found.from, 3);
    }
}
