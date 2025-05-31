use ractor::{Actor, ActorProcessingErr, ActorRef, SupervisionEvent, async_trait};
use tracing::{error, info};

use crate::NetActor;

pub(crate) struct SupervisorActor;

#[async_trait]
impl Actor for SupervisorActor {
    type Msg = ();
    type State = ();
    type Arguments = ();

    async fn pre_start(
        &self,
        myself: ActorRef<Self::Msg>,
        _: Self::Arguments,
    ) -> Result<Self::State, ActorProcessingErr> {
        info!("spawn NetActor");
        Actor::spawn_linked(Some("net".to_string()), NetActor, (), myself.get_cell()).await?;
        Ok(())
    }

    async fn handle_supervisor_evt(
        &self,
        myself: ActorRef<Self::Msg>,
        message: SupervisionEvent,
        _: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        match message {
            SupervisionEvent::ActorTerminated(_, _, reason) => {
                error!("actor terminated: {reason:?}");
                myself.stop(None);
            }
            SupervisionEvent::ActorFailed(_, err) => {
                error!("actor failed: {err}");
                myself.stop(None);
            }
            _ => {}
        }
        Ok(())
    }
}
