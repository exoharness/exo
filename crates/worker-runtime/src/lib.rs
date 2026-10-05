#![cfg(target_arch = "wasm32")]

mod execution;
mod host;
mod http;
mod operations;
mod policy;
mod sandbox;

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

use execution::{WorkerExecutor, WorkerModel, WorkerTools, WorkerTracer};
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
}

/// Runs the existing LocalProvider inside a Worker. JavaScript supplies I/O;
/// this queue polls Rust futures without requiring a Tokio runtime or threads.
#[wasm_bindgen]
pub struct WorkerRuntime {
    runtime: Runtime,
    mutations: Arc<tokio::sync::Mutex<()>>,
    host: Arc<Host>,
    running: FuturesUnordered<BoxFuture<'static, ()>>,
    completed: Arc<Mutex<Vec<Completion>>>,
    next_operation: u32,
    cancellations: HashMap<u32, AbortHandle>,
}

#[wasm_bindgen]
impl WorkerRuntime {
    #[wasm_bindgen(constructor)]
    pub fn new(master_key: &str) -> Result<WorkerRuntime, JsValue> {
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
        let state = Arc::new(
            exoharness::BasicExoHarness::hosted(
                Arc::new(HostStorage(host.clone())),
                key,
                vec![exoharness::SandboxBackendRegistration::from_backend(
                    exoharness::SandboxProvider::from_static("cloudflare"),
                    Arc::new(sandbox::CloudflareBackend(host.clone())),
                )],
                host.clone(),
            )
            .map_err(js_error)?,
        );
        let execution = Arc::new(WorkerExecutor {
            host: host.clone(),
            basic: BasicExecutor::with_pricing(
                Arc::new(WorkerModel(host.clone())),
                Arc::new(WorkerTools),
                Arc::new(cost::PricingTable::empty()),
            ),
        });
        let runtime = Runtime::with_tracer(
            LocalProvider::with_host(state, execution, host.clone()),
            Arc::new(WorkerTracer),
        );
        runtime.begin_recovery_scan();
        Ok(Self {
            runtime,
            mutations: Arc::default(),
            host,
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
        let updates = self.mutations.clone();
        let completed = self.completed.clone();
        let (cancel, registration) = AbortHandle::new_pair();
        self.cancellations.insert(id, cancel);
        self.running.push(Box::pin(async move {
            let result = Abortable::new(
                operations::run(runtime, host, updates, operation),
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
