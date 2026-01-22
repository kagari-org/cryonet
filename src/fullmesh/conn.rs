
use anyhow::{Result, anyhow};
use bytes::Bytes;
use rustrtc::{IceCandidate, PeerConnection, RtcConfiguration, RtpCodecParameters, SdpType, SessionDescription, media::{AudioFrame, AudioStreamTrack, MediaKind, SampleStreamSource, sample_track}};
use tokio::sync::mpsc;
use tracing::error;

pub(crate) struct PeerConn {
    peer: PeerConnection,
    tx: Option<mpsc::UnboundedSender<Bytes>>,
    source: SampleStreamSource,
}

impl PeerConn {
    pub(crate) async fn new(config: RtcConfiguration) -> Result<(Self, mpsc::UnboundedReceiver<Bytes>)> {
        let peer = PeerConnection::new(config);
        let (source, track, _) = sample_track(MediaKind::Audio, 1024);
        peer.add_track(track, RtpCodecParameters::default())?;

        let (tx, rx) = mpsc::unbounded_channel();

        Ok((Self {
            peer,
            source,
            tx: Some(tx),
        }, rx))
    }

    pub(crate) async fn offer(&mut self) -> Result<String> {
        let offer = self.peer.create_offer().await?;
        self.peer.set_local_description(offer.clone())?;
        self.run_recv()?;
        Ok(offer.to_sdp_string())
    }

    pub(crate) async fn answer(&mut self, sdp: &str) -> Result<String> {
        let offer = SessionDescription::parse(SdpType::Offer, sdp)?;
        self.peer.set_remote_description(offer).await?;
        let answer = self.peer.create_answer().await?;
        self.peer.set_local_description(answer.clone())?;
        self.run_recv()?;
        Ok(answer.to_sdp_string())
    }

    pub(crate) async fn answered(&self, sdp: &str) -> Result<()> {
        let answer = SessionDescription::parse(SdpType::Answer, sdp)?;
        self.peer.set_remote_description(answer).await?;
        Ok(())
    }

    pub(crate) async fn add_ice_candidate(&self, candidate: IceCandidate) -> Result<()> {
        self.peer.add_ice_candidate(candidate)?;
        Ok(())
    }

    fn run_recv(&mut self) -> Result<()> {
        let track = self.peer
            .get_transceivers()[0]
            .receiver()
            .ok_or(anyhow!("unexpected none"))?
            .track();
        let tx = match self.tx.take(){
            Some(tx) => tx,
            None => Err(anyhow!("recv already running"))?,
        };
        tokio::spawn(async move {
            loop {
                let frame = track.recv_audio().await;
                let frame = match frame {
                    Ok(frame) => frame,
                    Err(err) => {
                        error!("recv audio frame error: {:?}", err);
                        break;
                    },
                };
                if let Err(err) = tx.send(frame.data) {
                    error!("recv audio frame send error: {:?}", err);
                    break;
                }
            }
        });
        Ok(())
    }

    pub(crate) async fn send(&mut self, data: Bytes) -> Result<()> {
        let frame = AudioFrame {
            data,
            ..Default::default()
        };
        self.source.send_audio(frame).await?;
        Ok(())
    }
}
