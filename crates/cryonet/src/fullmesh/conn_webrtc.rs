use std::{sync::Arc, time::Instant};

use bytes::Bytes;
use cidr::AnyIpCidr;
use cryonet_uapi::ConnState;
use sactor::error::SactorResult;
use tokio::sync::{Mutex, broadcast, watch};
use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::{MIME_TYPE_OPUS, MediaEngine};
use webrtc::ice::candidate::Candidate;
use webrtc::ice::candidate::candidate_base::unmarshal_candidate;
use webrtc::interceptor::registry::Registry;
use webrtc::peer_connection::sdp::sdp_type::RTCSdpType;
use webrtc::{
    api::APIBuilder,
    ice_transport::ice_candidate::{RTCIceCandidate, RTCIceCandidateInit},
    peer_connection::{RTCPeerConnection, configuration::RTCConfiguration, peer_connection_state::RTCPeerConnectionState, sdp::session_description::RTCSessionDescription},
    rtp_transceiver::rtp_codec::RTCRtpCodecCapability,
    track::track_local::track_local_static_sample::TrackLocalStaticSample,
    track::track_remote::TrackRemote,
};

use crate::errors::Error;

pub(crate) struct PeerConn {
    candidate_filter_prefix: Option<AnyIpCidr>,
    peer: Arc<RTCPeerConnection>,
    sender: PeerConnSender,

    state_watcher: watch::Receiver<ConnState>,
    candidate_tx: broadcast::Sender<String>,
    track: Arc<Mutex<Option<Arc<TrackRemote>>>>,

    pub(crate) time: Instant,
    pub(crate) selected: bool,
}

impl PeerConn {
    pub(crate) async fn new(config: RTCConfiguration, candidate_filter_prefix: Option<AnyIpCidr>) -> SactorResult<Self> {
        let mut m = MediaEngine::default();
        m.register_default_codecs()?;
        let registry = Registry::new();
        let interceptor = register_default_interceptors(registry, &mut m)?;
        let api = APIBuilder::new().with_media_engine(m).with_interceptor_registry(interceptor).build();
        let peer = Arc::new(api.new_peer_connection(config).await?);

        let local_track = Arc::new(TrackLocalStaticSample::new(
            RTCRtpCodecCapability {
                mime_type: MIME_TYPE_OPUS.to_string(),
                ..Default::default()
            },
            "audio".to_string(),
            "cryonet".to_string(),
        ));

        peer.add_track(local_track.clone()).await?;

        let (state_tx, state_rx) = watch::channel(ConnState::New);
        peer.on_peer_connection_state_change(Box::new(move |state: RTCPeerConnectionState| {
            let state_tx = state_tx.clone();
            Box::pin(async move {
                let state = match state {
                    RTCPeerConnectionState::New => ConnState::New,
                    RTCPeerConnectionState::Connecting => ConnState::Connecting,
                    RTCPeerConnectionState::Connected => ConnState::Connected,
                    RTCPeerConnectionState::Disconnected => ConnState::Disconnected,
                    RTCPeerConnectionState::Failed => ConnState::Failed,
                    RTCPeerConnectionState::Closed => ConnState::Closed,
                    RTCPeerConnectionState::Unspecified => ConnState::Unknown,
                };
                let _ = state_tx.send(state);
            })
        }));

        let (candidate_tx, _) = broadcast::channel(64);
        let candidate_tx2 = candidate_tx.clone();
        peer.on_ice_candidate(Box::new(move |candidate: Option<RTCIceCandidate>| {
            let candidate_tx = candidate_tx2.clone();
            Box::pin(async move {
                if let Some(candidate) = candidate
                    && let Ok(candidate) = candidate.to_json()
                    && let Ok(candidate) = serde_json::to_string(&candidate)
                {
                    let _ = candidate_tx.send(candidate);
                }
            })
        }));

        let track_rx = Arc::new(Mutex::new(None));
        let track_rx2 = track_rx.clone();
        peer.on_track(Box::new(move |track, _, _| {
            let track_rx = track_rx2.clone();
            Box::pin(async move {
                *track_rx.lock().await = Some(track);
            })
        }));

        Ok(Self {
            candidate_filter_prefix,
            state_watcher: state_rx,
            peer,
            sender: PeerConnSender { track: local_track },
            time: Instant::now(),
            selected: false,
            candidate_tx,
            track: track_rx,
        })
    }

    pub(crate) fn subscribe_state(&self) -> watch::Receiver<ConnState> {
        self.state_watcher.clone()
    }

    pub(crate) fn get_state(&self) -> ConnState {
        *self.state_watcher.borrow()
    }

    pub(crate) fn is_connected(&self) -> bool {
        let status = *self.state_watcher.borrow();
        status == ConnState::Connected
    }

