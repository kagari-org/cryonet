use std::{any::Any, fmt::Debug};

use dyn_clone::{DynClone, clone_trait_object};
use serde::{Deserialize, Serialize};

pub type NodeId = u32;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Packet {
    pub src: NodeId,
    pub dst: NodeId,
    pub ttl: u8,
    pub payload: PacketPayload,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PacketPayload {
    Payload(Box<dyn Payload>),
    Reliable { seq: u64, payload: Box<dyn Payload> },
    Ack { seq: u64 },
}

impl PacketPayload {
    pub fn as_payload(&self) -> Option<&dyn Payload> {
        match self {
            PacketPayload::Payload(payload) | PacketPayload::Reliable { payload, .. } => {
                Some(payload.as_ref())
            }
            PacketPayload::Ack { .. } => None,
        }
    }
}

#[typetag::serde]
pub trait Payload: Debug + Send + Sync + Any + DynClone {}
clone_trait_object!(Payload);

#[cfg(test)]
mod tests {
    use super::*;

    #[typetag::serde]
    impl Payload for SerdeTestPayload {}

    #[derive(Debug, Clone, Serialize, Deserialize)]
    struct SerdeTestPayload {
        value: u32,
    }

    fn roundtrip(payload: PacketPayload) -> PacketPayload {
        let packet = Packet {
            src: 1,
            dst: 2,
            ttl: 16,
            payload,
        };
        let json = serde_json::to_string(&packet).unwrap();
        serde_json::from_str::<Packet>(&json).unwrap().payload
    }

    #[test]
    fn packet_payload_roundtrips_through_json() {
        let PacketPayload::Payload(payload) =
            roundtrip(PacketPayload::Payload(Box::new(SerdeTestPayload {
                value: 1,
            })))
        else {
            panic!("expected payload");
        };
        let payload = (payload.as_ref() as &dyn Any)
            .downcast_ref::<SerdeTestPayload>()
            .unwrap();
        assert_eq!(payload.value, 1);

        let PacketPayload::Reliable { seq, payload } = roundtrip(PacketPayload::Reliable {
            seq: 42,
            payload: Box::new(SerdeTestPayload { value: 2 }),
        }) else {
            panic!("expected reliable payload");
        };
        assert_eq!(seq, 42);
        let payload = (payload.as_ref() as &dyn Any)
            .downcast_ref::<SerdeTestPayload>()
            .unwrap();
        assert_eq!(payload.value, 2);

        assert!(matches!(
            roundtrip(PacketPayload::Ack { seq: 7 }),
            PacketPayload::Ack { seq: 7 }
        ));
    }
}
