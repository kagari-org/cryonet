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
pub struct Conn {
    pub state: ConnState,
    pub selected_candidate: Option<String>,
    pub sent: u64,
    pub received: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum CryonetUapi {
    GetLinks,
    GetLinksResponse(Vec<NodeId>),
    GetRoutes,
    GetRoutesResponse(HashMap<NodeId, NodeId>),
    GetIgpRoutes,
    GetIgpRoutesResponse(Vec<IgpRoute>),
    GetFullMeshPeers,
    GetFullMeshPeersResponse(HashMap<NodeId, Conn>),
    Ping(NodeId),
    Pong,
}
