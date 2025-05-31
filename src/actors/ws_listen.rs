use ractor::{async_trait, cast, registry::where_is, Actor, ActorProcessingErr, ActorRef};
use tokio::net::TcpListener;
use tracing::error;

use crate::{utils::ws::accept, CONFIG};

use super::net::NetActorMsg;

#[derive(Debug)]
pub(crate) enum WSListenActorMsg {
    Accept,
}

#[derive(Debug)]
pub(crate) struct WSListenActorState {
    listener: TcpListener,
}

pub(crate) struct WSListenActor;

#[async_trait]
impl Actor for WSListenActor {
    type Msg = WSListenActorMsg;
    type State = WSListenActorState;
    type Arguments = ();

    async fn pre_start(
        &self,
        myself: ActorRef<Self::Msg>,
        _: Self::Arguments,
    ) -> Result<Self::State, ActorProcessingErr> {
        let listener = TcpListener::bind(CONFIG.get().unwrap().listen).await?;
        cast!(myself, WSListenActorMsg::Accept)?;
        Ok(WSListenActorState { listener })
    }

    async fn handle(
        &self,
        myself: ActorRef<Self::Msg>,
        msg: Self::Msg,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        match state.listener.accept().await {
            Err(err) => error!("failed to accept {err}"),
            Ok((stream, addr)) => {
                tokio::spawn(async move {
                    let result: anyhow::Result<()> = try {
                        let peer = accept(stream, addr).await?;
                        let net: ActorRef<NetActorMsg> = where_is("net".to_string())
                            .unwrap().into();
                        cast!(net, NetActorMsg::NewPeer(peer))?;
                    };
                    if let Err(err) = result {
                        error!("failed to shake with peer: {err}");
                    }
                });
            },
        }

        assert!(matches!(msg, WSListenActorMsg::Accept));
        cast!(myself, msg)?;
        Ok(())
    }
}