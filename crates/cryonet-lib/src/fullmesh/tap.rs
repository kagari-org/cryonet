use std::{collections::HashMap, net::IpAddr, sync::Arc, time::Instant};

use anyhow::Result;
use async_trait::async_trait;
use bytes::{BufMut, BytesMut};
use pnet_packet::{
    Packet,
    arp::ArpPacket,
    ethernet::{EtherTypes, EthernetPacket},
    icmpv6::{
        Icmpv6Packet, Icmpv6Types,
        ndp::{NeighborAdvertPacket, NeighborSolicitPacket},
    },
    ip::IpNextHeaderProtocols,
    ipv6::Ipv6Packet,
};
use sactor::sactor;
use tokio::sync::{Mutex, mpsc, watch};
use tracing::{error, warn};
use tun_rs::{AsyncDevice, DeviceBuilder, Layer};

use crate::{
    fullmesh::{
        DeviceManager,
        conn::{ConnectionReceiver, ConnectionSender},
    },
    mesh::{MeshHandle, packet::NodeId},
};

pub struct TapManager {
    handle: TapManagerInnerHandle,
}

impl TapManager {
    pub fn new(mesh: MeshHandle, tap_mac_prefix: u16) -> Result<TapManager> {
        Ok(TapManager {
            handle: TapManagerInner::new(mesh, tap_mac_prefix)?,
        })
    }
}

#[async_trait]
impl DeviceManager for TapManager {
    async fn connected(&mut self, node_id: NodeId, sender: ConnectionSender, receiver: ConnectionReceiver) -> Result<()> {
        self.handle.connected(node_id, sender, receiver).await?
    }

    async fn disconnected(&mut self, node_id: NodeId) -> Result<()> {
        self.handle.disconnected(node_id).await?
    }

    async fn ips(&self) -> Result<Vec<IpAddr>> {
        self.handle.ips().await?
    }
}

struct TapManagerInner {
    mesh: MeshHandle,
    device: Arc<AsyncDevice>,
    tasks: HashMap<NodeId, watch::Sender<bool>>,
    tap_mac_prefix: [u8; 2],
}

#[sactor]
impl TapManagerInner {
    fn new(mesh: MeshHandle, tap_mac_prefix: u16) -> Result<TapManagerInnerHandle> {
        let mut tap_mac_prefix = tap_mac_prefix.to_be_bytes();
        // Set locally administered bit and ensure unicast
        tap_mac_prefix[0] = (tap_mac_prefix[0] & 0xfe) | 0x02;
        let device = Arc::new(DeviceBuilder::new().mtu(1280).layer(Layer::L2).enable(true).build_async()?);
        let (future, handle) = TapManagerInner::run(move |_| TapManagerInner {
            mesh,
            device,
            tasks: HashMap::new(),
            tap_mac_prefix,
        });
        tokio::task::spawn_local(future);
        Ok(handle)
    }

    fn connected(&mut self, node_id: NodeId, sender: ConnectionSender, receiver: ConnectionReceiver) -> Result<()> {
        Ok(())
    }

    fn disconnected(&mut self, node_id: NodeId) -> Result<()> {
        Ok(())
    }

    fn ips(&self) -> Result<Vec<IpAddr>> {
        Ok(vec![])
    }
}

enum SendLoopMessage {
    Stop,
    Connected(NodeId, Box<ConnectionSender>),
    Disconnected(NodeId),
}

