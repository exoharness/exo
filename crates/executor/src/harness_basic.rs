use std::collections::HashMap;
use std::sync::Arc;

use crate::{BasicExecutor, BraintrustRuntimeConfig, ModelClient, ToolRuntime};
use exoharness::{BasicExoHarness, BasicExoHarnessConfig, ExoHarness, Result};

use crate::harness_executor::ExecutorHarnessRuntime;
use crate::harness_facade::{SharedHarness, SharedHarnessBacked};
use crate::harness_runtime::RouterModelClient;
use crate::harness_tool::BasicToolRuntime;

pub struct BasicHarness<M, T> {
    inner: SharedHarness<ExecutorHarnessRuntime<BasicExecutor<M, T>>>,
}

impl<M, T> BasicHarness<M, T> {
    pub fn new(exoharness: Arc<dyn ExoHarness>, model: Arc<M>, tools: Arc<T>) -> Self
    where
        M: ModelClient + 'static,
        T: ToolRuntime + 'static,
    {
        let runtime = ExecutorHarnessRuntime::new(BasicExecutor::new(model, tools), None);
        Self {
            inner: SharedHarness::new(exoharness, runtime),
        }
    }
}

impl BasicHarness<RouterModelClient, BasicToolRuntime> {
    pub async fn from_config(
        exo_config: BasicExoHarnessConfig,
        runtime_config: Option<BraintrustRuntimeConfig>,
        env: HashMap<String, String>,
    ) -> Result<Self> {
        let exoharness = Arc::new(BasicExoHarness::new(exo_config).await?);
        let model = Arc::new(RouterModelClient::new(env));
        let tools = Arc::new(BasicToolRuntime);
        let runtime = ExecutorHarnessRuntime::new(BasicExecutor::new(model, tools), runtime_config);
        Ok(Self {
            inner: SharedHarness::new(exoharness, runtime),
        })
    }
}

impl<M, T> SharedHarnessBacked for BasicHarness<M, T>
where
    M: ModelClient + 'static,
    T: ToolRuntime + 'static,
{
    type Runtime = ExecutorHarnessRuntime<BasicExecutor<M, T>>;

    fn shared_harness(&self) -> &SharedHarness<Self::Runtime> {
        &self.inner
    }
}
