use std::{future::pending, sync::Arc};

use anyhow::anyhow;
use async_recursion::async_recursion;
use bytes::Bytes;
use cidr::AnyIpCidr;
use cryonet_uapi::ConnState;
use rustrtc::{
    DataChannelEvent, IceCandidate, PeerConnection, PeerConnectionEvent, PeerConnectionState, RtcConfiguration, RtpCodecParameters, SdpType, SessionDescription,
    media::{AudioFrame, AudioStreamTrack, MediaKind, SampleStreamSource, SampleStreamTrack, sample_track},
    transports::sctp::DataChannel,
};
use sactor::error::{SactorError, SactorResult};
use tokio::{select, sync::{Mutex, broadcast, watch}};

use crate::{
    errors::Error,
    fullmesh::{FullMeshType, IceServer},
    time::Instant,
};

pub struct PeerConn {
    candidate_filter_prefix: Option<AnyIpCidr>,

    peer: PeerConnection,
    send_source: Arc<SampleStreamSource>,
    dc: Arc<Mutex<Option<Arc<DataChannel>>>>,
    full_mesh_type: FullMeshType,

    state_watcher: watch::Receiver<ConnState>,
    candidate_tx: broadcast::Sender<String>,

    stop: watch::Sender<bool>,

    pub time: Instant,
    pub selected: bool,
}

