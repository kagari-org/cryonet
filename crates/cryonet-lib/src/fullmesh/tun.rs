use std::{
    collections::HashMap,
    sync::{Arc, Weak},
};

use bytes::Bytes;
use futures::future::select_all;
use sactor::{error::{SactorError, SactorResult}, sactor};
use tokio::{
    select,
    sync::broadcast::{self, error::RecvError},
    task::JoinHandle,
};
use tracing::{debug, error};
use tun_rs::{AsyncDevice, DeviceBuilder};

use crate::{fullmesh::FullMeshHandle, mesh::packet::NodeId};

pub struct TunManager {
    handle: TunManagerHandle,
    fm: FullMeshHandle,

    refresh: broadcast::Receiver<()>,

    interface_prefix: String,
    enable_packet_information: bool,

    devices: HashMap<NodeId, Weak<AsyncDevice>>,
    receivers: HashMap<NodeId, JoinHandle<()>>,
    senders: HashMap<NodeId, JoinHandle<()>>,
}

#[sactor(pub)]
impl TunManager {
    pub async fn new(fm: FullMeshHandle, interface_prefix: String, enable_packet_information: bool) -> SactorResult<TunManagerHandle> {
        let refresh = fm.subscribe_refresh().await?;
        let (future, tm) = TunManager::run(move |handle| TunManager {
            handle,
            fm,
            refresh,
            interface_prefix,
            enable_packet_information,
            devices: HashMap::new(),
            receivers: HashMap::new(),
            senders: HashMap::new(),
        });
        tokio::spawn(future);
        Ok(tm)
    }

    #[select]
    fn select(&mut self) -> Vec<Selection<'_>> {
        vec![selection!(self.refresh.recv().await, handle_refresh, it => it)]
    }

    #[no_reply]
    async fn handle_refresh(&mut self, refresh: Result<(), broadcast::error::RecvError>) -> SactorResult<()> {
        if let Err(broadcast::error::RecvError::Closed) = refresh {
            self.handle.stop();
            return Ok(());
        }
        let receivers = self.fm.get_receivers().await?;
        let senders = self.fm.get_senders().await?;
        let mut create_device = |node_id: NodeId| -> SactorResult<Arc<AsyncDevice>> {
            let create_device = || -> SactorResult<AsyncDevice> {
                let mut builder = DeviceBuilder::new().mtu(1280).name(format!("{}{:X}", self.interface_prefix, node_id)).enable(true);
                if self.enable_packet_information {
                    builder = builder.packet_information(true);
                }
                Ok(builder.build_async()?)
            };
            match self.devices.get(&node_id) {
                Some(device) => match device.upgrade() {
                    Some(device) => Ok(device),
                    None => {
                        let device = Arc::new(create_device()?);
                        self.devices.insert(node_id, Arc::downgrade(&device));
                        Ok(device)
                    }
                },
                None => {
                    let device = Arc::new(create_device()?);
                    self.devices.insert(node_id, Arc::downgrade(&device));
                    Ok(device)
                }
            }
        };
        for (node_id, _) in receivers {
            if let Some(handle) = self.receivers.get(&node_id)
                && !handle.is_finished()
            {
                continue;
            }
            let device = create_device(node_id)?;
            let fm = self.fm.clone();
            let handle = tokio::spawn(async move {
                let result = receive(node_id, fm, device).await;
                if let Err(err) = result {
                    error!("Error in receive loop for node {:X}: {}", node_id, err);
                }
            });
            self.receivers.insert(node_id, handle);
        }
        for (node_id, _) in senders {
            if let Some(handle) = self.senders.get(&node_id)
                && !handle.is_finished()
            {
                continue;
            }
            let device = create_device(node_id)?;
            let fm = self.fm.clone();
            let handle = tokio::spawn(async move {
                let result = send(node_id, fm, device).await;
                if let Err(err) = result {
                    error!("Error in send loop for node {:X}: {}", node_id, err);
                }
            });
            self.senders.insert(node_id, handle);
        }
        Ok(())
    }

    #[handle_error]
    fn handle_error(&mut self, err: &SactorError) {
        error!("Error: {:?}", err);
    }
}

async fn receive(node_id: NodeId, fm: FullMeshHandle, device: Arc<AsyncDevice>) -> SactorResult<()> {
    let mut refresh = fm.subscribe_refresh().await?;
    'outer: loop {
        let mut receivers = fm.get_receivers().await?;
        let receivers = receivers.remove(&node_id);
        let Some(receivers) = receivers else {
            break;
        };
        if receivers.is_empty() {
            break;
        }
        loop {
            let recv: Vec<_> = receivers.iter().map(|conn| Box::pin(conn.recv())).collect();
            select! {
                result = refresh.recv() => {
                    match result {
                        Ok(_) | Err(RecvError::Lagged(_)) => break,
                        Err(_) => break 'outer,
                    }
                },
                (packet, _, _) = select_all(recv) => {
                    let packet = match packet {
                        Ok(packet) => packet,
                        Err(err) => {
                            debug!("Failed to receive from node {:X}: {}", node_id, err);
                            break;
                        },
                    };
                    if let Err(err) = device.send(&packet).await {
                        error!("Failed to write to TUN device for node {:X}: {}", node_id, err);
                        break;
                    }
                },
            }
        }
    }
    Ok(())
}

async fn send(node_id: NodeId, fm: FullMeshHandle, device: Arc<AsyncDevice>) -> SactorResult<()> {
    let mut refresh = fm.subscribe_refresh().await?;
    'outer: loop {
        let mut senders = fm.get_senders().await?;
        let senders = senders.remove(&node_id);
        let Some(sender) = senders else {
            break;
        };
        let mut packet = [0u8; 2000];
        loop {
            select! {
                result = refresh.recv() => {
                    match result {
                        Ok(_) | Err(RecvError::Lagged(_)) => break,
                        Err(_) => break 'outer,
                    }
                },
                size = device.recv(&mut packet) => {
                    let size = match size {
                        Ok(size) => size,
                        Err(err) => {
                            error!("Failed to read from TUN device for node {:X}: {}", node_id, err);
                            break;
                        },
                    };
                    let bytes = Bytes::copy_from_slice(&packet[..size]);
                    if let Err(err) = sender.send(bytes).await {
                        error!("Failed to send to node {:X}: {}", node_id, err);
                        break;
                    }
                },
            }
        }
    }
    Ok(())
}
