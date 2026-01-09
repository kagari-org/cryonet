use thiserror::Error;

use crate::mesh::packet::NodeId;

#[derive(Debug, Error)]
pub(crate) enum Error {
    #[error("No link to node {0:X}")]
    NoSuchLink(NodeId),
    #[error("Route to node {0:X} is unreachable")]
    Unreachable(NodeId),
}
