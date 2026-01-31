use std::{collections::HashMap, sync::{Arc, Weak}};

use bytes::Bytes;
use futures::future::select_all;
use tokio::{select, sync::{Mutex, Notify, broadcast::error::RecvError}, task::JoinHandle};
use tracing::error;
use tun_rs::{AsyncDevice, DeviceBuilder};

use crate::{fullmesh::FullMesh, mesh::packet::NodeId};

pub(crate) struct TunManager {
    fm: Arc<Mutex<FullMesh>>,
    devices: HashMap<NodeId, Weak<AsyncDevice>>,
    receivers: HashMap<NodeId, JoinHandle<()>>,
    senders: HashMap<NodeId, JoinHandle<()>>,

    stop: Arc<Notify>,
}

impl TunManager {
    pub(crate) async fn new(
        fm: Arc<Mutex<FullMesh>>,
    ) -> Arc<Mutex<Self>> {
        let mut refresh = fm.lock().await.subscribe_refresh();
        let stop = Arc::new(Notify::new());
        let stop_rx = stop.clone();

        let tm = Arc::new(Mutex::new(Self {
            fm,
            stop,
            devices: HashMap::new(),
            receivers: HashMap::new(),
            senders: HashMap::new(),
        }));
        let tm2 = tm.clone();

        tokio::spawn(async move {
            let notified = stop_rx.notified();
            tokio::pin!(notified);
            loop {
                select! {
                    _ = &mut notified => break,
                    refresh = refresh.recv() => {
                        if let Err(RecvError::Closed) = refresh {
                            break;
                        }
                        tm.lock().await.handle_refresh().await;
                    },
                }
            }
        });
        tm2
    }

    async fn handle_refresh(&mut self) {
        let (receivers, senders) = {
            let fm = self.fm.lock().await;
            (fm.get_receivers(), fm.get_senders())
        };
        let mut create_device = |node_id: NodeId| {
            fn create_device() -> AsyncDevice {
                DeviceBuilder::new()
                    .mtu(1280)
                    .enable(true)
                    // .packet_information(true)
                    .build_async().unwrap()
            }
            match self.devices.get(&node_id) {
                Some(device) => {
                    match device.upgrade() {
                        Some(device) => device,
                        None => {
                            let device = Arc::new(create_device());
                            self.devices.insert(node_id, Arc::downgrade(&device));
                            device
                        },
                    }
                },
                None => {
                    let device = Arc::new(create_device());
                    self.devices.insert(node_id, Arc::downgrade(&device));
                    device
                },
            }
        };
        for (node_id, _) in receivers {
            if let Some(handle) = self.receivers.get(&node_id) && !handle.is_finished() {
                continue;
            }
            let device = create_device(node_id);
            let handle = tokio::spawn(receive(node_id, self.fm.clone(), device));
            self.receivers.insert(node_id, handle);
        }
        for (node_id, _) in senders {
            if let Some(handle) = self.senders.get(&node_id) && !handle.is_finished() {
                continue;
            }
            let device = create_device(node_id);
            let handle = tokio::spawn(send(node_id, self.fm.clone(), device));
            self.senders.insert(node_id, handle);
        }
    }

    pub(crate) fn stop(&self) {
        self.stop.notify_waiters();
    }
}

impl Drop for TunManager {
    fn drop(&mut self) {
        self.stop();
    }
}

async fn receive(
    node_id: NodeId,
    fm: Arc<Mutex<FullMesh>>,
    device: Arc<AsyncDevice>,
) {
    let mut refresh = fm.lock().await.subscribe_refresh();
    'outer: loop {
        let mut receivers = fm.lock().await.get_receivers();
        let receivers = receivers.remove(&node_id);
        let Some(receivers) = receivers else {
            break;
        };
        if receivers.is_empty() {
            break;
        }
        loop {
            let recv: Vec<_> = receivers.iter()
                .map(|conn| Box::pin(conn.recv()))
                .collect();
            select! {
                result = refresh.recv() => {
                    match result {
                        Ok(_) | Err(RecvError::Lagged(_)) => break,
                        Err(_) => break 'outer,
                    }
                },
                (packet, _, _) = select_all(recv) => {
                    let packet = match packet {
                        Ok(packet) => packet,
                        Err(err) => {
                            error!("receive error from {}: {}", node_id, err);
                            break;
                        },
                    };
                    if let Err(err) = device.send(&packet).await {
                        error!("tun write error for {}: {}", node_id, err);
                        break;
                    }
                },
            }
        }
    }
}

async fn send(
    node_id: NodeId,
    fm: Arc<Mutex<FullMesh>>,
    device: Arc<AsyncDevice>,
) {
    let mut refresh = fm.lock().await.subscribe_refresh();
    'outer: loop {
        let mut senders = fm.lock().await.get_senders();
        let senders = senders.remove(&node_id);
        let Some(sender) = senders else {
            break;
        };
        let mut packet = [0u8; 2000];
        loop {
            select! {
                result = refresh.recv() => {
                    match result {
                        Ok(_) | Err(RecvError::Lagged(_)) => break,
                        Err(_) => break 'outer,
                    }
                },
                size = device.recv(&mut packet) => {
                    let size = match size {
                        Ok(size) => size,
                        Err(err) => {
                            error!("tun read error for {}: {}", node_id, err);
                            break;
                        },
                    };
                    let bytes = Bytes::copy_from_slice(&packet[..size]);
                    if let Err(err) = sender.send(bytes).await {
                        error!("send error to {}: {}", node_id, err);
                        break;
                    }
                },
            }
        }
    }
}
