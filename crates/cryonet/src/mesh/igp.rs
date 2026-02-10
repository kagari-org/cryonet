// inspired by RFC 8966

use std::{
    any::Any,
    cmp::Ordering,
    collections::{HashMap, hash_map::Entry},
    fmt::Display,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::Result;
use serde::{Deserialize, Serialize};
use tokio::{
    select,
    sync::{Mutex, Notify, broadcast::error::RecvError},
    time::interval,
};
use tracing::{debug, error, warn};

use crate::mesh::{
    MeshEvent,
    packet::{NodeId, Payload},
    seq::{Seq, SeqMetric},
};

use super::Mesh;

pub(crate) struct Igp {
    id: NodeId,
    mesh: Arc<Mutex<Mesh>>,

    costs: HashMap<NodeId, Cost>,
    sources: HashMap<NodeId, (SeqMetric, Instant)>,
    requests: HashMap<NodeId, RouteRequest>,
    routes: HashMap<(NodeId, NodeId), Route>, // (dst, neigh) -> Route

    route_timeout: Duration,
    source_timeout: Duration,
    seqno_request_timeout: Duration,
    diameter: u16,
    update_threshold: u32,

    stop: Arc<Notify>,
}

#[typetag::serde]
impl Payload for IGPPayload {}
#[derive(Debug, Clone, Serialize, Deserialize)]
enum IGPPayload {
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Route {
    pub(crate) metric: SeqMetric,
    pub(crate) computed_metric: u32,
    pub(crate) dst: NodeId,
    pub(crate) from: NodeId,
    pub(crate) timeout: Instant,
    pub(crate) selected: bool,
}

impl Display for Route {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "to {:X} via {:X}, advertised metric {}, seq {}, metric {}, selected: {}",
            self.dst,
            self.from,
            self.metric.metric,
            self.metric.seq,
            self.computed_metric,
            self.selected,
        )
    }
}

