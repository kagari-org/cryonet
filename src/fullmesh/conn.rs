
use std::sync::Arc;

use anyhow::{Result, anyhow};
use bytes::Bytes;
use rustrtc::{IceCandidate, PeerConnection, PeerConnectionState, RtcConfiguration, RtpCodecParameters, SdpType, SessionDescription, media::{AudioFrame, AudioStreamTrack, MediaKind, SampleStreamSource, SampleStreamTrack, sample_track}};
use tokio::sync::{broadcast, watch};

pub(crate) struct PeerConn {
    peer: PeerConnection,
    sender: Arc<PeerConnSender>,

    state_watcher: watch::Receiver<PeerConnectionState>,
}

impl PeerConn {
    pub(crate) async fn new(config: RtcConfiguration) -> Result<Self> {
        let peer = PeerConnection::new(config);
        let (source, track, _) = sample_track(MediaKind::Audio, 1024);
        peer.add_track(track, RtpCodecParameters::default())?;

        Ok(Self {
            state_watcher: peer.subscribe_peer_state(),
            peer,
            sender: Arc::new(PeerConnSender { source }),
        })
    }

    pub(crate) fn subscribe_state(&self) -> watch::Receiver<PeerConnectionState> {
        self.peer.subscribe_peer_state()
    }

    pub(crate) fn connected(&self) -> bool {
        let status = *self.state_watcher.borrow();
        status == PeerConnectionState::Connected
    }

    pub(crate) async fn offer(&mut self) -> Result<String> {
        let offer = self.peer.create_offer().await?;
        self.peer.set_local_description(offer.clone())?;
        Ok(offer.to_sdp_string())
    }

    pub(crate) async fn answer(&mut self, sdp: &str) -> Result<String> {
        let offer = SessionDescription::parse(SdpType::Offer, sdp)?;
        self.peer.set_remote_description(offer).await?;
        let answer = self.peer.create_answer().await?;
        self.peer.set_local_description(answer.clone())?;
        Ok(answer.to_sdp_string())
    }

    pub(crate) async fn answered(&self, sdp: &str) -> Result<()> {
        let answer = SessionDescription::parse(SdpType::Answer, sdp)?;
        self.peer.set_remote_description(answer).await?;
        Ok(())
    }

    pub(crate) fn subscribe_candidates(&self) -> broadcast::Receiver<IceCandidate> {
        self.peer.subscribe_ice_candidates()
    }

    pub(crate) async fn add_ice_candidate(&self, candidate: IceCandidate) -> Result<()> {
        self.peer.add_ice_candidate(candidate)?;
        Ok(())
    }

    pub(crate) fn sender(&self) -> Arc<PeerConnSender> {
        self.sender.clone()
    }

    pub(crate) fn receiver(&self) -> Result<PeerConnReceiver> {
        let track = self.peer.get_transceivers()[0].receiver()
            .ok_or_else(|| anyhow!("unexpected missing receiver"))?.track();
        Ok(PeerConnReceiver { track })
    }

    pub(crate) fn close(&self) {
        self.peer.close();
    }
}

pub(crate) struct PeerConnSender {
    source: SampleStreamSource,
}

pub(crate) struct PeerConnReceiver {
    track: Arc<SampleStreamTrack>,
}

impl PeerConnSender {
    pub(crate) async fn send(&self, data: Bytes) -> Result<()> {
        self.source.send_audio(AudioFrame {
            data,
            ..Default::default()
        }).await?;
        Ok(())
    }
}

impl PeerConnReceiver {
    pub(crate) async fn recv(&self) -> Result<Bytes> {
        Ok(self.track.recv_audio().await?.data)
    }
}
