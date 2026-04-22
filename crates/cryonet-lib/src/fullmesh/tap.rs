use std::{
    collections::HashMap,
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    sync::Arc,
    time::Instant,
};

use anyhow::Result;
use async_trait::async_trait;
use bytes::{BufMut, BytesMut};
use pnet_packet::{
    MutablePacket, Packet,
    arp::{ArpHardwareTypes, ArpOperations, ArpPacket, MutableArpPacket},
    ethernet::{EtherType, EtherTypes, EthernetPacket, MutableEthernetPacket},
    icmpv6::{
        Icmpv6Packet, Icmpv6Types,
        ndp::{MutableNeighborAdvertPacket, NdpOption, NdpOptionTypes, NeighborSolicitPacket},
    },
    ip::IpNextHeaderProtocols,
    ipv6::{Ipv6Packet, MutableIpv6Packet},
    util::ipv6_checksum,
};
use sactor::sactor;
use tokio::sync::{Mutex, mpsc, watch};
use tracing::{error, warn};
use tun_rs::{AsyncDevice, DeviceBuilder, Layer};

use crate::{
    errors::CryonetError,
    fullmesh::{ConnectionReceiver, ConnectionSender, DeviceManager},
    mesh::packet::NodeId,
};

pub struct TapManager {
    handle: TapManagerInnerHandle,
}

impl TapManager {
    pub fn new(
        node_id: NodeId,
        tap_mac_prefix: u16,
        enable_packet_information: bool,
        ips: Arc<Mutex<HashMap<IpAddr, (NodeId, Instant)>>>,
    ) -> Result<TapManager> {
        Ok(TapManager {
            handle: TapManagerInner::new(node_id, tap_mac_prefix, enable_packet_information, ips)?,
        })
    }
}

#[async_trait]
impl DeviceManager for TapManager {
    async fn connected(
        &mut self,
        node_id: NodeId,
        sender: Box<dyn ConnectionSender>,
        receiver: Box<dyn ConnectionReceiver>,
    ) -> Result<()> {
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
    enable_packet_information: bool,

    device: Arc<AsyncDevice>,
    tap_mac: [u8; 6],
    send_msg_tx: mpsc::UnboundedSender<SendLoopMessage>,
    recv_tasks: HashMap<NodeId, watch::Sender<bool>>,
}

#[sactor]
impl TapManagerInner {
    fn new(
        node_id: NodeId,
        tap_mac_prefix: u16,
        enable_packet_information: bool,
        ips: Arc<Mutex<HashMap<IpAddr, (NodeId, Instant)>>>,
    ) -> Result<TapManagerInnerHandle> {
        let mut tap_mac = tap_mac_prefix.to_be_bytes().to_vec();
        // Set locally administered bit and ensure unicast
        tap_mac[0] = (tap_mac[0] & 0xfe) | 0x02;
        tap_mac.extend_from_slice(&node_id.to_be_bytes());
        let tap_mac: [u8; 6] = tap_mac.try_into().unwrap();

        let device = Arc::new(
            DeviceBuilder::new()
                .mtu(1280)
                .layer(Layer::L2)
                .mac_addr(tap_mac)
                .enable(true)
                .build_async()?,
        );
        let device2 = device.clone();

        let (send_msg_tx, send_msg_rx) = mpsc::unbounded_channel();
        let (future, handle) = TapManagerInner::run(move |_| TapManagerInner {
            device,
            tap_mac,
            enable_packet_information,
            send_msg_tx,
            recv_tasks: HashMap::new(),
        });
        tokio::task::spawn_local(future);
        tokio::spawn(send_loop(
            enable_packet_information,
            ips,
            device2,
            tap_mac,
            send_msg_rx,
        ));
        Ok(handle)
    }

    async fn connected(
        &mut self,
        node_id: NodeId,
        sender: Box<dyn ConnectionSender>,
        receiver: Box<dyn ConnectionReceiver>,
    ) -> Result<()> {
        self.send_msg_tx
            .send(SendLoopMessage::Connected(node_id, sender))?;
        let stop_tx = watch::channel(false).0;
        tokio::spawn(recv_loop(
            self.enable_packet_information,
            node_id,
            receiver,
            self.device.clone(),
            self.tap_mac,
            stop_tx.subscribe(),
        ));
        self.recv_tasks.insert(node_id, stop_tx);
        Ok(())
    }

    async fn disconnected(&mut self, node_id: NodeId) -> Result<()> {
        self.send_msg_tx
            .send(SendLoopMessage::Disconnected(node_id))?;
        if let Some(stop_tx) = self.recv_tasks.remove(&node_id) {
            let _ = stop_tx.send(true);
        }
        Ok(())
    }

