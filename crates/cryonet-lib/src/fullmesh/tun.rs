use std::{
    collections::HashMap,
    sync::{Arc, Weak},
};

use anyhow::{Error, Result};
use bytes::Bytes;
use sactor::sactor;
use tokio::{select, sync::watch};
use tracing::error;
use tun_rs::{AsyncDevice, DeviceBuilder};

use crate::{
    errors::CryonetError,
    fullmesh::conn::{ConnectionReceiver, ConnectionSender},
    mesh::packet::NodeId,
};

pub struct TunManager {
    interface_prefix: String,
    enable_packet_information: bool,

    devices: HashMap<NodeId, Weak<AsyncDevice>>,
    tasks: HashMap<NodeId, watch::Sender<bool>>,
}

#[sactor(pub)]
impl TunManager {
    pub async fn new(interface_prefix: String, enable_packet_information: bool) -> Result<TunManagerHandle> {
        let (future, tm) = TunManager::run(move |_| TunManager {
            interface_prefix,
            enable_packet_information,
            devices: HashMap::new(),
            tasks: HashMap::new(),
        });
        tokio::task::spawn_local(future);
        Ok(tm)
    }

    pub async fn connected(&mut self, node_id: NodeId, sender: ConnectionSender, receiver: ConnectionReceiver) -> Result<()> {
        if let Some(stop) = self.tasks.remove(&node_id) {
            let _ = stop.send(true);
        }
        let dev_send = self.create_device(node_id)?;
        let dev_recv = dev_send.clone();
        let stop_tx = watch::channel(false).0;
        tokio::spawn(send_loop(node_id, sender, dev_send, stop_tx.subscribe()));
        tokio::spawn(recv_loop(node_id, receiver, dev_recv, stop_tx.subscribe()));
        self.tasks.insert(node_id, stop_tx);
        Ok(())
    }

    pub async fn disconnected(&mut self, node_id: NodeId) {
        if let Some(stop_tx) = self.tasks.remove(&node_id) {
            let _ = stop_tx.send(true);
        }
    }

    fn create_device(&mut self, node_id: NodeId) -> Result<Arc<AsyncDevice>> {
        let create_device = || -> Result<AsyncDevice> {
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
    }

    #[handle_error]
    fn handle_error(&mut self, err: &Error) {
        error!("Error: {:?}", err);
    }
}

async fn send_loop(node_id: NodeId, mut sender: ConnectionSender, device: Arc<AsyncDevice>, mut stop: watch::Receiver<bool>) {
    let mut buf = [0u8; 2000];
    loop {
        select! {
            _ = stop.changed() => break,
            res = device.recv(&mut buf) => {
                let size = match res {
                    Ok(s) => s,
                    Err(err) => {
                        error!("Failed to read from TUN device for node {:X}: {}", node_id, err);
                        break;
                    }
                };
                let bytes = Bytes::copy_from_slice(&buf[..size]);
                if let Err(err) = sender.send(bytes).await {
                    error!("Failed to send to node {:X}: {}", node_id, err);
                    continue;
                }
            }
        }
    }
}

async fn recv_loop(node_id: NodeId, mut receiver: ConnectionReceiver, device: Arc<AsyncDevice>, mut stop: watch::Receiver<bool>) {
    loop {
        select! {
            _ = stop.changed() => break,
            res = receiver.recv() => {
                let (packet, _) = match res {
                    Ok(p) => p,
                    Err(err) => match err.downcast_ref::<CryonetError>() {
                        Some(CryonetError::ChannelClosed) => break,
                        _ => {
                            error!("Failed to receive from node {:X}: {}", node_id, err);
                            continue;
                        }
                    }
                };
                if let Err(err) = device.send(&packet).await {
                    error!("Failed to write to TUN device for node {:X}: {}", node_id, err);
                    break;
                }
            }
        }
    }
}
