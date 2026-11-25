use std::{
    collections::HashMap,
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    sync::Arc,
    time::Duration,
};

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use bytes::{BufMut, Bytes, BytesMut};
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
use tokio::sync::{Mutex, mpsc, watch};
use tracing::{debug, error, warn};
use tun_rs::{AsyncDevice, DeviceBuilder, Layer};

use crate::{
    errors::CryonetError,
    fullmesh::{
        DeviceManager, IpMap,
        ice::{IceReceiver, IceSender},
    },
    mesh::packet::NodeId,
    utils::{delayed_interval, enable_mpls_input, jitter, random_mac},
};

pub(crate) struct TapManager {
    enable_packet_information: bool,

    device: Arc<AsyncDevice>,
    tap_mac: [u8; 6],
    macs: HashMap<NodeId, [u8; 6]>,
    #[cfg(target_os = "linux")]
    if_index: Option<u32>,
    send_msg_tx: mpsc::UnboundedSender<SendLoopMessage>,
    recv_tasks: HashMap<NodeId, watch::Sender<bool>>,
}

impl TapManager {
    pub(crate) fn new(
        tap_interface_name: String,
        enable_packet_information: bool,
        enable_mpls: bool,
        ips: Arc<Mutex<IpMap>>,
        ipv6: Option<Ipv6Addr>,
    ) -> Result<TapManager> {
        let tap_mac = random_mac();
        let mut builder = DeviceBuilder::new()
            .mtu(1280)
            .layer(Layer::L2)
            .name(&tap_interface_name)
            .mac_addr(tap_mac)
            .enable(true);
        if let Some(ipv6) = ipv6 {
            builder = builder.ipv6(ipv6, 64u8);
        }
        let device = Arc::new(builder.build_async()?);
        if enable_mpls {
            enable_mpls_input(&device.name()?)?;
        }
        let tap_mac = device.mac_address()?;
        Self::new_with_device(tap_mac, enable_packet_information, ips, device)
    }

    fn new_with_device(
        tap_mac: [u8; 6],
        enable_packet_information: bool,
        ips: Arc<Mutex<IpMap>>,
        device: Arc<AsyncDevice>,
    ) -> Result<TapManager> {
        #[cfg(target_os = "linux")]
        let if_index = device.name().ok().and_then(|name| {
            let name = std::ffi::CString::new(name).ok()?;
            let index = unsafe { libc::if_nametoindex(name.as_ptr()) };
            (index != 0).then_some(index)
        });

        let (send_msg_tx, send_msg_rx) = mpsc::unbounded_channel();
        let context = TapContext {
            enable_packet_information,
            tap_mac,
            ips,
            device: device.clone(),
            #[cfg(target_os = "linux")]
            if_index,
        };
        tokio::spawn(context.run(send_msg_rx, Duration::from_secs(3)));
        Ok(TapManager {
            device,
            tap_mac,
            macs: HashMap::new(),
            #[cfg(target_os = "linux")]
            if_index,
            enable_packet_information,
            send_msg_tx,
            recv_tasks: HashMap::new(),
        })
    }

    fn mac(&mut self, node_id: NodeId) -> [u8; 6] {
        if let Some(mac) = self.macs.get(&node_id) {
            return *mac;
        }
        let mac = loop {
            let mac = random_mac();
            if mac != self.tap_mac && self.macs.values().all(|used| *used != mac) {
                break mac;
            }
        };
        self.macs.insert(node_id, mac);
        mac
    }
}

#[async_trait(?Send)]
impl DeviceManager for TapManager {
    async fn connected(
        &mut self,
        node_id: NodeId,
        sender: IceSender,
        receiver: IceReceiver,
    ) -> Result<()> {
        let peer_mac = self.mac(node_id);
        self.send_msg_tx
            .send(SendLoopMessage::Connected(
                node_id,
                peer_mac,
                Box::new(sender),
            ))
            .map_err(|_| anyhow!("Failed to send Connected"))?;
        let stop_tx = watch::channel(false).0;
        let future = recv_loop(
            self.enable_packet_information,
            node_id,
            receiver,
            self.device.clone(),
            self.tap_mac,
            peer_mac,
            stop_tx.subscribe(),
        );
        tokio::spawn(future);
        self.recv_tasks.insert(node_id, stop_tx);
        Ok(())
    }

