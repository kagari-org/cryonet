use thiserror::Error;

use crate::mesh::packet::NodeId;

#[derive(Debug, Error)]
pub enum Error {
    #[error("No link to node {0:X}")]
    NoSuchLink(NodeId),
    #[error("Route to node {0:X} is unreachable")]
    Unreachable(NodeId),
    #[error("Unknown error")]
    Unknown,
}
