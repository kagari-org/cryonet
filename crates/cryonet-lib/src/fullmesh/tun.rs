use std::{
    collections::HashMap,
    sync::{Arc, Weak},
};

use anyhow::{Error, Result};
use bytes::Bytes;
use sactor::sactor;
use tokio::{sync::mpsc, task::JoinHandle};
use tracing::error;
use tun_rs::{AsyncDevice, DeviceBuilder};

use crate::{
    fullmesh::{
        FullMeshEvent,
        conn::{ConnectionReceiver, ConnectionSender},
    },
    mesh::packet::NodeId,
};

pub struct TunManager {
    handle: TunManagerHandle,

    fm_event_rx: mpsc::Receiver<FullMeshEvent>,

    interface_prefix: String,
    enable_packet_information: bool,

    devices: HashMap<NodeId, Weak<AsyncDevice>>,
    tasks: HashMap<NodeId, (JoinHandle<()>, JoinHandle<()>)>,
}

#[sactor(pub)]
impl TunManager {
    pub async fn new(fm_event_rx: mpsc::Receiver<FullMeshEvent>, interface_prefix: String, enable_packet_information: bool) -> Result<TunManagerHandle> {
        let (future, tm) = TunManager::run(move |handle| TunManager {
            handle,
            fm_event_rx,
            interface_prefix,
            enable_packet_information,
            devices: HashMap::new(),
            tasks: HashMap::new(),
        });
        tokio::task::spawn_local(future);
        Ok(tm)
    }

    #[select]
    fn select(&mut self) -> Vec<Selection<'_>> {
        vec![selection!(self.fm_event_rx.recv().await, handle_event, it => it)]
    }

    #[no_reply]
    async fn handle_event(&mut self, event: Option<FullMeshEvent>) -> Result<()> {
        let Some(event) = event else {
            self.handle.stop();
            return Ok(());
        };
        let mut create_device = |node_id: NodeId| -> Result<Arc<AsyncDevice>> {
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
        };
        match event {
            FullMeshEvent::Connected { node_id, sender, receiver } => {
                // stop old tasks if any
                if let Some((r, s)) = self.tasks.remove(&node_id) {
                    r.abort();
                    s.abort();
                }
                let device = create_device(node_id)?;
                let dev_recv = device.clone();
                let dev_send = device;
                let recv_handle = tokio::spawn(async move {
                    if let Err(err) = recv_loop(node_id, receiver, dev_recv).await {
                        error!("Receive loop error for node {:X}: {}", node_id, err);
                    }
                });
                let send_handle = tokio::spawn(async move {
                    if let Err(err) = send_loop(node_id, sender, dev_send).await {
                        error!("Send loop error for node {:X}: {}", node_id, err);
                    }
                });
                self.tasks.insert(node_id, (recv_handle, send_handle));
            }
            FullMeshEvent::Disconnected { node_id } => {
                if let Some((r, s)) = self.tasks.remove(&node_id) {
                    r.abort();
                    s.abort();
                }
            }
        }
        Ok(())
    }

    #[handle_error]
    fn handle_error(&mut self, err: &Error) {
        error!("Error: {:?}", err);
    }
}

async fn recv_loop(node_id: NodeId, mut receiver: ConnectionReceiver, device: Arc<AsyncDevice>) -> Result<()> {
    loop {
        let (packet, _) = receiver.recv().await?;
        if let Err(err) = device.send(&packet).await {
            error!("Failed to write to TUN device for node {:X}: {}", node_id, err);
            break;
        }
    }
    Ok(())
}

async fn send_loop(node_id: NodeId, mut sender: ConnectionSender, device: Arc<AsyncDevice>) -> Result<()> {
    let mut buf = [0u8; 2000];
    loop {
        let size = device.recv(&mut buf).await?;
        let bytes = Bytes::copy_from_slice(&buf[..size]);
        if let Err(err) = sender.send(bytes).await {
            error!("Failed to send to node {:X}: {}", node_id, err);
            break;
        }
    }
    Ok(())
}
