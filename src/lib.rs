use std::collections::HashMap;

use peer::Peer;
use webrtc::ice_transport::ice_server::RTCIceServer;

pub(crate) mod error;
pub(crate) mod peer;
pub(crate) mod ws;
pub(crate) mod rtc;

#[derive(Debug, Clone)]
pub(crate) struct Config {
    ice_servers: Vec<RTCIceServer>,
}

#[derive(Debug)]
pub(crate) struct Net {
    // peers: HashMap<String, Peer>,
}

impl Net {
    // pub(crate) fn new() -> Net {
    //     Net { peers: HashMap::new() }
    // }
}
