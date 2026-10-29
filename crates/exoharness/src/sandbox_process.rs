use std::{
    collections::{HashMap, VecDeque},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use futures::{
    future::join_all,
    io::{AsyncReadExt, AsyncWriteExt},
};
use tokio::{
    io::{AsyncReadExt as TokioAsyncReadExt, AsyncWriteExt as TokioAsyncWriteExt},
    sync::{Mutex as AsyncMutex, Notify, OwnedSemaphorePermit, Semaphore, mpsc, oneshot},
    time::Instant,
};
use tokio_util::{
    compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt},
    sync::CancellationToken,
};

use crate::{
    BoxAsyncRead, BoxAsyncWrite, BoxSandboxTcpStream, GetSandboxProcessEventsResult,
    ManagedSandboxHandle, SandboxAttachment, SandboxCommand, SandboxCommandOutput,
    SandboxProcessEvent, SandboxProcessId, SandboxProcessParts, SandboxProcessStatus,
    SandboxProcessStdin, SandboxTerminalControl, SandboxTerminalParts, SandboxTerminalSize,
    SnapshotPayload, Uuid7,
};

const OUTPUT_READ_BYTES: usize = 64 * 1024;
const TRUNCATION_SUFFIX: &str = "... [truncated]";
const RETENTION_SWEEP_INTERVAL: Duration = Duration::from_secs(1);
const PIPE_BUFFER_BYTES: usize = 64 * 1024;

/// Bounds for the processes one sandbox handle manages. The defaults keep
/// every event for the handle's lifetime, which suits a single-tenant host;
/// hosts that serve many sandboxes set limits and share a budget.
#[derive(Clone)]
pub struct ProcessLimits {
    /// Output bytes one process retains. Older events are dropped and
    /// replaced by a `SandboxProcessEvent::Truncated` marker.
    pub max_output_bytes: Option<usize>,
    /// Longest error message kept in an event or status.
    pub max_error_bytes: Option<usize>,
    /// Retained output budget shared with other handles.
    pub output_budget: Option<Arc<ProcessOutputBudget>>,
    /// Every record, running or finished, holds one permit from each.
    pub record_slots: Vec<Arc<Semaphore>>,
    /// How long finished records stay readable.
    pub retention: Option<Duration>,
    /// How many finished records stay readable.
    pub max_retained: Option<usize>,
    /// How long to wait for output streams after the process exits.
    pub drain_grace: Duration,
}

impl Default for ProcessLimits {
    fn default() -> Self {
        Self {
            max_output_bytes: None,
            max_error_bytes: None,
            output_budget: None,
            record_slots: Vec::new(),
            retention: None,
            max_retained: None,
            drain_grace: Duration::from_secs(2),
        }
    }
}

/// Retained process output accounted against a byte limit, optionally
/// nested under a parent budget.
pub struct ProcessOutputBudget {
    parent: Option<Arc<ProcessOutputBudget>>,
    max_bytes: usize,
    retained_bytes: AtomicUsize,
}

impl ProcessOutputBudget {
    pub fn new(max_bytes: usize) -> Self {
        Self {
            parent: None,
            max_bytes,
            retained_bytes: AtomicUsize::new(0),
        }
    }

    pub fn child(parent: Arc<Self>, max_bytes: usize) -> Self {
        Self {
            parent: Some(parent),
            ..Self::new(max_bytes)
        }
    }

    pub fn max_bytes(&self) -> usize {
        self.max_bytes
    }

    pub fn retained_bytes(&self) -> usize {
        self.retained_bytes.load(Ordering::Relaxed)
    }

    fn try_reserve(&self, bytes: usize) -> bool {
        if self
            .retained_bytes
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |retained| {
                retained
                    .checked_add(bytes)
                    .filter(|total| *total <= self.max_bytes)
            })
            .is_err()
        {
            return false;
        }
        if self
            .parent
            .as_ref()
            .is_some_and(|parent| !parent.try_reserve(bytes))
        {
            self.retained_bytes.fetch_sub(bytes, Ordering::Relaxed);
            return false;
        }
        true
    }

    fn release(&self, bytes: usize) {
        if let Some(parent) = &self.parent {
            parent.release(bytes);
        }
        self.retained_bytes
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |retained| {
                Some(retained.saturating_sub(bytes))
            })
            .expect("releasing process output budget must succeed");
    }
}

/// A process could not start because every record slot is taken.
#[derive(Debug)]
pub struct ProcessLimitReached;

impl std::fmt::Display for ProcessLimitReached {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("sandbox process limit reached")
    }
}

impl std::error::Error for ProcessLimitReached {}

/// No managed process has this id on this handle, or its record expired.
#[derive(Debug)]
pub struct ProcessNotFound(pub SandboxProcessId);

impl std::fmt::Display for ProcessNotFound {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "sandbox process not found: {}", self.0)
    }
}

impl std::error::Error for ProcessNotFound {}

/// Give a stream-backed provider ID-based process operations. Records live for
/// the lifetime of the returned sandbox handle; they cannot survive a host
/// process restart. Providers with their own process service implement the
/// operations directly instead of installing this adapter.
pub fn with_process_management(
    handle: Arc<dyn ManagedSandboxHandle>,
) -> Arc<dyn ManagedSandboxHandle> {
    with_process_limits(handle, ProcessLimits::default())
}

/// `with_process_management` with explicit bounds. Wrapping an already
/// managed handle is fine: the outer registry owns every process it starts.
pub fn with_process_limits(
    handle: Arc<dyn ManagedSandboxHandle>,
    limits: ProcessLimits,
) -> Arc<dyn ManagedSandboxHandle> {
    Arc::new(ProcessManagedSandbox {
        handle,
        processes: ProcessRegistry::new(limits),
    })
}

/// Raw pipes over a managed process, for handles whose processes live behind
/// an id-based API such as a remote worker. Output arrives through
/// `next_process_events`, input goes through `write_process_input`, and closing
/// or dropping stdin closes the process's stdin. Dropping `wait` before the
/// process finishes cancels it.
pub fn managed_process_parts(
    handle: Arc<dyn ManagedSandboxHandle>,
    process_id: SandboxProcessId,
) -> SandboxProcessParts {
    let (stdout_writer, stdout) = tokio::io::duplex(PIPE_BUFFER_BYTES);
    let (stderr_writer, stderr) = tokio::io::duplex(PIPE_BUFFER_BYTES);
    let (stdin, stdin_reader) = tokio::io::duplex(PIPE_BUFFER_BYTES);
    let (exit_sender, exit) = oneshot::channel();
    let finished = Arc::new(AtomicBool::new(false));
    let cleanup = ManagedProcessCleanup {
        handle: Arc::clone(&handle),
        process_id: process_id.clone(),
        finished: Arc::clone(&finished),
    };
    tokio::spawn(forward_managed_input(
        Arc::clone(&handle),
        process_id.clone(),
        stdin_reader,
    ));
    tokio::spawn(forward_managed_output(
        handle,
        process_id,
        stdout_writer,
        stderr_writer,
        exit_sender,
        finished,
    ));
    SandboxProcessParts {
        stdout: Box::pin(stdout.compat()),
        stderr: Box::pin(stderr.compat()),
        stdin: Box::pin(stdin.compat_write()),
        wait: Box::pin(async move {
            let _cleanup = cleanup;
            exit.await
                .map_err(|_| anyhow!("sandbox process ended without a status"))?
        }),
    }
}

