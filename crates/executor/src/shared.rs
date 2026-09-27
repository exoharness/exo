use exoharness::{EventId, Result, TurnHandle};
use tokio::sync::mpsc;

use crate::ExecutionStreamEvent;

pub(crate) async fn finalize_turn(turn: &dyn TurnHandle, result: Result<()>) -> Result<EventId> {
    match result {
        Ok(()) => turn.finish().await,
        Err(error) => {
            if let Err(persist_error) = turn
                .add_events(vec![exoharness::EventData::Error {
                    message: format!("{error:#}"),
                    metadata: None,
                }])
                .await
            {
                return Err(error.context(format!("failed to persist turn error: {persist_error}")));
            }
            match turn.finish().await {
                Ok(_) => Err(error),
                Err(finish_error) => {
                    Err(error.context(format!("also failed to finish turn: {finish_error}")))
                }
            }
        }
    }
}

pub(crate) fn try_send_stream_event(
    event_tx: &mpsc::UnboundedSender<Result<ExecutionStreamEvent>>,
    event: ExecutionStreamEvent,
) {
    if event_tx.send(Ok(event)).is_err() {}
}

pub(crate) const HISTORY_CACHE_NAME: &str = "history cache poisoned";
