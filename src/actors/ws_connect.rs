use std::collections::HashSet;

use ractor::{async_trait, cast, registry::where_is, Actor, ActorProcessingErr, ActorRef};
use tracing::{error, info};

use crate::{actors::net::NetActorMsg, utils::ws::connect, CONFIG};

#[derive(Debug)]
pub(crate) enum WSConnectActorMsg {
    Connect,
    Connected(String),
}

#[derive(Debug)]
pub(crate) struct WSConnectActorState {
    connecting: HashSet<String>,
}

pub(crate) struct WSConnectActor;

#[async_trait]
impl Actor for WSConnectActor {
    type Msg = WSConnectActorMsg;
    type State = WSConnectActorState;
    type Arguments = ();

    async fn pre_start(
        &self,
        _: ActorRef<Self::Msg>,
        _: Self::Arguments,
    ) -> Result<Self::State, ActorProcessingErr> {
        Ok(WSConnectActorState {
            connecting: HashSet::new(),
        })
    }

    async fn handle(
        &self,
        myself: ActorRef<Self::Msg>,
        message: Self::Msg,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        let cfg = CONFIG.get().unwrap();
        match message {
            WSConnectActorMsg::Connect => {
                let ws_servers = HashSet::from_iter(cfg.ws_servers.clone());
                let diff = ws_servers.difference(&state.connecting).cloned();
                for addr in diff {
                    let myself = myself.clone();
                    tokio::spawn(async move {
                        info!("connecting to {addr}");
                        let result: anyhow::Result<()> = try {
                            let peer = connect(&addr).await?;
                            let net: ActorRef<NetActorMsg> = where_is("net".to_string())
                                .unwrap().into();
                            cast!(net, NetActorMsg::NewPeer(peer))?;
                        };
                        if let Err(err) = result {
                            let trace = err.backtrace();
                            error!("failed to connect ws: {err}\n{trace}");
                        };
                        if let Err(err) = cast!(myself, WSConnectActorMsg::Connected(addr)) {
                            error!("failed to send connected event: {err}");
                        }
                    });
                }
                state.connecting = ws_servers;
            },
            WSConnectActorMsg::Connected(addr) => {
                state.connecting.remove(&addr);
            },
        };
        Ok(())
    }
}
