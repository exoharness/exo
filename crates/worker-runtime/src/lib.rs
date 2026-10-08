#![cfg(target_arch = "wasm32")]

mod account;
mod execution;
mod host;
mod http;
mod operations;
mod policy;
mod sandbox;
mod turn_queue;

use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use executor::{BasicExecutor, LocalProvider, Runtime};
use futures::future::{AbortHandle, Abortable};
use futures::stream::FuturesUnordered;
use futures::{Stream, future::BoxFuture, task::waker};
use serde::Serialize;
use wasm_bindgen::prelude::*;

use execution::{WorkerExecutor, WorkerModel, WorkerTools};
use host::{Host, HostCall, HostStorage};

#[derive(Serialize)]
struct Completion {
    id: u32,
    result: Option<operations::Output>,
    error: Option<String>,
}
#[derive(Serialize)]
struct Progress {
    calls: Vec<HostCall>,
    cancelled: Vec<u32>,
    completed: Vec<Completion>,
    pending: bool,
    events: Vec<executor::typescript_runtime::RuntimeEvent>,
    watch_events: Vec<(u32, serde_json::Value)>,
}

/// Runs the existing LocalProvider inside a Worker. JavaScript supplies I/O;
/// this queue polls Rust futures without requiring a Tokio runtime or threads.
#[wasm_bindgen]
pub struct WorkerRuntime {
    runtime: Runtime,
    execution: Arc<WorkerExecutor>,
    mutations: Arc<tokio::sync::Mutex<()>>,
    host: Arc<Host>,
    progress: tokio::sync::broadcast::Sender<exoharness::Event>,
    running: FuturesUnordered<BoxFuture<'static, ()>>,
    completed: Arc<Mutex<Vec<Completion>>>,
    next_operation: u32,
    cancellations: HashMap<u32, AbortHandle>,
}

#[wasm_bindgen]
impl WorkerRuntime {
    /// Canonical resource IDs are essential: aliases must not create two queue owners.
    pub fn thread_name(account: &str, agent: &str, thread: &str) -> Result<String, JsValue> {
        let agent: exoharness::AgentId = agent.parse().map_err(js_error)?;
        let thread: exoharness::ThreadId = thread.parse().map_err(js_error)?;
        serde_json::to_string(&(account, agent, thread)).map_err(js_error)
    }

