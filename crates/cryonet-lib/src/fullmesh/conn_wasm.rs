use std::sync::Arc;

use anyhow::anyhow;
use bytes::Bytes;
use cidr::AnyIpCidr;
use cryonet_uapi::ConnState;
use sactor::error::{SactorError, SactorResult};
use tokio::sync::{Mutex, broadcast, watch};
use wasm_bindgen::JsValue;
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    MediaStreamTrack, MediaStreamTrackGenerator, MediaStreamTrackGeneratorInit, MediaStreamTrackProcessor, MediaStreamTrackProcessorInit, ReadableStreamDefaultReader, RtcConfiguration, RtcIceCandidateInit, RtcIceServer, RtcPeerConnection, RtcPeerConnectionIceEvent,
    RtcPeerConnectionState, RtcSdpType, RtcSessionDescriptionInit, RtcTrackEvent, WritableStreamDefaultWriter,
    js_sys::{Array, Reflect, Uint8Array},
    wasm_bindgen::{JsCast, prelude::Closure},
};

use crate::{errors::Error, fullmesh::IceServer, time::Instant, wasm::LOCAL_SET};

pub struct PeerConn {
    peer: RtcPeerConnection,
    sender: PeerConnSender,

    state_watcher: watch::Receiver<ConnState>,
    candidate_tx: broadcast::Sender<String>,
    track: Arc<Mutex<Option<MediaStreamTrack>>>,

    pub time: Instant,
    pub selected: bool,

    _on_connection_state_change: Closure<dyn Fn()>,
    _on_ice_candidate: Closure<dyn Fn(RtcPeerConnectionIceEvent)>,
    _on_track: Closure<dyn Fn(RtcTrackEvent)>,
}

