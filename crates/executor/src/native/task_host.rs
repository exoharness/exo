use crate::runtime_host::RuntimeHost;
use futures::future::BoxFuture;

pub struct TokioRuntimeHost;

impl RuntimeHost for TokioRuntimeHost {
    fn spawn(&self, task: BoxFuture<'static, ()>) {
        // Dropping the handle detaches the task; TaskGroup owns cancellation.
        drop(tokio::spawn(task));
    }
}
