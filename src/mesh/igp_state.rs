use std::{cmp::Ordering, collections::{HashMap, hash_map::Entry}, sync::Arc, time::{Duration, Instant}, u32};

use anyhow::Result;
use tokio::sync::Mutex;
use tracing::{info, warn};

use super::{seq::{SeqMetric, Seq}, igp_payload::IGPPayload, packet::NodeId, Mesh};

pub(crate) struct IGPState {
    pub(crate) costs: HashMap<NodeId, Cost>,
    pub(crate) sources: HashMap<NodeId, SeqMetric>,
    pub(crate) requests: HashMap<NodeId, RouteRequest>,
    pub(crate) routes: HashMap<(NodeId, NodeId), Route>, // (dst, neigh) -> Route
    pub(crate) mesh: Arc<Mutex<Mesh>>,

    pub(crate) route_timeout: Duration,
    pub(crate) seqno_request_timeout: Duration,
    pub(crate) diameter: u16,
    pub(crate) update_threshold: u32,
}

pub(crate) struct Cost {
    pub(crate) seq: Seq,
    pub(crate) start: Instant,
    pub(crate) cost: u32,
}

pub(crate) struct RouteRequest {
    pub(crate) seq: Seq,
    pub(crate) expiry: Instant,
}

pub(crate) struct Route {
    pub(crate) metric: SeqMetric,
    pub(crate) computed_metric: u32,
    pub(crate) dst: NodeId,
    pub(crate) from: NodeId,
    pub(crate) timeout: Instant,
    pub(crate) selected: bool,
}

