use std::{
    collections::HashMap,
    net::{IpAddr, Ipv6Addr},
    sync::{Arc, Weak},
    time::Duration,
};

use anyhow::Result;
use async_trait::async_trait;
use bytes::Bytes;
use tokio::{select, sync::watch};
use tracing::{debug, error, warn};
use tun_rs::{AsyncDevice, DeviceBuilder};

use crate::{
    errors::CryonetError,
    fullmesh::{
        DeviceManager,
        ice::{IceReceiver, IceSender},
    },
    mesh::packet::NodeId,
    utils::{delayed_interval, enable_mpls_input, jitter},
};

pub(crate) struct TunManager {
    interface_prefix: String,
    enable_packet_information: bool,
    enable_mpls: bool,
    ipv6: Option<Ipv6Addr>,
    keepalive_interval: Duration,
    next_device: u32,

    devices: HashMap<NodeId, Weak<AsyncDevice>>,
    tasks: HashMap<NodeId, watch::Sender<bool>>,
}

impl TunManager {
    pub(crate) fn new(
        interface_prefix: String,
        enable_packet_information: bool,
        enable_mpls: bool,
        ipv6: Option<Ipv6Addr>,
    ) -> TunManager {
        Self::new_with_parameters(
            interface_prefix,
            enable_packet_information,
            enable_mpls,
            ipv6,
            Duration::from_secs(3),
        )
    }

    pub(crate) fn new_with_parameters(
        interface_prefix: String,
        enable_packet_information: bool,
        enable_mpls: bool,
        ipv6: Option<Ipv6Addr>,
        keepalive_interval: Duration,
    ) -> TunManager {
        TunManager {
            interface_prefix,
            enable_packet_information,
            enable_mpls,
            ipv6,
            keepalive_interval,
            next_device: 0,
            devices: HashMap::new(),
            tasks: HashMap::new(),
        }
    }

    fn create_device(&mut self, node_id: NodeId) -> Result<Arc<AsyncDevice>> {
        self.devices.retain(|_, device| device.strong_count() > 0);
        if let Some(device) = self.devices.get(&node_id).and_then(Weak::upgrade) {
            return Ok(device);
        }
        let name = format!("{}{}", self.interface_prefix, self.next_device);
        self.next_device = self.next_device.wrapping_add(1);
        let mut builder = DeviceBuilder::new().mtu(1280).name(name).enable(true);
        if self.enable_packet_information {
            builder = builder.packet_information(true);
        }
        if let Some(ipv6) = self.ipv6 {
            builder = builder.ipv6(ipv6, 64u8);
        }
        let device = Arc::new(builder.build_async()?);
        if self.enable_mpls {
            enable_mpls_input(&device.name()?)?;
        }
        self.devices.insert(node_id, Arc::downgrade(&device));
        Ok(device)
    }
}

#[async_trait(?Send)]
impl DeviceManager for TunManager {
    async fn connected(
        &mut self,
        node_id: NodeId,
        sender: IceSender,
        receiver: IceReceiver,
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

    fn interface(&self, node_id: NodeId) -> Option<String> {
        self.devices
            .get(&node_id)
            .and_then(|device| device.upgrade())
            .and_then(|device| device.name().ok())
    }
}

async fn send_loop(
    node_id: NodeId,
    keepalive_interval: Duration,
    mut sender: IceSender,
    device: Arc<AsyncDevice>,
    mut stop: watch::Receiver<bool>,
) {
    let mut buf = [0u8; 2000];
    let mut keepalive_ticker = delayed_interval(jitter(keepalive_interval, 0.1));
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
    mut receiver: IceReceiver,
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
                if packet.is_empty() {
                    warn!("Received empty packet from node {node_id:X}");
                    continue;
                }
                if packet.len() == 1 {
                    // Keepalive packet, ignore
                    debug!("Received keepalive packet from node {node_id:X}");
                    continue;
                }
                if packet.len() > 2000 {
                    warn!("Dropping oversized packet from node {node_id:X}: {} bytes", packet.len());
                    continue;
                }
                if let Err(err) = device.send(&packet).await {
                    error!("Failed to write to TUN device for node {node_id:X}: {err}");
                }
            }
        }
    }
}