/// `managed_process_parts` for a terminal started with `start_managed_terminal`.
pub fn managed_terminal_parts(
    handle: Arc<dyn ManagedSandboxHandle>,
    process_id: SandboxProcessId,
) -> SandboxTerminalParts {
    let control = Arc::new(ManagedTerminalControl {
        handle: Arc::clone(&handle),
        process_id: process_id.clone(),
    });
    let parts = managed_process_parts(handle, process_id);
    SandboxTerminalParts {
        output: parts.stdout,
        input: parts.stdin,
        wait: parts.wait,
        control,
    }
}

struct ManagedTerminalControl {
    handle: Arc<dyn ManagedSandboxHandle>,
    process_id: SandboxProcessId,
}

#[async_trait]
impl SandboxTerminalControl for ManagedTerminalControl {
    async fn resize(&self, size: SandboxTerminalSize) -> Result<()> {
        self.handle.resize_terminal(&self.process_id, size).await
    }
}

struct ManagedProcessCleanup {
    handle: Arc<dyn ManagedSandboxHandle>,
    process_id: SandboxProcessId,
    finished: Arc<AtomicBool>,
}

impl Drop for ManagedProcessCleanup {
    fn drop(&mut self) {
        if self.finished.load(Ordering::Acquire) {
            return;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let handle = Arc::clone(&self.handle);
        let process_id = std::mem::take(&mut self.process_id);
        runtime.spawn(async move {
            let _ = handle.cancel_process(&process_id).await;
        });
    }
}

async fn forward_managed_input(
    handle: Arc<dyn ManagedSandboxHandle>,
    process_id: SandboxProcessId,
    mut reader: tokio::io::DuplexStream,
) {
    let mut buffer = vec![0; 8192];
    loop {
        match TokioAsyncReadExt::read(&mut reader, &mut buffer).await {
            Ok(0) | Err(_) => {
                let _ = handle.close_process_input(&process_id).await;
                return;
            }
            Ok(length) => {
                if handle
                    .write_process_input(&process_id, &buffer[..length])
                    .await
                    .is_err()
                {
                    return;
                }
            }
        }
    }
}

async fn forward_managed_output(
    handle: Arc<dyn ManagedSandboxHandle>,
    process_id: SandboxProcessId,
    mut stdout: tokio::io::DuplexStream,
    mut stderr: tokio::io::DuplexStream,
    exit: oneshot::Sender<Result<i32>>,
    finished: Arc<AtomicBool>,
) {
    let mut cursor = 0;
    let mut truncated = false;
    let result = loop {
        let result = match handle.next_process_events(&process_id, cursor).await {
            Ok(result) => result,
            Err(error) => break Err(error),
        };
        let mut ended = None;
        for event in result.events {
            cursor = event.cursor();
            match event {
                SandboxProcessEvent::Stdout { data, .. } => {
                    let _ = TokioAsyncWriteExt::write_all(&mut stdout, &data).await;
                }
                SandboxProcessEvent::Stderr { data, .. } => {
                    let _ = TokioAsyncWriteExt::write_all(&mut stderr, &data).await;
                }
                SandboxProcessEvent::Truncated { .. } => truncated = true,
                SandboxProcessEvent::Exit { exit_code, .. } => ended = Some(Ok(exit_code)),
                SandboxProcessEvent::Error { message, .. } => ended = Some(Err(anyhow!(message))),
                SandboxProcessEvent::Cancelled { .. } => {
                    ended = Some(Err(anyhow!("sandbox process was cancelled")))
                }
            }
        }
        if let Some(ended) = ended {
            break ended;
        }
        match result.status {
            SandboxProcessStatus::Running => {}
            SandboxProcessStatus::Exited { exit_code } => break Ok(exit_code),
            SandboxProcessStatus::Failed { message } => break Err(anyhow!(message)),
            SandboxProcessStatus::Cancelled => break Err(anyhow!("sandbox process was cancelled")),
        }
    };
    finished.store(true, Ordering::Release);
    drop(stdout);
    drop(stderr);
    let result = match result {
        Ok(_) if truncated => Err(anyhow!("sandbox process output was truncated")),
        result => result,
    };
    let _ = exit.send(result);
}

struct ProcessManagedSandbox {
    handle: Arc<dyn ManagedSandboxHandle>,
    processes: ProcessRegistry,
}

#[async_trait]
impl ManagedSandboxHandle for ProcessManagedSandbox {
    async fn command_environment(&self) -> Result<std::collections::HashMap<String, String>> {
        self.handle.command_environment().await
    }

