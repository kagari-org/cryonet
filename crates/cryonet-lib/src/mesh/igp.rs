// inspired by RFC 8966

use std::{
    any::Any,
    collections::{HashMap, hash_map::Entry},
    time::Duration,
};

use anyhow::{Error, Result};
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
use crate::time::{Instant, Interval, interval};

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
    route_timeout: Duration,
    source_timeout: Duration,
    seqno_request_timeout: Duration,
    diameter: u16,
    update_threshold: u32,

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
        Self::new_with_parameters(
            Duration::from_secs(4),
            Duration::from_secs(16),
            Duration::from_secs(60),
            Duration::from_secs(56),
            Duration::from_secs(3 * 60),
            Duration::from_secs(8),
            64,
            1000,
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
        id: NodeId,
        mesh: MeshHandle,
    ) -> Result<IgpHandle> {
        let packet_rx = mesh
            .add_dispatchee(Box::new(|packet| {
                (packet.payload.as_ref() as &dyn Any).is::<IgpPayload>()
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
            route_timeout,
            source_timeout,
            seqno_request_timeout,
            diameter,
            update_threshold,
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
        Ok(())
    }

    #[no_reply]
    async fn hello(&mut self) -> Result<()> {
        for link in self.mesh.get_links().await? {
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

        for ((dst, _), route) in &self.routes {
            if !route.selected {
                continue;
            }
            // update source timeout
            let distance = SeqMetric {
                seq: route.metric.seq,
                metric: route.computed_metric,
            };
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
            let update = generate_update(route);
            match neigh {
                Some(neigh) => self.mesh.send_packet_link(neigh, update).await?,
                None => self.mesh.broadcast_packet_local(update).await?,
            }
        }

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
        let payload = (packet.payload.as_ref() as &dyn Any)
            .downcast_ref::<IgpPayload>()
            .unwrap();
        match payload {
            IgpPayload::Hello { seq } => {
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
                let route = self
                    .routes
                    .iter()
                    .find(|((d, _), r)| d == dst && r.selected);
                match route {
                    Some((_, route)) => {
                        self.mesh
                            .broadcast_packet_local(generate_update(route))
                            .await?;
                    }
                    None => {
                        self.mesh
                            .broadcast_packet_local(generate_retraction(*dst))
                            .await?;
                    }
                }
            }

            IgpPayload::RouteDump => self.dump(Some(src)).await?,

            IgpPayload::SequenceRequest { seq, dst, ttl } => {
                let route = self.routes.iter_mut().find(|((d, n), r)| {
                    d == dst
                        && if *dst == self.id {
                            *n == self.id
                        } else {
                            r.selected
                        }
                });
                let route = match route {
                    Some((_, route)) => {
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
                    route.metric.seq = *seq;

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
                if dst == &self.id {
                    // ignore updates about ourself
                    return Ok(());
                }
                let computed_metric = match (self.costs.get(&src), metric.metric) {
                    (_, u32::MAX) => u32::MAX,
                    (None, _) => u32::MAX - 1,
                    (Some(cost), metric) => metric.saturating_add(cost.cost),
                };

                let source = self.sources.get(dst);
                let entry = self.routes.entry((*dst, src));
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
                            metric: *metric,
                            computed_metric,
                            dst: *dst,
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
                            route.metric = *metric;
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
                let route = &self.routes[&(*dst, src)];
                if let Some(seqno_request) = self.requests.get(dst)
                    && metric.seq >= seqno_request.seq
                {
                    self.requests.remove(dst);
                    // trigger update
                    self.mesh
                        .broadcast_packet_local(generate_update(route))
                        .await?;
                }

                self.select_route().await?;
            }
        }
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
        for (dst, routes) in &mut routes {
            let dst = *dst;
            let source = self.sources.get(&dst).copied();
            let best = routes
                .iter_mut()
                .filter(|(route, _)| {
                    // the local route is always selectable; it has no next-hop cost
                    if dst == self.id {
                        return route.metric.metric == 0 && route.computed_metric == 0;
                    }
                    // feasibility uses the metric advertised by the neighbour, not our
                    // computed metric; retractions and dead next hops are not selectable
                    route.metric.metric != u32::MAX
                        && route.computed_metric != u32::MAX
                        && source.is_none_or(|source| route.metric.feasible(&source.0))
                })
                .min_by_key(|(route, _)| route.computed_metric);

            let Some(best) = best else {
                self.mesh.remove_route(dst).await?;
                if let Some(source) = source {
                    pending_requests.push((dst, source.0.seq + Seq(1)));
                }
                continue;
            };
            let best = &mut *best.0;
            best.selected = true;
            self.mesh.set_route(dst, best.from).await?;

            let distance = SeqMetric {
                seq: best.metric.seq,
                metric: best.computed_metric,
            };

            // advertise when the distance we send changes, not only when the FD changes
            let significant = source.is_none()
                || previous.get(&dst).is_none_or(|previous| {
                    previous.seq != distance.seq
                        || previous.metric.abs_diff(distance.metric) > self.update_threshold
                });
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
                self.mesh
                    .broadcast_packet_local(generate_update(best))
                    .await?;
            }
        }
        drop(routes);

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

    pub fn get_routes(&self) -> Vec<Route> {
        self.routes.values().cloned().collect()
    }

    #[handle_error]
    fn handle_error(&mut self, err: &Error) {
        error!("Error: {err:?}");
    }
}

fn generate_update(route: &Route) -> Box<dyn Payload> {
    Box::new(IgpPayload::Update {
        dst: route.dst,
        metric: SeqMetric {
            seq: route.metric.seq,
            metric: route.computed_metric,
        },
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
