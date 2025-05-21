use std::sync::Arc;

use anyhow::Result;
use futures_util::StreamExt;
use tokio_tungstenite::connect_async;
use webrtc::{data_channel::RTCDataChannel, peer_connection::RTCPeerConnection};

use crate::error::CryonetError;

pub(crate) struct Peer {
    id: String,

    rtc: RTCPeerConnection,
    signal: Arc<RTCDataChannel>,
    data: Arc<RTCDataChannel>,
}

impl Peer {
    pub(crate) fn new(
        id: String,
        rtc: RTCPeerConnection,
        signal: Arc<RTCDataChannel>,
        data: Arc<RTCDataChannel>,
    ) -> Peer {
        Peer { id, rtc, signal, data }
    }
}
