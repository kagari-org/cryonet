use std::{
    collections::HashMap,
    net::IpAddr,
    sync::{Arc, Weak},
    time::Duration,
};

use anyhow::Result;
use async_trait::async_trait;
use bytes::Bytes;
use tokio::{select, sync::watch};
use tracing::{debug, error};
use tun_rs::{AsyncDevice, DeviceBuilder};

use crate::{
    errors::CryonetError,
    fullmesh::{ConnectionReceiver, ConnectionSender, DeviceManager},
    mesh::packet::NodeId,
    time::interval,
};

pub struct TunManager {
    interface_prefix: String,
    enable_packet_information: bool,
    keepalive_interval: Duration,

    devices: HashMap<NodeId, Weak<AsyncDevice>>,
    tasks: HashMap<NodeId, watch::Sender<bool>>,
}

impl TunManager {
    pub fn new(interface_prefix: String, enable_packet_information: bool) -> TunManager {
        Self::new_with_parameters(
            interface_prefix,
            enable_packet_information,
            Duration::from_secs(10),
        )
    }

    pub fn new_with_parameters(
        interface_prefix: String,
        enable_packet_information: bool,
        keepalive_interval: Duration,
    ) -> TunManager {
        TunManager {
            interface_prefix,
            enable_packet_information,
            keepalive_interval,
            devices: HashMap::new(),
            tasks: HashMap::new(),
        }
    }

    fn create_device(&mut self, node_id: NodeId) -> Result<Arc<AsyncDevice>> {
        let create_device = || -> Result<AsyncDevice> {
            let mut builder = DeviceBuilder::new()
                .mtu(1280)
                .name(format!("{}{node_id:X}", self.interface_prefix))
                .enable(true);
            if self.enable_packet_information {
                builder = builder.packet_information(true);
            }
            Ok(builder.build_async()?)
        };
        match self.devices.get(&node_id) {
            Some(device) => match device.upgrade() {
                Some(device) => Ok(device),
                None => {
                    let device = Arc::new(create_device()?);
                    self.devices.insert(node_id, Arc::downgrade(&device));
                    Ok(device)
                }
            },
            None => {
                let device = Arc::new(create_device()?);
                self.devices.insert(node_id, Arc::downgrade(&device));
                Ok(device)
            }
        }
    }
}

#[async_trait(?Send)]
impl DeviceManager for TunManager {
    async fn connected(
        &mut self,
        node_id: NodeId,
        sender: Box<dyn ConnectionSender>,
        receiver: Box<dyn ConnectionReceiver>,
    ) -> Result<()> {
        if let Some(stop) = self.tasks.remove(&node_id) {
            let _ = stop.send(true);
        }
        let dev_send = self.create_device(node_id)?;
        let dev_recv = dev_send.clone();
        let stop_tx = watch::channel(false).0;
        tokio::spawn(send_loop(
            node_id,
            self.keepalive_interval,
            sender,
            dev_send,
            stop_tx.subscribe(),
        ));
        tokio::spawn(recv_loop(node_id, receiver, dev_recv, stop_tx.subscribe()));
        self.tasks.insert(node_id, stop_tx);
        Ok(())
    }

    async fn disconnected(&mut self, node_id: NodeId) -> Result<()> {
        if let Some(stop_tx) = self.tasks.remove(&node_id) {
            let _ = stop_tx.send(true);
        }
        Ok(())
    }

    async fn ips(&self) -> Result<Vec<IpAddr>> {
        let mut result = Vec::new();
        for device in self.devices.values() {
            let Some(device) = device.upgrade() else {
                continue;
            };
            let addresses = device.addresses()?;
            result.extend(addresses);
        }
        Ok(result)
    }
}

async fn send_loop(
    node_id: NodeId,
    keepalive_interval: Duration,
    mut sender: Box<dyn ConnectionSender>,
    device: Arc<AsyncDevice>,
    mut stop: watch::Receiver<bool>,
) {
    let mut buf = [0u8; 2000];
    let mut keepalive_ticker = interval(keepalive_interval);
    // We use a single byte as keepalive packet, which is guaranteed to be smaller than any valid packet.
    let keepalive = Bytes::from_static(&[0u8; 1]);
    loop {
        select! {
            _ = stop.changed() => break,
            // Moving the keepalive logic to otherside is too annoying, so just send keepalive from here.
            _ = keepalive_ticker.tick() => {
                if let Err(err) = sender.send(keepalive.clone()).await {
                    error!("Failed to send keepalive to node {node_id:X}: {err}");
                }
            }
            res = device.recv(&mut buf) => {
                let size = match res {
                    Ok(s) => s,
                    Err(err) => {
                        error!("Failed to read from TUN device for node {node_id:X}: {err}");
                        continue;
                    }
                };
                let bytes = Bytes::copy_from_slice(&buf[..size]);
                if let Err(err) = sender.send(bytes).await {
                    error!("Failed to send to node {node_id:X}: {err}");
                }
            }
        }
    }
}

async fn recv_loop(
    node_id: NodeId,
    mut receiver: Box<dyn ConnectionReceiver>,
    device: Arc<AsyncDevice>,
    mut stop: watch::Receiver<bool>,
) {
    loop {
        select! {
            _ = stop.changed() => break,
            res = receiver.recv() => {
                let (packet, _) = match res {
                    Ok(p) => p,
                    Err(err) => match err.downcast_ref::<CryonetError>() {
                        Some(CryonetError::ChannelClosed) => break,
                        _ => {
                            error!("Failed to receive from node {node_id:X}: {err}");
                            continue;
                        }
                    }
                };
                if packet.len() == 1 {
                    // Keepalive packet, ignore
                    debug!("Received keepalive packet from node {node_id:X}");
                    continue;
                }
                if let Err(err) = device.send(&packet).await {
                    error!("Failed to write to TUN device for node {node_id:X}: {err}");
                }
            }
        }
    }
}
