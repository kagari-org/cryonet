use std::collections::HashMap;

use serde::{Deserialize, Serialize};

#[cfg(target_os = "android")]
uniffi::setup_scaffolding!();

pub type NodeId = u32;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(target_os = "android", derive(uniffi::Record))]
pub struct IgpRoute {
    pub seq: u16,
    pub metric: u32,
    pub computed_metric: u32,
    pub dst: NodeId,
    pub from: NodeId,
    pub selected: bool,
    pub timeout_remaining_ms: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(target_os = "android", derive(uniffi::Enum))]
pub enum ConnState {
    Connecting,
    Connected,
    Closed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(target_os = "android", derive(uniffi::Record))]
pub struct DaemonInfo {
    pub id: NodeId,
    pub version: String,
    pub uptime_ms: u64,
    pub mode: String,
    pub interface: String,
    pub mtu: u32,
    pub listen: String,
    pub ctl_path: String,
    pub encrypt_local_packets: bool,
    pub packet_information: bool,
    pub ice_servers: Vec<String>,
    pub candidate_filter_prefixes: Vec<String>,
    pub local_ips: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(target_os = "android", derive(uniffi::Record))]
pub struct LinkInfo {
    pub node: NodeId,
    pub alive: bool,
    pub last_recv_ms: i64,
    pub idle_timeout_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(target_os = "android", derive(uniffi::Record))]
pub struct NodeInfo {
    pub id: NodeId,
    pub ips: Vec<String>,
    pub connection_types: Vec<String>,
    pub last_seen_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(target_os = "android", derive(uniffi::Record))]
pub struct ServerInfo {
    pub url: String,
    pub node: Option<NodeId>,
    pub connecting: bool,
    pub connected: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(target_os = "android", derive(uniffi::Record))]
pub struct IgpNeighbor {
    pub node: NodeId,
    pub cost: u32,
    pub last_hello_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(target_os = "android", derive(uniffi::Record))]
pub struct IgpSource {
    pub dst: NodeId,
    pub seq: u16,
    pub metric: u32,
    pub expires_in_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(target_os = "android", derive(uniffi::Record))]
pub struct IgpRequest {
    pub dst: NodeId,
    pub seq: u16,
    pub retries: u8,
    pub expires_in_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(target_os = "android", derive(uniffi::Record))]
pub struct IgpStatus {
    pub self_seq: u16,
    pub neighbors: Vec<IgpNeighbor>,
    pub sources: Vec<IgpSource>,
    pub requests: Vec<IgpRequest>,
    pub hello_interval_ms: u64,
    pub dump_interval_ms: u64,
    pub gc_interval_ms: u64,
    pub route_timeout_ms: u64,
    pub source_timeout_ms: u64,
    pub seqno_request_timeout_ms: u64,
    pub diameter: u16,
    pub update_threshold: u32,
    pub dump_batch_size: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(target_os = "android", derive(uniffi::Record))]
pub struct PeerInfo {
    pub node: NodeId,
    pub state: ConnState,
    pub initiator: bool,
    pub peer_session: u64,
    pub conn_gen: u64,
    pub once_connected: bool,
    pub selected_candidate: Option<String>,
    pub sent: u64,
    pub received: u64,
    pub last_recv_ms: i64,
    pub last_rekey_ms: i64,
    pub key_index: Option<bool>,
    pub connection_type: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(target_os = "android", derive(uniffi::Record))]
pub struct PendingPeer {
    pub node: NodeId,
    pub retry_in_ms: i64,
    pub backoff_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(target_os = "android", derive(uniffi::Record))]
pub struct FullMeshStatus {
    pub session: u64,
    pub peers: Vec<PeerInfo>,
    pub pending: Vec<PendingPeer>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(target_os = "android", derive(uniffi::Record))]
pub struct MeshStats {
    pub links: u64,
    pub routes: u64,
    pub received: u64,
    pub sent: u64,
    pub forwarded: u64,
    pub dropped_ttl: u64,
    pub dropped_duplicate: u64,
    pub dropped_unreachable: u64,
    pub dropped_dispatch_full: u64,
    pub reliable_pending: u64,
    pub reliable_retries: u64,
    pub reliable_given_up: u64,
    pub reliable_acked: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(target_os = "android", derive(uniffi::Record))]
pub struct IgpStats {
    pub routes: u64,
    pub selected: u64,
    pub neighbors: u64,
    pub sources: u64,
    pub requests: u64,
    pub hellos_sent: u64,
    pub hellos_received: u64,
    pub updates_sent: u64,
    pub updates_received: u64,
    pub route_requests: u64,
    pub seqno_requests: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(target_os = "android", derive(uniffi::Record))]
pub struct FullMeshStats {
    pub peers: u64,
    pub connected: u64,
    pub connecting: u64,
    pub pending: u64,
    pub sent: u64,
    pub received: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(target_os = "android", derive(uniffi::Record))]
pub struct RegistryStats {
    pub nodes: u64,
    pub ips: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(target_os = "android", derive(uniffi::Record))]
pub struct Stats {
    pub uptime_ms: u64,
    pub mesh: MeshStats,
    pub igp: IgpStats,
    pub fullmesh: FullMeshStats,
    pub registry: RegistryStats,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum CryonetUapi {
    GetInfo,
    GetInfoResponse(DaemonInfo),
    GetStats,
    GetStatsResponse(Stats),
    GetLinks,
    GetLinksResponse(Vec<LinkInfo>),
    GetRoutes,
    GetRoutesResponse(HashMap<NodeId, NodeId>),
    GetIgpRoutes,
    GetIgpRoutesResponse(Vec<IgpRoute>),
    GetIgpStatus,
    GetIgpStatusResponse(IgpStatus),
    GetFullMeshPeers,
    GetFullMeshPeersResponse(FullMeshStatus),
    GetNodes,
    GetNodesResponse(Vec<NodeInfo>),
    GetServers,
    GetServersResponse(Vec<ServerInfo>),
    Ping(NodeId),
    Pong,
    Error(String),
}