async fn send_loop(enable_packet_information: bool, ips: Arc<Mutex<HashMap<IpAddr, (NodeId, Instant)>>>, device: Arc<AsyncDevice>, device_mac: [u8; 6], mut msg_rx: mpsc::Receiver<SendLoopMessage>) -> Result<()> {
    let mut senders = HashMap::new();
    let mut buf = [0u8; 2000];
    loop {
        tokio::select! {
            msg = msg_rx.recv() => match msg {
                None | Some(SendLoopMessage::Stop) => break,
                Some(SendLoopMessage::Connected(node_id, sender)) => {
                    senders.insert(node_id, sender);
                }
                Some(SendLoopMessage::Disconnected(node_id)) => {
                    senders.remove(&node_id);
                }
            },
            res = device.recv(&mut buf) => {
                let packet = match res {
                    Ok(size) => match EthernetPacket::new(&buf[..size]) {
                        Some(packet) => packet,
                        None => {
                            error!("Failed to parse Ethernet packet from TAP device");
                            continue;
                        }
                    },
                    Err(err) => {
                        error!("Failed to read from TAP device: {}", err);
                        continue;
                    }
                };
                if packet.get_source() != device_mac {
                    warn!("Received Ethernet packet with unexpected source MAC: {}", packet.get_source());
                    continue;
                }
                let dst_mac = packet.get_destination();
                if dst_mac.0 & 1 == 1 {
                    // multicast
                    let arp = packet.get_ethertype() == EtherTypes::Arp;
                    let ndp = is_ndp_ns(&packet);
                    if !(arp || ndp) {
                        // broadcast to all nodes
                        for (node_id, sender) in &mut senders {
                            let payload = packet.payload();
                            let mut bytes = BytesMut::with_capacity(payload.len() + 4);
                            if enable_packet_information {
                                bytes.put_u16(0); // flags
                                bytes.put_u16(packet.get_ethertype().0);
                            }
                            bytes.extend_from_slice(payload);
                            if let Err(err) = sender.send(bytes.freeze()).await {
                                error!("Failed to send packet to node {:X}: {}", node_id, err);
                            }
                        }
                        continue;
                    }
                    // anwser ARP and NDP
                    if arp {
                        let Some(arp) = ArpPacket::new(packet.payload()) else {
                            warn!("Failed to parse ARP packet");
                            continue;
                        };
                        let address = arp.get_target_proto_addr();
                        let target = {
                            let ips = ips.lock().await;
                            let Some((node_id, _)) = ips.get(&IpAddr::V4(address)) else {
                                warn!("Received ARP packet for unknown IP address: {}", address);
                                continue;
                            };
                            *node_id
                        };
                        // send ARP reply
                        // TODO
                    }
                    if ndp {
                        // we checked packets in is_ndp_ns
                        let ipv6 = Ipv6Packet::new(packet.payload()).unwrap();
                        let icmpv6 = Icmpv6Packet::new(ipv6.payload()).unwrap();
                        let ns = NeighborSolicitPacket::new(icmpv6.payload()).unwrap();
                        let address = ns.get_target_addr();
                        let target = {
                            let ips = ips.lock().await;
                            let Some((node_id, _)) = ips.get(&IpAddr::V6(address)) else {
                                warn!("Received NDP Neighbor Solicitation packet for unknown IP address: {}", address);
                                continue;
                            };
                            *node_id
                        };
                        // send na
                        // TODO
                    }
                    continue;
                }
                if !(dst_mac.0 == device_mac[0] && dst_mac.1 == device_mac[1]) {
                    warn!("Received Ethernet packet with unexpected destination MAC: {}", dst_mac);
                    continue;
                }
                // unicast
                if packet.get_ethertype() == EtherTypes::Arp || is_ndp_na(&packet) {
                    // drop ARP and NDP responses
                    continue;
                }
                let node_id = u32::from_be_bytes([dst_mac.2, dst_mac.3, dst_mac.4, dst_mac.5]);
                let Some(sender) = senders.get_mut(&node_id) else {
                    warn!("Received Ethernet packet for unknown node ID {:X}", node_id);
                    continue;
                };
                let payload = packet.payload();
                let mut bytes = BytesMut::with_capacity(payload.len() + 4);
                if enable_packet_information {
                    bytes.put_u16(0); // flags
                    bytes.put_u16(packet.get_ethertype().0);
                }
                bytes.extend_from_slice(payload);
                if let Err(err) = sender.send(bytes.freeze()).await {
                    error!("Failed to send packet to node {:X}: {}", node_id, err);
                }
            }
        }
    }
    Ok(())
}

async fn recv_loop(node_id: NodeId, receiver: ConnectionReceiver, device: Arc<AsyncDevice>, mut stop: watch::Receiver<bool>) -> Result<()> {
    Ok(())
}

fn is_ndp_ns<'a>(packet: &'a EthernetPacket<'a>) -> bool {
    if packet.get_ethertype() != EtherTypes::Ipv6 {
        return false;
    }
    let Some(ipv6) = Ipv6Packet::new(packet.payload()) else {
        return false;
    };
    if ipv6.get_next_header() != IpNextHeaderProtocols::Icmpv6 {
        return false;
    }
    let Some(icmpv6) = Icmpv6Packet::new(ipv6.payload()) else {
        return false;
    };
    if NeighborSolicitPacket::new(ipv6.payload()).is_none() {
        return false;
    }
    icmpv6.get_icmpv6_type() == Icmpv6Types::NeighborSolicit
}

fn is_ndp_na<'a>(packet: &'a EthernetPacket<'a>) -> bool {
    if packet.get_ethertype() != EtherTypes::Ipv6 {
        return false;
    }
    let Some(ipv6) = Ipv6Packet::new(packet.payload()) else {
        return false;
    };
    if ipv6.get_next_header() != IpNextHeaderProtocols::Icmpv6 {
        return false;
    }
    let Some(icmpv6) = Icmpv6Packet::new(ipv6.payload()) else {
        return false;
    };
    if NeighborAdvertPacket::new(ipv6.payload()).is_none() {
        return false;
    }
    icmpv6.get_icmpv6_type() == Icmpv6Types::NeighborAdvert
}