    async fn disconnected(&mut self, node_id: NodeId) -> Result<()> {
        self.send_msg_tx
            .send(SendLoopMessage::Disconnected(node_id))
            .map_err(|_| anyhow!("Failed to send Disconnected"))?;
        self.macs.remove(&node_id);
        if let Some(stop_tx) = self.recv_tasks.remove(&node_id) {
            let _ = stop_tx.send(true);
        }
        Ok(())
    }

    async fn ips(&self) -> Result<Vec<IpAddr>> {
        Ok(self.device.addresses()?)
    }

    fn interface(&self, _node_id: NodeId) -> Option<String> {
        self.device.name().ok()
    }

    async fn neighbor_changed(&mut self, _ip: IpAddr) -> Result<()> {
        #[cfg(target_os = "linux")]
        if let Some(if_index) = self.if_index {
            delete_neighbor(if_index, _ip);
        }
        Ok(())
    }
}

impl Drop for TapManager {
    fn drop(&mut self) {
        for stop_tx in self.recv_tasks.values() {
            let _ = stop_tx.send(true);
        }
        let _ = self.send_msg_tx.send(SendLoopMessage::Stop);
    }
}

enum SendLoopMessage {
    Stop,
    Connected(NodeId, [u8; 6], Box<IceSender>),
    Disconnected(NodeId),
}

#[cfg(target_os = "linux")]
fn delete_neighbor(if_index: u32, ip: IpAddr) {
    use netlink_packet_core::{NLM_F_REQUEST, NetlinkHeader, NetlinkMessage, NetlinkPayload};
    use netlink_packet_route::{
        AddressFamily, RouteNetlinkMessage,
        neighbour::{NeighbourAddress, NeighbourAttribute, NeighbourMessage},
    };
    use netlink_sys::{Socket, SocketAddr, protocols::NETLINK_ROUTE};

    let mut neighbour = NeighbourMessage::default();
    neighbour.header.family = match ip {
        IpAddr::V4(_) => AddressFamily::Inet,
        IpAddr::V6(_) => AddressFamily::Inet6,
    };
    neighbour.header.ifindex = if_index;
    neighbour
        .attributes
        .push(NeighbourAttribute::Destination(NeighbourAddress::from(ip)));

    let mut message = NetlinkMessage::new(
        NetlinkHeader::default(),
        NetlinkPayload::InnerMessage(RouteNetlinkMessage::DelNeighbour(neighbour)),
    );
    message.header.flags = NLM_F_REQUEST;
    message.header.sequence_number = 1;
    message.finalize();

    let mut buf = vec![0; message.buffer_len()];
    message.serialize(&mut buf);
    if let Ok(socket) = Socket::new(NETLINK_ROUTE) {
        let _ = socket.send_to(&buf, &SocketAddr::new(0, 0), 0);
    }
}

struct TapContext {
    enable_packet_information: bool,
    tap_mac: [u8; 6],
    ips: Arc<Mutex<IpMap>>,
    device: Arc<AsyncDevice>,
    #[cfg(target_os = "linux")]
    if_index: Option<u32>,
}

impl TapContext {
    async fn run(
        self,
        mut msg_rx: mpsc::UnboundedReceiver<SendLoopMessage>,
        keepalive_interval: Duration,
    ) {
        let mut peers = Peers::default();
        let mut buf = [0u8; 2000];
        let mut keepalive_ticker = delayed_interval(jitter(keepalive_interval, 0.1));
        // We use a single byte as keepalive packet, which is guaranteed to be smaller than any valid packet.
        let keepalive = Bytes::from_static(&[0u8; 1]);
        loop {
            tokio::select! {
                msg = msg_rx.recv() => match msg {
                    None | Some(SendLoopMessage::Stop) => break,
                    Some(SendLoopMessage::Connected(node_id, mac, sender)) => {
                        peers.connect(node_id, mac, sender);
                    }
                    Some(SendLoopMessage::Disconnected(node_id)) => {
                        self.disconnect(&mut peers, node_id).await;
                    }
                },
                // Moving the keepalive logic to otherside is too annoying, so just send keepalive from here.
                _ = keepalive_ticker.tick() => peers.keepalive(keepalive.clone()).await,
                res = self.device.recv(&mut buf) => match res {
                    Ok(size) => self.handle_frame(&mut peers, &buf[..size]).await,
                    Err(err) => error!("Failed to read from TAP device: {err}"),
                },
            }
        }
    }

