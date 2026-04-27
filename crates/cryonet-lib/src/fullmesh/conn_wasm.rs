use std::{
    any::Any,
    cell::RefCell,
    net::{Ipv4Addr, SocketAddr, SocketAddrV4},
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use anyhow::{Error, Result, anyhow};
use async_trait::async_trait;
use bytes::Bytes;
use cidr::AnyIpCidr;
use tokio::sync::mpsc;
use wasm_bindgen::JsValue;
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    MessageEvent, RtcConfiguration, RtcDataChannel, RtcDataChannelEvent, RtcIceCandidateInit,
    RtcIceGatheringState, RtcIceServer, RtcPeerConnection, RtcPeerConnectionIceEvent,
    RtcPeerConnectionState, RtcSdpType, RtcSessionDescriptionInit,
    js_sys::{Array, Function, Promise, Reflect, Uint8Array},
    wasm_bindgen::{JsCast, prelude::Closure},
};

use crate::{
    errors::CryonetError,
    fullmesh::{Connection, ConnectionReceiver, ConnectionSender, ConnectionState, IceServer},
    wasm::LOCAL_SET,
};

pub struct ConnectionWasmDataChannel {
    peer: RtcPeerConnection,
    dc: Rc<RefCell<Option<RtcDataChannel>>>,

    candidates: Rc<RefCell<Vec<String>>>,

    sent: Arc<AtomicU64>,
    received: Arc<AtomicU64>,

    _on_dc: Closure<dyn Fn(RtcDataChannelEvent)>,
    _on_ice_candidate: Closure<dyn Fn(RtcPeerConnectionIceEvent)>,
}

impl ConnectionWasmDataChannel {
    pub async fn new(
        ice_servers: Vec<IceServer>,
        _candidate_filter_prefix: Option<AnyIpCidr>,
    ) -> Result<Self> {
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

        let peer = RtcPeerConnection::new_with_configuration(&config).map_err(map_err)?;

        let candidates = Rc::new(RefCell::new(Vec::new()));
        let candidates2 = candidates.clone();
        let on_ice_candidate = Closure::new(move |candidate: RtcPeerConnectionIceEvent| {
            if let Some(candidate) = candidate.candidate()
                && let Some(candidate) = candidate.candidate().strip_prefix("candidate:")
            {
                candidates2.borrow_mut().push(candidate.to_string());
            }
        });
        peer.set_onicecandidate(Some(on_ice_candidate.as_ref().unchecked_ref()));

        let dc = Rc::new(RefCell::new(None));
        let dc2 = dc.clone();
        let on_dc = Closure::new(move |event: RtcDataChannelEvent| {
            let dc = dc2.clone();
            LOCAL_SET.with(|local_set| {
                local_set.spawn_local(async move {
                    *dc.borrow_mut() = Some(event.channel());
                });
            });
        });
        peer.set_ondatachannel(Some(on_dc.as_ref().unchecked_ref()));

        Ok(Self {
            peer,
            dc,
            candidates,
            sent: Arc::new(AtomicU64::new(0)),
            received: Arc::new(AtomicU64::new(0)),
            _on_dc: on_dc,
            _on_ice_candidate: on_ice_candidate,
        })
    }

    async fn wait_for_gathering_complete(&self) -> Result<()> {
        if self.peer.ice_gathering_state() == RtcIceGatheringState::Complete {
            return Ok(());
        }
        let pc = self.peer.clone();
        let mut cb = move |resolve: Function, _| {
            let pc2 = pc.clone();
            let on_ice_gather_state_change: Closure<dyn FnMut(RtcIceGatheringState)> =
                Closure::new(move |state: RtcIceGatheringState| {
                    if state == RtcIceGatheringState::Complete {
                        pc2.set_onicegatheringstatechange(None);
                        resolve.call0(&JsValue::NULL);
                    }
                });
            let on_ice_gather_state_change = on_ice_gather_state_change.into_js_value();

            pc.set_onicegatheringstatechange(Some(on_ice_gather_state_change.unchecked_ref()));
        };
        JsFuture::from(Promise::new(&mut cb))
            .await
            .map_err(map_err)?;
        Ok(())
    }

    pub async fn create_offer(&mut self) -> Result<(String, Vec<String>)> {
        *self.dc.borrow_mut() = Some(self.peer.create_data_channel("cryonet"));

        let offer = JsFuture::from(self.peer.create_offer())
            .await
            .map_err(map_err)?;
        let offer = Reflect::get(&offer, &JsValue::from_str("sdp"))
            .map_err(map_err)?
            .as_string()
            .unwrap();

        let offer_sdp = RtcSessionDescriptionInit::new(RtcSdpType::Offer);
        offer_sdp.set_sdp(&offer);
        JsFuture::from(self.peer.set_local_description(&offer_sdp))
            .await
            .map_err(map_err)?;

        self.wait_for_gathering_complete().await?;

        Ok((offer, self.candidates.borrow().clone()))
    }

