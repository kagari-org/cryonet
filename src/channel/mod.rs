use anyhow::Result;

pub(crate) mod ws;
pub(crate) mod webrtc;

#[derive(Debug, Clone)]
pub(crate) enum BroadcastPacket {
    // Shake(ShakePacket),
}

// #[derive(Debug, Clone)]
// pub(crate) struct ShakePacket {}

pub(crate) trait Channel {
    async fn recv(&mut self) -> Result<BroadcastPacket>;
    async fn send(&mut self, packet: &BroadcastPacket) -> Result<()>;
}
