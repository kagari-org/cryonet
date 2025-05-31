use anyhow::Result;
use tracing::debug;
use webrtc::{api::{interceptor_registry::register_default_interceptors, media_engine::MediaEngine, APIBuilder}, interceptor::registry::Registry, peer_connection::{configuration::RTCConfiguration, RTCPeerConnection}};

use crate::{error::CryonetError, CONFIG};

pub(crate) async fn create_rtc_connection() -> Result<RTCPeerConnection> {
    let cfg = CONFIG.get().unwrap();
    let mut media_engine = MediaEngine::default();
    media_engine.register_default_codecs()?;
    let mut registry = Registry::new();
    registry = register_default_interceptors(registry, &mut media_engine)?;
    let api = APIBuilder::new()
        .with_media_engine(media_engine)
        .with_interceptor_registry(registry)
        .build();
    debug!("ice servers: {:?}", cfg.ice_servers);
    let config = RTCConfiguration {
        ice_servers: cfg.ice_servers.clone(),
        ..Default::default()
    };
    Ok(api.new_peer_connection(config).await?)
}

pub(crate) fn is_master(id: &String, remote_id: &String) -> Result<bool> {
    if id.len() > remote_id.len() { return Ok(true); }
    let id = id.as_bytes();
    let remote_id = remote_id.as_bytes();
    for (x, y) in id.iter().zip(remote_id) {
        if x == y { continue; }
        return Ok(x > y)
    }
    Err(CryonetError::SameId)?
}
