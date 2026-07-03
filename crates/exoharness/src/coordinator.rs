//! The object-store-backed `TurnCoordinator` implementation.
//!
//! [`BasicTurnCoordinator`] keeps queue and lease state in an object store.
//! Over the substrate's filesystem store (`BasicExoHarness`), every process
//! attached to one root shares one coordination scope; over an in-memory
//! store ([`BasicTurnCoordinator::in_memory`]), it degrades to process-local
//! coordination — the executor fallback for backends that do not provide a
//! coordinator of their own (e.g. the HTTP client today). One implementation,
//! two stores: the coordination semantics live entirely in the protocol over
//! conditional puts, not in the storage medium.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use anyhow::bail;
use async_trait::async_trait;
use chrono::Utc;
use serde::{Deserialize, Serialize};

use crate::storage::BasicObjectStore;
use crate::{
    AddEventsRequest, CancelTurnOutcome, CompleteTurnOutcome, ConversationHandle, ConversationId,
    ConversationLease, EnqueueTurnRequest, EnqueueTurnResult, EventData, ExoHarness,
    LeasedHeadTurn, PendingTurn, ReadyStream, Result, RuntimeId, SessionId, TurnCoordinator,
    TurnId, TurnRecord, Uuid7,
};

/// Default time a conversation lease survives without renewal. Executors
/// renew at a third of this while a turn runs.
pub const DEFAULT_LEASE_TTL: Duration = Duration::from_secs(60);

fn eligible(turn: &PendingTurn) -> bool {
    turn.not_before
        .is_none_or(|not_before| not_before <= Utc::now())
}

fn pending_turn_from_request(
    conversation_id: ConversationId,
    request: EnqueueTurnRequest,
) -> PendingTurn {
    PendingTurn {
        id: Uuid7::now(),
        conversation_id,
        input: request.input,
        session_id: request.session_id,
        enqueued_at: Utc::now(),
        not_before: request.not_before,
        dedupe_key: request.dedupe_key,
    }
}

async fn append_enqueued_event(
    conversation: &dyn ConversationHandle,
    turn: &PendingTurn,
) -> Result<()> {
    conversation
        .add_events(AddEventsRequest {
            session_id: None,
            turn_id: None,
            data: vec![EventData::TurnEnqueued {
                turn_id: turn.id,
                not_before: turn.not_before,
            }],
        })
        .await?;
    Ok(())
}

async fn append_cancelled_event(
    conversation: &dyn ConversationHandle,
    turn_id: TurnId,
) -> Result<()> {
    conversation
        .add_events(AddEventsRequest {
            session_id: None,
            turn_id: None,
            data: vec![EventData::TurnCancelled { turn_id }],
        })
        .await?;
    Ok(())
}

/// Append `session_started`/`turn_started`/input under the pending turn's
/// ids, mirroring `begin_turn()`. The returned record rebuilds a live handle
/// through `ConversationHandle::turn_handle()`.
///
/// The attempt number comes from the log itself — one more than the count of
/// prior `turn_started` events for this turn id — so re-execution of a
/// crashed turn is first-class in history without the coordinator keeping
/// attempt state anywhere else.
async fn append_turn_begin_events(
    conversation: &dyn ConversationHandle,
    pending: PendingTurn,
) -> Result<TurnRecord> {
    let prior_attempts = conversation
        .get_events(Some(crate::EventQuery {
            turn_id: Some(pending.id),
            types: Some(vec![crate::EventKind::TURN_STARTED]),
            ..Default::default()
        }))
        .await?
        .events
        .len() as u32;
    let session_id: SessionId = pending.session_id.unwrap_or_else(Uuid7::now);
    let mut events = Vec::new();
    if pending.session_id.is_none() {
        events.push(EventData::SessionStarted);
    }
    events.push(EventData::TurnStarted {
        attempt: prior_attempts + 1,
    });
    // Input is appended once: a re-attempt inherits attempt 1's durable
    // input rather than duplicating it in history.
    if prior_attempts == 0 && !pending.input.is_empty() {
        events.push(EventData::Messages {
            messages: pending.input,
            response_id: None,
            usage: None,
        });
    }
    let turn_id = pending.id;
    conversation
        .add_events(AddEventsRequest {
            session_id: Some(session_id),
            turn_id: Some(turn_id),
            data: events,
        })
        .await?;
    Ok(TurnRecord {
        id: turn_id,
        session_id,
    })
}

