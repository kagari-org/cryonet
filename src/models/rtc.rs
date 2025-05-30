use anyhow::Result;
use webrtc::{api::{interceptor_registry::register_default_interceptors, media_engine::MediaEngine, APIBuilder}, interceptor::registry::Registry, peer_connection::{configuration::RTCConfiguration, RTCPeerConnection}};

use crate::{error::CryonetError, CONFIG};

// pub(crate) async fn negotiate(id: String, remote_id: String, mut channel: NetRTCChannel, cfg: Config) -> Result<()> {
//     let negotiate_timeout = tokio::spawn(sleep(cfg.negotiate_timeout));
//     let mut count = 0;
//     let mut master: bool = rand::random();
//     let master = 'outer: loop {
//         if negotiate_timeout.is_finished() { Err(CryonetError::Timeout)? }
//         let role = Signal {
//             from_id: id.clone(),
//             to_id: remote_id.clone(),
//             action: SignalAction::Role { count, master },
//         };
//         channel.send(role.clone()).await?;
//         let timeout = sleep(cfg.role_interval);
//         let SignalAction::Role { count: remote_count, master: remote_master } = (loop {
//             select! {
//                 _ = timeout => continue 'outer,
//                 Some(Signal { from_id, to_id, action: action@SignalAction::Role { .. } }) = channel.recv() => {
//                     if from_id != remote_id || to_id != id { continue 'outer; }
//                     break action;
//                 },
//             }
//         }) else { unreachable!() };
//         if remote_count != count {
//             if remote_count > count {
//                 count = remote_count;
//                 master = rand::random();
//             }
//             // wait for same count
//             continue;
//         }
//         if remote_master == master {
//             count = count + 1;
//             master = rand::random();
//             continue;
//         }
//         // re-send to confirm
//         channel.send(role).await?;
//         break master;
//     };
//     todo!()
// }

// async fn master(id: String, remote_id: String, mut channel: NetRTCChannel, cfg: Config) -> Result<()> {
//     // send offer
//     let rtc = create_rtc_connection(cfg).await?;
//     let offer = rtc.create_offer(None).await?;
//     let mut gather = rtc.gathering_complete_promise().await;
//     rtc.set_local_description(offer).await?;
//     gather.recv().await.ok_or(CryonetError::Connection)?;
//     let local_desc = rtc.local_description().await.ok_or(CryonetError::Connection)?;
//     // let msg = serde_json::to_vec(Signal {
//     //     from_id: id,
//     //     to_id: remote_id,
//     // });
//     Ok(())
// }

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
