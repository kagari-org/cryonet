use std::{
    net::{Ipv4Addr, SocketAddr, SocketAddrV4},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use anyhow::{Result, bail};
use async_trait::async_trait;
use bytes::Bytes;
use cidr::AnyIpCidr;
use rustrtc::{
    DataChannelEvent, IceCandidate, PeerConnection, PeerConnectionEvent, PeerConnectionState,
    RtcConfiguration, SdpType, SessionDescription, transports::sctp::DataChannel,
};
use tokio::sync::{Mutex, watch};

use crate::{
    errors::CryonetError,
    fullmesh::{ConnectionReceiver, ConnectionSender, IceServer},
};

pub struct ConnectionRustrtcDataChannel {
    candidate_filter_prefix: Option<AnyIpCidr>,

    pc: PeerConnection,
    dc: Arc<Mutex<Option<Arc<DataChannel>>>>,
    status: watch::Receiver<PeerConnectionState>,
    stop: watch::Sender<bool>,

    sent: Arc<AtomicU64>,
    received: Arc<AtomicU64>,
}

impl ConnectionRustrtcDataChannel {
    pub async fn new(ice_servers: Vec<IceServer>, candidate_filter_prefix: Option<AnyIpCidr>) {
        let pc = PeerConnection::new(RtcConfiguration {
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
        let dc = Arc::new(Mutex::new(None));
        let status = pc.subscribe_peer_state();

        let pc2 = pc.clone();
        let dc2 = dc.clone();
        let (stop, mut stop_rx) = watch::channel(false);
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = stop_rx.changed() => break,
                    event = pc2.recv() => {
                        let event = match event {
                            Some(event) => event,
                            None => break,
                        };
                        if let PeerConnectionEvent::DataChannel(dc) = event {
                            *dc2.lock().await = Some(dc);
                        }
                    },
                }
            }
        });

        ConnectionRustrtcDataChannel {
            candidate_filter_prefix,
            pc,
            dc,
            status,
            stop,
            sent: Arc::new(AtomicU64::new(0)),
            received: Arc::new(AtomicU64::new(0)),
        };
    }

    pub async fn create_offer(&self) -> Result<(String, Vec<String>)> {
        *self.dc.lock().await = Some(self.pc.create_data_channel("cryonet", None)?);

        let mut offer = self.pc.create_offer().await?;
        clear_candidates(&mut offer);
        self.pc.set_local_description(offer.clone())?;
        self.pc.wait_for_gathering_complete().await;
        let candidates = self.pc.ice_transport().local_candidates();
        let candidates = candidates
            .into_iter()
            .map(|candidate| candidate.to_sdp())
            .collect();
        Ok((offer.to_sdp_string(), candidates))
    }

    pub async fn create_answer(&self, offer: String) -> Result<(String, Vec<String>)> {
        let offer = SessionDescription::parse(SdpType::Offer, &offer)?;
        self.pc.set_remote_description(offer).await?;
        let mut answer = self.pc.create_answer().await?;
        clear_candidates(&mut answer);
        self.pc.set_local_description(answer.clone())?;
        self.pc.wait_for_gathering_complete().await;
        let candidates = self.pc.ice_transport().local_candidates();
        let candidates = candidates
            .into_iter()
            .map(|candidate| candidate.to_sdp())
            .collect();
        Ok((answer.to_sdp_string(), candidates))
    }

    pub async fn apply_answer(&self, answer: String) -> Result<()> {
        let answer = SessionDescription::parse(SdpType::Answer, &answer)?;
        self.pc.set_remote_description(answer).await?;
        Ok(())
    }

    pub async fn add_candidates(&self, candidates: &Vec<String>) -> Result<()> {
        for candidate in candidates {
            if let Ok(candidate) = IceCandidate::from_sdp(candidate) {
                if let Some(prefix) = &self.candidate_filter_prefix
                    && prefix.contains(&candidate.address.ip())
                {
                    continue;
                }
                self.pc.add_ice_candidate(candidate)?;
            }
        }
        Ok(())
    }

    pub fn status(&self) -> PeerConnectionState {
        *self.status.borrow()
    }

    pub fn sent(&self) -> u64 {
        self.sent.load(Ordering::Relaxed)
    }

    pub fn received(&self) -> u64 {
        self.received.load(Ordering::Relaxed)
    }

    pub fn sender(&self) -> ConnectionRustrtcDataChannelSender {
        ConnectionRustrtcDataChannelSender {
            pc: self.pc.clone(),
            sent: self.sent.clone(),
        }
    }

    pub async fn receiver(&self) -> Result<ConnectionRustrtcDataChannelReceiver> {
        let Some(dc) = self.dc.lock().await.clone() else {
            bail!("Data channel not established yet");
        };
        Ok(ConnectionRustrtcDataChannelReceiver {
            dc,
            received: self.received.clone(),
        })
    }
}

impl Drop for ConnectionRustrtcDataChannel {
    fn drop(&mut self) {
        self.pc.close();
        self.stop.send_replace(true);
    }
}

pub struct ConnectionRustrtcDataChannelSender {
    pc: PeerConnection,
    sent: Arc<AtomicU64>,
}

#[async_trait]
impl ConnectionSender for ConnectionRustrtcDataChannelSender {
    async fn send(&mut self, data: Bytes) -> Result<usize> {
        self.pc.send_data(0, &data).await?;
        let len = data.len();
        self.sent.fetch_add(len as u64, Ordering::Relaxed);
        Ok(len)
    }
}

pub struct ConnectionRustrtcDataChannelReceiver {
    dc: Arc<DataChannel>,
    received: Arc<AtomicU64>,
}

#[async_trait]
impl ConnectionReceiver for ConnectionRustrtcDataChannelReceiver {
    async fn recv(&mut self) -> Result<(Bytes, SocketAddr)> {
        const ADDR: SocketAddr = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0));
        loop {
            match self.dc.recv().await {
                None | Some(DataChannelEvent::Close) => {
                    return Err(CryonetError::ChannelClosed.into());
                }
                Some(DataChannelEvent::Open) => continue,
                Some(DataChannelEvent::Message(data)) => {
                    self.received
                        .fetch_add(data.len() as u64, Ordering::Relaxed);
                    return Ok((data, ADDR));
                }
            }
        }
    }
}

fn clear_candidates(desc: &mut SessionDescription) {
    for sections in &mut desc.media_sections {
        sections.attributes.retain(|attr| attr.key != "candidate");
    }
}
