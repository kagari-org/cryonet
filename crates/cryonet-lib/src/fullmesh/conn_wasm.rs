use std::{sync::Arc, time::Instant};

use anyhow::anyhow;
use bytes::Bytes;
use cidr::AnyIpCidr;
use cryonet_uapi::ConnState;
use sactor::error::{SactorError, SactorResult};
use tokio::sync::{Mutex, broadcast, watch};
use wasm_bindgen_futures::JsFuture;
use web_sys::{MediaStreamTrack, MediaStreamTrackGenerator, MediaStreamTrackGeneratorInit, MediaStreamTrackProcessor, MediaStreamTrackProcessorInit, ReadableStreamDefaultReader, RtcConfiguration, RtcIceCandidateInit, RtcIceServer, RtcPeerConnection, RtcPeerConnectionIceEvent, RtcPeerConnectionState, RtcSdpType, RtcSessionDescription, RtcSessionDescriptionInit, RtcTrackEvent, WritableStreamDefaultWriter, js_sys::{Array, JSON, Uint8Array}, wasm_bindgen::{JsCast, prelude::Closure}};

use crate::{errors::Error, fullmesh::IceServer};

pub(crate) struct PeerConn {
    candidate_filter_prefix: Option<AnyIpCidr>,
    peer: RtcPeerConnection,
    sender: PeerConnSender,

    state_watcher: watch::Receiver<ConnState>,
    candidate_tx: broadcast::Sender<String>,
    track: Arc<Mutex<Option<MediaStreamTrack>>>,

    pub(crate) time: Instant,
    pub(crate) selected: bool,

    _on_connection_state_change: Closure<dyn Fn() -> ()>,
    _on_ice_candidate: Closure<dyn Fn(RtcPeerConnectionIceEvent) -> ()>,
    _on_track: Closure<dyn Fn(RtcTrackEvent) -> ()>,
}

impl PeerConn {
    pub(crate) async fn new(ice_servers: Vec<IceServer>, candidate_filter_prefix: Option<AnyIpCidr>) -> SactorResult<Self> {
        let ice_servers: Vec<_> = ice_servers.into_iter().map(|server| {
            let ice_server = RtcIceServer::new();
            ice_server.set_url(&server.url);
            if let Some(username) = server.username {
                ice_server.set_username(&username);
            }
            if let Some(credential) = server.credential {
                ice_server.set_credential(&credential);
            }
            ice_server
        }).collect();
        let ice_servers: Array = Array::from_iter(ice_servers);
        let config = RtcConfiguration::new();
        config.set_ice_servers(&ice_servers);

        let peer = RtcPeerConnection::new_with_configuration(&config).map_err(|err| SactorError::Other(anyhow!("{:?}", err)))?;

        let track = MediaStreamTrackGenerator::new(&MediaStreamTrackGeneratorInit::new("audio")).map_err(|err| SactorError::Other(anyhow!("{:?}", err)))?;
        peer.add_track_0(&track, &web_sys::js_sys::Undefined::UNDEFINED.dyn_into().unwrap());
        let track = track.writable().get_writer().map_err(|err| SactorError::Other(anyhow!("{:?}", err)))?;

        let (state_tx, state_rx) = watch::channel(ConnState::New);
        let peer2 = peer.clone();
        let on_connection_state_change = Closure::new(move || {
            let _ = state_tx.send(match peer2.connection_state() {
                RtcPeerConnectionState::Closed => ConnState::Closed,
                RtcPeerConnectionState::Failed => ConnState::Failed,
                RtcPeerConnectionState::Disconnected => ConnState::Disconnected,
                RtcPeerConnectionState::New => ConnState::New,
                RtcPeerConnectionState::Connecting => ConnState::Connecting,
                RtcPeerConnectionState::Connected => ConnState::Connected,
                _ => ConnState::Unknown,
            });
        });
        peer.set_onconnectionstatechange(Some(on_connection_state_change.as_ref().unchecked_ref()));

        let (candidate_tx, _) = broadcast::channel(64);
        let candidate_tx2 = candidate_tx.clone();
        let on_ice_candidate = Closure::new(move |candidate: RtcPeerConnectionIceEvent| {
            if let Some(candidate) = candidate.candidate() {
                let _ = candidate_tx2.send(JSON::stringify(&candidate.to_json()).unwrap().as_string().unwrap());
            }
        });
        peer.set_onicecandidate(Some(&on_ice_candidate.as_ref().unchecked_ref()));

        let track_rx = Arc::new(Mutex::new(None));
        let track_rx2 = track_rx.clone();
        let on_track = Closure::new(move |event: RtcTrackEvent| {
            let track_rx = track_rx2.clone();
            tokio::task::spawn_local(async move {
                *track_rx.lock().await = Some(event.track());
            });
        });
        peer.set_ontrack(Some(&on_track.as_ref().unchecked_ref()));

        Ok(Self {
            candidate_filter_prefix,
            peer,
            sender: PeerConnSender { track },
            state_watcher: state_rx,
            candidate_tx,
            track: track_rx,
            time: Instant::now(),
            selected: false,
            _on_connection_state_change: on_connection_state_change,
            _on_ice_candidate: on_ice_candidate,
            _on_track: on_track,
        })
    }