    fn id(&self) -> &str {
        self.handle.id()
    }
    fn provider_state(&self) -> Option<serde_json::Value> {
        self.handle.provider_state()
    }
    fn effective_image(&self) -> Option<String> {
        self.handle.effective_image()
    }
    async fn is_running(&self) -> Result<Option<bool>> {
        self.handle.is_running().await
    }
    fn allocation_identity(&self) -> Option<String> {
        self.handle.allocation_identity()
    }
    fn recovery_point(&self) -> Option<crate::SandboxRecoveryPoint> {
        self.handle.recovery_point()
    }
    fn prepared_repositories(&self) -> bool {
        self.handle.prepared_repositories()
    }
    fn validity(&self) -> crate::HandleValidity {
        self.handle.validity()
    }
    async fn activity(&self) -> Result<crate::SandboxActivity> {
        self.handle.activity().await
    }
    async fn exec(&self, command: &SandboxCommand) -> Result<SandboxCommandOutput> {
        self.handle.exec(command).await
    }
    async fn start_process(&self, command: &SandboxCommand) -> Result<SandboxProcessParts> {
        self.handle.start_process(command).await
    }
    async fn start_managed_process(
        &self,
        command: &SandboxCommand,
        stdin: SandboxProcessStdin,
    ) -> Result<SandboxProcessId> {
        let slots = self.processes.reserve()?;
        let parts = self.handle.start_process(command).await?;
        Ok(self.processes.start(parts, stdin, None, slots))
    }
    async fn start_managed_terminal(
        &self,
        command: &SandboxCommand,
        size: SandboxTerminalSize,
    ) -> Result<SandboxProcessId> {
        let slots = self.processes.reserve()?;
        let SandboxTerminalParts {
            output,
            input,
            wait,
            control,
        } = self.handle.start_terminal(command, size).await?;
        let parts = SandboxProcessParts {
            stdout: output,
            stderr: Box::pin(futures::io::empty()),
            stdin: input,
            wait,
        };
        Ok(self
            .processes
            .start(parts, SandboxProcessStdin::Open, Some(control), slots))
    }
    async fn resize_terminal(&self, id: &str, size: SandboxTerminalSize) -> Result<()> {
        self.processes
            .get(id)?
            .terminal
            .as_ref()
            .with_context(|| format!("sandbox process is not a terminal: {id}"))?
            .resize(size)
            .await
    }
    async fn write_process_input(&self, id: &str, data: &[u8]) -> Result<()> {
        self.processes.get(id)?.write_input(data).await
    }
    async fn close_process_input(&self, id: &str) -> Result<()> {
        self.processes.get(id)?.stdin.lock().await.take();
        Ok(())
    }
    async fn process_events(&self, id: &str, after: u64) -> Result<GetSandboxProcessEventsResult> {
        Ok(self.processes.get(id)?.events(after))
    }
    async fn next_process_events(
        &self,
        id: &str,
        after: u64,
    ) -> Result<GetSandboxProcessEventsResult> {
        Ok(self.processes.get(id)?.next_events(after).await)
    }
    async fn wait_process(&self, id: &str) -> Result<SandboxProcessStatus> {
        Ok(self.processes.get(id)?.wait().await)
    }
    async fn cancel_process(&self, id: &str) -> Result<SandboxProcessStatus> {
        let process = self.processes.get(id)?;
        process.cancel.cancel();
        Ok(process.wait().await)
    }
    async fn start_terminal(
        &self,
        command: &SandboxCommand,
        size: SandboxTerminalSize,
    ) -> Result<SandboxTerminalParts> {
        self.handle.start_terminal(command, size).await
    }
    fn supports_tcp(&self) -> bool {
        self.handle.supports_tcp()
    }
    async fn connect_tcp(&self, port: u16) -> Result<Option<BoxSandboxTcpStream>> {
        self.handle.connect_tcp(port).await
    }
    async fn connect_websocket(
        &self,
        port: u16,
        path: &str,
    ) -> Result<Option<crate::BoxSandboxWebSocket>> {
        self.handle.connect_websocket(port, path).await
    }
    async fn stop(&self) -> Result<()> {
        self.handle.stop().await?;
        self.processes.clear().await;
        Ok(())
    }
    async fn release_processes(&self) {
        self.processes.clear().await;
    }
    async fn detach(&self) -> Result<SandboxAttachment> {
        self.handle.detach().await
    }
    async fn snapshot(&self, kind: crate::SnapshotKind) -> Result<SnapshotPayload> {
        self.handle.snapshot(kind).await
    }
    async fn snapshot_template(&self, kind: crate::SnapshotKind) -> Result<SnapshotPayload> {
        self.handle.snapshot_template(kind).await
    }
    async fn delete_snapshot(&self, payload: SnapshotPayload) -> Result<()> {
        self.handle.delete_snapshot(payload).await
    }
}

struct ProcessRegistry {
    inner: Arc<RegistryInner>,
    sweeper: Option<tokio::task::AbortHandle>,
}

struct RegistryInner {
    limits: ProcessLimits,
    records: Mutex<Records>,
}

#[derive(Default)]
struct Records {
    processes: HashMap<SandboxProcessId, ProcessRecord>,
    finished: VecDeque<FinishedRecord>,
}

struct ProcessRecord {
    process: Arc<Process>,
    _slots: Vec<OwnedSemaphorePermit>,
}

struct FinishedRecord {
    id: SandboxProcessId,
    expires_at: Option<Instant>,
}

impl ProcessRegistry {
    fn new(limits: ProcessLimits) -> Self {
        let inner = Arc::new(RegistryInner {
            limits,
            records: Mutex::new(Records::default()),
        });
        let sweeper = inner.limits.retention.is_some().then(|| {
            let registry = Arc::downgrade(&inner);
            tokio::spawn(async move {
                let mut interval = tokio::time::interval(RETENTION_SWEEP_INTERVAL);
                interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                loop {
                    interval.tick().await;
                    let Some(registry) = registry.upgrade() else {
                        return;
                    };
                    registry.records().evict_expired(Instant::now());
                }
            })
            .abort_handle()
        });
        Self { inner, sweeper }
    }

    /// Take one permit from every record slot before a process starts so a
    /// slow start still counts against the limit.
    fn reserve(&self) -> Result<Vec<OwnedSemaphorePermit>> {
        self.inner.records().evict_expired(Instant::now());
        self.inner
            .limits
            .record_slots
            .iter()
            .map(|slots| {
                Arc::clone(slots)
                    .try_acquire_owned()
                    .map_err(|_| ProcessLimitReached.into())
            })
            .collect()
    }

    fn start(
        &self,
        parts: SandboxProcessParts,
        stdin_mode: SandboxProcessStdin,
        terminal: Option<Arc<dyn SandboxTerminalControl>>,
        slots: Vec<OwnedSemaphorePermit>,
    ) -> SandboxProcessId {
        let id = Uuid7::now().to_string();
        let limits = &self.inner.limits;
        let SandboxProcessParts {
            stdout,
            stderr,
            stdin,
            wait,
        } = parts;
        let process = Arc::new(Process {
            stdin: AsyncMutex::new(match stdin_mode {
                SandboxProcessStdin::Open => Some(stdin),
                SandboxProcessStdin::None => None,
            }),
            state: Mutex::new(ProcessState {
                events: VecDeque::new(),
                status: SandboxProcessStatus::Running,
                next_cursor: 1,
                retained_bytes: 0,
                retained_output_bytes: 0,
                max_output_bytes: limits.max_output_bytes,
                max_error_bytes: limits.max_error_bytes,
                budget: limits.output_budget.clone(),
            }),
            cancel: CancellationToken::new(),
            exited: CancellationToken::new(),
            notify: Notify::new(),
            terminal,
        });
        self.inner.records().processes.insert(
            id.clone(),
            ProcessRecord {
                process: Arc::clone(&process),
                _slots: slots,
            },
        );
        let registry = Arc::downgrade(&self.inner);
        let drain_grace = limits.drain_grace;
        let process_id = id.clone();
        tokio::spawn(async move {
            let status = run_process(Arc::clone(&process), stdout, stderr, wait, drain_grace).await;
            process.stdin.lock().await.take();
            {
                let mut state = process.state.lock().unwrap();
                state.push(match &status {
                    SandboxProcessStatus::Exited { exit_code } => ProcessEvent::Exit(*exit_code),
                    SandboxProcessStatus::Failed { message } => {
                        ProcessEvent::Error(message.clone())
                    }
                    SandboxProcessStatus::Cancelled => ProcessEvent::Cancelled,
                    SandboxProcessStatus::Running => unreachable!(),
                });
                state.status = status;
            }
            process.notify.notify_waiters();
            if let Some(registry) = registry.upgrade() {
                registry.records().finish(&registry.limits, process_id);
            }
        });
        id
    }

    fn get(&self, id: &str) -> Result<Arc<Process>> {
        self.inner
            .records()
            .processes
            .get(id)
            .map(|record| Arc::clone(&record.process))
            .ok_or_else(|| ProcessNotFound(id.to_string()).into())
    }

    fn processes(&self) -> Vec<Arc<Process>> {
        self.inner
            .records()
            .processes
            .values()
            .map(|record| Arc::clone(&record.process))
            .collect()
    }

    async fn cancel_all(&self) {
        let processes = self.processes();
        for process in &processes {
            process.cancel.cancel();
        }
        join_all(processes.iter().map(|process| process.wait())).await;
    }

