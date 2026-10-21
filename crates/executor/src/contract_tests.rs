use std::time::Duration;

use anyhow::{Context, Result, bail};
use exoharness::{AgentId, EventData};
use futures::StreamExt;

use crate::{Runtime, SendRequest, harness::HarnessTurnKey};

pub async fn test_provider(runtime: &Runtime, agent_id: AgentId, release: impl Fn()) -> Result<()> {
    let agent = runtime
        .get_agent(&agent_id.to_string())
        .await?
        .context("agent")?;
    let thread = agent.new_thread(Default::default()).await?;
    let request = SendRequest {
        input: vec![crate::harness_helpers::user_message("hello")],
        session_id: None,
    };
    let (turn, mut stream) = runtime
        .start_turn(
            agent.clone(),
            thread.clone(),
            request.clone(),
            true,
            None,
            Default::default(),
        )
        .await?;
    release();
    let completed = tokio::time::timeout(Duration::from_secs(30), async {
        while let Some(event) = stream.next().await {
            if let crate::ExecutionStreamEvent::Completed(result) = event? {
                return Ok(result);
            }
        }
        bail!("turn did not complete")
    })
    .await??;
    assert_eq!(completed.turn_id, turn.id);
    assert_eq!(completed.session_id, turn.session_id);
    let history = thread.get_events(None).await?.events;
    assert_eq!(
        history
            .iter()
            .filter(|event| matches!(event.data, EventData::TurnStarted { .. }))
            .count(),
        1
    );
    assert_eq!(
        history
            .iter()
            .filter(|event| matches!(event.data, EventData::TurnEnded))
            .count(),
        1
    );
    assert_eq!(
        history
            .iter()
            .find(|event| event.turn_id == Some(turn.id)
                && matches!(event.data, EventData::TurnEnded))
            .context("terminal event")?
            .id,
        completed.latest_event_id
    );

    let (turn, mut stream) = runtime
        .start_turn(
            agent,
            thread.clone(),
            request,
            true,
            None,
            Default::default(),
        )
        .await?;
    runtime
        .cancel_turn(HarnessTurnKey::new(agent_id, thread.record().id, turn.id))
        .await?;
    tokio::time::timeout(Duration::from_secs(30), async {
        while let Some(event) = stream.next().await {
            if let Err(error) = event {
                assert!(error.to_string().contains("cancel"));
                return Ok(());
            }
        }
        bail!("cancellation did not finish the turn")
    })
    .await??;
    Ok(())
}