    pub(crate) fn subscribe_state(&self) -> watch::Receiver<ConnState> {
        self.state_watcher.clone()
    }

    pub(crate) fn get_state(&self) -> ConnState {
        *self.state_watcher.borrow()
    }

    pub(crate) fn is_connected(&self) -> bool {
        *self.state_watcher.borrow() == ConnState::Connected
    }

    pub(crate) async fn is_answered(&self) -> bool {
        self.peer.remote_description().is_some()
    }

    pub(crate) async fn offer(&mut self) -> SactorResult<String> {
        let offer: RtcSessionDescription = JsFuture::from(self.peer.create_offer())
            .await
            .map_err(|err| SactorError::Other(anyhow!("{:?}", err)))?
            .dyn_into()
            .map_err(|err| SactorError::Other(anyhow!("{:?}", err)))?;
        let sdp = RtcSessionDescriptionInit::new(RtcSdpType::Offer);
        sdp.set_sdp(&offer.sdp());
        JsFuture::from(self.peer.set_local_description(&sdp)).await
            .map_err(|err| SactorError::Other(anyhow!("{:?}", err)))?;
        Ok(offer.sdp())
    }

    pub(crate) async fn answer(&mut self, sdp_str: String) -> SactorResult<String> {
        // TODO: filter candidate
        let sdp = RtcSessionDescriptionInit::new(RtcSdpType::Offer);
        sdp.set_sdp(&sdp_str);
        JsFuture::from(self.peer.set_remote_description(&sdp)).await
            .map_err(|err| SactorError::Other(anyhow!("{:?}", err)))?;
        let answer: RtcSessionDescription = JsFuture::from(self.peer.create_answer())
            .await
            .map_err(|err| SactorError::Other(anyhow!("{:?}", err)))?
            .dyn_into()
            .map_err(|err| SactorError::Other(anyhow!("{:?}", err)))?;
        let sdp = RtcSessionDescriptionInit::new(RtcSdpType::Answer);
        sdp.set_sdp(&answer.sdp());
        JsFuture::from(self.peer.set_local_description(&sdp)).await
            .map_err(|err| SactorError::Other(anyhow!("{:?}", err)))?;
        Ok(answer.sdp())
    }

    pub(crate) async fn answered(&self, sdp_str: String) -> SactorResult<()> {
        // TODO: filter candidate
        let sdp = RtcSessionDescriptionInit::new(RtcSdpType::Answer);
        sdp.set_sdp(&sdp_str);
        JsFuture::from(self.peer.set_remote_description(&sdp)).await
            .map_err(|err| SactorError::Other(anyhow!("{:?}", err)))?;
        Ok(())
    }

    pub(crate) fn subscribe_candidates(&self) -> broadcast::Receiver<String> {
        self.candidate_tx.subscribe()
    }

    pub(crate) async fn add_ice_candidate(&self, candidate: String) -> SactorResult<()> {
        // TODO: filter candidate
        let candidate: RtcIceCandidateInit = JSON::parse(&candidate)
            .map_err(|err| SactorError::Other(anyhow!("{:?}", err)))?
            .dyn_into()
            .map_err(|err| SactorError::Other(anyhow!("{:?}", err)))?;
        JsFuture::from(self.peer.add_ice_candidate_with_opt_rtc_ice_candidate_init(Some(&candidate)))
            .await
            .map_err(|err| SactorError::Other(anyhow!("{:?}", err)))?;
        Ok(())
    }

