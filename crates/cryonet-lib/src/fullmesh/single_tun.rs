use std::{collections::HashMap, net::IpAddr, os::fd::RawFd, sync::Arc, time::Duration};

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use bytes::Bytes;
use pnet_packet::{ipv4::Ipv4Packet, ipv6::Ipv6Packet};
use tokio::sync::{Mutex, mpsc, watch};
use tracing::{debug, error, warn};
use tun_rs::{AsyncDevice, DeviceBuilder};

use crate::{
    errors::CryonetError,
    fullmesh::{ConnectionReceiver, ConnectionSender, DeviceManager},
    mesh::packet::NodeId,
    time::{Instant, interval},
};

pub struct SingleTunManager {
    device: Arc<AsyncDevice>,

    addresses: Option<Vec<IpAddr>>,

    send_msg_tx: mpsc::UnboundedSender<SendLoopMessage>,
    recv_tasks: HashMap<NodeId, watch::Sender<bool>>,
}

impl SingleTunManager {
    pub fn new(
        interface_name: String,
        enable_packet_information: bool,
        ips: Arc<Mutex<HashMap<IpAddr, (NodeId, Instant)>>>,
    ) -> Result<SingleTunManager> {
        let mut builder = DeviceBuilder::new()
            .mtu(1280)
            .name(interface_name)
            .enable(true);
        if enable_packet_information {
            builder = builder.packet_information(true);
        }
        let device = Arc::new(builder.build_async()?);
        Self::new_with_device(device, enable_packet_information, ips, None)
    }

    // Safety: fd must be a valid, open TUN file descriptor.
    #[allow(clippy::missing_safety_doc)]
    pub unsafe fn new_from_fd(
        fd: RawFd,
        enable_packet_information: bool,
        ips: Arc<Mutex<HashMap<IpAddr, (NodeId, Instant)>>>,
        addresses: Vec<IpAddr>,
    ) -> Result<SingleTunManager> {
        let device = Arc::new(unsafe { AsyncDevice::from_fd(fd)? });
        Self::new_with_device(device, enable_packet_information, ips, Some(addresses))
    }

    pub fn new_with_device(
        device: Arc<AsyncDevice>,
        enable_packet_information: bool,
        ips: Arc<Mutex<HashMap<IpAddr, (NodeId, Instant)>>>,
        addresses: Option<Vec<IpAddr>>,
    ) -> Result<SingleTunManager> {
        Self::new_with_parameters(
            device,
            enable_packet_information,
            Duration::from_secs(3),
            ips,
            addresses,
        )
    }

    pub fn new_with_parameters(
        device: Arc<AsyncDevice>,
        enable_packet_information: bool,
        keepalive_interval: Duration,
        ips: Arc<Mutex<HashMap<IpAddr, (NodeId, Instant)>>>,
        addresses: Option<Vec<IpAddr>>,
    ) -> Result<SingleTunManager> {
        let (send_msg_tx, send_msg_rx) = mpsc::unbounded_channel();
        tokio::spawn(send_loop(
            keepalive_interval,
            enable_packet_information,
            ips,
            device.clone(),
            send_msg_rx,
        ));
        Ok(SingleTunManager {
            device,
            addresses,
            send_msg_tx,
            recv_tasks: HashMap::new(),
        })
    }
}

#[async_trait(?Send)]
impl DeviceManager for SingleTunManager {
    async fn connected(
        &mut self,
        node_id: NodeId,
        sender: Box<dyn ConnectionSender>,
        receiver: Box<dyn ConnectionReceiver>,
    ) -> Result<()> {
        if let Some(stop) = self.recv_tasks.remove(&node_id) {
            let _ = stop.send(true);
        }
        self.send_msg_tx
            .send(SendLoopMessage::Connected(node_id, sender))
            .map_err(|_| anyhow!("Failed to send Connected"))?;
        let stop_tx = watch::channel(false).0;
        tokio::spawn(recv_loop(
            node_id,
            receiver,
            self.device.clone(),
            stop_tx.subscribe(),
        ));
        self.recv_tasks.insert(node_id, stop_tx);
        Ok(())
    }

    async fn disconnected(&mut self, node_id: NodeId) -> Result<()> {
        let _ = self
            .send_msg_tx
            .send(SendLoopMessage::Disconnected(node_id));
        if let Some(stop_tx) = self.recv_tasks.remove(&node_id) {
            let _ = stop_tx.send(true);
        }
        Ok(())
    }

    async fn ips(&self) -> Result<Vec<IpAddr>> {
        match &self.addresses {
            Some(addresses) => Ok(addresses.clone()),
            None => Ok(self.device.addresses()?),
        }
    }
}

enum SendLoopMessage {
    Connected(NodeId, Box<dyn ConnectionSender>),
    Disconnected(NodeId),
}

async fn send_loop(
    keepalive_interval: Duration,
    enable_packet_information: bool,
    ips: Arc<Mutex<HashMap<IpAddr, (NodeId, Instant)>>>,
    device: Arc<AsyncDevice>,
    mut msg_rx: mpsc::UnboundedReceiver<SendLoopMessage>,
) {
    let mut senders = HashMap::new();
    let mut buf = [0u8; 2000];
    let mut keepalive_ticker = interval(keepalive_interval);
    // We use a single byte as keepalive packet, which is guaranteed to be smaller than any valid packet.
    let keepalive = Bytes::from_static(&[0u8; 1]);
    loop {
        tokio::select! {
            msg = msg_rx.recv() => match msg {
                None => break,
                Some(SendLoopMessage::Connected(node_id, sender)) => {
                    senders.insert(node_id, sender);
                }
                Some(SendLoopMessage::Disconnected(node_id)) => {
                    senders.remove(&node_id);
                }
            },
            // Moving the keepalive logic to otherside is too annoying, so just send keepalive from here.
            _ = keepalive_ticker.tick() => {
                for (node_id, sender) in &mut senders {
                    if let Err(err) = sender.send(keepalive.clone()).await {
                        error!("Failed to send keepalive to node {node_id:X}: {err}");
                    }
                }
            }
            res = device.recv(&mut buf) => {
                let size = match res {
                    Ok(size) => size,
                    Err(err) => {
                        error!("Failed to read from TUN device: {err}");
                        continue;
                    }
                };
                let packet = if enable_packet_information {
                    if size < 4 {
                        warn!("Received undersized packet with packet information from TUN device");
                        continue;
                    }
                    &buf[4..size]
                } else {
                    &buf[..size]
                };
                // Reads the destination address from a raw IPv4 or IPv6 packet.
                let dst = match packet.first().map(|byte| byte >> 4) {
                    Some(4) => Ipv4Packet::new(packet).map(|packet| IpAddr::V4(packet.get_destination())),
                    Some(6) => Ipv6Packet::new(packet).map(|packet| IpAddr::V6(packet.get_destination())),
                    _ => None,
                };
                let Some(dst) = dst else {
                    warn!("Received packet with unknown IP version from TUN device");
                    continue;
                };
                let node_id = {
                    let ips = ips.lock().await;
                    match ips.get(&dst) {
                        Some((node_id, _)) => *node_id,
                        None => {
                            warn!("Received packet for unknown destination address: {dst}");
                            continue;
                        }
                    }
                };
                let Some(sender) = senders.get_mut(&node_id) else {
                    warn!("Received packet for node {node_id:X} without an active connection");
                    continue;
                };
                let bytes = Bytes::copy_from_slice(&buf[..size]);
                if let Err(err) = sender.send(bytes).await {
                    error!("Failed to send packet to node {node_id:X}: {err}");
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
        tokio::select! {
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