    /// Cancel every process and forget every record; the sandbox is gone.
    async fn clear(&self) {
        self.cancel_all().await;
        let mut records = self.inner.records();
        records.processes.clear();
        records.finished.clear();
    }
}

impl Drop for ProcessRegistry {
    fn drop(&mut self) {
        if let Some(sweeper) = &self.sweeper {
            sweeper.abort();
        }
        for record in self.inner.records().processes.values() {
            record.process.cancel.cancel();
        }
    }
}

impl RegistryInner {
    fn records(&self) -> std::sync::MutexGuard<'_, Records> {
        self.records
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl Records {
    fn finish(&mut self, limits: &ProcessLimits, id: SandboxProcessId) {
        if !self.processes.contains_key(&id) {
            return;
        }
        if limits.retention.is_none() && limits.max_retained.is_none() {
            return;
        }
        self.finished.push_back(FinishedRecord {
            id,
            expires_at: limits.retention.map(|retention| Instant::now() + retention),
        });
        if let Some(max_retained) = limits.max_retained {
            while self.finished.len() > max_retained {
                let Some(evicted) = self.finished.pop_front() else {
                    break;
                };
                self.processes.remove(&evicted.id);
            }
        }
    }

    fn evict_expired(&mut self, now: Instant) {
        while self.finished.front().is_some_and(|record| {
            record
                .expires_at
                .is_some_and(|expires_at| expires_at <= now)
        }) {
            let expired = self
                .finished
                .pop_front()
                .expect("checked finished record must exist");
            self.processes.remove(&expired.id);
        }
    }
}

struct Process {
    stdin: AsyncMutex<Option<BoxAsyncWrite>>,
    state: Mutex<ProcessState>,
    cancel: CancellationToken,
    exited: CancellationToken,
    notify: Notify,
    terminal: Option<Arc<dyn SandboxTerminalControl>>,
}

struct ProcessState {
    events: VecDeque<SandboxProcessEvent>,
    status: SandboxProcessStatus,
    next_cursor: u64,
    retained_bytes: usize,
    retained_output_bytes: usize,
    max_output_bytes: Option<usize>,
    max_error_bytes: Option<usize>,
    budget: Option<Arc<ProcessOutputBudget>>,
}

enum ProcessEvent {
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
    Exit(i32),
    Error(String),
    Cancelled,
}

impl Drop for ProcessState {
    fn drop(&mut self) {
        if let Some(budget) = &self.budget {
            budget.release(self.retained_output_bytes);
        }
    }
}

impl ProcessState {
    fn latest_cursor(&self) -> Option<u64> {
        (self.next_cursor > 1).then(|| self.next_cursor - 1)
    }

    /// Append an event, dropping the oldest output first when the process
    /// or the shared budget is over its limit. Dropped output leaves a
    /// `Truncated` marker so readers know the replay has a gap.
    fn push(&mut self, event: ProcessEvent) {
        let cursor = self.next_cursor;
        self.next_cursor = self.next_cursor.saturating_add(1);
        let event = match event {
            ProcessEvent::Stdout(data) => SandboxProcessEvent::Stdout { cursor, data },
            ProcessEvent::Stderr(data) => SandboxProcessEvent::Stderr { cursor, data },
            ProcessEvent::Exit(exit_code) => SandboxProcessEvent::Exit { cursor, exit_code },
            ProcessEvent::Error(message) => SandboxProcessEvent::Error {
                cursor,
                message: bounded_message(message, self.max_error_bytes),
            },
            ProcessEvent::Cancelled => SandboxProcessEvent::Cancelled { cursor },
        };
        let bytes = event_bytes(&event);
        let output_bytes = output_bytes(&event);
        let mut dropped_bytes = 0usize;
        let mut dropped_through = 0;
        if let Some(max_output_bytes) = self.max_output_bytes {
            while self.retained_bytes.saturating_add(bytes) > max_output_bytes {
                let Some(removed) = self.pop_front() else {
                    break;
                };
                dropped_through = dropped_through.max(removed.cursor());
                dropped_bytes = dropped_bytes.saturating_add(dropped_bytes_of(&removed));
            }
        }
        if let Some(budget) = self.budget.clone() {
            while output_bytes > 0 && !budget.try_reserve(output_bytes) {
                let Some(removed) = self.pop_front() else {
                    self.record_head_truncation(dropped_through, dropped_bytes);
                    self.record_tail_truncation(cursor, bytes);
                    return;
                };
                dropped_through = dropped_through.max(removed.cursor());
                dropped_bytes = dropped_bytes.saturating_add(dropped_bytes_of(&removed));
            }
        }
        self.record_head_truncation(dropped_through, dropped_bytes);
        self.retained_bytes = self.retained_bytes.saturating_add(bytes);
        self.retained_output_bytes = self.retained_output_bytes.saturating_add(output_bytes);
        self.events.push_back(event);
    }

    fn pop_front(&mut self) -> Option<SandboxProcessEvent> {
        let removed = self.events.pop_front()?;
        if self.events.capacity() > self.events.len().saturating_mul(2).max(4) {
            self.events.shrink_to_fit();
        }
        let output = output_bytes(&removed);
        self.retained_bytes = self.retained_bytes.saturating_sub(event_bytes(&removed));
        self.retained_output_bytes = self.retained_output_bytes.saturating_sub(output);
        if let Some(budget) = &self.budget {
            budget.release(output);
        }
        Some(removed)
    }

    fn record_head_truncation(&mut self, cursor: u64, dropped_bytes: usize) {
        if dropped_bytes == 0 {
            return;
        }
        let dropped_bytes = u64::try_from(dropped_bytes).unwrap_or(u64::MAX);
        if let Some(SandboxProcessEvent::Truncated {
            cursor: existing_cursor,
            dropped_bytes: existing_dropped,
        }) = self.events.front_mut()
        {
            *existing_cursor = (*existing_cursor).max(cursor);
            *existing_dropped = existing_dropped.saturating_add(dropped_bytes);
            return;
        }
        self.events.push_front(SandboxProcessEvent::Truncated {
            cursor,
            dropped_bytes,
        });
    }

    fn record_tail_truncation(&mut self, cursor: u64, dropped_bytes: usize) {
        if dropped_bytes == 0 {
            return;
        }
        let dropped_bytes = u64::try_from(dropped_bytes).unwrap_or(u64::MAX);
        if let Some(SandboxProcessEvent::Truncated {
            cursor: existing_cursor,
            dropped_bytes: existing_dropped,
        }) = self.events.back_mut()
        {
            *existing_cursor = (*existing_cursor).max(cursor);
            *existing_dropped = existing_dropped.saturating_add(dropped_bytes);
            return;
        }
        self.events.push_back(SandboxProcessEvent::Truncated {
            cursor,
            dropped_bytes,
        });
    }

