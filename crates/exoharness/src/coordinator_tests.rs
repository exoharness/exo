use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use lingua::Message;
use lingua::universal::UserContent;
use tempfile::TempDir;

use crate::coordinator::BasicTurnCoordinator;
use crate::storage::BasicObjectStore;
use crate::test_support::local_test_config;
use crate::{
    BasicExoHarness, CancelTurnOutcome, CompleteTurnOutcome, ConversationHandle,
    EnqueueTurnRequest, EventKind, EventQuery, ExoHarness, NewAgentRequest, NewConversationRequest,
    TurnCoordinator,
};

const LEASE_TTL: Duration = Duration::from_secs(30);

#[derive(Clone, Copy)]
enum CoordinatorKind {
    InMemory,
    FileBacked,
}

/// The same coordinator implementation over its two stores: coordination
/// semantics must not depend on the storage medium.
async fn make_coordinator(
    kind: CoordinatorKind,
    harness: &Arc<dyn ExoHarness>,
    root: &Path,
    lease_ttl: Duration,
) -> Arc<dyn TurnCoordinator> {
    match kind {
        CoordinatorKind::InMemory => Arc::new(BasicTurnCoordinator::in_memory(
            Arc::clone(harness),
            lease_ttl,
        )),
        CoordinatorKind::FileBacked => {
            let storage = BasicObjectStore::local_filesystem(root)
                .await
                .expect("storage");
            Arc::new(BasicTurnCoordinator::new(
                Arc::clone(harness),
                storage,
                lease_ttl,
            ))
        }
    }
}

async fn make_harness(root: &Path) -> Arc<dyn ExoHarness> {
    Arc::new(
        BasicExoHarness::new(local_test_config(root))
            .await
            .expect("harness should initialize"),
    )
}

async fn make_conversation(
    harness: &Arc<dyn ExoHarness>,
    slug: &str,
) -> Arc<dyn ConversationHandle> {
    let agent = harness
        .new_agent(NewAgentRequest {
            slug: slug.to_string(),
            name: slug.to_string(),
        })
        .await
        .expect("agent");
    agent
        .new_conversation(NewConversationRequest::default())
        .await
        .expect("conversation")
}

async fn setup(
    kind: CoordinatorKind,
    lease_ttl: Duration,
) -> (
    TempDir,
    Arc<dyn ExoHarness>,
    Arc<dyn ConversationHandle>,
    Arc<dyn TurnCoordinator>,
) {
    let tempdir = TempDir::new().expect("tempdir");
    let harness = make_harness(tempdir.path()).await;
    let conversation = make_conversation(&harness, "agent").await;
    let coordinator = make_coordinator(kind, &harness, tempdir.path(), lease_ttl).await;
    (tempdir, harness, conversation, coordinator)
}

fn user_message(text: &str) -> Message {
    Message::User {
        content: UserContent::String(text.to_string()),
    }
}

fn input_request(text: &str) -> EnqueueTurnRequest {
    EnqueueTurnRequest {
        input: vec![user_message(text)],
        ..Default::default()
    }
}

/// Run every shared coordinator test against both implementations.
macro_rules! coordinator_suite {
    ($($test:ident),* $(,)?) => {
        mod in_memory {
            $(
                #[tokio::test(flavor = "current_thread")]
                async fn $test() {
                    super::$test(super::CoordinatorKind::InMemory).await;
                }
            )*
        }
        mod file_backed {
            $(
                #[tokio::test(flavor = "current_thread")]
                async fn $test() {
                    super::$test(super::CoordinatorKind::FileBacked).await;
                }
            )*
        }
    };
}

coordinator_suite!(
    enqueue_claim_begin_complete_round_trip,
    turns_execute_in_enqueue_order_and_head_is_enforced,
    claim_conversation_is_a_mutex_until_released,
    stale_lease_tokens_are_fenced,
    expired_leases_can_be_reclaimed_and_head_reexecuted,
    dedupe_key_returns_the_existing_pending_turn,
    cancellation_is_durable_and_observable_under_the_lease,
);