    pub(crate) async fn is_answered(&self) -> bool {
        self.peer.remote_description().await.is_some()
    }

    pub(crate) async fn offer(&mut self) -> SactorResult<String> {
        let offer = self.peer.create_offer(None).await?;
        self.peer.set_local_description(offer.clone()).await?;
        Ok(serde_json::to_string(&offer)?)
    }

    pub(crate) async fn answer(&mut self, sdp: String) -> SactorResult<String> {
        let sdp = serde_json::from_str::<RTCSessionDescription>(&sdp)?;
        let sdp = filter_candidate(sdp, &self.candidate_filter_prefix)?;
        self.peer.set_remote_description(sdp).await?;
        let answer = self.peer.create_answer(None).await?;
        self.peer.set_local_description(answer.clone()).await?;
        Ok(serde_json::to_string(&answer)?)
    }

    pub(crate) async fn answered(&self, sdp: String) -> SactorResult<()> {
        let sdp = serde_json::from_str::<RTCSessionDescription>(&sdp)?;
        let sdp = filter_candidate(sdp, &self.candidate_filter_prefix)?;
        self.peer.set_remote_description(sdp).await?;
        Ok(())
    }

    pub(crate) fn subscribe_candidates(&self) -> broadcast::Receiver<String> {
        self.candidate_tx.subscribe()
    }

    pub(crate) async fn add_ice_candidate(&self, candidate: String) -> SactorResult<()> {
        let candidate = serde_json::from_str::<RTCIceCandidateInit>(&candidate)?;
        if check_candidate(&candidate, &self.candidate_filter_prefix) {
            self.peer.add_ice_candidate(candidate).await?;
        }
        Ok(())
    }

    pub(crate) async fn get_selected_candidate(&self) -> Option<String> {
        None
    }

    pub(crate) async fn sender(&self) -> PeerConnSender {
        self.sender.clone()
    }

    pub(crate) async fn receiver(&self) -> SactorResult<PeerConnReceiver> {
        let track = self.track.lock().await.clone().ok_or(Error::Unknown)?;
        Ok(PeerConnReceiver { track })
    }
}

impl Drop for PeerConn {
    fn drop(&mut self) {
        let peer = self.peer.clone();
        tokio::spawn(async move {
            let _ = peer.close().await;
        });
    }
}

#[derive(Clone)]
pub(crate) struct PeerConnSender {
    track: Arc<TrackLocalStaticSample>,
}

pub(crate) struct PeerConnReceiver {
    track: Arc<TrackRemote>,
}

impl PeerConnSender {
    pub(crate) async fn send(&self, data: Bytes) -> SactorResult<()> {
        let sample = webrtc::media::Sample { data, ..Default::default() };
        self.track.write_sample(&sample).await?;
        Ok(())
    }
}

impl PeerConnReceiver {
    pub(crate) async fn recv(&self) -> SactorResult<Bytes> {
        let (rtp_packet, _) = self.track.read_rtp().await?;
        Ok(Bytes::from(rtp_packet.payload.to_vec()))
    }
}

fn filter_candidate(sdp: RTCSessionDescription, cidr: &Option<AnyIpCidr>) -> SactorResult<RTCSessionDescription> {
    let mut s = sdp.unmarshal()?;
    for sections in &mut s.media_descriptions {
        sections.attributes.retain(|attr| {
            if attr.key != "candidate" {
                return true;
            }
            if let Some(val) = &attr.value {
                let candidate_value = match val.strip_prefix("candidate:") {
                    Some(s) => s,
                    None => val.as_str(),
                };
                if candidate_value.is_empty() {
                    return false;
                }
                if let Ok(candidate) = unmarshal_candidate(candidate_value)
                    && let Some(cidr) = cidr
                {
                    if cidr.contains(&candidate.addr().ip()) {
                        return false;
                    }
                }
            }
            true
        });
    }
    match sdp.sdp_type {
        RTCSdpType::Offer => Ok(RTCSessionDescription::offer(s.marshal())?),
        RTCSdpType::Answer => Ok(RTCSessionDescription::answer(s.marshal())?),
        RTCSdpType::Pranswer => Ok(RTCSessionDescription::pranswer(s.marshal())?),
        _ => Err(Error::Unknown.into()),
    }
}

fn check_candidate(candidate: &RTCIceCandidateInit, cidr: &Option<AnyIpCidr>) -> bool {
    let Some(cidr) = cidr else {
        return true;
    };
    let candidate_value = match candidate.candidate.strip_prefix("candidate:") {
        Some(s) => s,
        None => candidate.candidate.as_str(),
    };
    if candidate_value.is_empty() {
        return false;
    }
    let Ok(candidate) = unmarshal_candidate(candidate_value) else {
        return false;
    };
    !cidr.contains(&candidate.addr().ip())
}
