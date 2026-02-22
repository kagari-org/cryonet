use std::{sync::Arc, time::Instant};

use bytes::Bytes;
use cidr::AnyIpCidr;
use cryonet_uapi::ConnState;
use rustrtc::{
    IceCandidate, PeerConnection, PeerConnectionState, RtcConfiguration, RtpCodecParameters, SdpType, SessionDescription,
    media::{AudioFrame, AudioStreamTrack, MediaKind, SampleStreamSource, SampleStreamTrack, sample_track},
};
use sactor::error::{SactorError, SactorResult};
use tokio::sync::{broadcast, watch};

use crate::{errors::Error, fullmesh::IceServer};

pub(crate) struct PeerConn {
    candidate_filter_prefix: Option<AnyIpCidr>,

    peer: PeerConnection,
    sender: PeerConnSender,

    state_watcher: watch::Receiver<ConnState>,
    candidate_tx: broadcast::Sender<String>,

    pub(crate) time: Instant,
    pub(crate) selected: bool,
}

impl PeerConn {
    pub(crate) async fn new(ice_servers: Vec<IceServer>, candidate_filter_prefix: Option<AnyIpCidr>) -> SactorResult<Self> {
        let peer = PeerConnection::new(RtcConfiguration {
            ice_servers: ice_servers.into_iter().map(|s| rustrtc::IceServer {
                urls: vec![s.url],
                username: s.username,
                credential: s.credential,
                ..Default::default()
            }).collect(),
            ..Default::default()
        });
        let (source, track, _) = sample_track(MediaKind::Audio, 1024);
        peer.add_track(track, RtpCodecParameters::default())?;

        let (state_tx, state_watcher) = watch::channel(ConnState::New);
        let mut peer_state = peer.subscribe_peer_state();
        tokio::spawn(async move {
            while peer_state.changed().await.is_ok() {
                let state = match *peer_state.borrow_and_update() {
                    PeerConnectionState::New => ConnState::New,
                    PeerConnectionState::Connecting => ConnState::Connecting,
                    PeerConnectionState::Connected => ConnState::Connected,
                    PeerConnectionState::Disconnected => ConnState::Disconnected,
                    PeerConnectionState::Failed => ConnState::Failed,
                    PeerConnectionState::Closed => ConnState::Closed,
                };
                let _ = state_tx.send(state);
            }
        });

        let (candidate_tx, _) = broadcast::channel(64);
        let candidate_tx2 = candidate_tx.clone();
        let mut peer_candidate = peer.subscribe_ice_candidates();
        tokio::spawn(async move {
            while let Ok(candidate) = peer_candidate.recv().await {
                let _ = candidate_tx2.send(candidate.to_sdp());
            }
        });

        Ok(Self {
            candidate_filter_prefix,
            peer,
            sender: PeerConnSender { source: Arc::new(source) },
            state_watcher,
            candidate_tx,
            time: Instant::now(),
            selected: false,
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
        self.peer.remote_description().is_some()
    }

    pub(crate) async fn offer(&mut self) -> SactorResult<String> {
        let offer = self.peer.create_offer().await?;
        self.peer.set_local_description(offer.clone())?;
        Ok(offer.to_sdp_string())
    }

    pub(crate) async fn answer(&mut self, sdp: String) -> SactorResult<String> {
        let mut sdp = SessionDescription::parse(SdpType::Offer, &sdp)?;
        filter_candidate(&mut sdp, &self.candidate_filter_prefix);
        self.peer.set_remote_description(sdp).await?;
        let answer = self.peer.create_answer().await?;
        self.peer.set_local_description(answer.clone())?;
        Ok(answer.to_sdp_string())
    }

    pub(crate) async fn answered(&self, sdp: String) -> SactorResult<()> {
        let mut sdp = SessionDescription::parse(SdpType::Answer, &sdp)?;
        filter_candidate(&mut sdp, &self.candidate_filter_prefix);
        self.peer.set_remote_description(sdp).await?;
        Ok(())
    }

    pub(crate) fn subscribe_candidates(&self) -> broadcast::Receiver<String> {
        self.candidate_tx.subscribe()
    }

    pub(crate) async fn add_ice_candidate(&self, candidate: String) -> SactorResult<()> {
        let candidate = IceCandidate::from_sdp(&candidate).map_err(SactorError::Other)?;
        if check_candidate(&candidate, &self.candidate_filter_prefix) {
            self.peer.add_ice_candidate(candidate)?;
        }
        Ok(())
    }

    pub(crate) async fn get_selected_candidate(&self) -> Option<String> {
        self.peer.ice_transport().get_selected_pair().await.map(|pair| pair.remote.to_sdp())
    }

    pub(crate) async fn sender(&self) -> PeerConnSender {
        self.sender.clone()
    }

    pub(crate) async fn receiver(&self) -> SactorResult<PeerConnReceiver> {
        let track = self.peer.get_transceivers()[0].receiver().ok_or(Error::Unknown)?.track();
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
    source: Arc<SampleStreamSource>,
}

pub struct PeerConnReceiver {
    track: Arc<SampleStreamTrack>,
}

impl PeerConnSender {
    pub(crate) async fn send(&self, data: Bytes) -> SactorResult<()> {
        self.source.send_audio(AudioFrame { data, ..Default::default() }).await?;
        Ok(())
    }
}

impl PeerConnReceiver {
    pub(crate) async fn recv(&self) -> SactorResult<Bytes> {
        Ok(self.track.recv_audio().await?.data)
    }
}

fn filter_candidate(sdp: &mut SessionDescription, cidr: &Option<AnyIpCidr>) {
    for sections in &mut sdp.media_sections {
        sections.attributes.retain(|attr| {
            if attr.key != "candidate" {
                return true;
            }
            if let Some(val) = &attr.value
                && let Ok(candidate) = IceCandidate::from_sdp(val)
                && !check_candidate(&candidate, cidr)
            {
                return false;
            }
            true
        });
    }
}

fn check_candidate(candidate: &IceCandidate, cidr: &Option<AnyIpCidr>) -> bool {
    let Some(cidr) = cidr else {
        return true;
    };
    !cidr.contains(&candidate.address.ip())
}