impl PeerConn {
    pub async fn new(ice_servers: Vec<IceServer>, _candidate_filter_prefix: Option<AnyIpCidr>) -> SactorResult<Self> {
        let ice_servers: Vec<_> = ice_servers
            .into_iter()
            .map(|server| {
                let ice_server = RtcIceServer::new();
                ice_server.set_url(&server.url);
                if let Some(username) = server.username {
                    ice_server.set_username(&username);
                }
                if let Some(credential) = server.credential {
                    ice_server.set_credential(&credential);
                }
                ice_server
            })
            .collect();
        let ice_servers: Array = Array::from_iter(ice_servers);
        let config = RtcConfiguration::new();
        config.set_ice_servers(&ice_servers);

        let peer = RtcPeerConnection::new_with_configuration(&config).map_err(|err| SactorError::Other(anyhow!("{:?}", err)))?;

        let track = MediaStreamTrackGenerator::new(&MediaStreamTrackGeneratorInit::new("audio")).map_err(|err| SactorError::Other(anyhow!("{:?}", err)))?;
        peer.add_track_0(&track);
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
            if let Some(candidate) = candidate.candidate()
                && let Some(candidate) = candidate.candidate().strip_prefix("candidate:")
            {
                let _ = candidate_tx2.send(candidate.to_string());
            }
        });
        peer.set_onicecandidate(Some(on_ice_candidate.as_ref().unchecked_ref()));

        let track_rx = Arc::new(Mutex::new(None));
        let track_rx2 = track_rx.clone();
        let on_track = Closure::new(move |event: RtcTrackEvent| {
            let track_rx = track_rx2.clone();
            LOCAL_SET.with(|local_set| {
                local_set.spawn_local(async move {
                    let track = event.track();
                    let mut track_rx = track_rx.lock().await;
                    *track_rx = Some(track);
                })
            });
        });
        peer.set_ontrack(Some(on_track.as_ref().unchecked_ref()));

        Ok(Self {
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

    pub fn subscribe_state(&self) -> watch::Receiver<ConnState> {
        self.state_watcher.clone()
    }

    pub fn get_state(&self) -> ConnState {
        *self.state_watcher.borrow()
    }

    pub fn is_connected(&self) -> bool {
        *self.state_watcher.borrow() == ConnState::Connected
    }

    pub async fn is_answered(&self) -> bool {
        self.peer.remote_description().is_some()
    }

    pub async fn offer(&mut self) -> SactorResult<String> {
        let offer = JsFuture::from(self.peer.create_offer()).await.map_err(|err| SactorError::Other(anyhow!("{:?}", err)))?;
        let sdp_str = Reflect::get(&offer, &JsValue::from_str("sdp")).map_err(|err| SactorError::Other(anyhow!("{:?}", err)))?.as_string().unwrap();
        let sdp = RtcSessionDescriptionInit::new(RtcSdpType::Offer);
        sdp.set_sdp(&sdp_str);
        JsFuture::from(self.peer.set_local_description(&sdp)).await.map_err(|err| SactorError::Other(anyhow!("{:?}", err)))?;
        Ok(sdp_str)
    }

    pub async fn answer(&mut self, sdp_str: String) -> SactorResult<String> {
        let sdp = RtcSessionDescriptionInit::new(RtcSdpType::Offer);
        sdp.set_sdp(&sdp_str);
        JsFuture::from(self.peer.set_remote_description(&sdp)).await.map_err(|err| SactorError::Other(anyhow!("{:?}", err)))?;
        let answer = JsFuture::from(self.peer.create_answer()).await.map_err(|err| SactorError::Other(anyhow!("{:?}", err)))?;
        let sdp_str = Reflect::get(&answer, &JsValue::from_str("sdp")).map_err(|err| SactorError::Other(anyhow!("{:?}", err)))?.as_string().unwrap();
        let sdp = RtcSessionDescriptionInit::new(RtcSdpType::Answer);
        sdp.set_sdp(&sdp_str);
        JsFuture::from(self.peer.set_local_description(&sdp)).await.map_err(|err| SactorError::Other(anyhow!("{:?}", err)))?;
        Ok(sdp_str)
    }

    pub async fn answered(&self, sdp_str: String) -> SactorResult<()> {
        let sdp = RtcSessionDescriptionInit::new(RtcSdpType::Answer);
        sdp.set_sdp(&sdp_str);
        JsFuture::from(self.peer.set_remote_description(&sdp)).await.map_err(|err| SactorError::Other(anyhow!("{:?}", err)))?;
        Ok(())
    }

    pub fn subscribe_candidates(&self) -> broadcast::Receiver<String> {
        self.candidate_tx.subscribe()
    }

    pub async fn add_ice_candidate(&self, candidate: String) -> SactorResult<()> {
        let candidate = format!("candidate:{}", candidate);
        let candidate = RtcIceCandidateInit::new(&candidate);
        candidate.set_sdp_m_line_index(Some(0));
        JsFuture::from(self.peer.add_ice_candidate_with_opt_rtc_ice_candidate_init(Some(&candidate))).await.map_err(|err| SactorError::Other(anyhow!("{:?}", err)))?;
        Ok(())
    }

    pub async fn get_selected_candidate(&self) -> Option<String> {
        None
    }

    pub async fn sender(&self) -> PeerConnSender {
        self.sender.clone()
    }

    pub async fn receiver(&self) -> SactorResult<PeerConnReceiver> {
        let track = self.track.lock().await.clone().ok_or(Error::Unknown)?;
        let track = MediaStreamTrackProcessor::new(&MediaStreamTrackProcessorInit::new(&track))
            .map_err(|err| SactorError::Other(anyhow!("10 {:?}", err)))?
            .readable()
            .get_reader()
            .dyn_into()
            .map_err(|err| SactorError::Other(anyhow!("11 {:?}", err)))?;
        Ok(PeerConnReceiver { track })
    }
}

impl Drop for PeerConn {
    fn drop(&mut self) {
        self.peer.close();
    }
}

#[derive(Clone)]
pub struct PeerConnSender {
    track: WritableStreamDefaultWriter,
}

pub struct PeerConnReceiver {
    track: ReadableStreamDefaultReader,
}

impl PeerConnSender {
    pub async fn send(&self, data: Bytes) -> SactorResult<()> {
        let data = Uint8Array::from(&data[..]);
        JsFuture::from(self.track.write_with_chunk(&data)).await.map_err(|err| SactorError::Other(anyhow!("{:?}", err)))?;
        Ok(())
    }
}

impl PeerConnReceiver {
    pub async fn recv(&self) -> SactorResult<Bytes> {
        let result = JsFuture::from(self.track.read())
            .await
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