async fn enqueue_claim_begin_complete_round_trip(kind: CoordinatorKind) {
    let (_tempdir, _harness, conversation, coordinator) = setup(kind, LEASE_TTL).await;
    let conversation_id = conversation.record().id;

    let enqueued = coordinator
        .enqueue_turn(conversation_id, input_request("hello"))
        .await
        .expect("enqueue");
    assert!(!enqueued.deduplicated);
    assert!(enqueued.leased_by.is_none());

    let lease = coordinator
        .claim_conversation(conversation_id)
        .await
        .expect("claim")
        .expect("lease should be granted");

    let head = coordinator
        .peek_turn(&lease)
        .await
        .expect("peek")
        .expect("head turn");
    assert_eq!(head.id, enqueued.turn.id);

    let record = coordinator
        .begin_pending_turn(&lease, head.id)
        .await
        .expect("begin");
    assert_eq!(record.id, enqueued.turn.id);

    // The started turn is rebuildable through the existing handle path and
    // finishes normally.
    let turn = conversation
        .turn_handle(record)
        .await
        .expect("turn handle should rebuild from durable ids");
    turn.finish().await.expect("finish");

    let outcome = coordinator
        .complete_turn(&lease, enqueued.turn.id)
        .await
        .expect("complete");
    assert_eq!(outcome, CompleteTurnOutcome::QueueEmpty);

    // Durable intent and execution both live in the event log.
    let events = conversation
        .get_events(Some(EventQuery {
            types: Some(vec![
                EventKind::TURN_ENQUEUED,
                EventKind::SESSION_STARTED,
                EventKind::TURN_STARTED,
                EventKind::MESSAGES,
                EventKind::TURN_ENDED,
            ]),
            ..Default::default()
        }))
        .await
        .expect("events");
    let kinds: Vec<_> = events
        .events
        .iter()
        .map(|event| event.data.kind())
        .collect();
    assert_eq!(
        kinds,
        vec![
            EventKind::TURN_ENQUEUED,
            EventKind::SESSION_STARTED,
            EventKind::TURN_STARTED,
            EventKind::MESSAGES,
            EventKind::TURN_ENDED,
        ]
    );
}

async fn turns_execute_in_enqueue_order_and_head_is_enforced(kind: CoordinatorKind) {
    let (_tempdir, _harness, conversation, coordinator) = setup(kind, LEASE_TTL).await;
    let conversation_id = conversation.record().id;

    let first = coordinator
        .enqueue_turn(conversation_id, input_request("first"))
        .await
        .expect("enqueue first");
    let second = coordinator
        .enqueue_turn(conversation_id, input_request("second"))
        .await
        .expect("enqueue second");

    let lease = coordinator
        .claim_conversation(conversation_id)
        .await
        .expect("claim")
        .expect("lease");

    // The second turn is not at the head: neither begin nor complete accepts it.
    assert!(
        coordinator
            .begin_pending_turn(&lease, second.turn.id)
            .await
            .is_err()
    );
    assert!(
        coordinator
            .complete_turn(&lease, second.turn.id)
            .await
            .is_err()
    );

    coordinator
        .begin_pending_turn(&lease, first.turn.id)
        .await
        .expect("begin first");
    assert_eq!(
        coordinator
            .complete_turn(&lease, first.turn.id)
            .await
            .expect("complete first"),
        CompleteTurnOutcome::MorePending
    );
    assert_eq!(
        coordinator
            .peek_turn(&lease)
            .await
            .expect("peek")
            .expect("head")
            .id,
        second.turn.id
    );
}

async fn claim_conversation_is_a_mutex_until_released(kind: CoordinatorKind) {
    let (_tempdir, _harness, conversation, coordinator) = setup(kind, LEASE_TTL).await;
    let conversation_id = conversation.record().id;

    let lease = coordinator
        .claim_conversation(conversation_id)
        .await
        .expect("claim")
        .expect("lease");
    // A second claim fails while the lease is live, even from the same runtime.
    assert!(
        coordinator
            .claim_conversation(conversation_id)
            .await
            .expect("claim")
            .is_none()
    );

    assert!(coordinator.release_idle(&lease).await.expect("release"));
    assert!(
        coordinator
            .claim_conversation(conversation_id)
            .await
            .expect("claim")
            .is_some()
    );
}

async fn stale_lease_tokens_are_fenced(kind: CoordinatorKind) {
    let (_tempdir, _harness, conversation, coordinator) = setup(kind, LEASE_TTL).await;
    let conversation_id = conversation.record().id;

    coordinator
        .enqueue_turn(conversation_id, input_request("hello"))
        .await
        .expect("enqueue");
    let stale = coordinator
        .claim_conversation(conversation_id)
        .await
        .expect("claim")
        .expect("lease");
    assert!(coordinator.release_idle(&stale).await.expect("release"));
    let live = coordinator
        .claim_conversation(conversation_id)
        .await
        .expect("claim")
        .expect("second lease");

    // The stale token no longer renews, peeks, begins, or completes.
    assert!(!coordinator.renew(&stale, false).await.expect("renew"));
    assert!(coordinator.peek_turn(&stale).await.is_err());
    let head = coordinator
        .peek_turn(&live)
        .await
        .expect("peek")
        .expect("head");
    assert!(
        coordinator
            .begin_pending_turn(&stale, head.id)
            .await
            .is_err()
    );
    assert!(coordinator.complete_turn(&stale, head.id).await.is_err());

    // The live lease still works.
    assert!(coordinator.renew(&live, true).await.expect("renew"));
}

