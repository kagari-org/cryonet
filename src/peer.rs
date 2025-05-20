use anyhow::Result;

use crate::channel::{webrtc::WebRTCChannel, ws::WSChannel};

#[derive(Debug)]
struct Peer {
    id: String,

    ws: Option<WSChannel>,
    webrtc: Option<WebRTCChannel>,
    data: Option<WebRTCChannel>,
}

impl Peer {
    pub(crate) async fn from_ws(endpoint: String) -> Result<Peer> {
        let (ws, id) = WSChannel::new(endpoint).await?;
        Ok(Peer {
            id, ws: Some(ws),
            webrtc: None,
            data: None,
        })
    }
}