    async fn disconnect(&self, peers: &mut Peers, node_id: NodeId) {
        peers.remove(node_id);
        #[cfg(target_os = "linux")]
        if let Some(if_index) = self.if_index {
            let peer_ips: Vec<IpAddr> = {
                let ip_map = self.ips.lock().await;
                ip_map
                    .iter()
                    .filter(|(_, entry)| entry.node == node_id)
                    .map(|(ip, _)| *ip)
                    .collect()
            };
            for ip in peer_ips {
                delete_neighbor(if_index, ip);
            }
        }
    }

    async fn handle_frame(&self, peers: &mut Peers, frame: &[u8]) {
        let Some(packet) = EthernetPacket::new(frame) else {
            error!("Failed to parse Ethernet packet from TAP device");
            return;
        };
        if packet.get_source() != self.tap_mac {
            warn!(
                "Received Ethernet packet with unexpected source MAC: {}",
                packet.get_source()
            );
            return;
        }
        let dst_mac = packet.get_destination();
        if dst_mac.0 & 1 == 1 {
            self.handle_multicast(peers, &packet).await;
        } else {
            self.forward_unicast(peers, &packet).await;
        }
    }

    async fn handle_multicast(&self, peers: &mut Peers, packet: &EthernetPacket<'_>) {
        let arp = packet.get_ethertype() == EtherTypes::Arp;
        let ndp = is_ndp(packet, NsNa::Ns);
        if !(arp || ndp) {
            // broadcast to all nodes
            let bytes = self.encode(packet);
            for (node_id, sender) in &mut peers.senders {
                if let Err(err) = sender.send(bytes.clone()).await {
                    error!("Failed to send packet to node {node_id:X}: {err}");
                }
            }
            return;
        }
        // anwser ARP and NDP
        if arp {
            self.answer_arp(peers, packet).await;
        }
        if ndp {
            self.answer_ndp(peers, packet).await;
        }
    }

    fn encode(&self, packet: &EthernetPacket<'_>) -> Bytes {
        let payload = packet.payload();
        let mut bytes = BytesMut::with_capacity(payload.len() + 4);
        if self.enable_packet_information {
            bytes.put_u16(0); // flags
            bytes.put_u16(packet.get_ethertype().0);
        }
        bytes.extend_from_slice(payload);
        bytes.freeze()
    }

    async fn answer_arp(&self, peers: &Peers, packet: &EthernetPacket<'_>) {
        let Some(arp) = ArpPacket::new(packet.payload()) else {
            warn!("Failed to parse ARP packet");
            return;
        };
        let src_ip = arp.get_target_proto_addr();
        let node_id = {
            let ips = self.ips.lock().await;
            ips.get(&IpAddr::V4(src_ip)).map(|entry| entry.node)
        };
        let Some(node_id) = node_id else {
            warn!("Received ARP packet for unknown IP address: {src_ip}");
            return;
        };
        let Some(src_mac) = peers.by_node.get(&node_id).copied() else {
            debug!("No address assigned for node {node_id:X}");
            return;
        };
        let dst_ip = arp.get_sender_proto_addr();
        let dst_mac = self.tap_mac;
        if let Err(err) = send_arp_reply(&self.device, src_mac, dst_mac, src_ip, dst_ip).await {
            error!("Failed to send ARP reply for IP {src_ip}: {err}");
        }
    }

    async fn answer_ndp(&self, peers: &Peers, packet: &EthernetPacket<'_>) {
        // we checked packets in is_ndp_ns
        let ipv6 = Ipv6Packet::new(packet.payload()).unwrap();
        let Some(ns) = NeighborSolicitPacket::new(ipv6.payload()) else {
            warn!("Failed to parse Neighbor Solicitation packet");
            return;
        };
        let src_ip = ns.get_target_addr();
        let node_id = {
            let ips = self.ips.lock().await;
            ips.get(&IpAddr::V6(src_ip)).map(|entry| entry.node)
        };
        let Some(node_id) = node_id else {
            warn!("Received NDP Neighbor Solicitation packet for unknown IP address: {src_ip}");
            return;
        };
        let Some(src_mac) = peers.by_node.get(&node_id).copied() else {
            debug!("No address assigned for node {node_id:X}");
            return;
        };
        let dst_ip = ipv6.get_source();
        let dst_mac = self.tap_mac;
        if let Err(err) = send_ndp_na(&self.device, src_mac, dst_mac, src_ip, dst_ip).await {
            error!("Failed to send NDP Neighbor Advertisement for IP {src_ip}: {err}");
        }
    }