    pub(crate) async fn get_selected_candidate(&self) -> Option<String> {
        None
    }

    pub(crate) async fn sender(&self) -> PeerConnSender {
        self.sender.clone()
    }

    pub(crate) async fn receiver(&self) -> SactorResult<PeerConnReceiver> {
        let track = self.track.lock().await.clone()
            .ok_or(Error::Unknown)?;
        let track = MediaStreamTrackProcessor::new(&MediaStreamTrackProcessorInit::new(&track))
            .map_err(|err| SactorError::Other(anyhow!("{:?}", err)))?
            .readable()
            .get_reader()
            .dyn_into()
            .map_err(|err| SactorError::Other(anyhow!("{:?}", err)))?;
        Ok(PeerConnReceiver { track })
    }
}

impl Drop for PeerConn {
    fn drop(&mut self) {
        self.peer.close();
    }
}

#[derive(Clone)]
pub(crate) struct PeerConnSender {
    track: WritableStreamDefaultWriter,
}

pub(crate) struct PeerConnReceiver {
    track: ReadableStreamDefaultReader,
}

impl PeerConnSender {
    pub(crate) async fn send(&self, data: Bytes) -> SactorResult<()> {
        let data = Uint8Array::from(&data[..]);
        JsFuture::from(self.track.write_with_chunk(&data)).await
            .map_err(|err| SactorError::Other(anyhow!("{:?}", err)))?;
        Ok(())
    }
}

impl PeerConnReceiver {
    pub(crate) async fn recv(&self) -> SactorResult<Bytes> {
        let result = JsFuture::from(self.track.read()).await
            .map_err(|err| SactorError::Other(anyhow!("{:?}", err)))?
            .dyn_into::<web_sys::ReadableStreamReadResult>()
            .map_err(|err| SactorError::Other(anyhow!("{:?}", err)))?;
        if let Some(true) = result.get_done() {
            return Err(Error::Unknown.into());
        }
        let data = Uint8Array::new(&result.get_value());
        Ok(Bytes::copy_from_slice(&data.to_vec()))
    }
}

// fn filter_candidate(sdp: RTCSessionDescription, cidr: &Option<AnyIpCidr>) -> SactorResult<RTCSessionDescription> {
//     let mut s = sdp.unmarshal()?;
//     for sections in &mut s.media_descriptions {
//         sections.attributes.retain(|attr| {
//             if attr.key != "candidate" {
//                 return true;
//             }
//             if let Some(val) = &attr.value {
//                 let candidate_value = match val.strip_prefix("candidate:") {
//                     Some(s) => s,
//                     None => val.as_str(),
//                 };
//                 if candidate_value.is_empty() {
//                     return false;
//                 }
//                 if let Ok(candidate) = unmarshal_candidate(candidate_value)
//                     && let Some(cidr) = cidr
//                 {
//                     if cidr.contains(&candidate.addr().ip()) {
//                         return false;
//                     }
//                 }
//             }
//             true
//         });
//     }
//     match sdp.sdp_type {
//         RTCSdpType::Offer => Ok(RTCSessionDescription::offer(s.marshal())?),
//         RTCSdpType::Answer => Ok(RTCSessionDescription::answer(s.marshal())?),
//         RTCSdpType::Pranswer => Ok(RTCSessionDescription::pranswer(s.marshal())?),
//         _ => Err(Error::Unknown.into()),
//     }
// }

// fn check_candidate(candidate: &RTCIceCandidateInit, cidr: &Option<AnyIpCidr>) -> bool {
//     let Some(cidr) = cidr else {
//         return true;
//     };
//     let candidate_value = match candidate.candidate.strip_prefix("candidate:") {
//         Some(s) => s,
//         None => candidate.candidate.as_str(),
//     };
//     if candidate_value.is_empty() {
//         return false;
//     }
//     let Ok(candidate) = unmarshal_candidate(candidate_value) else {
//         return false;
//     };
//     !cidr.contains(&candidate.addr().ip())
// }
