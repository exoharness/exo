//! Task scheduling supplied by the deployment host. Provider logic does not
//! require a Tokio runtime; Workers can keep these futures alive with waitUntil.
use std::future::Future;
use std::sync::Arc;

use anyhow::{Result, anyhow};
use futures::future::{AbortHandle, Abortable, BoxFuture};
use futures::stream::FuturesUnordered;
use futures::{FutureExt, StreamExt};
use tokio::sync::oneshot;

pub trait RuntimeHost: Send + Sync {
    fn spawn(&self, task: BoxFuture<'static, ()>);
}

struct CancelOnDrop(AbortHandle);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

pub(crate) struct TaskGroup {
    host: Arc<dyn RuntimeHost>,
    tasks: FuturesUnordered<BoxFuture<'static, Result<()>>>,
}

impl TaskGroup {
    pub(crate) fn new(host: Arc<dyn RuntimeHost>) -> Self {
        Self {
            host,
            tasks: FuturesUnordered::new(),
        }
    }

    pub(crate) fn spawn(&mut self, task: impl Future<Output = ()> + Send + 'static) {
        let (done, completion) = oneshot::channel();
        let (cancel, cancelled) = AbortHandle::new_pair();
        let cancel = CancelOnDrop(cancel);
        self.host.spawn(Box::pin(async move {
            let result = match std::panic::AssertUnwindSafe(Abortable::new(task, cancelled))
                .catch_unwind()
                .await
            {
                Ok(Ok(())) => Ok(()),
                Ok(Err(_)) => Err(anyhow!("runtime task cancelled")),
                Err(_) => Err(anyhow!("runtime task panicked")),
            };
            if done.send(result).is_err() {
                tracing::debug!("runtime task group closed before completion");
            }
        }));
        self.tasks.push(Box::pin(async move {
            let _cancel = cancel;
            completion
                .await
                .map_err(|error| anyhow!("runtime task stopped: {error}"))?
        }));
    }

    pub(crate) fn try_join_next(&mut self) -> Option<Result<()>> {
        self.tasks.next().now_or_never().flatten()
    }

    pub(crate) async fn join_next(&mut self) -> Option<Result<()>> {
        self.tasks.next().await
    }
}
