use super::*;
use crate::Uuid7;

fn queued(principal: &str, attention: TurnAttention) -> TurnSubmission<u32> {
    TurnSubmission {
        turn: TurnRecord {
            id: Uuid7::now(),
            session_id: Uuid7::now(),
        },
        work: 42,
        principal: Some(principal.into()),
        idempotency_key: None,
        attention,
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
    coordinator.enqueue_turn(thread, first.clone(), ()).await?;
    let lease = coordinator.claim(thread).await?.unwrap();
    assert!(coordinator.claim(thread).await?.is_none());
    coordinator.start(&lease, first.turn.id).await?;
    let other = queued("bob", TurnAttention::Interrupt);
    coordinator.enqueue_turn(thread, other.clone(), ()).await?;
    assert_eq!(
        coordinator.peek_turn(&lease).await?.unwrap().control,
        TurnControl::Run
    );
    assert_eq!(
        coordinator
            .cancel_turn(thread, first.turn.id, "bob", false)
            .await?,
        TurnControlOutcome::NotAccessible
    );
    let replacement = queued("alice", TurnAttention::Interrupt);
    coordinator
        .enqueue_turn(thread, replacement.clone(), ())
        .await?;
    assert!(coordinator.turn_cancelled(&lease, first.turn.id).await?);
    assert_eq!(
        coordinator.peek_turn(&lease).await?.unwrap().control,
        TurnControl::Cancel
    );
    assert!(!coordinator.release_if_idle(&lease).await?);
    coordinator.complete_turn(&lease, first.turn.id).await?;
    coordinator.complete_turn(&lease, first.turn.id).await?;
    assert_eq!(
        coordinator.peek_turn(&lease).await?.unwrap().turn.id,
        other.turn.id
    );
    coordinator.complete_turn(&lease, other.turn.id).await?;
    coordinator
        .complete_turn(&lease, replacement.turn.id)
        .await?;
    assert!(coordinator.release_if_idle(&lease).await?);
    assert!(coordinator.peek_turn(&lease).await.is_err());
    let next = coordinator.claim(thread).await?.unwrap();
    assert!(!Arc::ptr_eq(&next.identity, &lease.identity));
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
    coordinator.enqueue_turn(thread, first.clone(), ()).await?;
    let lease = coordinator.claim(thread).await?.unwrap();
    coordinator.complete_turn(&lease, first.turn.id).await?;
    let mut retry = first.clone();
    retry.turn = TurnRecord {
        id: Uuid7::now(),
        session_id: Uuid7::now(),
    };
    let receipt = coordinator.enqueue_turn(thread, retry.clone(), ()).await?;
    assert!(receipt.duplicate);
    assert_eq!(receipt.turn, first.turn);
    retry.principal = Some("bob".into());
    assert!(!coordinator.enqueue_turn(thread, retry, ()).await?.duplicate);
    assert!(!coordinator.release_if_idle(&lease).await?);
    Ok(())
}

#[tokio::test]
async fn suspension_preserves_order_and_control_watch_has_no_registration_gap() -> Result<()> {
    use futures::StreamExt;
    let coordinator = StoredTurnCoordinator::in_memory();
    let thread = TurnThread {
        agent_id: Uuid7::now(),
        thread_id: Uuid7::now(),
    };
    let first = queued("alice", TurnAttention::Wake);
    let second = queued("alice", TurnAttention::Wake);
    coordinator.enqueue_turn(thread, first.clone(), ()).await?;
    coordinator.enqueue_turn(thread, second, ()).await?;
    let lease = coordinator.claim(thread).await?.unwrap();
    coordinator
        .control(
            thread,
            first.turn.id,
            TurnAuthority::Submitter("alice".into()),
            TurnControl::Suspend,
        )
        .await?;
    let started = coordinator.start(&lease, first.turn.id).await?;
    let head = started.head;
    let mut control = started.control;
    assert_eq!(control.next().await.unwrap()?, TurnControl::Suspend);
    assert!(!head.started);
    assert_eq!(head.turn, first.turn);
    assert!(coordinator.release_if_idle(&lease).await?);
    drop(control);
    drop(lease);
    assert!(
        coordinator
            .enqueue_turn(thread, first.clone(), ())
            .await?
            .duplicate
    );
    let lease = coordinator.claim(thread).await?.unwrap();
    assert_eq!(
        coordinator.peek_turn(&lease).await?.unwrap().control,
        TurnControl::Suspend
    );
    assert_eq!(
        coordinator
            .control(
                thread,
                first.turn.id,
                TurnAuthority::Submitter("bob".into()),
                TurnControl::Run
            )
            .await?,
        TurnControlOutcome::NotAccessible
    );
    coordinator
        .control(
            thread,
            first.turn.id,
            TurnAuthority::ThreadOwner,
            TurnControl::Run,
        )
        .await?;
    let mut control = coordinator.start(&lease, first.turn.id).await?.control;
    assert_eq!(control.next().await.unwrap()?, TurnControl::Run);
    coordinator
        .control(
            thread,
            first.turn.id,
            TurnAuthority::ThreadOwner,
            TurnControl::Cancel,
        )
        .await?;
    assert_eq!(control.next().await.unwrap()?, TurnControl::Cancel);
    assert!(
        coordinator
            .control(
                thread,
                first.turn.id,
                TurnAuthority::ThreadOwner,
                TurnControl::Run
            )
            .await
            .is_err()
    );
    coordinator.complete_turn(&lease, first.turn.id).await?;
    drop(control);
    drop(lease);
    assert!(coordinator.locks.threads.lock().unwrap().is_empty());
    Ok(())
}
