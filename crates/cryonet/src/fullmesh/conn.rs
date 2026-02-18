use std::{sync::Arc, time::Instant};

use bytes::Bytes;
use rustrtc::{
    IceCandidate, PeerConnection, PeerConnectionState, RtcConfiguration, RtpCodecParameters, SessionDescription,
    media::{AudioFrame, AudioStreamTrack, MediaKind, SampleStreamSource, SampleStreamTrack, sample_track},
};
use sactor::error::SactorResult;
use tokio::sync::{broadcast, watch};

use crate::errors::Error;

pub(crate) struct PeerConn {
    pub(crate) peer: PeerConnection,
    pub(crate) sender: PeerConnSender,

    pub(crate) time: Instant,
    pub(crate) selected: bool,

    pub(crate) state_watcher: watch::Receiver<PeerConnectionState>,
}

impl PeerConn {
    pub(crate) async fn new(config: RtcConfiguration) -> SactorResult<Self> {
        let peer = PeerConnection::new(config);
        let (source, track, _) = sample_track(MediaKind::Audio, 1024);
        peer.add_track(track, RtpCodecParameters::default())?;

        Ok(Self {
            state_watcher: peer.subscribe_peer_state(),
            peer,
            sender: PeerConnSender { source: Arc::new(source) },
            time: Instant::now(),
            selected: false,
        })
    }

    pub(crate) fn subscribe_state(&self) -> watch::Receiver<PeerConnectionState> {
        self.peer.subscribe_peer_state()
    }

    pub(crate) fn connected(&self) -> bool {
        let status = *self.state_watcher.borrow();
        status == PeerConnectionState::Connected
    }

    pub(crate) async fn offer(&mut self) -> SactorResult<String> {
        let offer = self.peer.create_offer().await?;
        self.peer.set_local_description(offer.clone())?;
        Ok(offer.to_sdp_string())
    }

    pub(crate) async fn answer(&mut self, sdp: SessionDescription) -> SactorResult<String> {
        self.peer.set_remote_description(sdp).await?;
        let answer = self.peer.create_answer().await?;
        self.peer.set_local_description(answer.clone())?;
        Ok(answer.to_sdp_string())
    }

    pub(crate) async fn answered(&self, sdp: SessionDescription) -> SactorResult<()> {
        self.peer.set_remote_description(sdp).await?;
        Ok(())
    }

    pub(crate) fn subscribe_candidates(&self) -> broadcast::Receiver<IceCandidate> {
        self.peer.subscribe_ice_candidates()
    }

    pub(crate) async fn add_ice_candidate(&self, candidate: IceCandidate) -> SactorResult<()> {
        self.peer.add_ice_candidate(candidate)?;
        Ok(())
    }

    pub(crate) fn sender(&self) -> PeerConnSender {
        self.sender.clone()
    }

    pub(crate) fn receiver(&self) -> SactorResult<PeerConnReceiver> {
        let track = self.peer.get_transceivers()[0].receiver().ok_or_else(|| Error::Unknown)?.track();
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
    source: Arc<SampleStreamSource>,
}

pub(crate) struct PeerConnReceiver {
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
