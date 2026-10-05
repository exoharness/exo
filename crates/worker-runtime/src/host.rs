use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use executor::runtime_host::RuntimeHost;
use executor::{AgentConfig, ConversationConfig, ModelRequest, SendRequest};
use exoharness::{AgentId, ThreadId, TurnRecord};
use futures::channel::oneshot;
use futures::future::BoxFuture;
use futures::task::ArcWake;
use serde::{Deserialize, Serialize, de::DeserializeOwned};

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum HostRequest {
    Storage {
        operation: StorageOperation,
    },
    Sandbox {
        command: crate::sandbox::Command,
    },
    Model {
        request: ModelRequest,
    },
    Harness {
        sandbox_id: String,
        agent_id: AgentId,
        thread_id: ThreadId,
        turn: TurnRecord,
        agent_config: AgentConfig,
        conversation_config: ConversationConfig,
        request: SendRequest,
        recovering: bool,
    },
}

#[derive(Serialize)]
pub(crate) struct HostCall {
    pub id: u32,
    pub request: HostRequest,
}

#[derive(Deserialize)]
pub(crate) struct HostReply {
    pub result: Option<String>,
    pub error: Option<String>,
}

#[derive(Default)]
pub(crate) struct Host {
    next_id: AtomicU32,
    calls: Mutex<Vec<HostCall>>,
    replies: Mutex<HashMap<u32, oneshot::Sender<HostReply>>>,
    pub tasks: Mutex<Vec<BoxFuture<'static, ()>>>,
    pub awake: AtomicBool,
}

impl Host {
    pub async fn call<T: DeserializeOwned>(&self, request: HostRequest) -> Result<T> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (reply, receiver) = oneshot::channel();
        self.replies
            .lock()
            .expect("host replies poisoned")
            .insert(id, reply);
        self.calls
            .lock()
            .expect("host calls poisoned")
            .push(HostCall { id, request });
        let reply = receiver
            .await
            .context("Worker host stopped before replying")?;
        if let Some(error) = reply.error {
            bail!("{error}");
        }
        serde_json::from_str(&reply.result.context("missing Worker host response")?)
            .context("decoding Worker host response")
    }

    pub fn resolve(&self, id: u32, reply: HostReply) -> bool {
        if let Some(sender) = self
            .replies
            .lock()
            .expect("host replies poisoned")
            .remove(&id)
        {
            return sender.send(reply).is_ok();
        }
        false
    }

    pub fn take_calls(&self) -> (Vec<HostCall>, Vec<u32>) {
        let mut replies = self.replies.lock().expect("host replies poisoned");
        let cancelled = replies
            .iter()
            .filter_map(|(id, reply)| reply.is_canceled().then_some(*id))
            .collect::<Vec<_>>();
        for id in &cancelled {
            replies.remove(id);
        }
        let calls = std::mem::take(&mut *self.calls.lock().expect("host calls poisoned"));
        (
            calls
                .into_iter()
                .filter(|call| replies.contains_key(&call.id))
                .collect(),
            cancelled,
        )
    }
}

impl RuntimeHost for Host {
    fn spawn(&self, task: BoxFuture<'static, ()>) {
        self.tasks.lock().expect("host tasks poisoned").push(task);
        self.awake.store(true, Ordering::Release);
    }
}

impl ArcWake for Host {
    fn wake_by_ref(host: &Arc<Self>) {
        host.awake.store(true, Ordering::Release);
    }
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum StorageOperation {
    Put {
        key: String,
        bytes: Vec<u8>,
        blob: bool,
    },
    Get {
        key: String,
    },
    List {
        prefix: String,
    },
    Delete {
        key: String,
    },
    Copy {
        source: String,
        destination: String,
    },
}

pub(crate) struct HostStorage(pub Arc<Host>);
#[async_trait]
impl exoharness::Storage for HostStorage {
    async fn put(&self, key: &str, bytes: Vec<u8>, blob: bool) -> Result<()> {
        self.0
            .call(HostRequest::Storage {
                operation: StorageOperation::Put {
                    key: key.into(),
                    bytes,
                    blob,
                },
            })
            .await
    }
    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        self.0
            .call(HostRequest::Storage {
                operation: StorageOperation::Get { key: key.into() },
            })
            .await
    }
    async fn list(&self, prefix: &str) -> Result<Vec<String>> {
        self.0
            .call(HostRequest::Storage {
                operation: StorageOperation::List {
                    prefix: prefix.into(),
                },
            })
            .await
    }
    async fn delete(&self, key: &str) -> Result<()> {
        self.0
            .call(HostRequest::Storage {
                operation: StorageOperation::Delete { key: key.into() },
            })
            .await
    }
    async fn copy(&self, source: &str, destination: &str) -> Result<()> {
        self.0
            .call(HostRequest::Storage {
                operation: StorageOperation::Copy {
                    source: source.into(),
                    destination: destination.into(),
                },
            })
            .await
    }
}
