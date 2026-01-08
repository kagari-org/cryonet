use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::{packet::{NodeId, Payload}, seq::{Seq, SeqMetric}};

#[typetag::serde]
impl Payload for IGPPayload {}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) enum IGPPayload {
    Hello { seq: Seq },
    HelloReply { seq: Seq },
    RouteRequest { dst: NodeId }, // only used when route is about to expire
    SequenceRequest {
        seq: Seq,
        dst: NodeId,
        ttl: u16,
    },
    Update {
        metric: SeqMetric,
        dst: NodeId,
        timeout: Duration,
    },
}
