use std::sync::Arc;

use crate::{BasicExecutor, BraintrustRuntimeConfig, ModelClient, ToolRuntime};
use cost::PricingTable;
use exoharness::ExoHarness;

use crate::harness_executor::ExecutorHarnessRuntime;
use crate::harness_facade::{SharedHarness, SharedHarnessBacked};

pub struct BasicHarness<M, T> {
    inner: SharedHarness<ExecutorHarnessRuntime<BasicExecutor<M, T>>>,
}

impl<M, T> BasicHarness<M, T> {
    pub fn with_runtime_config(
        exoharness: Arc<dyn ExoHarness>,
        model: Arc<M>,
        tools: Arc<T>,
        pricing: Arc<PricingTable>,
        runtime_config: Option<BraintrustRuntimeConfig>,
    ) -> Self
    where
        M: ModelClient + 'static,
        T: ToolRuntime + 'static,
    {
        let runtime = ExecutorHarnessRuntime::new(
            BasicExecutor::with_pricing(model, tools, pricing),
            runtime_config,
        );
        Self {
            inner: SharedHarness::new(exoharness, runtime),
        }
    }

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

    /// Construct a harness whose executor fills cost from an explicit table.
    pub fn with_pricing_table(
        exoharness: Arc<dyn ExoHarness>,
        model: Arc<M>,
        tools: Arc<T>,
        pricing: Arc<PricingTable>,
    ) -> Self
    where
        M: ModelClient + 'static,
        T: ToolRuntime + 'static,
    {
        let runtime =
            ExecutorHarnessRuntime::new(BasicExecutor::with_pricing(model, tools, pricing), None);
        Self {
            inner: SharedHarness::new(exoharness, runtime),
        }
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