/// Resolve a conversation handle by id through the root handle, caching hits.
struct ConversationResolver {
    exoharness: Arc<dyn ExoHarness>,
    cache: Mutex<HashMap<ConversationId, Arc<dyn ConversationHandle>>>,
}

impl ConversationResolver {
    fn new(exoharness: Arc<dyn ExoHarness>) -> Self {
        Self {
            exoharness,
            cache: Mutex::new(HashMap::new()),
        }
    }

    async fn resolve(
        &self,
        conversation_id: ConversationId,
    ) -> Result<Arc<dyn ConversationHandle>> {
        if let Some(conversation) = self
            .cache
            .lock()
            .expect("conversation resolver cache poisoned")
            .get(&conversation_id)
        {
            return Ok(Arc::clone(conversation));
        }
        for agent in self.exoharness.list_agents().await? {
            if let Some(conversation) = agent.get_conversation(&conversation_id).await? {
                self.cache
                    .lock()
                    .expect("conversation resolver cache poisoned")
                    .insert(conversation_id, Arc::clone(&conversation));
                return Ok(conversation);
            }
        }
        bail!("conversation {conversation_id} not found");
    }
}

// ---------------------------------------------------------------------------
// Durable, object-store-backed implementation.
// ---------------------------------------------------------------------------

/// Coordinator state layout under the substrate store:
///
/// - `coordinator/leases/<conversation_id>.json`: the live lease, acquired
///   with an atomic create. Expiry is a wall-clock timestamp in the record so
///   every process on the store judges liveness the same way.
/// - `coordinator/queues/<conversation_id>/<turn_id>.json`: pending turns.
///   Turn ids are UUIDv7, so lexicographic key order is enqueue order and the
///   head is the smallest key.
/// - `coordinator/cancelled/<conversation_id>/<turn_id>.json`: cancellation
///   markers, removed when the turn completes.
///
/// Takeover of an expired lease is delete-then-create. As with the wakeup
/// file lock this replaces, there is a small window where an owner that
/// renews at the exact moment of expiry can lose its lease to a reaper; the
/// renewal read-back detects this and reports the lease lost rather than
/// letting two owners run.
pub struct BasicTurnCoordinator {
    resolver: ConversationResolver,
    storage: BasicObjectStore,
    runtime_id: RuntimeId,
    lease_ttl: Duration,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LeaseRecord {
    runtime_id: RuntimeId,
    token: String,
    expires_at_ms: i64,
}

impl LeaseRecord {
    fn live(&self) -> bool {
        self.expires_at_ms > Utc::now().timestamp_millis()
    }
}

fn lease_key(conversation_id: ConversationId) -> PathBuf {
    PathBuf::from(format!("coordinator/leases/{conversation_id}.json"))
}

fn queue_prefix(conversation_id: ConversationId) -> PathBuf {
    PathBuf::from(format!("coordinator/queues/{conversation_id}"))
}

fn turn_key(conversation_id: ConversationId, turn_id: TurnId) -> PathBuf {
    PathBuf::from(format!(
        "coordinator/queues/{conversation_id}/{turn_id}.json"
    ))
}

fn cancelled_key(conversation_id: ConversationId, turn_id: TurnId) -> PathBuf {
    PathBuf::from(format!(
        "coordinator/cancelled/{conversation_id}/{turn_id}.json"
    ))
}

async fn read_lease_record(
    storage: &BasicObjectStore,
    conversation_id: ConversationId,
) -> Result<Option<LeaseRecord>> {
    storage.get_json_if_exists(lease_key(conversation_id)).await
}

/// Pending turns in enqueue order: keys list sorted and turn ids are UUIDv7,
/// so lexicographic order is chronological order.
async fn list_pending_turns(
    storage: &BasicObjectStore,
    conversation_id: ConversationId,
) -> Result<Vec<PendingTurn>> {
    storage
        .list_json_matching_suffix(queue_prefix(conversation_id), ".json")
        .await
}

async fn head_pending_turn(
    storage: &BasicObjectStore,
    conversation_id: ConversationId,
) -> Result<Option<PendingTurn>> {
    Ok(list_pending_turns(storage, conversation_id)
        .await?
        .into_iter()
        .next())
}

/// Conversations with at least one pending turn, from the queue listing.
async fn list_queued_conversations(storage: &BasicObjectStore) -> Result<Vec<ConversationId>> {
    let keys = storage.list_keys("coordinator/queues").await?;
    let mut conversations = Vec::new();
    let mut seen = HashSet::new();
    for key in keys {
        let Some(rest) = key.strip_prefix("coordinator/queues/") else {
            continue;
        };
        let Some((conversation, _)) = rest.split_once('/') else {
            continue;
        };
        let Ok(conversation_id) = conversation.parse::<Uuid7>() else {
            continue;
        };
        if seen.insert(conversation_id) {
            conversations.push(conversation_id);
        }
    }
    Ok(conversations)
}

impl BasicTurnCoordinator {
    pub(crate) fn new(
        exoharness: Arc<dyn ExoHarness>,
        storage: BasicObjectStore,
        lease_ttl: Duration,
    ) -> Self {
        Self {
            resolver: ConversationResolver::new(exoharness),
            storage,
            runtime_id: Uuid7::now().to_string(),
            lease_ttl,
        }
    }