async fn expired_leases_can_be_reclaimed_and_head_reexecuted(kind: CoordinatorKind) {
    let (_tempdir, _harness, conversation, coordinator) =
        setup(kind, Duration::from_millis(20)).await;
    let conversation_id = conversation.record().id;

    let enqueued = coordinator
        .enqueue_turn(conversation_id, input_request("hello"))
        .await
        .expect("enqueue");
    let dead = coordinator
        .claim_conversation(conversation_id)
        .await
        .expect("claim")
        .expect("lease");
    coordinator
        .begin_pending_turn(&dead, enqueued.turn.id)
        .await
        .expect("begin");

    // The "runtime" dies: no renewals. After the TTL the conversation is
    // claimable again and the same head turn re-executes.
    tokio::time::sleep(Duration::from_millis(40)).await;
    let claimed = coordinator
        .claim_ready(8)
        .await
        .expect("claim ready")
        .into_iter()
        .find(|lease| lease.conversation_id == conversation_id)
        .expect("expired conversation should be reclaimable");
    let head = coordinator
        .peek_turn(&claimed)
        .await
        .expect("peek")
        .expect("head turn survives the dead runtime");
    assert_eq!(head.id, enqueued.turn.id);
    coordinator
        .begin_pending_turn(&claimed, head.id)
        .await
        .expect("re-execution begins");

    // Both attempts are first-class in history, numbered in order.
    let starts = conversation
        .get_events(Some(EventQuery {
            turn_id: Some(enqueued.turn.id),
            types: Some(vec![EventKind::TURN_STARTED]),
            ..Default::default()
        }))
        .await
        .expect("events")
        .events;
    let attempts: Vec<u32> = starts
        .iter()
        .map(|event| match event.data {
            crate::EventData::TurnStarted { attempt } => attempt,
            _ => unreachable!("filtered to turn_started"),
        })
        .collect();
    assert_eq!(attempts, vec![1, 2]);
}

async fn dedupe_key_returns_the_existing_pending_turn(kind: CoordinatorKind) {
    let (_tempdir, _harness, conversation, coordinator) = setup(kind, LEASE_TTL).await;
    let conversation_id = conversation.record().id;

    let request = EnqueueTurnRequest {
        input: vec![user_message("wake")],
        dedupe_key: Some("wake:123".to_string()),
        ..Default::default()
    };
    let first = coordinator
        .enqueue_turn(conversation_id, request.clone())
        .await
        .expect("enqueue");
    let second = coordinator
        .enqueue_turn(conversation_id, request)
        .await
        .expect("enqueue again");
    assert!(!first.deduplicated);
    assert!(second.deduplicated);
    assert_eq!(first.turn.id, second.turn.id);

    // Only one turn_enqueued event was appended.
    let events = conversation
        .get_events(Some(EventQuery {
            types: Some(vec![EventKind::TURN_ENQUEUED]),
            ..Default::default()
        }))
        .await
        .expect("events");
    assert_eq!(events.events.len(), 1);
}

async fn cancellation_is_durable_and_observable_under_the_lease(kind: CoordinatorKind) {
    let (_tempdir, _harness, conversation, coordinator) = setup(kind, LEASE_TTL).await;
    let conversation_id = conversation.record().id;

    let enqueued = coordinator
        .enqueue_turn(conversation_id, input_request("hello"))
        .await
        .expect("enqueue");
    let lease = coordinator
        .claim_conversation(conversation_id)
        .await
        .expect("claim")
        .expect("lease");
    assert!(
        !coordinator
            .turn_cancelled(&lease, enqueued.turn.id)
            .await
            .expect("cancelled check")
    );

    let outcome = coordinator
        .cancel_turn(conversation_id, enqueued.turn.id)
        .await
        .expect("cancel");
    assert!(matches!(outcome, CancelTurnOutcome::Cancelled { .. }));
    assert!(
        coordinator
            .turn_cancelled(&lease, enqueued.turn.id)
            .await
            .expect("cancelled check")
    );

    // Cancelling an unknown turn reports NotFound.
    let missing = coordinator
        .cancel_turn(conversation_id, crate::Uuid7::now())
        .await
        .expect("cancel missing");
    assert!(matches!(missing, CancelTurnOutcome::NotFound));

    let events = conversation
        .get_events(Some(EventQuery {
            types: Some(vec![EventKind::TURN_CANCELLED]),
            ..Default::default()
        }))
        .await
        .expect("events");
    assert_eq!(events.events.len(), 1);
}

