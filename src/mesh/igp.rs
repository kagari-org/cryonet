// inspired by RFC 8966

use std::{any::Any, collections::HashMap, sync::Arc, time::Duration};

use tokio::{sync::{oneshot, Mutex}, time::interval};
use tracing::{error, warn};

use crate::mesh::{LinkEvent, igp_payload::IGPPayload};

use super::{igp_state::IGPState, Mesh};

pub(crate) struct IGP {
    state: Arc<Mutex<IGPState>>,
    stop: Option<oneshot::Sender<()>>,
    mesh: Arc<Mutex<Mesh>>,
}

impl IGP {
    pub(crate) async fn new(mesh: Arc<Mutex<Mesh>>) -> Self {
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
        ).await
    }

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
    ) -> Self {
        let id = mesh.lock().await.id;
        let (stop_tx, stop_rx) = oneshot::channel();
        let igp = IGP {
            mesh: mesh.clone(),
            state: Arc::new(Mutex::new(IGPState {
                id,
                costs: HashMap::new(),
                sources: HashMap::new(),
                requests: HashMap::new(),
                routes: HashMap::new(),
                mesh,
                route_timeout,
                source_timeout,
                seqno_request_timeout,
                diameter,
                update_threshold,
            })),
            stop: Some(stop_tx),
        };
        igp.start(
            hello_interval,
            dump_interval,
            gc_interval,
            stop_rx,
        ).await;
        igp
    }

    async fn start(
        &self,
        hello_interval: Duration,
        dump_interval: Duration,
        gc_interval: Duration,
        mut stop: oneshot::Receiver<()>,
    ) {
        let mesh = self.mesh.clone();
        let state = self.state.clone();

        let mut packet_rx = mesh.lock().await.add_dispatchee(|packet|
            (packet.payload.as_ref() as &dyn Any).is::<IGPPayload>()).await;
        let mut link_event_rx = self.mesh.lock().await.subscribe_link_events().await;
        let mut hello_ticker = interval(hello_interval);
        let mut dump_ticker = interval(dump_interval);
        let mut gc_ticker = interval(gc_interval);

        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = &mut stop => break,
                    packet = packet_rx.recv() => {
                        let Some(packet) = packet else {
                            warn!("IGP packet receiver closed");
                            break;
                        };
                        let igp_payload = (packet.payload.as_ref() as &dyn Any)
                            .downcast_ref::<IGPPayload>().unwrap();
                        let result = state.lock().await.handle_packet(packet.src, igp_payload).await;
                        if let Err(err) = result {
                            warn!("Failed to handle IGP packet: {}", err);
                        }
                    },
                    event = link_event_rx.recv() => {
                        let event = match event {
                            Ok(event) => event,
                            Err(err) => {
                                warn!("Failed to receive IGP link event: {}", err);
                                break;
                            },
                        };
                        match event {
                            LinkEvent::Up(_) => {
                                // re-export routes after unexpected link disconnection
                                state.lock().await.export().await;
                            },
                            _ => {},
                        }
                    },
                    _ = hello_ticker.tick() => state.lock().await.send_hello().await,
                    _ = dump_ticker.tick() => state.lock().await.dump(None).await,
                    _ = gc_ticker.tick() => state.lock().await.gc().await,
                }
            } 
        });
    }
}

impl Drop for IGP {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            if let Err(_) = stop.send(()) {
                error!("IGP stop signal receiver already dropped");
            }
        } else {
            error!("IGP already stopped");
        }
    }
}
