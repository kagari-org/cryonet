use anyhow::Result;

use super::{BroadcastPacket, Channel};

#[derive(Debug)]
pub(crate)  struct WebRTCChannel {}

impl Channel for WebRTCChannel {
    async fn recv(&mut self) -> Result<BroadcastPacket> {
        todo!()
    }

    async fn send(&mut self, packet: &BroadcastPacket) -> Result<()> {
        todo!()
    }
}