// ---------------------------------------------------------------------------
// File-backed-only behavior: the substrate accessor, cross-process
// coordination, and durability across restarts.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn basic_harness_provides_a_shared_coordinator() {
    let tempdir = TempDir::new().expect("tempdir");
    let harness = make_harness(tempdir.path()).await;
    let first = harness
        .turn_coordinator()
        .expect("basic backend should provide a coordinator");
    let second = harness
        .turn_coordinator()
        .expect("accessor should keep providing it");
    // One runtime identity per harness instance.
    assert_eq!(first.runtime_id(), second.runtime_id());
}

/// Two coordinator instances over the same store stand in for two processes:
/// they share the queue and exclude each other through the lease.
#[tokio::test(flavor = "current_thread")]
async fn separate_instances_share_queue_and_exclude_each_other() {
    let tempdir = TempDir::new().expect("tempdir");
    let harness_a = make_harness(tempdir.path()).await;
    let harness_b = make_harness(tempdir.path()).await;
    let conversation = make_conversation(&harness_a, "agent").await;
    let conversation_id = conversation.record().id;
    let coordinator_a = make_coordinator(
        CoordinatorKind::FileBacked,
        &harness_a,
        tempdir.path(),
        LEASE_TTL,
    )
    .await;
    let coordinator_b = make_coordinator(
        CoordinatorKind::FileBacked,
        &harness_b,
        tempdir.path(),
        LEASE_TTL,
    )
    .await;
    assert_ne!(coordinator_a.runtime_id(), coordinator_b.runtime_id());

    // A enqueues; B sees the turn and reports A's enqueue through its own view.
    let enqueued = coordinator_a
        .enqueue_turn(conversation_id, input_request("cross-process"))
        .await
        .expect("enqueue via A");

    // B claims; A is excluded while B holds the lease.
    let lease_b = coordinator_b
        .claim_conversation(conversation_id)
        .await
        .expect("claim via B")
        .expect("B should acquire the lease");
    assert!(
        coordinator_a
            .claim_conversation(conversation_id)
            .await
            .expect("claim via A")
            .is_none(),
        "A must not acquire the lease while B holds it"
    );

    // B executes A's turn: the queue is shared state, not producer state.
    let head = coordinator_b
        .peek_turn(&lease_b)
        .await
        .expect("peek via B")
        .expect("B sees A's pending turn");
    assert_eq!(head.id, enqueued.turn.id);
    coordinator_b
        .begin_pending_turn(&lease_b, head.id)
        .await
        .expect("begin via B");
    coordinator_b
        .complete_turn(&lease_b, head.id)
        .await
        .expect("complete via B");
    assert!(coordinator_b.release_idle(&lease_b).await.expect("release"));

    // With the lease released, A can claim.
    assert!(
        coordinator_a
            .claim_conversation(conversation_id)
            .await
            .expect("claim via A")
            .is_some()
    );
}

/// Pending turns survive every process dying: a fresh instance over the same
/// store picks up the queue where it was left.
#[tokio::test(flavor = "current_thread")]
async fn pending_turns_survive_restart() {
    let tempdir = TempDir::new().expect("tempdir");
    let enqueued_turn_id = {
        let harness = make_harness(tempdir.path()).await;
        let conversation = make_conversation(&harness, "agent").await;
        let coordinator = make_coordinator(
            CoordinatorKind::FileBacked,
            &harness,
            tempdir.path(),
            LEASE_TTL,
        )
        .await;
        let enqueued = coordinator
            .enqueue_turn(conversation.record().id, input_request("durable"))
            .await
            .expect("enqueue");
        enqueued.turn.id
        // Everything dropped: the "process" dies with an unexecuted turn.
    };

    let harness = make_harness(tempdir.path()).await;
    let coordinator = harness
        .turn_coordinator()
        .expect("restarted harness provides a coordinator");
    let leases = coordinator.claim_ready(8).await.expect("claim ready");
    assert_eq!(leases.len(), 1, "the queued conversation should be ready");
    let lease = &leases[0];
    let head = coordinator
        .peek_turn(lease)
        .await
        .expect("peek")
        .expect("pending turn survived the restart");
    assert_eq!(head.id, enqueued_turn_id);
    coordinator
        .begin_pending_turn(lease, head.id)
        .await
        .expect("the surviving turn executes");
    coordinator
        .complete_turn(lease, head.id)
        .await
        .expect("complete");
}