    pub async fn create_answer(&mut self, offer: String) -> Result<(String, Vec<String>)> {
        let offer_sdp = RtcSessionDescriptionInit::new(RtcSdpType::Offer);
        offer_sdp.set_sdp(&offer);
        JsFuture::from(self.peer.set_remote_description(&offer_sdp))
            .await
            .map_err(map_err)?;
        let answer = JsFuture::from(self.peer.create_answer())
            .await
            .map_err(map_err)?;
        let answer = Reflect::get(&answer, &JsValue::from_str("sdp"))
            .map_err(map_err)?
            .as_string()
            .unwrap();

        let answer_sdp = RtcSessionDescriptionInit::new(RtcSdpType::Answer);
        answer_sdp.set_sdp(&answer);
        JsFuture::from(self.peer.set_local_description(&answer_sdp))
            .await
            .map_err(map_err)?;

        self.wait_for_gathering_complete().await?;

        Ok((answer, self.candidates.borrow().clone()))
    }

    pub async fn apply_answer(&self, answer: String) -> Result<()> {
        let answer_sdp = RtcSessionDescriptionInit::new(RtcSdpType::Answer);
        answer_sdp.set_sdp(&answer);
        JsFuture::from(self.peer.set_remote_description(&answer_sdp))
            .await
            .map_err(map_err)?;
        Ok(())
    }

    pub async fn add_candidates(&self, candidates: &Vec<String>) -> Result<()> {
        for candidate in candidates {
            let candidate = format!("candidate:{candidate}");
            let candidate = RtcIceCandidateInit::new(&candidate);
            candidate.set_sdp_m_line_index(Some(0));
            JsFuture::from(
                self.peer
                    .add_ice_candidate_with_opt_rtc_ice_candidate_init(Some(&candidate)),
            )
            .await
            .map_err(map_err)?;
        }
        Ok(())
    }
}

impl Drop for ConnectionWasmDataChannel {
    fn drop(&mut self) {
        self.peer.close();
    }
}

#[async_trait(?Send)]
impl Connection for ConnectionWasmDataChannel {
    async fn sender(&self) -> Result<Box<dyn ConnectionSender>> {
        let Some(dc) = self.dc.borrow().clone() else {
            return Err(anyhow!("Data channel not established"));
        };
        Ok(Box::new(ConnectionWasmDataChannelSender { dc }))
    }

    async fn receiver(&self) -> Result<Box<dyn ConnectionReceiver>> {
        let Some(dc) = self.dc.borrow().clone() else {
            return Err(anyhow!("Data channel not established"));
        };
        Ok(Box::new(ConnectionWasmDataChannelReceiver::new(dc)))
    }

    fn sent(&self) -> u64 {
        self.sent.load(Ordering::Relaxed)
    }

    fn received(&self) -> u64 {
        self.received.load(Ordering::Relaxed)
    }

    fn status(&self) -> ConnectionState {
        use RtcPeerConnectionState::*;
        match self.peer.connection_state() {
            New | Connecting => ConnectionState::Connecting,
            Connected | Disconnected => ConnectionState::Connected,
            _ => ConnectionState::Closed,
        }
    }

    async fn selected_candidate(&self) -> Option<String> {
        // TODO
        None
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

pub struct ConnectionWasmDataChannelSender {
    dc: RtcDataChannel,
}

#[async_trait(?Send)]
impl ConnectionSender for ConnectionWasmDataChannelSender {
    async fn send(&mut self, data: Bytes) -> Result<usize> {
        self.dc.send_with_u8_array(&data).map_err(map_err)?;
        Ok(data.len())
    }
}

pub struct ConnectionWasmDataChannelReceiver {
    dc: RtcDataChannel,
    rx: mpsc::Receiver<Bytes>,

    _on_message: Closure<dyn Fn(MessageEvent)>,
}

impl ConnectionWasmDataChannelReceiver {
    pub fn new(dc: RtcDataChannel) -> Self {
        let (tx, rx) = mpsc::channel(16384);
        let on_message = Closure::new(move |event: MessageEvent| {
            let data = event.data();
            let Some(data) = data.dyn_ref::<Uint8Array>() else {
                return;
            };
            let data = data.to_vec();
            let _ = tx.try_send(Bytes::from(data));
        });
        dc.set_onmessage(Some(on_message.as_ref().unchecked_ref()));
        Self {
            dc,
            rx,
            _on_message: on_message,
        }
    }
}

#[async_trait(?Send)]
impl ConnectionReceiver for ConnectionWasmDataChannelReceiver {
    async fn recv(&mut self) -> Result<(Bytes, SocketAddr)> {
        const ADDR: SocketAddr = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0));
        let data = self.rx.recv().await.ok_or(CryonetError::ChannelClosed)?;
        Ok((data, ADDR))
    }
}

fn map_err(error: JsValue) -> Error {
    anyhow!("{error:?}")
}