    #[wasm_bindgen(constructor)]
    pub fn new(master_key: &str, thread: bool) -> Result<WorkerRuntime, JsValue> {
        if master_key.len() != 64 || !master_key.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(JsValue::from_str(
                "VAULT_KEY must be a 32-byte hex encryption key",
            ));
        }
        let mut key = [0; 32];
        for (i, byte) in key.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&master_key[i * 2..i * 2 + 2], 16).map_err(js_error)?;
        }
        let host = Arc::new(Host::default());
        let backends = vec![exoharness::SandboxBackendRegistration::from_backend(
            exoharness::SandboxProvider::from_static("cloudflare"),
            Arc::new(sandbox::CloudflareBackend(host.clone())),
        )];
        let sandbox_default = backends[0].provider();
        let state: Arc<dyn exoharness::ExoHarness> = if thread {
            Arc::new(
                exoharness::HttpExoHarness::from_transport(Arc::new(account::AccountTransport {
                    host: host.clone(),
                    endpoint: url::Url::parse("https://exo.internal/exo").map_err(js_error)?,
                }))
                .with_runtime_host(host.clone()),
            )
        } else {
            Arc::new(
                exoharness::BasicExoHarness::hosted(
                    Arc::new(HostStorage(host.clone())),
                    key,
                    backends,
                    host.clone(),
                )
                .map_err(js_error)?,
            )
        };
        let coordinator = Arc::new(exoharness::turn_coordinator::StoredTurnCoordinator::new(
            Arc::new(turn_queue::QueueStore(host.clone())),
            Arc::default(),
        ));
        let execution = Arc::new(WorkerExecutor {
            host: host.clone(),
            state: state.clone(),
            sandbox_default,
            harnesses: Mutex::default(),
            basic: BasicExecutor::with_pricing(
                Arc::new(WorkerModel(host.clone())),
                Arc::new(WorkerTools),
                Arc::new(cost::PricingTable::empty()),
            ),
            coordinator: coordinator.clone(),
        });
        let runtime = Runtime::with_tracer(
            LocalProvider::with_host(state, execution.clone(), host.clone())
                .with_turn_coordinator(coordinator),
            Arc::new(executor::execution_tracing::NoopExecutionTracer),
        );
        Ok(Self {
            runtime,
            execution,
            mutations: Arc::default(),
            host,
            progress: tokio::sync::broadcast::channel(1024).0,
            running: FuturesUnordered::new(),
            completed: Arc::default(),
            next_operation: 0,
            cancellations: HashMap::new(),
        })
    }

    pub fn submit(&mut self, input: JsValue) -> Result<u32, JsValue> {
        let operation: operations::Operation =
            serde_wasm_bindgen::from_value(input).map_err(js_error)?;
        let id = self.next_operation;
        self.next_operation = self
            .next_operation
            .checked_add(1)
            .ok_or_else(|| JsValue::from_str("Worker operation id overflow"))?;
        let runtime = self.runtime.clone();
        let host = self.host.clone();
        let execution = self.execution.clone();
        let updates = self.mutations.clone();
        let completed = self.completed.clone();
        let progress = self.progress.clone();
        let (cancel, registration) = AbortHandle::new_pair();
        self.cancellations.insert(id, cancel);
        self.running.push(Box::pin(async move {
            let result = Abortable::new(
                operations::run(runtime, execution, host, updates, progress, id, operation),
                registration,
            )
            .await
            .unwrap_or_else(|error| Err(error.into()));
            let completion = match result {
                Ok(result) => Completion {
                    id,
                    result: Some(result),
                    error: None,
                },
                Err(error) => Completion {
                    id,
                    result: None,
                    error: Some(format!("{error:#}")),
                },
            };
            completed
                .lock()
                .expect("completed operations poisoned")
                .push(completion);
        }));
        self.host.awake.store(true, Ordering::Release);
        Ok(id)
    }

    pub fn cancel(&self, id: u32) {
        if let Some(handle) = self.cancellations.get(&id) {
            handle.abort();
        }
        self.host.awake.store(true, Ordering::Release);
    }

    pub fn resolve(&self, id: u32, result: JsValue, error: Option<String>) -> bool {
        self.host.resolve(id, error.map_or(Ok(result), Err))
    }

    pub fn poll(&mut self) -> Result<JsValue, JsValue> {
        let waker = waker(self.host.clone());
        let mut cx = Context::from_waker(&waker);
        loop {
            self.host.awake.store(false, Ordering::Release);
            let tasks = std::mem::take(&mut *self.host.tasks.lock().expect("host tasks poisoned"));
            for task in tasks {
                self.running.push(task);
            }
            while matches!(
                std::pin::Pin::new(&mut self.running).poll_next(&mut cx),
                Poll::Ready(Some(()))
            ) {}
            if !self.host.awake.load(Ordering::Acquire) {
                break;
            }
        }
        let (calls, cancelled) = self.host.take_calls();
        let completed = std::mem::take(
            &mut *self
                .completed
                .lock()
                .expect("completed operations poisoned"),
        );
        for completion in &completed {
            self.cancellations.remove(&completion.id);
        }
        Progress {
            calls,
            cancelled,
            completed,
            pending: !self.running.is_empty(),
            events: std::mem::take(&mut *self.host.events.lock().expect("Worker events poisoned")),
            watch_events: std::mem::take(
                &mut *self
                    .host
                    .watch_events
                    .lock()
                    .expect("Worker watches poisoned"),
            ),
        }
        .serialize(
            &serde_wasm_bindgen::Serializer::json_compatible().serialize_bytes_as_arrays(false),
        )
        .map_err(js_error)
    }
}

fn js_error(error: impl std::fmt::Display) -> JsValue {
    JsValue::from_str(&error.to_string())
}
