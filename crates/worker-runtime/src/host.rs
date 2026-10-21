use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use async_trait::async_trait;
use executor::ModelRequest;
use executor::runtime_host::RuntimeHost;
use executor::typescript_runtime::{RuntimeEvent, TypeScriptInitPayload};
use futures::channel::oneshot;
use futures::future::BoxFuture;
use futures::task::ArcWake;
use serde::{Serialize, de::DeserializeOwned};
use wasm_bindgen::JsValue;

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum HostRequest {
    Sleep {
        millis: u64,
    },
    Storage {
        operation: StorageOperation,
    },
    Sandbox {
        command: crate::sandbox::Command,
    },
    Mcp {
        command: crate::mcp::Command,
    },
    McpHttp {
        request: crate::mcp::HttpRequest,
        token: Option<String>,
    },
    OauthHttp {
        request: OAuthRequest,
    },
    Model {
        request: ModelRequest,
    },
    ResetHarness,
    Harness {
        payload: TypeScriptInitPayload,
    },
    AccountRequest {
        request: exoharness::protocol::Request,
    },
    Subscribe {
        agent_id: exoharness::AgentId,
        thread_id: exoharness::ThreadId,
        after: std::ops::Bound<exoharness::EventId>,
    },
    Subscription {
        command: crate::account::SubscriptionCommand,
    },
}

#[derive(Serialize)]
pub(crate) struct HostCall {
    pub id: u32,
    pub request: HostRequest,
}

type HostReply = Result<JsValue, String>;

#[derive(Default)]
pub(crate) struct Host {
    next_id: AtomicU32,
    calls: Mutex<Vec<HostCall>>,
    replies: Mutex<HashMap<u32, oneshot::Sender<HostReply>>>,
    pub tasks: Mutex<Vec<BoxFuture<'static, ()>>>,
    pub awake: AtomicBool,
    pub events: Mutex<Vec<RuntimeEvent>>,
    pub watches: crate::watch::Watches,
}

impl Host {
    pub fn call<T: DeserializeOwned + 'static>(
        &self,
        request: HostRequest,
    ) -> BoxFuture<'static, Result<T>> {
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
        Box::pin(async move {
            let reply = receiver
                .await
                .context("Worker host stopped before replying")?;
            let result = reply.map_err(anyhow::Error::msg)?;
            serde_wasm_bindgen::from_value(result).context("decoding Worker host response")
        })
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
    fn sleep(&self, duration: std::time::Duration) -> BoxFuture<'static, Result<()>> {
        self.call(HostRequest::Sleep {
            millis: duration.as_millis() as u64,
        })
    }

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
        #[serde(with = "serde_bytes")]
        bytes: Vec<u8>,
        blob: bool,
        alarm: Option<bool>,
    },
    Get {
        key: String,
    },
    List {
        prefix: String,
        after: Option<String>,
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
                    alarm: None,
                },
            })
            .await
    }
    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        let bytes: Option<serde_bytes::ByteBuf> = self
            .0
            .call(HostRequest::Storage {
                operation: StorageOperation::Get { key: key.into() },
            })
            .await?;
        Ok(bytes.map(serde_bytes::ByteBuf::into_vec))
    }
    async fn list_after(&self, prefix: &str, after: Option<&str>) -> Result<Vec<String>> {
        self.0
            .call(HostRequest::Storage {
                operation: StorageOperation::List {
                    prefix: prefix.into(),
                    after: after.map(str::to_owned),
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

#[derive(Serialize)]
pub(crate) struct OAuthRequest {
    url: String,
    method: String,
    headers: Vec<(String, String)>,
    #[serde(with = "serde_bytes")]
    body: Vec<u8>,
}
#[derive(serde::Deserialize)]
struct OAuthResponse {
    status: u16,
    headers: Vec<(String, String)>,
    #[serde(with = "serde_bytes")]
    body: Vec<u8>,
}
pub(crate) struct WorkerOAuthHttpClient(pub Arc<Host>);
#[async_trait]
impl exoharness::vault::OAuthHttpClient for WorkerOAuthHttpClient {
    async fn request(&self, request: oauth2::HttpRequest) -> Result<oauth2::HttpResponse> {
        let (parts, body) = request.into_parts();
        let response: OAuthResponse = self
            .0
            .call(HostRequest::OauthHttp {
                request: OAuthRequest {
                    url: parts.uri.to_string(),
                    method: parts.method.to_string(),
                    body,
                    headers: parts
                        .headers
                        .iter()
                        .map(|(name, value)| Ok((name.to_string(), value.to_str()?.to_owned())))
                        .collect::<Result<_>>()?,
                },
            })
            .await?;
        let mut builder = http::Response::builder().status(response.status);
        for (name, value) in response.headers {
            builder = builder.header(name, value);
        }
        Ok(builder.body(response.body)?)
    }
}