impl PeerConn {
    pub async fn new(ice_servers: Vec<IceServer>, candidate_filter_prefix: Option<AnyIpCidr>) -> SactorResult<Self> {
        let peer = PeerConnection::new(RtcConfiguration {
            ice_servers: ice_servers
                .into_iter()
                .map(|s| rustrtc::IceServer {
                    urls: vec![s.url],
                    username: s.username,
                    credential: s.credential,
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        });
        let (source, track, _) = sample_track(MediaKind::Audio, 1024);
        peer.add_track(track, RtpCodecParameters::default())?;

        let (stop_tx, _) = watch::channel(false);

        let (state_tx, state_watcher) = watch::channel(ConnState::New);
        let mut peer_state = peer.subscribe_peer_state();
        let mut stop_rx = state_tx.subscribe();
        tokio::spawn(async move {
            loop {
                select! {
                    _ = stop_rx.changed() => break,
                    Ok(_) = peer_state.changed() => {
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
                    else => break,
                }
            }
        });

        let (candidate_tx, _) = broadcast::channel(64);
        let candidate_tx2 = candidate_tx.clone();
        let mut peer_candidate = peer.subscribe_ice_candidates();
        let mut stop_rx = stop_tx.subscribe();
        tokio::spawn(async move {
            loop {
                select! {
                    _ = stop_rx.changed() => break,
                    Ok(candidate) = peer_candidate.recv() => {
                        let _ = candidate_tx2.send(candidate.to_sdp());
                    }
                    else => break,
                }
            }
        });

        let peer2 = peer.clone();
        let dc = Arc::new(Mutex::new(None));
        let dc2 = dc.clone();
        let mut stop_rx = stop_tx.subscribe();
        tokio::spawn(async move {
            loop {
                select! {
                    _ = stop_rx.changed() => break,
                    Some(event) = peer2.recv() => {
                        if let PeerConnectionEvent::DataChannel(dc) = event {
                            *dc2.lock().await = Some(dc);
                        }
                    }
                    else => break,
                }
            }
        });

        Ok(Self {
            candidate_filter_prefix,
            peer,
            state_watcher,
            candidate_tx,
            send_source: Arc::new(source),
            dc,
            full_mesh_type: FullMeshType::Any,
            stop: stop_tx,
            time: Instant::now(),
            selected: false,
        })
    }

    pub fn subscribe_state(&self) -> watch::Receiver<ConnState> {
        self.state_watcher.clone()
    }

    pub fn get_state(&self) -> ConnState {
        *self.state_watcher.borrow()
    }

    pub fn is_connected(&self) -> bool {
        let status = *self.state_watcher.borrow();
        status == ConnState::Connected
    }

    pub async fn is_answered(&self) -> bool {
        self.peer.remote_description().is_some()
    }

    pub async fn offer(&mut self) -> SactorResult<(String, FullMeshType)> {
        *self.dc.lock().await = Some(self.peer.create_data_channel("cryonet", None)?);
        let offer = self.peer.create_offer().await?;
        self.peer.set_local_description(offer.clone())?;
        Ok((offer.to_sdp_string(), FullMeshType::Any))
    }

    pub async fn answer(&mut self, sdp: String, full_mesh_type: FullMeshType) -> SactorResult<(String, FullMeshType)> {
        let mut sdp = SessionDescription::parse(SdpType::Offer, &sdp)?;
        filter_candidate(&mut sdp, &self.candidate_filter_prefix);
        self.peer.set_remote_description(sdp).await?;
        let answer = self.peer.create_answer().await?;
        self.peer.set_local_description(answer.clone())?;
        self.full_mesh_type = full_mesh_type;
        Ok((answer.to_sdp_string(), full_mesh_type))
    }

    pub async fn answered(&mut self, sdp: String, full_mesh_type: FullMeshType) -> SactorResult<()> {
        let mut sdp = SessionDescription::parse(SdpType::Answer, &sdp)?;
        filter_candidate(&mut sdp, &self.candidate_filter_prefix);
        self.peer.set_remote_description(sdp).await?;
        self.full_mesh_type = full_mesh_type;
        Ok(())
    }

    pub fn subscribe_candidates(&self) -> broadcast::Receiver<String> {
        self.candidate_tx.subscribe()
    }

    pub async fn add_ice_candidate(&self, candidate: String) -> SactorResult<()> {
        let candidate = IceCandidate::from_sdp(&candidate).map_err(SactorError::Other)?;
        if check_candidate(&candidate, &self.candidate_filter_prefix) {
            self.peer.add_ice_candidate(candidate)?;
        }
        Ok(())
    }

    pub async fn get_selected_candidate(&self) -> Option<String> {
        self.peer.ice_transport().get_selected_pair().await.map(|pair| pair.remote.to_sdp())
    }

    pub async fn sender(&self) -> SactorResult<PeerConnSender> {
        match self.full_mesh_type {
            FullMeshType::Any => Ok(PeerConnSender::Srtp(self.send_source.clone())),
            FullMeshType::DataChannel => Ok(PeerConnSender::DataChannel(self.peer.clone())),
        }
    }

    pub async fn receiver(&self) -> SactorResult<PeerConnReceiver> {
        match self.full_mesh_type {
            FullMeshType::Any => {
                let track = self.peer.get_transceivers()[0].receiver().ok_or(Error::Unknown)?.track();
                Ok(PeerConnReceiver::Srtp(track))
            }
            FullMeshType::DataChannel => {
                let recv_dc = self.dc.lock().await.as_ref().ok_or(Error::Unknown)?.clone();
                Ok(PeerConnReceiver::DataChannel(self.peer.clone(), recv_dc))
            }
        }
    }
}

impl Drop for PeerConn {
    fn drop(&mut self) {
        self.peer.close();
        let _ = self.stop.send(true);
    }
}

#[derive(Clone)]
pub enum PeerConnSender {
    Srtp(Arc<SampleStreamSource>),
    DataChannel(PeerConnection),
}

pub enum PeerConnReceiver {
    Srtp(Arc<SampleStreamTrack>),
    DataChannel(PeerConnection, Arc<DataChannel>),
}

impl PeerConnSender {
    pub async fn send(&self, data: Bytes) -> SactorResult<()> {
        match self {
            PeerConnSender::Srtp(source) => source.send_audio(AudioFrame { data, ..Default::default() }).await?,
            PeerConnSender::DataChannel(peer) => peer.send_data(0, &data).await?,
        }
        Ok(())
    }
}

impl PeerConnReceiver {
    #[async_recursion]
    pub async fn recv(&self) -> SactorResult<Bytes> {
        match self {
            PeerConnReceiver::Srtp(track) => Ok(track.recv_audio().await?.data),
            PeerConnReceiver::DataChannel(pc, dc) => {
                let event = dc.recv().await.ok_or_else(|| SactorError::Other(anyhow!("Data channel closed")))?;
                match event {
                    DataChannelEvent::Open => self.recv().await,
                    DataChannelEvent::Message(bytes) => Ok(bytes),
                    DataChannelEvent::Close => {
                        pc.close();
                        // wait for refresh
                        pending().await
                    }
                }
            }
        }
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
