use crate::remote::IdentityGrant;
use anyhow::Result;
use exoharness::{AgentId, EventId, Storage, ThreadId, TurnRecord, Uuid7};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
};

pub fn digest(value: &str) -> String {
    format!("{:x}", Sha256::digest(value.as_bytes()))
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(super) struct IncomingMessage {
    pub id: String,
    pub account: String,
    pub conversation: String,
    pub sender: String,
    pub text: String,
    pub channel: String,
    pub message_ts: String,
    pub reply_ts: Option<String>,
    pub direct: bool,
    pub external: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct Inbox {
    pub accepted: Uuid7,
    pub message: IncomingMessage,
    pub grant: Option<IdentityGrant>,
    pub thread: Option<ThreadId>,
    pub turn: Option<TurnRecord>,
    pub cursor: Option<EventId>,
    pub final_text: Option<String>,
    #[serde(default)]
    pub progress: super::progress::Progress,
    pub complete: bool,
    pub error: Option<InboxError>,
    pub observer_retry: Option<ObserverRetry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) enum InboxError {
    TurnFailed(String),
    AccessRemoved,
    InvalidProgress,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct ObserverRetry {
    pub attempts: u32,
    pub retry_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct SenderContext {
    pub account: String,
    pub sender: String,
    pub channel: String,
    pub direct: bool,
    pub external: bool,
}
impl From<&IncomingMessage> for SenderContext {
    fn from(message: &IncomingMessage) -> Self {
        Self {
            account: message.account.clone(),
            sender: message.sender.clone(),
            channel: message.channel.clone(),
            direct: message.direct,
            external: message.external,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Authority {
    pub deployment: String,
    pub agent: AgentId,
    pub thread: ThreadId,
    pub adapter: String,
    pub source: SenderContext,
    pub grant: Option<IdentityGrant>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct Route {
    pub thread: Option<ThreadId>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) enum DeliveryKind {
    Stream {
        chunks: Vec<slack_morphism::prelude::SlackStreamChunk>,
        complete: bool,
        sender: String,
        thread_ts: String,
    },
    Status {
        inbox: String,
        thread_ts: String,
        text: String,
    },
    Text {
        text: String,
    },
    Reaction {
        message_ts: String,
    },
    Approval {
        turn: TurnRecord,
        approval: crate::permissions::ApprovalRequest,
        inbox: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct StreamReceipt {
    pub cursor: Option<EventId>,
    pub progress: super::progress::Progress,
    pub complete: bool,
    pub final_text: Option<String>,
    pub error: Option<InboxError>,
    pub observer_retry: Option<ObserverRetry>,
}

impl From<&Inbox> for StreamReceipt {
    fn from(inbox: &Inbox) -> Self {
        Self {
            cursor: inbox.cursor,
            progress: inbox.progress.clone(),
            complete: inbox.complete,
            final_text: inbox.final_text.clone(),
            error: inbox.error.clone(),
            observer_retry: inbox.observer_retry.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct Outbox {
    pub id: String,
    pub account: String,
    pub channel: String,
    pub reply_ts: Option<String>,
    pub kind: DeliveryKind,
    pub sent: bool,
    pub failure: Option<String>,
    pub attempts: u32,
    pub retry_at: i64,
    pub external_id: Option<String>,
    pub receipt: Option<StreamReceipt>,
}

pub(super) trait PendingRecord: Serialize + DeserializeOwned {
    fn pending(&self) -> bool;
}
impl PendingRecord for Inbox {
    fn pending(&self) -> bool {
        !self.complete
    }
}
impl PendingRecord for Outbox {
    fn pending(&self) -> bool {
        !self.sent && self.failure.is_none()
    }
}
#[derive(Default)]
struct PendingSet {
    loaded: bool,
    generation: u64,
    keys: BTreeSet<String>,
    changes: BTreeMap<String, bool>,
}
type PendingIndex = Arc<Mutex<BTreeMap<String, PendingSet>>>;

// An interrupted write may already have reached storage. Reload that prefix
// rather than trusting an in-memory index that never received its completion.
struct WriteGuard {
    index: PendingIndex,
    prefix: String,
    key: String,
    committed: bool,
}
impl Drop for WriteGuard {
    fn drop(&mut self) {
        if !self.committed {
            self.invalidate();
        }
    }
}
impl WriteGuard {
    fn invalidate(&self) {
        let mut index = self.index.lock().expect("adapter work index poisoned");
        let set = index.entry(self.prefix.clone()).or_default();
        set.loaded = false;
        set.generation += 1;
        set.changes.remove(&self.key);
    }
}
#[derive(Clone)]
pub(super) struct AdapterStore {
    storage: exoharness::BasicObjectStore,
    pending: PendingIndex,
}
impl AdapterStore {
    pub fn new(storage: Arc<dyn Storage>) -> Self {
        Self {
            storage: exoharness::BasicObjectStore::new(storage),
            pending: Default::default(),
        }
    }
    pub async fn get<T: DeserializeOwned>(&self, key: &str) -> Result<Option<T>> {
        self.storage.get_json_if_exists(key).await
    }
    fn write_guard(&self, key: &str) -> WriteGuard {
        WriteGuard {
            index: self.pending.clone(),
            prefix: key.rsplit_once('/').map_or("", |(prefix, _)| prefix).into(),
            key: key.into(),
            committed: false,
        }
    }
    pub async fn put<T: Serialize>(&self, key: &str, value: &T) -> Result<()> {
        let mut guard = self.write_guard(key);
        self.storage.put_json(key, value).await?;
        guard.committed = true;
        Ok(())
    }
    pub async fn save<T: PendingRecord>(&self, key: &str, value: &T) -> Result<()> {
        let mut guard = self.write_guard(key);
        self.storage.put_json(key, value).await?;
        let mut index = self.pending.lock().expect("adapter work index poisoned");
        let set = index.entry(guard.prefix.clone()).or_default();
        if value.pending() {
            set.keys.insert(key.into());
        } else {
            set.keys.remove(key);
        }
        if !set.loaded {
            set.changes.insert(key.into(), value.pending());
        }
        guard.committed = true;
        Ok(())
    }
    #[cfg(test)]
    pub async fn list<T: DeserializeOwned>(&self, prefix: &str) -> Result<Vec<T>> {
        let mut records = Vec::new();
        for key in self.storage.list_keys(prefix).await? {
            if key.ends_with(".json")
                && let Some(record) = self.get(&key).await?
            {
                records.push(record);
            }
        }
        Ok(records)
    }
    pub async fn pending<T: PendingRecord>(&self, prefix: &str) -> Result<Vec<T>> {
        let keys = loop {
            let generation = {
                let mut index = self.pending.lock().expect("adapter work index poisoned");
                let set = index.entry(prefix.into()).or_default();
                if set.loaded {
                    break set.keys.clone();
                }
                set.generation
            };
            let mut keys = BTreeSet::new();
            for key in self.storage.list_keys(prefix).await? {
                if !key.ends_with(".json") {
                    continue;
                }
                match self.get::<T>(&key).await {
                    Ok(Some(record)) if record.pending() => {
                        keys.insert(key);
                    }
                    Err(error) => {
                        tracing::warn!(%key, %error, "unreadable adapter record retained");
                        keys.insert(key);
                    }
                    _ => {}
                }
            }
            let mut index = self.pending.lock().expect("adapter work index poisoned");
            let set = index.entry(prefix.into()).or_default();
            // A cancelled write can commit after our key listing. Do not mark
            // that stale listing warm and lose the newly written work.
            if set.generation != generation {
                continue;
            }
            if !set.loaded {
                for (key, pending) in std::mem::take(&mut set.changes) {
                    if pending {
                        keys.insert(key);
                    } else {
                        keys.remove(&key);
                    }
                }
                set.keys = keys;
                set.loaded = true;
            }
            break set.keys.clone();
        };
        let mut records = Vec::new();
        for key in keys {
            match self.get::<T>(&key).await {
                Ok(Some(record)) if record.pending() => records.push(record),
                Err(error) => tracing::warn!(%key, %error, "unreadable adapter record retained"),
                _ => {}
            }
        }
        Ok(records)
    }
    pub fn deployment(agent: AgentId, name: &str) -> String {
        digest(&format!("{agent}/{name}"))
    }
    pub fn inbox(deployment: &str, id: &str) -> String {
        format!("adapters/v1/{deployment}/inbox/{}.json", digest(id))
    }
    pub fn route(deployment: &str, message: &IncomingMessage) -> String {
        format!(
            "adapters/v1/{deployment}/routes/{}.json",
            digest(&format!("{}/{}", message.account, message.conversation))
        )
    }
    pub fn outbox(deployment: &str, id: &str) -> String {
        format!("adapters/v1/{deployment}/outbox/{}.json", digest(id))
    }
    pub fn authority(principal: &str) -> String {
        format!("adapters/v1/authorities/{}.json", digest(principal))
    }
}