    /// Process-local coordinator over an in-memory object store: identical
    /// semantics, no durability, no cross-process scope. The executor
    /// fallback for substrates that do not provide their own coordinator.
    pub fn in_memory(exoharness: Arc<dyn ExoHarness>, lease_ttl: Duration) -> Self {
        Self::new(exoharness, BasicObjectStore::memory(), lease_ttl)
    }

    fn fresh_lease_record(&self) -> LeaseRecord {
        LeaseRecord {
            runtime_id: self.runtime_id.clone(),
            token: Uuid7::now().to_string(),
            expires_at_ms: Utc::now().timestamp_millis() + self.lease_ttl.as_millis() as i64,
        }
    }

    async fn read_lease(&self, conversation_id: ConversationId) -> Result<Option<LeaseRecord>> {
        read_lease_record(&self.storage, conversation_id).await
    }

    /// Acquire the conversation lease if it is absent or expired.
    async fn try_acquire(
        &self,
        conversation_id: ConversationId,
    ) -> Result<Option<ConversationLease>> {
        match self.read_lease(conversation_id).await? {
            Some(existing) if existing.live() => return Ok(None),
            Some(_expired) => {
                self.storage.delete_key(lease_key(conversation_id)).await?;
            }
            None => {}
        }
        let record = self.fresh_lease_record();
        if !self
            .storage
            .put_json_if_absent(lease_key(conversation_id), &record)
            .await?
        {
            return Ok(None);
        }
        Ok(Some(ConversationLease {
            conversation_id,
            runtime_id: self.runtime_id.clone(),
            token: record.token,
        }))
    }

    async fn verify_lease(&self, lease: &ConversationLease) -> Result<()> {
        match self.read_lease(lease.conversation_id).await? {
            Some(record) if record.live() && record.token == lease.token => Ok(()),
            _ => bail!(
                "lost conversation lease for {}: token is no longer live",
                lease.conversation_id
            ),
        }
    }

    async fn pending_turns(&self, conversation_id: ConversationId) -> Result<Vec<PendingTurn>> {
        list_pending_turns(&self.storage, conversation_id).await
    }

    async fn head_turn(&self, conversation_id: ConversationId) -> Result<Option<PendingTurn>> {
        head_pending_turn(&self.storage, conversation_id).await
    }
}

#[async_trait]
impl TurnCoordinator for BasicTurnCoordinator {
    fn runtime_id(&self) -> &RuntimeId {
        &self.runtime_id
    }

    fn lease_ttl(&self) -> Duration {
        self.lease_ttl
    }

