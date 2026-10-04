use std::{collections::HashMap, path::PathBuf, sync::Arc};

use anyhow::Result;
use exoharness::ExoHarness;

use crate::{LocalProvider, ModelClient, Provider, Runtime, ToolRuntime};

use crate::harness_executor::HarnessExecutor;

use crate::braintrust::{BraintrustRuntimeConfig, BraintrustTracer};

impl LocalProvider {
    pub(crate) fn new(state: Arc<dyn ExoHarness>, executor: Arc<dyn HarnessExecutor>) -> Self {
        Self::with_host(state, executor, Arc::new(crate::TokioRuntimeHost))
    }
    pub fn basic<M: ModelClient + 'static, T: ToolRuntime + 'static>(
        state: Arc<dyn ExoHarness>,
        model: Arc<M>,
        tools: Arc<T>,
        pricing: Arc<cost::PricingTable>,
    ) -> Self {
        Self::basic_with_host(
            state,
            model,
            tools,
            pricing,
            Arc::new(crate::TokioRuntimeHost),
        )
    }
    pub fn rlm<M: ModelClient + 'static>(
        state: Arc<dyn ExoHarness>,
        model: Arc<M>,
        tools: Arc<dyn ToolRuntime>,
    ) -> Self {
        Self::new(state, Arc::new(crate::rlm::RlmExecutor { model, tools }))
    }
    pub fn typescript<T: ToolRuntime + 'static>(
        state: Arc<dyn ExoHarness>,
        workspace: PathBuf,
        env: HashMap<String, String>,
        tools: Arc<T>,
    ) -> Self {
        let executor =
            crate::typescript::TypeScriptExecutor::new(Arc::clone(&state), workspace, env, tools);
        Self::new(state, Arc::new(executor))
    }
}

impl Runtime {
    pub fn new(
        provider: impl Provider + 'static,
        runtime_config: Option<BraintrustRuntimeConfig>,
    ) -> Self {
        Self::with_tracer(provider, Arc::new(BraintrustTracer::new(runtime_config)))
    }
}

impl LocalProvider {
    pub fn managed(
        state: Arc<dyn ExoHarness>,
        config: exoharness::BasicExoHarnessConfig,
        env: std::collections::HashMap<String, String>,
        pricing: Arc<cost::PricingTable>,
    ) -> Result<Self> {
        let executor = crate::native::managed_executor::ManagedExecutor::new(
            state.clone(),
            config,
            env,
            pricing,
        )?;
        Ok(Self::new(state, Arc::new(executor)))
    }
}
