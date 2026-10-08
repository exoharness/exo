//! The existing ExoHarness client over a trusted account Durable Object binding.
use crate::host::{Host, HostRequest};
use anyhow::Result;
use async_trait::async_trait;
use executor::runtime_host::RuntimeHost;
use exoharness::{
    AgentId, Event, EventId, EventStream, ExoHttpTransport, ResourceScope, SandboxActivity,
    SandboxId, ThreadId,
};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use std::{ops::Bound, sync::Arc};
use wasm_bindgen::JsValue;

pub(crate) struct AccountTransport {
    pub host: Arc<Host>,
    pub endpoint: url::Url,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(transparent)]
pub(crate) struct Subscription(#[serde(with = "serde_wasm_bindgen::preserve")] JsValue);

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum SubscriptionCommand {
    Read { subscription: Subscription },
    Close { subscription: Subscription },
}
struct CloseSubscription {
    host: Arc<Host>,
    subscription: Subscription,
}
impl Drop for CloseSubscription {
    fn drop(&mut self) {
        let host = self.host.clone();
        let subscription = self.subscription.clone();
        self.host.spawn(Box::pin(async move {
            if let Err(error) = host
                .call::<()>(HostRequest::Subscription {
                    command: SubscriptionCommand::Close { subscription },
                })
                .await
            {
                tracing::warn!(%error, "failed to close account event subscription");
            }
        }));
    }
}

#[async_trait]
impl ExoHttpTransport for AccountTransport {
    fn endpoint(&self) -> &url::Url {
        &self.endpoint
    }
    async fn request(
        &self,
        request: exoharness::protocol::Request,
    ) -> Result<exoharness::protocol::Response> {
        self.host
            .call(HostRequest::AccountRequest { request })
            .await
    }
    async fn sandbox_activity(
        &self,
        scope: ResourceScope,
        id: SandboxId,
    ) -> Result<SandboxActivity> {
        crate::sandbox::activity(self.host.clone(), scope, id).await
    }
    async fn watch_events(
        &self,
        agent_id: AgentId,
        thread_id: ThreadId,
        after: Bound<EventId>,
    ) -> Result<EventStream> {
        let subscription = self
            .host
            .call(HostRequest::Subscribe {
                agent_id,
                thread_id,
                after,
            })
            .await?;
        let guard = CloseSubscription {
            host: self.host.clone(),
            subscription,
        };
        Ok(futures::stream::try_unfold(guard, |guard| async move {
            let event: Option<Event> = guard
                .host
                .call(HostRequest::Subscription {
                    command: SubscriptionCommand::Read {
                        subscription: guard.subscription.clone(),
                    },
                })
                .await?;
            Ok(event.map(|event| (event, guard)))
        })
        .boxed())
    }
}
