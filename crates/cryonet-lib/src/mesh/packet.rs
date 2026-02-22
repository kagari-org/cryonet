use std::{any::Any, fmt::Debug};

use dyn_clone::{DynClone, clone_trait_object};
use serde::{Deserialize, Serialize};

pub type NodeId = u32;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Packet {
    pub src: NodeId,
    pub dst: NodeId,
    pub ttl: u8,
    pub payload: Box<dyn Payload>,
}

#[typetag::serde]
pub trait Payload: Debug + Send + Sync + Any + DynClone {}
clone_trait_object!(Payload);