    fn snapshot(&self, after: u64) -> GetSandboxProcessEventsResult {
        GetSandboxProcessEventsResult {
            events: self
                .events
                .iter()
                .filter(|event| event.cursor() > after)
                .cloned()
                .collect(),
            cursor: self.latest_cursor(),
            status: self.status.clone(),
        }
    }
}

fn event_bytes(event: &SandboxProcessEvent) -> usize {
    match event {
        SandboxProcessEvent::Stdout { data, .. } | SandboxProcessEvent::Stderr { data, .. } => {
            data.len()
        }
        SandboxProcessEvent::Error { message, .. } => message.len(),
        SandboxProcessEvent::Exit { .. }
        | SandboxProcessEvent::Cancelled { .. }
        | SandboxProcessEvent::Truncated { .. } => 0,
    }
}

fn output_bytes(event: &SandboxProcessEvent) -> usize {
    match event {
        SandboxProcessEvent::Stdout { data, .. } | SandboxProcessEvent::Stderr { data, .. } => {
            data.len()
        }
        _ => 0,
    }
}

/// Bytes a dropped event represents, including output an earlier truncation
/// marker already accounted for.
fn dropped_bytes_of(event: &SandboxProcessEvent) -> usize {
    match event {
        SandboxProcessEvent::Truncated { dropped_bytes, .. } => {
            usize::try_from(*dropped_bytes).unwrap_or(usize::MAX)
        }
        _ => event_bytes(event),
    }
}

fn bounded_message(mut message: String, max_bytes: Option<usize>) -> String {
    let Some(max_bytes) = max_bytes else {
        return message;
    };
    if message.len() <= max_bytes {
        return message;
    }
    let suffix = &TRUNCATION_SUFFIX[..TRUNCATION_SUFFIX.len().min(max_bytes)];
    let mut end = max_bytes - suffix.len();
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    message.truncate(end);
    message.push_str(suffix);
    message.shrink_to_fit();
    message
}

impl Process {
    async fn write_input(&self, data: &[u8]) -> Result<()> {
        tokio::select! {
            biased;
            _ = self.cancel.cancelled() => bail!("sandbox process was cancelled"),
            _ = self.exited.cancelled() => bail!("sandbox process has exited"),
            result = async {
                let mut stdin = self.stdin.lock().await;
                let stdin = stdin.as_mut().ok_or_else(|| anyhow!("sandbox process stdin is closed"))?;
                stdin.write_all(data).await?;
                stdin.flush().await?;
                Ok(())
            } => result,
        }
    }

    fn events(&self, after: u64) -> GetSandboxProcessEventsResult {
        self.state.lock().unwrap().snapshot(after)
    }

    async fn next_events(&self, after: u64) -> GetSandboxProcessEventsResult {
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let result = self.events(after);
            if !result.events.is_empty() || !result.status.is_running() {
                return result;
            }
            notified.await;
        }
    }

    async fn wait(&self) -> SandboxProcessStatus {
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let status = self.state.lock().unwrap().status.clone();
            if !status.is_running() {
                return status;
            }
            notified.await;
        }
    }
}

/// Drive a process to completion: wait for exit or cancellation, give the
/// output readers a grace period to drain, and report the final status.
async fn run_process(
    process: Arc<Process>,
    stdout: BoxAsyncRead,
    stderr: BoxAsyncRead,
    wait: futures::future::BoxFuture<'static, Result<i32>>,
    drain_grace: Duration,
) -> SandboxProcessStatus {
    let (output_failure, mut output_failures) = mpsc::channel(2);
    let stdout = tokio::spawn(read_output(
        Arc::clone(&process),
        stdout,
        true,
        output_failure.clone(),
    ));
    let stderr = tokio::spawn(read_output(
        Arc::clone(&process),
        stderr,
        false,
        output_failure,
    ));
    let mut status = tokio::select! {
        biased;
        _ = process.cancel.cancelled() => SandboxProcessStatus::Cancelled,
        Some(message) = output_failures.recv() => SandboxProcessStatus::Failed { message },
        result = wait => match result {
            Ok(exit_code) => SandboxProcessStatus::Exited { exit_code },
            Err(error) => SandboxProcessStatus::Failed { message: error.to_string() },
        },
    };
    process.exited.cancel();
    let stdout_abort = stdout.abort_handle();
    let stderr_abort = stderr.abort_handle();
    // A background child may retain a pipe after its parent exits.
    // Drain both streams concurrently, but do not wait for it forever.
    let drained = if matches!(status, SandboxProcessStatus::Failed { .. }) {
        None
    } else {
        tokio::select! {
            biased;
            _ = process.cancel.cancelled() => None,
            Some(message) = output_failures.recv() => {
                status = SandboxProcessStatus::Failed { message };
                None
            },
            result = tokio::time::timeout(drain_grace, async {
                tokio::join!(stdout, stderr)
            }) => result.ok(),
        }
    };
    let status = match drained {
        Some((stdout, stderr)) => {
            let error = stdout
                .map_err(anyhow::Error::from)
                .and_then(|r| r)
                .and(stderr.map_err(anyhow::Error::from).and_then(|r| r))
                .err();
            match error {
                Some(error) if !matches!(status, SandboxProcessStatus::Cancelled) => {
                    SandboxProcessStatus::Failed {
                        message: error.to_string(),
                    }
                }
                _ => status,
            }
        }
        None => {
            stdout_abort.abort();
            stderr_abort.abort();
            status
        }
    };
    match status {
        SandboxProcessStatus::Failed { message } => SandboxProcessStatus::Failed {
            message: bounded_message(message, process.state.lock().unwrap().max_error_bytes),
        },
        status => status,
    }
}