    fn ips(&self) -> Result<Vec<IpAddr>> {
        Ok(self.device.addresses()?)
    }
}

impl Drop for TapManagerInner {
    fn drop(&mut self) {
        for stop_tx in self.recv_tasks.values() {
            let _ = stop_tx.send(true);
        }
        let _ = self.send_msg_tx.send(SendLoopMessage::Stop);
    }
}

enum SendLoopMessage {
    Stop,
    Connected(NodeId, Box<dyn ConnectionSender>),
    Disconnected(NodeId),
}

async fn send_loop(
    enable_packet_information: bool,
    ips: Arc<Mutex<HashMap<IpAddr, (NodeId, Instant)>>>,
    device: Arc<AsyncDevice>,
    device_mac: [u8; 6],
    mut msg_rx: mpsc::UnboundedReceiver<SendLoopMessage>,
) {
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
                    let ndp = is_ndp(&packet, NsNa::Ns);
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
                        let src_ip = arp.get_target_proto_addr();
                        let src_mac: [u8; 6] = {
                            let ips = ips.lock().await;
                            let Some((node_id, _)) = ips.get(&IpAddr::V4(src_ip)) else {
                                warn!("Received ARP packet for unknown IP address: {}", src_ip);
                                continue;
                            };
                            let mut src_mac = vec![device_mac[0], device_mac[1]];
                            src_mac.extend_from_slice(&node_id.to_be_bytes());
                            src_mac.try_into().unwrap()
                        };
                        let dst_ip = arp.get_sender_proto_addr();
                        let dst_mac = device_mac;
                        // send ARP reply
                        if let Err(err) = send_arp_reply(&device, src_mac, dst_mac, src_ip, dst_ip).await {
                            error!("Failed to send ARP reply for IP {}: {}", src_ip, err);
                        }
                    }
                    if ndp {
                        // we checked packets in is_ndp_ns
                        let ipv6 = Ipv6Packet::new(packet.payload()).unwrap();
                        let Some(ns) = NeighborSolicitPacket::new(ipv6.payload()) else {
                            warn!("Failed to parse Neighbor Solicitation packet");
                            continue;
                        };
                        let src_ip = ns.get_target_addr();
                        let src_mac: [u8; 6] = {
                            let ips = ips.lock().await;
                            let Some((node_id, _)) = ips.get(&IpAddr::V6(src_ip)) else {
                                warn!("Received NDP Neighbor Solicitation packet for unknown IP address: {}", src_ip);
                                continue;
                            };
                            let mut src_mac = vec![device_mac[0], device_mac[1]];
                            src_mac.extend_from_slice(&node_id.to_be_bytes());
                            src_mac.try_into().unwrap()
                        };
                        let dst_ip = ipv6.get_source();
                        let dst_mac = device_mac;
                        // send na
                        if let Err(err) = send_ndp_na(&device, src_mac, dst_mac, src_ip, dst_ip).await {
                            error!("Failed to send NDP Neighbor Advertisement for IP {}: {}", src_ip, err);
                        }
                    }
                    continue;
                }
                if !(dst_mac.0 == device_mac[0] && dst_mac.1 == device_mac[1]) {
                    warn!("Received Ethernet packet with unexpected destination MAC: {}", dst_mac);
                    continue;
                }
                // unicast
                if packet.get_ethertype() == EtherTypes::Arp || is_ndp(&packet, NsNa::Na) {
                    // drop ARP and NDP responses
                    continue;
                }
                // TODO: drop packets other than IPv4, IPv6 and MPLS?
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
}

async fn recv_loop(
    enable_packet_information: bool,
    peer_id: NodeId,
    mut receiver: Box<dyn ConnectionReceiver>,
    device: Arc<AsyncDevice>,
    device_mac: [u8; 6],
    mut stop: watch::Receiver<bool>,
) {
    let peer_mac: [u8; 6] = {
        let mut prefix = vec![device_mac[0], device_mac[1]];
        prefix.extend_from_slice(&peer_id.to_be_bytes());
        prefix.try_into().unwrap()
    };
    let mut buf = [0u8; 2000];
    loop {
        tokio::select! {
            _ = stop.changed() => break,
            res = receiver.recv() => {
                let (packet, _) = match res {
                    Ok(p) => p,
                    Err(err) => match err.downcast_ref::<CryonetError>() {
                        Some(CryonetError::ChannelClosed) => break,
                        _ => {
                            error!("Failed to receive from node {:X}: {}", peer_id, err);
                            continue;
                        }
                    }
                };
                let (payload, ethertype, len) = if enable_packet_information {
                    if packet.len() < 4 {
                        warn!("Received packet with insufficient length for packet information from node {:X}", peer_id);
                        continue;
                    }
                    let ethertype = EtherType::new(u16::from_be_bytes([packet[2], packet[3]]));
                    (&packet[4..], ethertype, packet.len() - 4)
                } else {
                    // assume IPv4 or IPv6
                    let ethertype = if packet[0] >> 4 == 4 { EtherTypes::Ipv4 } else { EtherTypes::Ipv6 };
                    (&packet[..], ethertype, packet.len())
                };
                let mut ethernet = MutableEthernetPacket::new(&mut buf).unwrap();
                ethernet.set_ethertype(ethertype);
                ethernet.set_source(peer_mac.into());
                ethernet.set_destination(device_mac.into());
                ethernet.set_payload(payload);
                // 14 is the size of the Ethernet header
                if let Err(err) = device.send(&buf[..(len + 14)]).await {
                    error!("Failed to write to TAP device for node {:X}: {}", peer_id, err);
                }
            }
        }
    }
}