    async fn enqueue_turn(
        &self,
        conversation_id: ConversationId,
        request: EnqueueTurnRequest,
    ) -> Result<EnqueueTurnResult> {
        let conversation = self.resolver.resolve(conversation_id).await?;
        if let Some(key) = &request.dedupe_key {
            if let Some(existing) = self
                .pending_turns(conversation_id)
                .await?
                .into_iter()
                .find(|turn| turn.dedupe_key.as_ref() == Some(key))
            {
                let leased_by = self
                    .read_lease(conversation_id)
                    .await?
                    .filter(LeaseRecord::live)
                    .map(|record| record.runtime_id);
                return Ok(EnqueueTurnResult {
                    turn: existing,
                    deduplicated: true,
                    leased_by,
                });
            }
        }
        let turn = pending_turn_from_request(conversation_id, request);
        self.storage
            .put_json(turn_key(conversation_id, turn.id), &turn)
            .await?;
        append_enqueued_event(conversation.as_ref(), &turn).await?;
        let leased_by = self
            .read_lease(conversation_id)
            .await?
            .filter(LeaseRecord::live)
            .map(|record| record.runtime_id);
        Ok(EnqueueTurnResult {
            turn,
            deduplicated: false,
            leased_by,
        })
    }

    async fn claim_ready(&self, max: usize) -> Result<Vec<ConversationLease>> {
        let mut leases = Vec::new();
        for conversation_id in list_queued_conversations(&self.storage).await? {
            if leases.len() >= max {
                break;
            }
            if !self
                .head_turn(conversation_id)
                .await?
                .is_some_and(|turn| eligible(&turn))
            {
                continue;
            }
            if let Some(lease) = self.try_acquire(conversation_id).await? {
                leases.push(lease);
            }
        }
        Ok(leases)
    }

    async fn claim_conversation(
        &self,
        conversation_id: ConversationId,
    ) -> Result<Option<ConversationLease>> {
        self.try_acquire(conversation_id).await
    }

    async fn renew(&self, lease: &ConversationLease, _processing: bool) -> Result<bool> {
        match self.read_lease(lease.conversation_id).await? {
            Some(record) if record.live() && record.token == lease.token => {}
            _ => return Ok(false),
        }
        let renewed = LeaseRecord {
            runtime_id: lease.runtime_id.clone(),
            token: lease.token.clone(),
            expires_at_ms: Utc::now().timestamp_millis() + self.lease_ttl.as_millis() as i64,
        };
        self.storage
            .put_json(lease_key(lease.conversation_id), &renewed)
            .await?;
        // Read back: a reaper that raced the renewal wins, and this runtime
        // must observe the loss instead of assuming the rewrite stuck.
        Ok(self
            .read_lease(lease.conversation_id)
            .await?
            .is_some_and(|record| record.token == lease.token))
    }

