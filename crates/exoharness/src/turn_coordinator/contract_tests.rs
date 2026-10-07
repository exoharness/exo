use super::*;
use crate::Uuid7;
use futures::StreamExt;

fn submission<Work>(work: Work, principal: &str, attention: TurnAttention) -> TurnSubmission<Work> {
    TurnSubmission {
        turn: TurnRecord {
            id: Uuid7::now(),
            session_id: Uuid7::now(),
        },
        work,
        principal: Some(principal.into()),
        options: TurnOptions {
            attention,
            ..Default::default()
        },
    }
}

pub async fn interruption_is_scoped_and_acknowledgment_cannot_remove_the_next_head<
    Work: Clone + Send + Sync,
>(
    coordinator: &dyn TurnQueue<Work>,
    thread: TurnThread,
    work: Work,
) -> Result<()> {
    let first = submission(work.clone(), "alice", TurnAttention::Wake);
    coordinator.enqueue(thread, first.clone()).await?;
    let lease = coordinator.claim(thread).await?.unwrap();
    assert!(coordinator.claim(thread).await?.is_none());
    let other = submission(work.clone(), "bob", TurnAttention::Interrupt);
    coordinator.enqueue(thread, other.clone()).await?;
    assert_eq!(
        coordinator.peek(&lease).await?.unwrap().control,
        TurnControl::Run
    );
    assert_eq!(
        coordinator
            .cancel(
                thread,
                first.turn.id,
                TurnAuthority::Submitter("bob".into())
            )
            .await?,
        TurnControlOutcome::NotAccessible
    );
    let replacement = submission(work.clone(), "alice", TurnAttention::Interrupt);
    coordinator.enqueue(thread, replacement.clone()).await?;
    assert_eq!(
        coordinator.peek(&lease).await?.unwrap().control,
        TurnControl::Cancel
    );
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
    assert!(coordinator.peek(&next).await?.is_none());
    assert!(coordinator.peek(&lease).await.is_err());
    assert!(coordinator.release_if_idle(&next).await?);
    Ok(())
}

pub async fn idempotency_survives_acknowledgment_and_is_scoped_to_the_submitter<
    Work: Clone + Send + Sync,
>(
    coordinator: &dyn TurnQueue<Work>,
    thread: TurnThread,
    work: Work,
) -> Result<()> {
    let mut first = submission(work.clone(), "alice", TurnAttention::Wake);
    first.options.idempotency_key = Some("request-1".into());
    coordinator.enqueue(thread, first.clone()).await?;
    let lease = coordinator.claim(thread).await?.unwrap();
    coordinator.acknowledge(&lease, first.turn.id).await?;
    let mut retry = first.clone();
    let receipt = coordinator.enqueue(thread, retry.clone()).await?;
    assert!(receipt.duplicate);
    assert_eq!(receipt.turn, first.turn);
    retry.principal = Some("bob".into());
    retry.turn = TurnRecord {
        id: Uuid7::now(),
        session_id: Uuid7::now(),
    };
    assert!(!coordinator.enqueue(thread, retry).await?.duplicate);
    assert!(!coordinator.release_if_idle(&lease).await?);
    let head = coordinator.peek(&lease).await?.unwrap();
    coordinator.acknowledge(&lease, head.turn.id).await?;
    assert!(coordinator.release_if_idle(&lease).await?);
    Ok(())
}

pub async fn suspension_preserves_order_and_control_watch_has_no_registration_gap<
    Work: Clone + Send + Sync,
>(
    coordinator: &dyn TurnQueue<Work>,
    thread: TurnThread,
    work: Work,
) -> Result<()> {
    let first = submission(work.clone(), "alice", TurnAttention::Wake);
    let second = submission(work.clone(), "alice", TurnAttention::Wake);
    coordinator.enqueue(thread, first.clone()).await?;
    coordinator.enqueue(thread, second.clone()).await?;
    let lease = coordinator.claim(thread).await?.unwrap();
    coordinator
        .set_suspended(
            thread,
            first.turn.id,
            TurnAuthority::Submitter("alice".into()),
            true,
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
    assert!(coordinator.enqueue(thread, first.clone()).await?.duplicate);
    let lease = coordinator.claim(thread).await?.unwrap();
    assert_eq!(
        coordinator.peek(&lease).await?.unwrap().control,
        TurnControl::Suspend
    );
    assert_eq!(
        coordinator
            .set_suspended(
                thread,
                first.turn.id,
                TurnAuthority::Submitter("bob".into()),
                false
            )
            .await?,
        TurnControlOutcome::NotAccessible
    );
    coordinator
        .set_suspended(thread, first.turn.id, TurnAuthority::ThreadOwner, false)
        .await?;
    let mut control = coordinator.start(&lease, first.turn.id).await?.control;
    assert_eq!(control.next().await.unwrap()?, TurnControl::Run);
    coordinator
        .cancel(thread, first.turn.id, TurnAuthority::ThreadOwner)
        .await?;
    assert_eq!(control.next().await.unwrap()?, TurnControl::Cancel);
    assert!(
        coordinator
            .set_suspended(thread, first.turn.id, TurnAuthority::ThreadOwner, false)
            .await
            .is_err()
    );
    coordinator.acknowledge(&lease, first.turn.id).await?;
    drop(control);
    assert_eq!(coordinator.peek(&lease).await?.unwrap().turn, second.turn);
    coordinator.acknowledge(&lease, second.turn.id).await?;
    assert!(coordinator.release_if_idle(&lease).await?);
    Ok(())
}