enum NsNa {
    Ns,
    Na,
}

fn is_ndp<'a>(packet: &'a EthernetPacket<'a>, n: NsNa) -> bool {
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
    match n {
        NsNa::Ns => icmpv6.get_icmpv6_type() == Icmpv6Types::NeighborSolicit,
        NsNa::Na => icmpv6.get_icmpv6_type() == Icmpv6Types::NeighborAdvert,
    }
}

async fn send_arp_reply(
    device: &AsyncDevice,
    src_mac: [u8; 6],
    dst_mac: [u8; 6],
    src_ip: Ipv4Addr,
    dst_ip: Ipv4Addr,
) -> Result<()> {
    const TOTAL_LEN: usize =
        MutableEthernetPacket::minimum_packet_size() + MutableArpPacket::minimum_packet_size();

    let mut buf = [0u8; 2000];

    let mut ethernet = MutableEthernetPacket::new(&mut buf).unwrap();
    ethernet.set_destination(dst_mac.into());
    ethernet.set_source(src_mac.into());
    ethernet.set_ethertype(EtherTypes::Arp);

    let mut arp = MutableArpPacket::new(ethernet.payload_mut()).unwrap();
    arp.set_hardware_type(ArpHardwareTypes::Ethernet);
    arp.set_protocol_type(EtherTypes::Ipv4);
    arp.set_hw_addr_len(6);
    arp.set_proto_addr_len(4);
    arp.set_operation(ArpOperations::Reply);
    arp.set_sender_hw_addr(src_mac.into());
    arp.set_sender_proto_addr(src_ip);
    arp.set_target_hw_addr(dst_mac.into());
    arp.set_target_proto_addr(dst_ip);

    device.send(&buf[..TOTAL_LEN]).await?;
    Ok(())
}

async fn send_ndp_na(
    device: &AsyncDevice,
    src_mac: [u8; 6],
    dst_mac: [u8; 6],
    src_ip: Ipv6Addr,
    dst_ip: Ipv6Addr,
) -> Result<()> {
    const TLL_OPTION_LEN: usize = 8; // type(1) + length(1) + MAC(6)
    const ICMPV6_LEN: usize = MutableNeighborAdvertPacket::minimum_packet_size() + TLL_OPTION_LEN;
    const ETHERNET_HEADER_LEN: usize = MutableEthernetPacket::minimum_packet_size();
    const IPV6_HEADER_LEN: usize = MutableIpv6Packet::minimum_packet_size();
    const TOTAL_LEN: usize = ETHERNET_HEADER_LEN + IPV6_HEADER_LEN + ICMPV6_LEN;

    let mut buf = [0u8; 2000];

    let mut ethernet = MutableEthernetPacket::new(&mut buf).unwrap();
    ethernet.set_destination(dst_mac.into());
    ethernet.set_source(src_mac.into());
    ethernet.set_ethertype(EtherTypes::Ipv6);

    let mut ipv6 = MutableIpv6Packet::new(ethernet.payload_mut()).unwrap();
    ipv6.set_version(6);
    ipv6.set_payload_length(ICMPV6_LEN as u16);
    ipv6.set_next_header(IpNextHeaderProtocols::Icmpv6);
    ipv6.set_hop_limit(255);
    ipv6.set_source(src_ip);
    ipv6.set_destination(dst_ip);

    let mut na = MutableNeighborAdvertPacket::new(ipv6.payload_mut()).unwrap();
    na.set_icmpv6_type(Icmpv6Types::NeighborAdvert);
    na.set_icmpv6_code(pnet_packet::icmpv6::Icmpv6Code(0));
    na.set_flags(0x60); // solicited + override
    na.set_target_addr(src_ip);
    na.set_options(&[NdpOption {
        option_type: NdpOptionTypes::TargetLLAddr,
        length: 1,
        data: src_mac.to_vec(),
    }]);
    na.set_checksum(ipv6_checksum(
        na.packet(),
        1,
        &[],
        &src_ip,
        &dst_ip,
        IpNextHeaderProtocols::Icmpv6,
    ));

    device.send(&buf[..TOTAL_LEN]).await?;
    Ok(())
}