async fn read_output(
    process: Arc<Process>,
    mut reader: BoxAsyncRead,
    stdout: bool,
    output_failure: mpsc::Sender<String>,
) -> Result<()> {
    let read_bytes = process
        .state
        .lock()
        .unwrap()
        .max_output_bytes
        .map_or(OUTPUT_READ_BYTES, |max| max.clamp(1, OUTPUT_READ_BYTES));
    let mut buffer = vec![0; read_bytes];
    loop {
        let length = match reader.read(&mut buffer).await {
            Ok(length) => length,
            Err(error) => {
                let _ = output_failure.send(error.to_string()).await;
                return Err(error.into());
            }
        };
        if length == 0 {
            return Ok(());
        }
        let mut state = process.state.lock().unwrap();
        if !state.status.is_running() {
            return Ok(());
        }
        let data = buffer[..length].to_vec();
        state.push(if stdout {
            ProcessEvent::Stdout(data)
        } else {
            ProcessEvent::Stderr(data)
        });
        drop(state);
        process.notify.notify_waiters();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::{future, io::Cursor};
    use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

    fn parts(
        stdout: &[u8],
        wait: futures::future::BoxFuture<'static, Result<i32>>,
    ) -> SandboxProcessParts {
        SandboxProcessParts {
            stdout: Box::pin(Cursor::new(stdout.to_vec())),
            stderr: Box::pin(Cursor::new(Vec::new())),
            stdin: Box::pin(futures::io::sink()),
            wait,
        }
    }

    fn registry(limits: ProcessLimits) -> ProcessRegistry {
        ProcessRegistry::new(limits)
    }

    fn start(
        registry: &ProcessRegistry,
        parts: SandboxProcessParts,
        stdin: SandboxProcessStdin,
    ) -> Result<SandboxProcessId> {
        let slots = registry.reserve()?;
        Ok(registry.start(parts, stdin, None, slots))
    }

    async fn finished(registry: &ProcessRegistry, id: &str) -> Result<SandboxProcessStatus> {
        let process = registry.get(id)?;
        Ok(tokio::time::timeout(Duration::from_secs(3), process.wait()).await?)
    }

    #[tokio::test]
    async fn cancelled_process_retains_output_and_has_one_terminal_event() -> Result<()> {
        let registry = registry(ProcessLimits::default());
        let id = start(
            &registry,
            parts(b"pending", Box::pin(future::pending())),
            SandboxProcessStdin::None,
        )?;
        let process = registry.get(&id)?;
        tokio::time::timeout(Duration::from_secs(1), async {
            while process.events(0).events.is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await?;
        process.cancel.cancel();
        assert!(matches!(
            process.wait().await,
            SandboxProcessStatus::Cancelled
        ));
        process.cancel.cancel();
        let result = process.events(0);
        assert!(matches!(result.events.as_slice(), [
            SandboxProcessEvent::Stdout { cursor: 1, data },
            SandboxProcessEvent::Cancelled { cursor: 2 },
        ] if data == b"pending"));
        assert_eq!(result.cursor, Some(2));
        assert!(process.events(2).events.is_empty());
        assert_eq!(process.events(2).cursor, Some(2));
        Ok(())
    }

    #[tokio::test]
    async fn process_exit_is_emitted_even_if_an_output_stream_stays_open() -> Result<()> {
        let registry = registry(ProcessLimits::default());
        let (reader, _writer) = tokio::io::duplex(8);
        let mut pipes = parts(b"", Box::pin(async { Ok(0) }));
        pipes.stdout = Box::pin(reader.compat());
        let id = start(&registry, pipes, SandboxProcessStdin::None)?;
        assert!(matches!(
            finished(&registry, &id).await?,
            SandboxProcessStatus::Exited { exit_code: 0 }
        ));
        assert!(matches!(
            registry.get(&id)?.events(0).events.as_slice(),
            [SandboxProcessEvent::Exit {
                cursor: 1,
                exit_code: 0
            }]
        ));
        Ok(())
    }

    #[tokio::test]
    async fn cancellation_interrupts_blocked_stdin() -> Result<()> {
        let registry = registry(ProcessLimits::default());
        let (_reader, writer) = tokio::io::duplex(1);
        let mut pipes = parts(b"", Box::pin(future::pending()));
        pipes.stdin = Box::pin(writer.compat_write());
        let id = start(&registry, pipes, SandboxProcessStdin::Open)?;
        let process = registry.get(&id)?;
        let write = process.write_input(b"cannot fit without a reader");
        tokio::pin!(write);
        assert!(
            tokio::time::timeout(Duration::from_millis(20), &mut write)
                .await
                .is_err()
        );
        process.cancel.cancel();
        assert!(
            tokio::time::timeout(Duration::from_secs(1), &mut write)
                .await?
                .is_err()
        );
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(1), process.wait()).await?,
            SandboxProcessStatus::Cancelled
        ));
        Ok(())
    }

    #[tokio::test]
    async fn next_events_waits_for_output_and_returns_on_exit() -> Result<()> {
        let registry = registry(ProcessLimits::default());
        let (reader, mut writer) = tokio::io::duplex(64);
        let (exit, exited) = tokio::sync::oneshot::channel();
        let mut pipes = parts(b"", Box::pin(async move { Ok(exited.await?) }));
        pipes.stdout = Box::pin(reader.compat());
        let id = start(&registry, pipes, SandboxProcessStdin::None)?;
        let process = registry.get(&id)?;
        let pending = process.next_events(0);
        tokio::pin!(pending);
        assert!(
            tokio::time::timeout(Duration::from_millis(20), &mut pending)
                .await
                .is_err()
        );
        tokio::io::AsyncWriteExt::write_all(&mut writer, b"hello").await?;
        let result = tokio::time::timeout(Duration::from_secs(1), &mut pending).await?;
        assert!(matches!(
            result.events.as_slice(),
            [SandboxProcessEvent::Stdout { cursor: 1, data }] if data == b"hello"
        ));
        exit.send(3).unwrap();
        drop(writer);
        let result = tokio::time::timeout(Duration::from_secs(1), process.next_events(1)).await?;
        assert!(matches!(
            result.events.as_slice(),
            [SandboxProcessEvent::Exit {
                cursor: 2,
                exit_code: 3
            }]
        ));
        assert_eq!(result.cursor, Some(2));
        Ok(())
    }

    #[tokio::test]
    async fn output_over_the_limit_is_truncated_with_monotonic_cursors() -> Result<()> {
        let registry = registry(ProcessLimits {
            max_output_bytes: Some(1024),
            ..ProcessLimits::default()
        });
        let (reader, mut writer) = tokio::io::duplex(256);
        let (exit, exited) = tokio::sync::oneshot::channel();
        let mut pipes = parts(b"", Box::pin(async move { Ok(exited.await?) }));
        pipes.stdout = Box::pin(reader.compat());
        let id = start(&registry, pipes, SandboxProcessStdin::None)?;
        let process = registry.get(&id)?;
        for _ in 0..8 {
            tokio::io::AsyncWriteExt::write_all(&mut writer, &[b'x'; 256]).await?;
            tokio::time::timeout(Duration::from_secs(1), async {
                while process
                    .events(0)
                    .events
                    .iter()
                    .map(output_bytes)
                    .sum::<usize>()
                    == 0
                {
                    tokio::task::yield_now().await;
                }
            })
            .await?;
        }
        drop(writer);
        exit.send(0).unwrap();
        assert!(matches!(
            finished(&registry, &id).await?,
            SandboxProcessStatus::Exited { exit_code: 0 }
        ));
        let result = process.events(0);
        let retained: usize = result.events.iter().map(output_bytes).sum();
        assert!(retained <= 1024, "retained {retained} bytes");
        let dropped = match result.events.first() {
            Some(SandboxProcessEvent::Truncated { dropped_bytes, .. }) => *dropped_bytes,
            other => panic!("expected a truncation marker first, got {other:?}"),
        };
        assert_eq!(dropped as usize + retained, 8 * 256);
        let cursors: Vec<_> = result
            .events
            .iter()
            .map(SandboxProcessEvent::cursor)
            .collect();
        assert!(cursors.windows(2).all(|pair| pair[0] < pair[1]));
        assert!(matches!(
            result.events.last(),
            Some(SandboxProcessEvent::Exit { exit_code: 0, .. })
        ));
        Ok(())
    }

    #[tokio::test]
    async fn shared_budget_bounds_retained_output_across_registries() -> Result<()> {
        let budget = Arc::new(ProcessOutputBudget::new(1024));
        let registries: Vec<_> = (0..2)
            .map(|_| {
                registry(ProcessLimits {
                    output_budget: Some(Arc::new(ProcessOutputBudget::child(
                        Arc::clone(&budget),
                        4096,
                    ))),
                    ..ProcessLimits::default()
                })
            })
            .collect();
        let mut ids = Vec::new();
        for registry in &registries {
            let id = start(
                registry,
                parts(&[b'y'; 1024], Box::pin(async { Ok(0) })),
                SandboxProcessStdin::None,
            )?;
            finished(registry, &id).await?;
            ids.push(id);
        }
        assert!(budget.retained_bytes() <= 1024);
        let truncated = registries
            .iter()
            .zip(&ids)
            .filter(|(registry, id)| {
                registry
                    .get(id)
                    .unwrap()
                    .events(0)
                    .events
                    .iter()
                    .any(|event| matches!(event, SandboxProcessEvent::Truncated { .. }))
            })
            .count();
        assert!(truncated >= 1);
        drop(registries);
        tokio::task::yield_now().await;
        assert_eq!(budget.retained_bytes(), 0);
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn finished_records_expire_and_are_bounded() -> Result<()> {
        let registry = registry(ProcessLimits {
            retention: Some(Duration::from_secs(600)),
            max_retained: Some(2),
            ..ProcessLimits::default()
        });
        let mut ids = Vec::new();
        for _ in 0..3 {
            let id = start(
                &registry,
                parts(b"", Box::pin(async { Ok(0) })),
                SandboxProcessStdin::None,
            )?;
            finished(&registry, &id).await?;
            ids.push(id);
        }
        assert!(registry.get(&ids[0]).is_err());
        assert!(registry.get(&ids[1]).is_ok());
        assert!(registry.get(&ids[2]).is_ok());
        tokio::time::advance(Duration::from_secs(598)).await;
        assert!(registry.get(&ids[2]).is_ok());
        tokio::time::advance(Duration::from_secs(3)).await;
        tokio::time::timeout(Duration::from_secs(5), async {
            while registry.get(&ids[2]).is_ok() {
                tokio::task::yield_now().await;
            }
        })
        .await?;
        assert!(registry.get(&ids[1]).is_err());
        Ok(())
    }

    #[tokio::test]
    async fn record_slots_bound_running_and_retained_processes() -> Result<()> {
        let slots = Arc::new(Semaphore::new(1));
        let registry = registry(ProcessLimits {
            record_slots: vec![Arc::clone(&slots)],
            max_retained: Some(4),
            ..ProcessLimits::default()
        });
        let id = start(
            &registry,
            parts(b"", Box::pin(async { Ok(0) })),
            SandboxProcessStdin::None,
        )?;
        finished(&registry, &id).await?;
        let error = registry
            .reserve()
            .expect_err("second process must be rejected");
        assert!(error.downcast_ref::<ProcessLimitReached>().is_some());
        registry.clear().await;
        assert_eq!(slots.available_permits(), 1);
        let Err(error) = registry.get(&id) else {
            panic!("cleared record must be gone");
        };
        assert!(error.downcast_ref::<ProcessNotFound>().is_some());
        Ok(())
    }

    #[tokio::test]
    async fn oversized_error_messages_are_bounded() -> Result<()> {
        let registry = registry(ProcessLimits {
            max_error_bytes: Some(64),
            ..ProcessLimits::default()
        });
        let id = start(
            &registry,
            parts(b"", Box::pin(async { bail!("{}", "e".repeat(1024)) })),
            SandboxProcessStdin::None,
        )?;
        let status = finished(&registry, &id).await?;
        let SandboxProcessStatus::Failed { message } = status else {
            panic!("expected a failed status");
        };
        assert!(message.len() <= 64);
        assert!(message.ends_with(TRUNCATION_SUFFIX));
        let events = registry.get(&id)?.events(0).events;
        assert!(matches!(
            events.as_slice(),
            [SandboxProcessEvent::Error { message, .. }] if message.len() <= 64
        ));
        Ok(())
    }

    #[tokio::test]
    async fn local_backend_exposes_process_ids_and_enforces_sandbox_scope() -> Result<()> {
        use crate::{
            LocalProcessSandboxBackend, ManagedSandboxBackend, SandboxLifecycleConfig,
            SandboxNetworkPolicy, SandboxRequest, SandboxSpec,
        };
        let backend = LocalProcessSandboxBackend::new();
        let request = SandboxRequest {
            sandbox_id: Uuid7::now().to_string(),
            scope: crate::ResourceScope::Global,
            spec: SandboxSpec {
                tcp_ports: vec![],
                image: String::new(),
                resources: Default::default(),
                mounts: vec![],
                durable_file_systems: vec![],
                policy: SandboxNetworkPolicy::Unrestricted.into(),
                default_workdir: "/tmp".into(),
            },
            lifecycle: SandboxLifecycleConfig::default(),
            provider_state: None,
        };
        let sandbox = backend.acquire(request.clone()).await?;
        let other = backend
            .acquire(SandboxRequest {
                sandbox_id: Uuid7::now().to_string(),
                ..request
            })
            .await?;
        let id = sandbox
            .start_managed_process(
                &SandboxCommand {
                    argv: vec![
                        "/bin/sh".into(),
                        "-c".into(),
                        "cat; printf stderr >&2; exit 7".into(),
                    ],
                    env: HashMap::new(),
                    display_argv: None,
                    cwd: None,
                    timeout: None,
                },
                SandboxProcessStdin::Open,
            )
            .await?;
        sandbox.write_process_input(&id, b"stdout").await?;
        sandbox.close_process_input(&id).await?;
        assert!(matches!(
            sandbox.wait_process(&id).await?,
            SandboxProcessStatus::Exited { exit_code: 7 }
        ));
        let result = sandbox.process_events(&id, 0).await?;
        let output: Vec<u8> = result
            .events
            .iter()
            .filter_map(|event| match event {
                SandboxProcessEvent::Stdout { data, .. } => Some(data.as_slice()),
                _ => None,
            })
            .flatten()
            .copied()
            .collect();
        assert_eq!(output, b"stdout");
        assert!(matches!(
            result.events.last(),
            Some(SandboxProcessEvent::Exit { exit_code: 7, .. })
        ));
        assert!(
            sandbox
                .process_events(&id, result.cursor.unwrap())
                .await?
                .events
                .is_empty()
        );
        assert!(other.process_events(&id, 0).await.is_err());
        sandbox.stop().await?;
        assert!(sandbox.process_events(&id, 0).await.is_err());
        Ok(())
    }

    #[tokio::test]
    async fn cancellation_does_not_wait_for_inherited_output_pipes() -> Result<()> {
        let registry = registry(ProcessLimits::default());
        let (reader, _writer) = tokio::io::duplex(8);
        let mut pipes = parts(b"", Box::pin(future::pending()));
        pipes.stdout = Box::pin(reader.compat());
        let id = start(&registry, pipes, SandboxProcessStdin::None)?;
        let process = registry.get(&id)?;
        process.cancel.cancel();
        let status = tokio::time::timeout(Duration::from_millis(500), process.wait()).await?;
        assert!(matches!(status, SandboxProcessStatus::Cancelled));
        Ok(())
    }

    #[tokio::test]
    async fn failed_output_terminates_a_process_that_has_not_exited() -> Result<()> {
        use futures::TryStreamExt;
        let registry = registry(ProcessLimits::default());
        let mut pipes = parts(b"", Box::pin(future::pending()));
        pipes.stdout = Box::pin(
            futures::stream::iter([Err::<Vec<u8>, _>(std::io::Error::other(
                "output disconnected",
            ))])
            .into_async_read(),
        );
        let id = start(&registry, pipes, SandboxProcessStdin::None)?;
        let process = registry.get(&id)?;
        let status = tokio::time::timeout(Duration::from_millis(500), process.wait()).await?;
        assert!(
            matches!(status, SandboxProcessStatus::Failed { message } if message == "output disconnected")
        );
        assert!(matches!(
            process.events(0).events.as_slice(),
            [SandboxProcessEvent::Error { cursor: 1, .. }]
        ));
        Ok(())
    }

    struct TerminalHandle {
        resizes: Arc<Mutex<Vec<SandboxTerminalSize>>>,
    }

    struct TerminalControl(Arc<Mutex<Vec<SandboxTerminalSize>>>);

    #[async_trait]
    impl SandboxTerminalControl for TerminalControl {
        async fn resize(&self, size: SandboxTerminalSize) -> Result<()> {
            self.0.lock().unwrap().push(size);
            Ok(())
        }
    }

    #[async_trait]
    impl ManagedSandboxHandle for TerminalHandle {
        fn id(&self) -> &str {
            "terminal"
        }
        async fn exec(&self, _command: &SandboxCommand) -> Result<SandboxCommandOutput> {
            bail!("not supported")
        }
        async fn start_process(&self, _command: &SandboxCommand) -> Result<SandboxProcessParts> {
            bail!("not supported")
        }
        async fn start_terminal(
            &self,
            _command: &SandboxCommand,
            _size: SandboxTerminalSize,
        ) -> Result<SandboxTerminalParts> {
            let (output, input) = tokio::io::duplex(64);
            Ok(SandboxTerminalParts {
                output: Box::pin(output.compat()),
                input: Box::pin(input.compat_write()),
                wait: Box::pin(future::pending()),
                control: Arc::new(TerminalControl(Arc::clone(&self.resizes))),
            })
        }
        async fn stop(&self) -> Result<()> {
            Ok(())
        }
        async fn detach(&self) -> Result<SandboxAttachment> {
            bail!("not supported")
        }
        async fn snapshot(&self, _kind: crate::SnapshotKind) -> Result<SnapshotPayload> {
            bail!("not supported")
        }
    }

    /// A handle that only offers the managed operations, like a remote worker.
    struct ManagedOnly(Arc<dyn ManagedSandboxHandle>);

    #[async_trait]
    impl ManagedSandboxHandle for ManagedOnly {
        fn id(&self) -> &str {
            self.0.id()
        }
        async fn exec(&self, command: &SandboxCommand) -> Result<SandboxCommandOutput> {
            self.0.exec(command).await
        }
        async fn start_process(&self, command: &SandboxCommand) -> Result<SandboxProcessParts> {
            let id = self
                .0
                .start_managed_process(command, SandboxProcessStdin::Open)
                .await?;
            Ok(managed_process_parts(Arc::clone(&self.0), id))
        }
        async fn start_managed_process(
            &self,
            command: &SandboxCommand,
            stdin: SandboxProcessStdin,
        ) -> Result<SandboxProcessId> {
            self.0.start_managed_process(command, stdin).await
        }
        async fn write_process_input(&self, id: &str, data: &[u8]) -> Result<()> {
            self.0.write_process_input(id, data).await
        }
        async fn close_process_input(&self, id: &str) -> Result<()> {
            self.0.close_process_input(id).await
        }
        async fn process_events(
            &self,
            id: &str,
            after: u64,
        ) -> Result<GetSandboxProcessEventsResult> {
            self.0.process_events(id, after).await
        }
        async fn next_process_events(
            &self,
            id: &str,
            after: u64,
        ) -> Result<GetSandboxProcessEventsResult> {
            self.0.next_process_events(id, after).await
        }
        async fn wait_process(&self, id: &str) -> Result<SandboxProcessStatus> {
            self.0.wait_process(id).await
        }
        async fn cancel_process(&self, id: &str) -> Result<SandboxProcessStatus> {
            self.0.cancel_process(id).await
        }
        async fn stop(&self) -> Result<()> {
            self.0.stop().await
        }
        async fn detach(&self) -> Result<SandboxAttachment> {
            self.0.detach().await
        }
        async fn snapshot(&self, kind: crate::SnapshotKind) -> Result<SnapshotPayload> {
            self.0.snapshot(kind).await
        }
    }

    async fn local_sandbox() -> Result<Arc<dyn ManagedSandboxHandle>> {
        use crate::{
            LocalProcessSandboxBackend, ManagedSandboxBackend, SandboxLifecycleConfig,
            SandboxNetworkPolicy, SandboxRequest, SandboxSpec,
        };
        LocalProcessSandboxBackend::new()
            .acquire(SandboxRequest {
                sandbox_id: Uuid7::now().to_string(),
                scope: crate::ResourceScope::Global,
                spec: SandboxSpec {
                    tcp_ports: vec![],
                    image: String::new(),
                    resources: Default::default(),
                    mounts: vec![],
                    durable_file_systems: vec![],
                    policy: SandboxNetworkPolicy::Unrestricted.into(),
                    default_workdir: "/tmp".into(),
                },
                lifecycle: SandboxLifecycleConfig::default(),
                provider_state: None,
            })
            .await
    }

    #[cfg(feature = "contract-tests")]
    #[tokio::test]
    async fn managed_process_parts_satisfy_the_raw_process_contracts() -> Result<()> {
        let handle: Arc<dyn ManagedSandboxHandle> = Arc::new(ManagedOnly(local_sandbox().await?));
        crate::contract_tests::sandbox_handle_start_process_supports_interactive_stdio_and_env(
            Arc::clone(&handle),
        )
        .await?;
        crate::contract_tests::sandbox_handle_start_process_supports_long_running_request_response_protocol(
            handle,
        )
        .await
    }

    #[tokio::test]
    async fn dropping_managed_wait_cancels_the_process() -> Result<()> {
        let sandbox = local_sandbox().await?;
        let id = sandbox
            .start_managed_process(
                &SandboxCommand {
                    argv: vec!["/bin/sh".into(), "-c".into(), "sleep 30".into()],
                    env: HashMap::new(),
                    display_argv: None,
                    cwd: None,
                    timeout: None,
                },
                SandboxProcessStdin::None,
            )
            .await?;
        let parts = managed_process_parts(Arc::clone(&sandbox), id.clone());
        drop(parts);
        let status =
            tokio::time::timeout(Duration::from_secs(5), sandbox.wait_process(&id)).await??;
        assert!(matches!(status, SandboxProcessStatus::Cancelled));
        Ok(())
    }

    #[tokio::test]
    async fn terminals_are_managed_processes_with_resize() -> Result<()> {
        let resizes = Arc::new(Mutex::new(Vec::new()));
        let handle = with_process_management(Arc::new(TerminalHandle {
            resizes: Arc::clone(&resizes),
        }));
        let command = SandboxCommand {
            argv: vec!["sh".into()],
            env: HashMap::new(),
            display_argv: None,
            cwd: None,
            timeout: None,
        };
        let size = SandboxTerminalSize { rows: 24, cols: 80 };
        let id = handle.start_managed_terminal(&command, size).await?;
        handle.write_process_input(&id, b"echo").await?;
        let result =
            tokio::time::timeout(Duration::from_secs(1), handle.next_process_events(&id, 0))
                .await??;
        assert!(matches!(
            result.events.as_slice(),
            [SandboxProcessEvent::Stdout { data, .. }] if data == b"echo"
        ));
        let resized = SandboxTerminalSize {
            rows: 40,
            cols: 120,
        };
        handle.resize_terminal(&id, resized).await?;
        assert_eq!(resizes.lock().unwrap().as_slice(), &[resized]);
        assert!(matches!(
            handle.cancel_process(&id).await?,
            SandboxProcessStatus::Cancelled
        ));
        let plain = handle
            .start_managed_process(&command, SandboxProcessStdin::None)
            .await;
        assert!(plain.is_err());
        Ok(())
    }
}
