mod execution;
mod host;
mod operations;

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use executor::{BasicExecutor, LocalProvider, Runtime};
use futures::stream::FuturesUnordered;
use futures::{Stream, future::BoxFuture, task::waker};
use serde::Serialize;
use wasm_bindgen::prelude::*;

use execution::{WorkerExecutor, WorkerModel, WorkerTools, WorkerTracer};
use host::{Host, HostCall, HostReply, StateTransport, WorkerProcesses};

#[derive(Serialize)]
struct Completion {
    id: u32,
    result: Option<String>,
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
    host: Arc<Host>,
    running: FuturesUnordered<BoxFuture<'static, ()>>,
    completed: Arc<Mutex<Vec<Completion>>>,
    next_operation: u32,
}

#[wasm_bindgen]
impl WorkerRuntime {
    #[wasm_bindgen(constructor)]
    pub fn new() -> Result<WorkerRuntime, JsValue> {
        let host = Arc::new(Host::default());
        let state = Arc::new(exoharness::HttpExoHarness::from_transports(
            Arc::new(StateTransport {
                host: host.clone(),
                endpoint: url::Url::parse("https://worker.internal/request").map_err(js_error)?,
            }),
            Arc::new(WorkerProcesses),
        ));
        let execution = Arc::new(WorkerExecutor {
            host: host.clone(),
            basic: BasicExecutor::with_pricing(
                Arc::new(WorkerModel(host.clone())),
                Arc::new(WorkerTools(host.clone())),
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
            host,
            running: FuturesUnordered::new(),
            completed: Arc::default(),
            next_operation: 0,
        })
    }

    pub fn submit(&mut self, json: &str) -> Result<u32, JsValue> {
        let operation: operations::Operation = serde_json::from_str(json).map_err(js_error)?;
        let id = self.next_operation;
        self.next_operation = self
            .next_operation
            .checked_add(1)
            .ok_or_else(|| JsValue::from_str("Worker operation id overflow"))?;
        let runtime = self.runtime.clone();
        let host = self.host.clone();
        let completed = self.completed.clone();
        self.running.push(Box::pin(async move {
            let result = operations::run(runtime, host, operation).await;
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

    pub fn resolve(&self, id: u32, json: &str) -> Result<bool, JsValue> {
        let reply: HostReply = serde_json::from_str(json).map_err(js_error)?;
        Ok(self.host.resolve(id, reply))
    }

    pub fn poll(&mut self) -> Result<String, JsValue> {
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
        serde_json::to_string(&Progress {
            calls,
            cancelled,
            completed,
            pending: !self.running.is_empty(),
        })
        .map_err(js_error)
    }
}

fn js_error(error: impl std::fmt::Display) -> JsValue {
    JsValue::from_str(&error.to_string())
}
