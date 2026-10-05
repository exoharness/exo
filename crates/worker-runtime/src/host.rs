use std::collections::HashMap;
use std::ops::Bound;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use executor::runtime_host::RuntimeHost;
use executor::{AgentConfig, ConversationConfig, ModelRequest, SendRequest};
use exoharness::protocol::{Request, Response};
use exoharness::{AgentId, EventId, EventStream, ResourceScope, ThreadId, TurnRecord};
use futures::channel::oneshot;
use futures::future::BoxFuture;
use futures::task::ArcWake;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use url::Url;

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum HostRequest {
    State {
        request: Request,
    },
    Watch {
        agent_id: AgentId,
        thread_id: ThreadId,
        after: Option<EventId>,
    },
    Model {
        request: ModelRequest,
    },
    Tool {
        agent_id: AgentId,
        thread_id: ThreadId,
        request: exoharness::ToolRequest,
    },
    Harness {
        agent_id: AgentId,
        thread_id: ThreadId,
        turn: TurnRecord,
        agent_config: AgentConfig,
        conversation_config: ConversationConfig,
        request: SendRequest,
        recovering: bool,
    },
    StopSandbox {
        thread_id: ThreadId,
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

pub(crate) struct StateTransport {
    pub host: Arc<Host>,
    pub endpoint: Url,
}

#[derive(Deserialize)]
struct WatchedEvents {
    events: Vec<exoharness::Event>,
}

#[async_trait]
impl exoharness::ExoHttpTransport for StateTransport {
    fn endpoint(&self) -> &Url {
        &self.endpoint
    }
    async fn request(&self, request: Request) -> exoharness::Result<Response> {
        self.host.call(HostRequest::State { request }).await
    }
    async fn watch_events(
        &self,
        agent_id: AgentId,
        thread_id: ThreadId,
        after: Bound<EventId>,
    ) -> exoharness::Result<EventStream> {
        let after = match after {
            Bound::Excluded(id) => Some(id),
            Bound::Unbounded => None,
            Bound::Included(_) => bail!("watch_events requires an exclusive cursor"),
        };
        let host = self.host.clone();
        Ok(Box::pin(futures::stream::try_unfold(
            (host, after, std::collections::VecDeque::new()),
            move |(host, mut after, mut buffered)| async move {
                while buffered.is_empty() {
                    let result: WatchedEvents = host
                        .call(HostRequest::Watch {
                            agent_id,
                            thread_id,
                            after,
                        })
                        .await?;
                    buffered.extend(result.events);
                }
                let event: exoharness::Event = buffered.pop_front().expect("watched event");
                after = Some(event.id);
                Ok::<_, anyhow::Error>(Some((event, (host, after, buffered))))
            },
        )))
    }
}

// Worker harnesses use their JavaScript SandboxProcess adapter directly. The
// state RPC endpoint does not yet expose the durable process protocol.
pub(crate) struct WorkerProcesses;
#[async_trait]
impl exoharness::ExoHttpProcessTransport for WorkerProcesses {
    async fn run_in_sandbox(
        &self,
        _harness: &exoharness::HttpExoHarness,
        _scope: ResourceScope,
        _request: exoharness::RunInSandboxRequest,
    ) -> exoharness::Result<Box<dyn exoharness::SandboxProcess>> {
        bail!("use the Worker SandboxProcess adapter for byte streams")
    }
}
