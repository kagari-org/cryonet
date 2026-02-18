use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub type NodeId = u32;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IgpRoute {
    pub seq: u16,
    pub metric: u32,
    pub computed_metric: u32,
    pub dst: NodeId,
    pub from: NodeId,
    pub selected: bool,
    pub timeout_remaining_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ConnState {
    New,
    Connecting,
    Connected,
    Disconnected,
    Failed,
    Closed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Conn {
    pub selected: bool,
    pub state: ConnState,
    pub selected_candidate: Option<String>,
    pub elapsed_ms: u64,
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
    GetFullMeshPeersResponse(HashMap<NodeId, HashMap<Uuid, Conn>>),
    Ping(NodeId),
    Pong,
}