    async fn release_idle(&self, lease: &ConversationLease) -> Result<bool> {
        match self.read_lease(lease.conversation_id).await? {
            Some(record) if record.token == lease.token => {
                self.storage
                    .delete_key(lease_key(lease.conversation_id))
                    .await?;
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    async fn peek_turn(&self, lease: &ConversationLease) -> Result<Option<PendingTurn>> {
        self.verify_lease(lease).await?;
        Ok(self
            .head_turn(lease.conversation_id)
            .await?
            .filter(eligible))
    }

    async fn begin_pending_turn(
        &self,
        lease: &ConversationLease,
        turn_id: TurnId,
    ) -> Result<TurnRecord> {
        self.verify_lease(lease).await?;
        let pending = match self.head_turn(lease.conversation_id).await? {
            Some(turn) if turn.id == turn_id => turn,
            Some(turn) => bail!(
                "turn {turn_id} is not at the head of conversation {} (head is {})",
                lease.conversation_id,
                turn.id
            ),
            None => bail!(
                "conversation {} has no pending turns",
                lease.conversation_id
            ),
        };
        let conversation = self.resolver.resolve(lease.conversation_id).await?;
        append_turn_begin_events(conversation.as_ref(), pending).await
    }

    async fn complete_turn(
        &self,
        lease: &ConversationLease,
        turn_id: TurnId,
    ) -> Result<CompleteTurnOutcome> {
        self.verify_lease(lease).await?;
        match self.head_turn(lease.conversation_id).await? {
            Some(turn) if turn.id == turn_id => {}
            Some(turn) => bail!(
                "cannot complete turn {turn_id}: head of conversation {} is {}",
                lease.conversation_id,
                turn.id
            ),
            None => bail!(
                "cannot complete turn {turn_id}: conversation {} has no pending turns",
                lease.conversation_id
            ),
        }
        self.storage
            .delete_key(turn_key(lease.conversation_id, turn_id))
            .await?;
        self.storage
            .delete_key(cancelled_key(lease.conversation_id, turn_id))
            .await?;
        Ok(match self.head_turn(lease.conversation_id).await? {
            Some(_) => CompleteTurnOutcome::MorePending,
            None => CompleteTurnOutcome::QueueEmpty,
        })
    }

    async fn cancel_turn(
        &self,
        conversation_id: ConversationId,
        turn_id: TurnId,
    ) -> Result<CancelTurnOutcome> {
        let pending = self
            .pending_turns(conversation_id)
            .await?
            .iter()
            .any(|turn| turn.id == turn_id);
        if !pending {
            return Ok(CancelTurnOutcome::NotFound);
        }
        self.storage
            .put_json(
                cancelled_key(conversation_id, turn_id),
                &serde_json::json!({}),
            )
            .await?;
        let conversation = self.resolver.resolve(conversation_id).await?;
        append_cancelled_event(conversation.as_ref(), turn_id).await?;
        let owner = self
            .read_lease(conversation_id)
            .await?
            .filter(LeaseRecord::live)
            .map(|record| record.runtime_id);
        Ok(CancelTurnOutcome::Cancelled { owner })
    }

    async fn turn_cancelled(&self, lease: &ConversationLease, turn_id: TurnId) -> Result<bool> {
        self.verify_lease(lease).await?;
        Ok(self
            .storage
            .get_json_if_exists::<serde_json::Value>(cancelled_key(lease.conversation_id, turn_id))
            .await?
            .is_some())
    }

    async fn leased_head_turn(
        &self,
        conversation_id: ConversationId,
    ) -> Result<Option<LeasedHeadTurn>> {
        let Some(record) = self
            .read_lease(conversation_id)
            .await?
            .filter(LeaseRecord::live)
        else {
            return Ok(None);
        };
        Ok(self
            .head_turn(conversation_id)
            .await?
            .map(|turn| LeasedHeadTurn {
                turn_id: turn.id,
                owner: record.runtime_id.clone(),
            }))
    }

    async fn watch_ready(&self) -> Result<ReadyStream> {
        // Poll-based hint stream: emit conversations that currently have an
        // eligible, unleased head. Correctness comes from `claim_ready()`;
        // this only reduces wakeup latency for idle runtimes.
        const WATCH_POLL: Duration = Duration::from_millis(250);
        let storage = self.storage.clone();
        let stream = futures::stream::unfold(
            (storage, VecDeque::<ConversationId>::new()),
            |(storage, mut buffered)| async move {
                loop {
                    if let Some(id) = buffered.pop_front() {
                        return Some((Ok(id), (storage, buffered)));
                    }
                    tokio::time::sleep(WATCH_POLL).await;
                    let Ok(conversations) = list_queued_conversations(&storage).await else {
                        continue;
                    };
                    for conversation_id in conversations {
                        let head_eligible = head_pending_turn(&storage, conversation_id)
                            .await
                            .ok()
                            .flatten()
                            .is_some_and(|turn| eligible(&turn));
                        let unleased = read_lease_record(&storage, conversation_id)
                            .await
                            .ok()
                            .flatten()
                            .filter(LeaseRecord::live)
                            .is_none();
                        if head_eligible && unleased {
                            buffered.push_back(conversation_id);
                        }
                    }
                }
            },
        );
        Ok(Box::pin(stream))
    }
}