    async fn forward_unicast(&self, peers: &mut Peers, packet: &EthernetPacket<'_>) {
        let dst_mac = packet.get_destination();
        if packet.get_ethertype() == EtherTypes::Arp || is_ndp(packet, NsNa::Na) {
            // drop ARP and NDP responses
            return;
        }
        // TODO: drop packets other than IPv4, IPv6 and MPLS?
        let Some(node_id) = peers.by_mac.get(&dst_mac.octets()).copied() else {
            warn!("Received Ethernet packet with unexpected destination MAC: {dst_mac}");
            return;
        };
        let Some(sender) = peers.senders.get_mut(&node_id) else {
            warn!("Received Ethernet packet for unknown node ID {node_id:X}");
            return;
        };
        let bytes = self.encode(packet);
        if let Err(err) = sender.send(bytes).await {
            error!("Failed to send packet to node {node_id:X}: {err}");
        }
    }
}

#[derive(Default)]
struct Peers {
    senders: HashMap<NodeId, Box<IceSender>>,
    by_node: HashMap<NodeId, [u8; 6]>,
    by_mac: HashMap<[u8; 6], NodeId>,
}

impl Peers {
    fn connect(&mut self, node_id: NodeId, mac: [u8; 6], sender: Box<IceSender>) {
        self.by_node.insert(node_id, mac);
        self.by_mac.insert(mac, node_id);
        self.senders.insert(node_id, sender);
    }

    fn remove(&mut self, node_id: NodeId) {
        if let Some(mac) = self.by_node.remove(&node_id) {
            self.by_mac.remove(&mac);
        }
        self.senders.remove(&node_id);
    }

    async fn keepalive(&mut self, keepalive: Bytes) {
        for (node_id, sender) in &mut self.senders {
            if let Err(err) = sender.send(keepalive.clone()).await {
                error!("Failed to send keepalive to node {node_id:X}: {err}");
            }
        }
    }
}

async fn recv_loop(
    enable_packet_information: bool,
    peer_id: NodeId,
    mut receiver: IceReceiver,
    device: Arc<AsyncDevice>,
    device_mac: [u8; 6],
    peer_mac: [u8; 6],
    mut stop: watch::Receiver<bool>,
) {
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
                            error!("Failed to receive from node {peer_id:X}: {err}");
                            continue;
                        }
                    }
                };
                if packet.is_empty() {
                    warn!("Received empty packet from node {peer_id:X}");
                    continue;
                }
                if packet.len() == 1 {
                    // Keepalive packet, ignore
                    debug!("Received keepalive packet from node {peer_id:X}");
                    continue;
                }
                let (payload, ethertype, len) = if enable_packet_information {
                    if packet.len() < 4 {
                        warn!("Received packet with insufficient length for packet information from node {peer_id:X}");
                        continue;
                    }
                    let ethertype = EtherType::new(u16::from_be_bytes([packet[2], packet[3]]));
                    (&packet[4..], ethertype, packet.len() - 4)
                } else {
                    // assume IPv4 or IPv6
                    let ethertype = if packet[0] >> 4 == 4 { EtherTypes::Ipv4 } else { EtherTypes::Ipv6 };
                    (&packet[..], ethertype, packet.len())
                };
                let frame_len = len + 14;
                if frame_len > buf.len() {
                    warn!(
                        "Dropping oversized packet from node {peer_id:X}: {len} bytes of payload"
                    );
                    continue;
                }
                let mut ethernet = MutableEthernetPacket::new(&mut buf).unwrap();
                ethernet.set_ethertype(ethertype);
                ethernet.set_source(peer_mac.into());
                ethernet.set_destination(device_mac.into());
                ethernet.set_payload(payload);
                if let Err(err) = device.send(&buf[..frame_len]).await {
                    error!("Failed to write to TAP device for node {peer_id:X}: {err}");
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