impl Igp {
    pub(crate) async fn new(mesh: Arc<Mutex<Mesh>>) -> Arc<Mutex<Self>> {
        Self::new_with_parameters(
            Duration::from_secs(4),
            Duration::from_secs(16),
            Duration::from_secs(60),
            Duration::from_secs(56),
            Duration::from_secs(3 * 60),
            Duration::from_secs(8),
            64,
            1000,
            mesh,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn new_with_parameters(
        hello_interval: Duration,
        dump_interval: Duration,
        gc_interval: Duration,
        route_timeout: Duration,
        source_timeout: Duration,
        seqno_request_timeout: Duration,
        diameter: u16,
        update_threshold: u32,
        mesh: Arc<Mutex<Mesh>>,
    ) -> Arc<Mutex<Self>> {
        let stop = Arc::new(Notify::new());
        let stop_rx = stop.clone();
        let igp = Arc::new(Mutex::new(Igp {
            id: mesh.lock().await.id,
            mesh: mesh.clone(),
            costs: HashMap::new(),
            sources: HashMap::new(),
            requests: HashMap::new(),
            routes: HashMap::new(),
            route_timeout,
            source_timeout,
            seqno_request_timeout,
            diameter,
            update_threshold,
            stop,
        }));
        let igp2 = igp.clone();
        tokio::spawn(async move {
            let mut hello_ticker = interval(hello_interval);
            let mut dump_ticker = interval(dump_interval);
            let mut gc_ticker = interval(gc_interval);
            let mut packet_rx = mesh
                .lock()
                .await
                .add_dispatchee(|packet| (packet.payload.as_ref() as &dyn Any).is::<IGPPayload>());
            let mut mesh_event_rx = mesh.lock().await.subscribe_mesh_events();
            let notified = stop_rx.notified();
            tokio::pin!(notified);
            loop {
                select! {
                    _ = &mut notified => break,
                    _ = hello_ticker.tick() => igp.lock().await.hello().await,
                    _ = dump_ticker.tick() => igp.lock().await.dump(None).await,
                    _ = gc_ticker.tick() => igp.lock().await.gc().await,
                    packet = packet_rx.recv() => {
                        let Some(packet) = packet else {
                            error!("IGP packet receiver closed unexpectedly");
                            break;
                        };
                        let igp_payload = (packet.payload.as_ref() as &dyn Any)
                            .downcast_ref::<IGPPayload>().unwrap();
                        let result = igp.lock().await
                            .handle_packet(packet.src, igp_payload)
                            .await;
                        if let Err(err) = result {
                            warn!("Failed to handle IGP packet from node {:X}: {}", packet.src, err);
                        }
                    },
                    event = mesh_event_rx.recv() => {
                        let event = match event {
                            Ok(event) => event,
                            Err(RecvError::Lagged(_)) => continue,
                            Err(err) => {
                                error!("Failed to receive mesh event: {}", err);
                                break;
                            },
                        };
                        match event {
                            MeshEvent::LinkUp(_) => {
                                // re-export routes after unexpected link disconnection
                                igp.lock().await.export().await;
                            },
                            MeshEvent::LinkDown(node_id) => {
                                let mut igp = igp.lock().await;
                                if let Some(cost) = igp.costs.get_mut(&node_id) {
                                    cost.cost = u32::MAX;
                                }
                                igp.update_route_costs(node_id, u32::MAX).await;
                            }
                            _ => {},
                        };
                    },
                }
            }
        });
        igp2
    }

    async fn handle_packet(&mut self, src: NodeId, payload: &IGPPayload) -> Result<()> {
        match payload {
            IGPPayload::Hello { seq } => {
                self.mesh
                    .lock()
                    .await
                    .send_packet_link(src, IGPPayload::HelloReply { seq: *seq })
                    .await?;
            }

            IGPPayload::HelloReply { seq } => {
                let time = Instant::now();
                let Entry::Occupied(mut cost) = self.costs.entry(src) else {
                    warn!("Received unexpected HelloReply from node {:X}", src);
                    return Ok(());
                };
                let cost = cost.get_mut();
                if cost.seq != *seq {
                    debug!(
                        "Ignoring HelloReply with mismatched seq {:?} from node {:X}",
                        seq, src
                    );
                    return Ok(());
                }
                let rtt = time.duration_since(cost.start).as_millis() + 100;
                let origin = cost.cost;
                if cost.cost == u32::MAX {
                    cost.cost = rtt as u32;
                    // first hello reply received, request route dump
                    self.mesh
                        .lock()
                        .await
                        .send_packet_link(src, IGPPayload::RouteDump)
                        .await?;
                } else {
                    cost.cost = ((836 * cost.cost as u128 + 164 * rtt) / 1000) as u32;
                }
                if cost.cost != origin {
                    let cost = cost.cost;
                    self.update_route_costs(src, cost).await;
                }
            }

            IGPPayload::RouteRequest { dst } => {
                let mut mesh = self.mesh.lock().await;
                let route = self
                    .routes
                    .iter()
                    .find(|((d, _), r)| d == dst && r.selected);
                match route {
                    Some((_, route)) => {
                        mesh.broadcast_packet_local(generate_update(route)).await?;
                    }
                    None => {
                        mesh.broadcast_packet_local(generate_retraction(*dst))
                            .await?;
                    }
                }
            }

            IGPPayload::RouteDump => self.dump(Some(src)).await,

            IGPPayload::SequenceRequest { seq, dst, ttl } => {
                let mut mesh = self.mesh.lock().await;
                let route = self
                    .routes
                    .iter_mut()
                    .find(|((d, _), r)| d == dst && r.selected);
                let route = match route {
                    Some((_, route)) => {
                        if route.metric.metric == u32::MAX {
                            debug!(
                                "Route to node {:X} is unreachable, dropping SequenceRequest",
                                dst
                            );
                            return Ok(());
                        }
                        route
                    }
                    None => {
                        debug!("No route to node {:X}, dropping SequenceRequest", dst);
                        return Ok(());
                    }
                };
                if *seq <= route.metric.seq {
                    mesh.broadcast_packet_local(generate_update(route)).await?;
                    return Ok(());
                }
                if *dst == mesh.id {
                    // if the route is from ourself
                    route.metric.seq += Seq(1);
                    mesh.broadcast_packet_local(generate_update(route)).await?;
                    return Ok(());
                }
                // forward the request
                if *ttl == 0 {
                    debug!("SequenceRequest to node {:X} expired: TTL=0", dst);
                    return Ok(());
                }
                let source = match self.sources.get(dst) {
                    Some(source) => source,
                    None => {
                        warn!(
                            "Missing source for node {:X}, dropping SequenceRequest",
                            dst
                        );
                        return Ok(());
                    }
                };
                let existing_request = self.requests.get(dst);
                if let Some(existing_request) = existing_request
                    && *seq <= existing_request.seq
                    && Instant::now() < existing_request.expiry
                {
                    debug!(
                        "Suppressing duplicate SequenceRequest for node {:X} with seq {:?}",
                        dst, seq
                    );
                    return Ok(());
                }
                self.requests.insert(
                    *dst,
                    RouteRequest {
                        seq: *seq,
                        expiry: Instant::now() + self.seqno_request_timeout,
                    },
                );
                let mut route_feasible: Option<&Route> = None;
                let mut route_any: Option<&Route> = None;
                for ((_, neigh), route) in &self.routes {
                    if *neigh == src || *dst != route.dst {
                        continue;
                    }
                    if route.metric.feasible(&source.0) {
                        match route_feasible {
                            None => route_feasible = Some(route),
                            Some(rf) => {
                                if route.metric.metric < rf.metric.metric {
                                    route_feasible = Some(route);
                                }
                            }
                        }
                    }
                    match route_any {
                        None => route_any = Some(route),
                        Some(ra) => {
                            if route.metric.metric < ra.metric.metric {
                                route_any = Some(route);
                            }
                        }
                    }
                }
                // ttl decrement
                let mut payload = payload.clone();
                match &mut payload {
                    IGPPayload::SequenceRequest { ttl, .. } => *ttl = ttl.saturating_sub(1),
                    _ => unreachable!(),
                }
                // send
                if let Some(route) = route_feasible {
                    mesh.send_packet_link(route.from, payload).await?;
                    return Ok(());
                }
                if let Some(route) = route_any {
                    mesh.send_packet_link(route.from, payload).await?;
                    return Ok(());
                }
                debug!(
                    "No alternative route to node {:X}, dropping SequenceRequest",
                    dst
                );
            }

            IGPPayload::Update { metric, dst } => {
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
                        let source = match source {
                            Some(source) => source,
                            None => {
                                warn!("Missing source for node {:X}, dropping Update", dst);
                                return Ok(());
                            }
                        };
                        if route.selected && !metric.feasible(&source.0) {
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
                        if !route.metric.feasible(&source.0) {
                            route.selected = false;
                        }
                    }
                }

                // check seqno requests
                let route = &self.routes[&(*dst, src)];
                if let Some(seqno_request) = self.requests.get(dst) {
                    if metric.seq >= seqno_request.seq {
                        self.requests.remove(dst);
                    }
                    // trigger update
                    let mut mesh = self.mesh.lock().await;
                    mesh.broadcast_packet_local(generate_update(route)).await?;
                }

                self.select().await;
            }
        }
        Ok(())
    }

    async fn hello(&mut self) {
        let mut mesh = self.mesh.lock().await;
        for link in mesh.get_links() {
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
            let result = mesh
                .send_packet_link(link, IGPPayload::Hello { seq: cost.seq })
                .await;
            if let Err(err) = result {
                warn!("Failed to send Hello to node {:X}: {}", link, err);
            }
        }
    }

    async fn update_route_costs(&mut self, node_id: NodeId, cost: u32) {
        let mut changed = false;
        for ((_, neigh), route) in &mut self.routes {
            if *neigh != node_id {
                continue;
            }
            route.computed_metric = route.metric.metric.saturating_add(cost);
            changed = true;
        }
        if changed {
            self.select().await;
        }
    }

    async fn select(&mut self) {
        let mut routes = HashMap::new();
        for ((dst, neigh), route) in &mut self.routes {
            route.selected = false;
            routes
                .entry(*dst)
                .or_insert((dst, Vec::new()))
                .1
                .push((route, neigh));
        }
        for (dst, routes) in routes.values_mut() {
            let best = routes
                .iter_mut()
                .min_by(|a, b| match a.0.metric.seq.cmp(&b.0.metric.seq) {
                    // we find the route with the highest seqno and lowest computed metric
                    Ordering::Less => Ordering::Greater,
                    Ordering::Greater => Ordering::Less,
                    Ordering::Equal => a.0.computed_metric.cmp(&b.0.computed_metric),
                })
                .unwrap(); // the list is non-empty
            match self.sources.entry(**dst) {
                Entry::Vacant(source) => {
                    if best.0.metric.metric == u32::MAX {
                        self.mesh.lock().await.remove_route(**dst);
                        // no reachable route
                    } else {
                        // here should be unreachable, but just in case
                        let mut mesh = self.mesh.lock().await;
                        source.insert((best.0.metric, Instant::now() + self.source_timeout));
                        mesh.set_route(**dst, best.0.from);
                        let result = mesh.broadcast_packet_local(generate_update(best.0)).await;
                        if let Err(err) = result {
                            warn!("Failed to broadcast Update for node {:X}: {}", dst, err);
                        }
                    }
                }
                Entry::Occupied(mut source) => {
                    let source = source.get_mut();
                    if best.0.metric.metric == u32::MAX || !best.0.metric.feasible(&source.0) {
                        let mut mesh = self.mesh.lock().await;
                        mesh.remove_route(**dst);
                        // no reachable route
                        // Don't send retraction. The routes of neighbors will time out eventually.
                        let existing_request = self.requests.get(dst);
                        let seq = source.0.seq + Seq(1);
                        if let Some(existing_request) = existing_request
                            && seq <= existing_request.seq
                            && Instant::now() < existing_request.expiry
                        {
                            debug!(
                                "Suppressing duplicate SequenceRequest for node {:X} with seq {:?}",
                                dst, seq
                            );
                            return;
                        }
                        self.requests.insert(
                            **dst,
                            RouteRequest {
                                seq,
                                expiry: Instant::now() + self.seqno_request_timeout,
                            },
                        );
                        let result = mesh
                            .broadcast_packet_local(IGPPayload::SequenceRequest {
                                seq,
                                dst: **dst,
                                ttl: self.diameter,
                            })
                            .await;
                        if let Err(err) = result {
                            warn!(
                                "Failed to broadcast SequenceRequest for node {:X}: {}",
                                dst, err
                            );
                        }
                    } else {
                        let origin = *source;
                        *source = (best.0.metric, Instant::now() + self.source_timeout);
                        best.0.selected = true;
                        let mut mesh = self.mesh.lock().await;
                        mesh.set_route(**dst, best.0.from);
                        if source.0.metric.abs_diff(origin.0.metric) > self.update_threshold {
                            let result = mesh.broadcast_packet_local(generate_update(best.0)).await;
                            if let Err(err) = result {
                                warn!("Failed to broadcast Update for node {:X}: {}", dst, err);
                            }
                        }
                    }
                }
            };
        }
    }

    async fn export(&self) {
        let mut mesh = self.mesh.lock().await;
        for ((dst, neigh), route) in &self.routes {
            if route.selected {
                mesh.set_route(*dst, *neigh);
            }
        }
    }

    async fn dump(&mut self, neigh: Option<NodeId>) {
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

        self.select().await;

        let mut mesh = self.mesh.lock().await;
        for ((dst, _), route) in &self.routes {
            if !route.selected {
                continue;
            }
            // update source timeout
            match self.sources.entry(*dst) {
                Entry::Occupied(mut occupied_entry) => {
                    occupied_entry.get_mut().1 = Instant::now() + self.source_timeout;
                }
                Entry::Vacant(vacant_entry) => {
                    vacant_entry.insert((route.metric, Instant::now() + self.source_timeout));
                    warn!(
                        "Missing source for node {:X} during RouteDump, inserting",
                        dst
                    );
                }
            }
            let update = generate_update(route);
            let result = match neigh {
                Some(neigh) => mesh.send_packet_link(neigh, update).await,
                None => mesh.broadcast_packet_local(update).await,
            };
            if let Err(err) = result {
                warn!("Failed to send RouteDump for node {:X}: {}", dst, err);
            }
        }
    }

    async fn gc(&mut self) {
        let now = Instant::now();
        self.sources.retain(|_, (_, expiry)| *expiry > now);
        self.requests.retain(|_, request| request.expiry > now);
        self.routes
            .retain(|_, route| !(route.timeout <= now && route.metric.metric == u32::MAX));

        let mut mesh = self.mesh.lock().await;
        for ((dst, neigh), route) in &mut self.routes {
            if route.timeout <= now {
                route.metric.metric = u32::MAX;
                route.computed_metric = u32::MAX;
                route.timeout = now + self.route_timeout;
                route.selected = false;
                let result = mesh
                    .send_packet_link(*neigh, IGPPayload::RouteRequest { dst: *dst })
                    .await;
                if let Err(err) = result {
                    warn!("Failed to send RouteRequest for node {:X}: {}", dst, err);
                }
            }
        }
        drop(mesh);
        self.select().await;
    }

    pub(crate) fn get_routes(&self) -> Vec<Route> {
        self.routes.values().cloned().collect()
    }

    pub(crate) fn stop(&self) {
        self.stop.notify_waiters();
    }
}

impl Drop for Igp {
    fn drop(&mut self) {
        self.stop();
    }
}

fn generate_update(route: &Route) -> IGPPayload {
    IGPPayload::Update {
        dst: route.dst,
        metric: SeqMetric {
            seq: route.metric.seq,
            metric: route.computed_metric,
        },
    }
}

fn generate_retraction(dst: NodeId) -> IGPPayload {
    IGPPayload::Update {
        dst,
        metric: SeqMetric {
            seq: Seq(0),
            metric: u32::MAX,
        },
    }
}