impl IGPState {
    pub(crate) async fn handle_packet(
        &mut self,
        src: NodeId,
        payload: &IGPPayload,
    ) -> Result<()> {
        match payload {
            IGPPayload::Hello { seq } => {
                let mesh = self.mesh.lock().await;
                mesh.send_packet_link(src, IGPPayload::HelloReply { seq: *seq }).await?;
            },


            IGPPayload::HelloReply { seq } => {
                let time = Instant::now();
                let Entry::Occupied(mut cost) = self.costs.entry(src) else {
                    warn!("Received unexpected HelloReply from {:X}", src);
                    return Ok(());
                };
                let cost = cost.get_mut();
                if cost.seq != *seq {
                    warn!("Ignoring unexpected HelloReply with seq {:?} from {:X}", seq, src);
                    return Ok(());
                }
                let rtt = time.duration_since(cost.start).as_millis();
                let origin = cost.cost;
                if cost.cost == u32::MAX {
                    cost.cost = rtt as u32;
                    // TODO: first hello reply received, request route dump
                } else {
                    cost.cost = ((836 * cost.cost as u128 + 164 * rtt) / 1000) as u32;
                }
                if cost.cost != origin {
                    let mut changed = false;
                    for ((_, neigh), route) in &mut self.routes {
                        if *neigh != src {
                            continue;
                        }
                        route.computed_metric = route.metric.metric.saturating_add(cost.cost);
                        changed = true;
                    }
                    if changed {
                        self.select().await;
                    }
                }
            },


            IGPPayload::RouteRequest { dst } => {
                let mesh = self.mesh.lock().await;
                let route = self.routes.iter().find(|((d, _), r)| d == dst && r.selected);
                // ignore when we don't have a route to dst
                if let Some((_, route)) = route {
                    mesh.broadcast_packet_local(IGPPayload::Update {
                        dst: *dst,
                        metric: SeqMetric { metric: route.computed_metric, ..route.metric },
                    }).await?;
                }
            },


            IGPPayload::SequenceRequest { seq, dst, ttl } => {
                let mesh = self.mesh.lock().await;
                let route = self.routes.iter_mut().find(|((d, _), r)| d == dst && r.selected);
                let route = match route {
                    Some((_, route)) => {
                        if route.metric.metric == u32::MAX {
                            warn!("The route to {:X} is unreachable, dropping SequenceRequest", dst);
                            return Ok(());
                        }
                        route
                    },
                    None => {
                        warn!("Routes to {:X} not found, dropping SequenceRequest", dst);
                        return Ok(());
                    },
                };
                if *seq <= route.metric.seq {
                    mesh.broadcast_packet_local(IGPPayload::Update {
                        dst: *dst,
                        metric: SeqMetric { metric: route.computed_metric, ..route.metric },
                    }).await?;
                    return Ok(());
                }
                if *dst == mesh.id {
                    // if the route is from ourself
                    route.metric.seq += Seq(1);
                    mesh.broadcast_packet_local(IGPPayload::Update {
                        dst: *dst,
                        metric: SeqMetric { metric: route.computed_metric, ..route.metric },
                    }).await?;
                    return Ok(());
                }
                // forward the request
                if *ttl == 0 {
                    warn!("TTL expired for SequenceRequest to {:X}, dropping", dst);
                    return Ok(());
                }
                let source = match self.sources.get(dst) {
                    Some(source) => source,
                    None => {
                        warn!("Unexpected missing source for {:X}, dropping SequenceRequest", dst);
                        return Ok(());
                    },
                };
                let existing_request = self.requests.get(dst);
                if let Some(existing_request) = existing_request {
                    if *seq <= existing_request.seq && Instant::now() < existing_request.expiry {
                        info!("Supressing SequenceRequest for {:X} with seq {:?}", dst, seq);
                        return Ok(());
                    }
                }
                self.requests.insert(*dst, RouteRequest {
                    seq: *seq,
                    expiry: Instant::now() + self.seqno_request_timeout,
                });
                let mut route_feasible: Option<&Route> = None;
                let mut route_any: Option<&Route> = None;
                for ((_, neigh), route) in &self.routes {
                    if *neigh == src || *dst != route.dst {
                        continue;
                    }
                    if route.metric.feasible(&source) {
                        match route_feasible {
                            None => route_feasible = Some(route),
                            Some(rf) => {
                                if route.metric.metric < rf.metric.metric {
                                    route_feasible = Some(route);
                                }
                            },
                        }
                    }
                    match route_any {
                        None => route_any = Some(route),
                        Some(ra) => {
                            if route.metric.metric < ra.metric.metric {
                                route_any = Some(route);
                            }
                        },
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
                warn!("No alternative route to {:X} found, dropping SequenceRequest", dst);
            },


            IGPPayload::Update { metric, dst } => {
                let cost = self.costs.get(&src);
                let computed_metric = match (cost, metric.metric) {
                    (_, u32::MAX) => u32::MAX,
                    (None, _) => {
                        warn!("Unexpected missing cost for {:X}", src);
                        u32::MAX - 1
                    },
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
                        if let Some(source) = source && !metric.feasible(source) {
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
                    },
                    Entry::Occupied(mut entry) => {
                        let route = entry.get_mut();
                        let source = match source {
                            Some(source) => source,
                            None => {
                                warn!("Unexpected missing source for {:X}, dropping Update", dst);
                                return Ok(());
                            },
                        };
                        if route.selected && !metric.feasible(&source) {
                            // ignore
                            return Ok(())
                        }
                        route.metric = *metric;
                        route.computed_metric = computed_metric;
                        route.timeout = Instant::now() + self.route_timeout;
                        if !route.metric.feasible(source) {
                            route.selected = false;
                        }
                    },
                }

                let route = &self.routes[&(*dst, src)];                
                if let Some(seqno_request) = self.requests.get(dst) {
                    if metric.seq >= seqno_request.seq {
                        self.requests.remove(dst);
                    }
                    // trigger update
                    let mesh = self.mesh.lock().await;
                    mesh.broadcast_packet_local(IGPPayload::Update {
                        dst: *dst,
                        metric: SeqMetric { metric: route.computed_metric, ..route.metric },
                    }).await?;
                }

                self.select().await;
            },
        }
        Ok(())
    }

    pub(crate) async fn send_hello(&mut self) {
        let mesh = self.mesh.lock().await;
        let links = mesh.get_links().await;
        for link in links {
            let cost = self.costs.entry(link)
                .and_modify(|cost| {
                    cost.seq += Seq(1);
                    cost.start = Instant::now();
                })
                .or_insert(Cost {
                    seq: Seq(0),
                    start: Instant::now(),
                    cost: u32::MAX,
                });
            let result = mesh.send_packet_link(link,
                IGPPayload::Hello { seq: cost.seq }).await;
            if let Err(err) = result {
                warn!("Failed to send IGP Hello to {:X}: {}", link, err);
            }
        }
    }

    pub(crate) async fn select(&mut self) {
        let mut routes = HashMap::new();
        for ((dst, neigh), route) in &mut self.routes {
            routes.entry(*dst).or_insert((dst, Vec::new())).1.push((route, neigh));
        }
        for (dst, routes) in routes.values_mut() {
            let best = routes.iter_mut().min_by(|a, b| match a.0.metric.seq.cmp(&b.0.metric.seq) {
                // we find the route with the highest seqno and lowest computed metric
                Ordering::Less => Ordering::Greater,
                Ordering::Greater => Ordering::Less,
                Ordering::Equal => a.0.computed_metric.cmp(&b.0.computed_metric),
            }).unwrap(); // the list is non-empty
            match self.sources.entry(**dst) {
                Entry::Vacant(source) => {
                    if best.0.metric.metric == u32::MAX {
                        self.mesh.lock().await.remove_route(**dst).await;
                        // no reachable route
                    } else {
                        source.insert(best.0.metric);
                        self.mesh.lock().await.set_route(**dst, best.0.from).await;
                    }
                },
                Entry::Occupied(mut source) => {
                    let source = source.get_mut();
                    if best.0.metric.metric == u32::MAX || !best.0.metric.feasible(source) {
                        let mesh = self.mesh.lock().await;
                        mesh.remove_route(**dst).await;
                        // no reachable route
                        // Don't send retraction. The routes of neighbors will time out eventually.
                        let existing_request = self.requests.get(dst);
                        let seq = source.seq + Seq(1);
                        if let Some(existing_request) = existing_request {
                            if seq <= existing_request.seq && Instant::now() < existing_request.expiry {
                                info!("Supressing SequenceRequest for {:X} with seq {:?}", dst, seq);
                                return;
                            }
                        }
                        self.requests.insert(**dst, RouteRequest {
                            seq,
                            expiry: Instant::now() + self.seqno_request_timeout,
                        });
                        let result = mesh.broadcast_packet_local(IGPPayload::SequenceRequest {
                            seq,
                            dst: **dst,
                            ttl: self.diameter,
                        }).await;
                        if let Err(err) = result {
                            warn!("Failed to broadcast SequenceRequest for {:X}: {}", dst, err);
                        }
                    } else {
                        let origin = *source;
                        *source = best.0.metric;
                        best.0.selected = true;
                        let mesh = self.mesh.lock().await;
                        mesh.set_route(**dst, best.0.from).await;
                        if source.metric.abs_diff(origin.metric) > self.update_threshold {
                            let result = mesh.broadcast_packet_local(IGPPayload::Update {
                                dst: **dst,
                                metric: *source,
                            }).await;
                            if let Err(err) = result {
                                warn!("Failed to broadcast Update for {:X}: {}", dst, err);
                            }
                        }
                    }
                },
            };
        };
    }

    pub(crate) async fn export(&self) {
        let mesh = self.mesh.lock().await;
        for ((dst, neigh), route) in &self.routes {
            if route.selected {
                mesh.set_route(*dst, *neigh).await;
            }
        }
    }

    pub(crate) async fn dump(&self) {
        let mesh = self.mesh.lock().await;
        for ((dst, neigh), route) in &self.routes {
            let result = mesh.broadcast_packet_local(IGPPayload::Update {
                metric: route.metric,
                dst: *dst,
            }).await;
            if let Err(err) = result {
                warn!("Failed to dump route to {:X} via {:X}: {}", dst, neigh, err);
            }
        }
    }

    pub(crate) async fn gc(&mut self) {
        let now = Instant::now();
        self.requests.retain(|_, request| request.expiry > now);
        self.routes.retain(|_, route| !(route.timeout <= now && route.metric.metric == u32::MAX));

        let mesh = self.mesh.lock().await;
        let mut futures = Vec::new();
        for ((dst, neigh), route) in &mut self.routes {
            if route.timeout <= now {
                route.metric.metric = u32::MAX;
                route.computed_metric = u32::MAX;
                route.timeout = now + self.route_timeout;
                route.selected = false;
                futures.push(mesh.send_packet_link(*neigh, IGPPayload::RouteRequest { dst: *dst }));
            }
        }
        let results = futures::future::join_all(futures).await;
        for result in results {
            if let Err(err) = result {
                warn!("Failed to send RouteRequest: {}", err);
            }
        }
        drop(mesh);
        self.select().await;
    }
}
