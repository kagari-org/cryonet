use anyhow::Result;
use webrtc::{api::{interceptor_registry::register_default_interceptors, media_engine::MediaEngine, APIBuilder}, ice_transport::ice_server::RTCIceServer, interceptor::registry::Registry, peer_connection::{configuration::RTCConfiguration, RTCPeerConnection}};

use crate::Config;

pub(crate) async fn create_rtc_connection(cfg: Config) -> Result<RTCPeerConnection> {
    let mut media_engine = MediaEngine::default();
    media_engine.register_default_codecs()?;
    let mut registry = Registry::new();
    registry = register_default_interceptors(registry, &mut media_engine)?;
    let api = APIBuilder::new()
        .with_media_engine(media_engine)
        .with_interceptor_registry(registry)
        .build();
    let config = RTCConfiguration {
        ice_servers: cfg.ice_servers,
        ..Default::default()
    };
    Ok(api.new_peer_connection(config).await?)
}
