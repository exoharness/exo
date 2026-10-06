use super::*;

fn queued(principal: &str, attention: TurnAttention) -> QueuedTurn<u32> {
    QueuedTurn {
        turn: TurnRecord {
            id: Uuid7::now(),
            session_id: Uuid7::now(),
        },
        work: 42,
        principal: Some(principal.into()),
        idempotency_key: None,
        attention,
        started: false,
        cancelled: false,
    }
}

#[tokio::test]
async fn interruption_is_scoped_and_acknowledgment_cannot_remove_the_next_head() -> Result<()> {
    let coordinator = StoredTurnCoordinator::in_memory();
    let thread = TurnThread {
        agent_id: Uuid7::now(),
        thread_id: Uuid7::now(),
    };
    let first = queued("alice", TurnAttention::Wake);
    coordinator.enqueue(thread, first.clone()).await?;
    let lease = coordinator.claim(thread).await?.unwrap();
    assert!(coordinator.claim(thread).await?.is_none());
    coordinator.start(&lease, first.turn.id).await?;
    let other = queued("bob", TurnAttention::Interrupt);
    assert!(
        coordinator
            .enqueue(thread, other.clone())
            .await?
            .interrupted
            .is_none()
    );
    assert!(!coordinator.cancelled(&lease, first.turn.id).await?);
    assert_eq!(
        coordinator
            .cancel(
                thread,
                first.turn.id,
                CancelAuthority::Submitter("bob".into())
            )
            .await?,
        CancelTurnOutcome::NotAccessible
    );
    let replacement = queued("alice", TurnAttention::Interrupt);
    assert_eq!(
        coordinator
            .enqueue(thread, replacement.clone())
            .await?
            .interrupted,
        Some(first.turn.id)
    );
    assert!(coordinator.cancelled(&lease, first.turn.id).await?);
    assert!(!coordinator.release_if_idle(&lease).await?);
    coordinator.acknowledge(&lease, first.turn.id).await?;
    coordinator.acknowledge(&lease, first.turn.id).await?;
    assert_eq!(
        coordinator.peek(&lease).await?.unwrap().turn.id,
        other.turn.id
    );
    coordinator.acknowledge(&lease, other.turn.id).await?;
    coordinator.acknowledge(&lease, replacement.turn.id).await?;
    assert!(coordinator.release_if_idle(&lease).await?);
    assert!(coordinator.peek(&lease).await.is_err());
    let next = coordinator.claim(thread).await?.unwrap();
    assert_ne!(next.token(), lease.token());
    Ok(())
}

#[tokio::test]
async fn idempotency_survives_acknowledgment_and_is_scoped_to_the_submitter() -> Result<()> {
    let coordinator = StoredTurnCoordinator::in_memory();
    let thread = TurnThread {
        agent_id: Uuid7::now(),
        thread_id: Uuid7::now(),
    };
    let mut first = queued("alice", TurnAttention::Wake);
    first.idempotency_key = Some("request-1".into());
    coordinator.enqueue(thread, first.clone()).await?;
    let lease = coordinator.claim(thread).await?.unwrap();
    coordinator.acknowledge(&lease, first.turn.id).await?;
    let mut retry = first.clone();
    retry.turn = TurnRecord {
        id: Uuid7::now(),
        session_id: Uuid7::now(),
    };
    let receipt = coordinator.enqueue(thread, retry.clone()).await?;
    assert!(receipt.duplicate);
    assert_eq!(receipt.turn, first.turn);
    retry.principal = Some("bob".into());
    assert!(!coordinator.enqueue(thread, retry).await?.duplicate);
    assert!(!coordinator.release_if_idle(&lease).await?);
    Ok(())
}
