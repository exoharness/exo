use super::*;
#[cfg(feature = "basic-backend")]
use crate::BoxSandboxTcpStream;
use crate::sandbox::{
    ManagedSandboxBackend, ManagedSandboxHandle, SANDBOX_MAIN_MOUNT_DIR, SandboxCommand,
    SandboxLifecycleConfig, SandboxMount, SandboxMountAccess, SandboxNetworkPolicy, SandboxRequest,
    SandboxSpec, SnapshotFormat, SnapshotPayload, sandbox_spec_hash,
};
use crate::vault::SecretReference;
use crate::{
    BoxAsyncRead, BoxAsyncWrite, DurableFileSystem, EventKind, SandboxProcessEvent,
    SandboxProcessId, SandboxProcessMode, SandboxProcessParts, SandboxProcessStdin,
};
use bytes::Bytes;
use chrono::{DateTime, Utc};
use futures::future::BoxFuture;
use futures::io::{AsyncReadExt, AsyncWriteExt};
use serde_json::Value;
use std::collections::BTreeMap;
use tokio::sync::Notify;
const SANDBOX_PROVIDER_STATE_EVENT: &str = "sandbox_provider_state";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(super) struct SandboxProviderStatePayload {
    pub(super) sandbox_id: SandboxId,
    pub(super) provider: SandboxProvider,
    pub(super) state_key: String,
    pub(super) state: Value,
}

type SandboxBackendFactory = Arc<
    dyn for<'a> Fn(
            &'a Arc<BasicExoHarnessInner>,
        ) -> BoxFuture<'a, Result<Arc<dyn ManagedSandboxBackend>>>
        + Send
        + Sync,
>;

/// Registers a backend implementation for a [`SandboxProvider`].
///
/// Built-in registrations resolve remote-provider secrets lazily on first use.
/// Callers with their own provider implementation can register an already-built
/// trait object with [`SandboxBackendRegistration::from_backend`].
#[derive(Clone)]
pub struct SandboxBackendRegistration {
    provider: SandboxProvider,
    is_local: bool,
    pub(super) factory: SandboxBackendFactory,
}

impl SandboxBackendRegistration {
    pub fn from_backend(
        provider: SandboxProvider,
        backend: Arc<dyn ManagedSandboxBackend>,
    ) -> Self {
        let is_local = backend.is_local();
        Self::from_factory(provider, is_local, move |_| {
            let backend = Arc::clone(&backend);
            Box::pin(async move { Ok(backend) })
        })
    }

    pub fn provider(&self) -> SandboxProvider {
        self.provider.clone()
    }

    pub fn is_local(&self) -> bool {
        self.is_local
    }

    pub(super) fn from_factory<F>(provider: SandboxProvider, is_local: bool, factory: F) -> Self
    where
        F: for<'a> Fn(
                &'a Arc<BasicExoHarnessInner>,
            ) -> BoxFuture<'a, Result<Arc<dyn ManagedSandboxBackend>>>
            + Send
            + Sync
            + 'static,
    {
        Self {
            provider,
            is_local,
            factory: Arc::new(factory),
        }
    }
}

pub(super) async fn prepare_sandbox_scopes_for_deletion(
    harness: &BasicExoHarness,
    scopes: &[BasicScopedSandboxHandle<'_>],
) -> Result<bool> {
    let mut sandbox_ids = Vec::new();
    for scope in scopes {
        let sandboxes = harness
            .inner
            .storage
            .list_json_matching_suffix::<StoredSandbox>(scope.sandboxes_dir(), ".json")
            .await?;
        if sandboxes
            .iter()
            .any(|sandbox| sandbox.running && sandbox.attachment.is_none())
        {
            return Ok(false);
        }
        sandbox_ids.extend(sandboxes.into_iter().map(|sandbox| sandbox.id));
    }

    let mut running = harness.inner.running_sandboxes.lock().await;
    for sandbox_id in sandbox_ids {
        running.remove(&sandbox_id);
    }
    Ok(true)
}

pub(super) struct BasicScopedSandboxHandle<'a> {
    pub(super) harness: &'a BasicExoHarness,
    pub(super) owner_dir: PathBuf,
    pub(super) owner: ResourceScope,
    pub(super) event_sink: BasicSandboxEventSink<'a>,
}

pub(super) enum BasicSandboxEventSink<'a> {
    None,
    Conversation {
        conversation_id: ConversationId,
    },
    Turn {
        conversation_id: ConversationId,
        session_id: SessionId,
        turn_id: TurnId,
        state: &'a Mutex<BasicTurnState>,
    },
}

impl<'a> BasicScopedSandboxHandle<'a> {
    pub(super) fn agent(harness: &'a BasicExoHarness, agent_id: AgentId) -> Self {
        Self {
            harness,
            owner_dir: harness.owner_dir(ResourceScope::Agent { agent_id }),
            owner: ResourceScope::Agent { agent_id },
            event_sink: BasicSandboxEventSink::None,
        }
    }

    pub(super) fn conversation(
        harness: &'a BasicExoHarness,
        agent_id: AgentId,
        conversation_id: ConversationId,
    ) -> Self {
        let owner = ResourceScope::Thread {
            agent_id,
            thread_id: conversation_id,
        };
        Self {
            harness,
            owner_dir: harness.owner_dir(owner),
            owner,
            event_sink: BasicSandboxEventSink::Conversation { conversation_id },
        }
    }

    pub(super) fn turn(
        harness: &'a BasicExoHarness,
        agent_id: AgentId,
        conversation_id: ConversationId,
        conversation_dir: PathBuf,
        session_id: SessionId,
        turn_id: TurnId,
        state: &'a Mutex<BasicTurnState>,
    ) -> Self {
        Self {
            harness,
            owner_dir: conversation_dir,
            owner: ResourceScope::Thread {
                agent_id,
                thread_id: conversation_id,
            },
            event_sink: BasicSandboxEventSink::Turn {
                conversation_id,
                session_id,
                turn_id,
                state,
            },
        }
    }

    pub(super) fn sandboxes_dir(&self) -> PathBuf {
        self.owner_dir.join("sandboxes")
    }

    pub(super) async fn list_sandboxes(&self) -> Result<Vec<SandboxRecord>> {
        self.harness.check(self.owner).await?;
        let mut sandboxes = self
            .harness
            .inner
            .storage
            .list_json_matching_suffix::<StoredSandbox>(self.sandboxes_dir(), ".json")
            .await?
            .into_iter()
            .filter(|sandbox| self.harness.check_sandbox(sandbox).is_ok())
            .map(SandboxRecord::from)
            .collect::<Vec<_>>();
        sandboxes.sort_unstable_by(|left, right| right.id.cmp(&left.id));
        Ok(sandboxes)
    }

    pub(super) async fn create_sandbox(&self, request: CreateSandboxRequest) -> Result<SandboxId> {
        self.harness.check(self.owner).await?;
        self.ensure_full_sandbox_scope("create_sandbox")?;
        if request.name.is_none() {
            return self.create_new_sandbox(request).await;
        }
        let prepared = prepare_sandbox_request(self.harness, self.owner, request).await?;
        let _guard = self.harness.inner.write_lock.lock().await;
        if let Some((sandbox_id, sandbox)) = self.find_matching_sandbox(&prepared).await? {
            let (_handle, provider_state_event) = active_sandbox_handle_locked(
                self.harness,
                &self.owner_dir,
                self.owner,
                &sandbox_id,
                &sandbox,
            )
            .await?;
            if let Some(event) = provider_state_event {
                self.append_events_locked(vec![event]).await?;
            }
            return Ok(sandbox_id);
        }
        self.create_new_sandbox_locked(prepared).await
    }

    pub(super) async fn fork_sandbox(&self, request: ForkSandboxRequest) -> Result<SandboxId> {
        self.harness.check(self.owner).await?;
        self.ensure_full_sandbox_scope("fork_sandbox")?;
        let source = self.load_sandbox(&request.source_id).await?;
        if source.attachment.is_some() {
            bail!("attached sandboxes cannot be forked");
        }
        if !source.running {
            bail!("source sandbox is not running: {}", request.source_id);
        }
        let prepared = prepare_sandbox_request(self.harness, self.owner, request.sandbox).await?;
        if prepared.provider != source.provider {
            bail!(
                "source provider {} does not match target provider {}",
                source.provider,
                prepared.provider
            );
        }
        let sandbox_id = format!("sandbox-{}", Uuid7::now());
        let sandbox = prepared.stored_sandbox(sandbox_id.clone());
        let backend = self
            .harness
            .inner
            .sandbox_backend_for_provider(sandbox.provider.clone())
            .await?;
        let source_request = sandbox_request(self.owner, &request.source_id, &source, None);
        let target_request = sandbox_request(self.owner, &sandbox_id, &sandbox, None);
        let sandbox_handle = backend.fork_sandbox(source_request, target_request).await?;
        let provider_state_event = sandbox_provider_state_event(
            &sandbox_id,
            sandbox.provider.clone(),
            sandbox_provider_state_key(self.owner, &sandbox_id, &sandbox),
            None,
            &sandbox_handle,
        )?;
        let _guard = self.harness.inner.write_lock.lock().await;
        self.persist_created_sandbox_locked(sandbox, sandbox_handle, provider_state_event)
            .await
    }

    pub(super) async fn restore_sandbox(
        &self,
        request: RestoreSandboxRequest,
    ) -> Result<SandboxId> {
        self.harness.check(self.owner).await?;
        self.ensure_full_sandbox_scope("restore_sandbox")?;
        let payload =
            load_snapshot_payload(self.harness, &self.owner_dir, request.snapshot_id).await?;
        let prepared = prepare_sandbox_request(self.harness, self.owner, request.sandbox).await?;
        let sandbox_id = format!("sandbox-{}", Uuid7::now());
        let mut sandbox = prepared.stored_sandbox(sandbox_id.clone());
        sandbox.latest_snapshot_id = Some(request.snapshot_id);
        let backend = self
            .harness
            .inner
            .sandbox_backend_for_provider(sandbox.provider.clone())
            .await?;
        ensure_snapshot_format_supported(backend.as_ref(), &sandbox.provider, &payload.format)?;
        let sandbox_handle = backend
            .acquire_from_snapshot(
                sandbox_request(self.owner, &sandbox_id, &sandbox, None),
                payload,
            )
            .await?;
        if let Some(effective_image) = sandbox_handle.effective_image()
            && effective_image != sandbox.image
        {
            sandbox.requested_image = Some(sandbox.image.clone());
            sandbox.image = effective_image;
        }
        let provider_state_event = sandbox_provider_state_event(
            &sandbox_id,
            sandbox.provider.clone(),
            sandbox_provider_state_key(self.owner, &sandbox_id, &sandbox),
            None,
            &sandbox_handle,
        )?;
        let _guard = self.harness.inner.write_lock.lock().await;
        self.persist_created_sandbox_locked(sandbox, sandbox_handle, provider_state_event)
            .await
    }

    pub(super) async fn terminate_sandbox(&self, id: SandboxId) -> Result<()> {
        self.harness.check(self.owner).await?;
        self.ensure_full_sandbox_scope("terminate_sandbox")?;
        let _guard = self.harness.inner.write_lock.lock().await;
        self.terminate_sandbox_locked(id).await
    }

    pub(super) async fn terminate_sandbox_locked(&self, id: SandboxId) -> Result<()> {
        self.harness.check(self.owner).await?;
        let sandbox = self.load_sandbox(&id).await?;
        if sandbox.attachment.is_some() {
            bail!("attached sandboxes cannot be terminated");
        }
        if sandbox.running {
            let backend = self
                .harness
                .inner
                .sandbox_backend_for_provider(sandbox.provider.clone())
                .await?;
            let state_key = sandbox_provider_state_key(self.owner, &id, &sandbox);
            let provider_state = load_sandbox_provider_state(
                self.harness,
                &self.owner_dir,
                self.owner,
                &id,
                sandbox.provider.clone(),
                &state_key,
            )
            .await?;
            backend
                .terminate(sandbox_request(self.owner, &id, &sandbox, provider_state))
                .await?;
            self.harness
                .inner
                .running_sandboxes
                .lock()
                .await
                .remove(&id);
        }
        self.harness
            .inner
            .storage
            .delete_key_if_exists(self.sandboxes_dir().join(format!("{id}.json")))
            .await?;
        if sandbox.running {
            self.append_events_locked(vec![EventData::SandboxStopped { sandbox_id: id }])
                .await?;
        }
        Ok(())
    }

    pub(super) async fn attach_sandbox(&self, request: AttachSandboxRequest) -> Result<SandboxId> {
        self.harness.check(self.owner).await?;
        self.ensure_full_sandbox_scope("attach_sandbox")?;
        let sandbox_id = format!("sandbox-{}", Uuid7::now());
        let provider = request.attachment.provider();
        let sandbox = StoredSandbox {
            tcp_ports: vec![],
            principal: None,
            credentials: BTreeMap::new(),
            id: sandbox_id.clone(),
            name: None,
            provider,
            image: String::new(),
            resources: Default::default(),
            requested_image: None,
            default_workdir: Some(request.default_workdir.unwrap_or_default()),
            file_system_mounts: Vec::new(),
            durable_file_systems: Vec::new(),
            network: StoredSandboxPolicy::Policy {
                policy: self
                    .harness
                    .inner
                    .sandbox_policy
                    .clone()
                    .unwrap_or_else(|| SandboxNetworkPolicy::Unrestricted.into()),
            },
            idle_seconds: 0,
            running: true,
            latest_snapshot_id: None,
            attachment: Some(request.attachment),
        };
        let (sandbox_handle, provider_state_event) = create_sandbox_handle(
            self.harness,
            &self.owner_dir,
            self.owner,
            &sandbox_id,
            &sandbox,
        )
        .await?;
        let _guard = self.harness.inner.write_lock.lock().await;
        self.persist_attached_sandbox_locked(sandbox, sandbox_handle, provider_state_event)
            .await
    }

    pub(super) async fn detach_sandbox(&self, sandbox_id: SandboxId) -> Result<SandboxAttachment> {
        self.harness.check(self.owner).await?;
        self.ensure_full_sandbox_scope("detach_sandbox")?;
        let sandbox = self.load_sandbox(&sandbox_id).await?;
        if !sandbox.running {
            if let Some(attachment) = sandbox.attachment {
                return Ok(attachment);
            }
            bail!("sandbox is not running: {sandbox_id}");
        }
        let (sandbox_handle, provider_state_event) = active_sandbox_handle(
            self.harness,
            &self.owner_dir,
            self.owner,
            &sandbox_id,
            &sandbox,
        )
        .await?;
        let _guard = self.harness.inner.write_lock.lock().await;
        let mut sandbox = self.load_sandbox(&sandbox_id).await?;
        if !sandbox.running {
            bail!("sandbox is not running: {sandbox_id}");
        }
        if sandbox.attachment.is_some() {
            bail!("sandbox is already detached: {sandbox_id}");
        }
        let attachment = sandbox_handle.detach().await?;
        self.harness
            .inner
            .running_sandboxes
            .lock()
            .await
            .remove(&sandbox_id);
        sandbox.running = false;
        sandbox.attachment = Some(attachment.clone());
        self.harness
            .inner
            .storage
            .put_json(
                self.sandboxes_dir().join(format!("{sandbox_id}.json")),
                &sandbox,
            )
            .await?;
        let mut events = Vec::new();
        if let Some(event) = provider_state_event {
            events.push(event);
        }
        events.push(EventData::SandboxDetached {
            sandbox_id,
            attachment: attachment.clone(),
        });
        self.append_events_locked(events).await?;
        Ok(attachment)
    }

    pub(super) async fn snapshot_sandbox(&self, id: SandboxId) -> Result<SnapshotId> {
        self.harness.check(self.owner).await?;
        let (snapshot_id, event) =
            snapshot_sandbox_side_effect(self.harness, &self.owner_dir, id).await?;
        self.append_events(vec![event]).await?;
        Ok(snapshot_id)
    }

    pub(super) async fn start_sandbox(&self, request: StartSandboxRequest) -> Result<()> {
        self.harness.check(self.owner).await?;
        let event =
            start_sandbox_side_effect(self.harness, &self.owner_dir, self.owner, request).await?;
        self.append_events(vec![event]).await?;
        Ok(())
    }

    pub(super) async fn stop_sandbox(&self, id: SandboxId) -> Result<()> {
        self.harness.check(self.owner).await?;
        self.ensure_full_sandbox_scope("stop_sandbox")?;
        let _guard = self.harness.inner.write_lock.lock().await;
        let mut sandbox = self.load_sandbox(&id).await?;
        if sandbox.attachment.is_some() {
            bail!("attached sandboxes must be detached, not stopped");
        }
        if !sandbox.running {
            return Ok(());
        }
        let sandbox_handle = self
            .harness
            .inner
            .running_sandboxes
            .lock()
            .await
            .remove(&id);
        let sandbox_handle = match sandbox_handle {
            Some(sandbox_handle) => sandbox_handle,
            None => {
                create_sandbox_handle(self.harness, &self.owner_dir, self.owner, &id, &sandbox)
                    .await?
                    .0
            }
        };
        sandbox_handle.stop().await?;

        sandbox.running = false;
        self.harness
            .inner
            .storage
            .put_json(self.sandboxes_dir().join(format!("{id}.json")), &sandbox)
            .await?;
        self.append_events_locked(vec![EventData::SandboxStopped { sandbox_id: id }])
            .await?;
        Ok(())
    }

    #[cfg(feature = "basic-backend")]
    pub(super) async fn connect_sandbox_tcp(
        &self,
        id: SandboxId,
        port: u16,
    ) -> Result<Option<BoxSandboxTcpStream>> {
        self.harness.check(self.owner).await?;
        self.ensure_full_sandbox_scope("connect_sandbox_tcp")?;
        let sandbox = self.load_sandbox(&id).await?;
        if !sandbox.running {
            bail!("sandbox is not running: {id}");
        }
        let sandbox_handle = self.active_sandbox_handle(&id, &sandbox).await?;
        sandbox_handle.connect_tcp(port).await
    }

    #[cfg(feature = "basic-backend")]
    pub(super) async fn sandbox_supports_tcp(&self, id: SandboxId) -> Result<bool> {
        self.harness.check(self.owner).await?;
        self.ensure_full_sandbox_scope("sandbox_supports_tcp")?;
        let sandbox = self.load_sandbox(&id).await?;
        if !sandbox.running {
            bail!("sandbox is not running: {id}");
        }
        let sandbox_handle = self.active_sandbox_handle(&id, &sandbox).await?;
        Ok(sandbox_handle.supports_tcp())
    }

    pub(super) async fn start_sandbox_process(
        &self,
        request: StartSandboxProcessRequest,
    ) -> Result<SandboxProcessRecord> {
        self.harness.check(self.owner).await?;
        self.ensure_full_sandbox_scope("start_sandbox_process")?;
        let pending = prepare_sandbox_process(
            self.harness,
            &self.owner_dir,
            self.owner,
            self.process_event_log(),
            request,
        )
        .await?;
        let mut events = Vec::new();
        if let Some(event) = pending.provider_state_event.clone() {
            events.push(event);
        }
        events.push(pending.started_event.clone());
        self.append_events(events).await?;
        spawn_pending_sandbox_process(self.harness, pending).await
    }

    pub(super) async fn write_sandbox_process_input(
        &self,
        request: WriteSandboxProcessInputRequest,
    ) -> Result<()> {
        self.harness.check(self.owner).await?;
        self.ensure_full_sandbox_scope("write_sandbox_process_input")?;
        let process = self
            .require_sandbox_process(&request.sandbox_id, &request.process_id)
            .await?;
        if !sandbox_process_status(&process).await.is_running() {
            bail!("sandbox process is not running: {}", request.process_id);
        }
        let mut stdin = process.stdin.lock().await;
        let stdin = stdin
            .as_mut()
            .ok_or_else(|| anyhow!("sandbox process stdin is closed: {}", request.process_id))?;
        stdin.write_all(&request.data).await?;
        stdin.flush().await?;
        Ok(())
    }

    pub(super) async fn close_sandbox_process_input(
        &self,
        request: CloseSandboxProcessInputRequest,
    ) -> Result<()> {
        self.harness.check(self.owner).await?;
        self.ensure_full_sandbox_scope("close_sandbox_process_input")?;
        let process = self
            .require_sandbox_process(&request.sandbox_id, &request.process_id)
            .await?;
        process.stdin.lock().await.take();
        Ok(())
    }

    pub(super) async fn get_sandbox_process_events(
        &self,
        query: SandboxProcessEventQuery,
    ) -> Result<GetSandboxProcessEventsResult> {
        self.harness.check(self.owner).await?;
        self.ensure_full_sandbox_scope("get_sandbox_process_events")?;
        let process = self
            .require_sandbox_process(&query.sandbox_id, &query.process_id)
            .await?;
        let after = query.after.unwrap_or_default();
        let limit = query.limit.unwrap_or(u32::MAX) as usize;
        loop {
            let notified = process.notify.notified();
            futures::pin_mut!(notified);
            notified.as_mut().enable();
            let events = process
                .events
                .lock()
                .await
                .iter()
                .filter(|event| event.cursor() > after)
                .take(limit)
                .cloned()
                .collect::<Vec<_>>();
            let cursor = events
                .last()
                .map(SandboxProcessEvent::cursor)
                .or(query.after);
            let status = sandbox_process_status(&process).await;
            if !query.follow.unwrap_or(false)
                || limit == 0
                || !events.is_empty()
                || !status.is_running()
            {
                return Ok(GetSandboxProcessEventsResult {
                    events,
                    cursor,
                    status,
                });
            }
            notified.await;
        }
    }

    pub(super) async fn wait_sandbox_process(
        &self,
        request: WaitSandboxProcessRequest,
    ) -> Result<SandboxProcessStatus> {
        self.harness.check(self.owner).await?;
        self.ensure_full_sandbox_scope("wait_sandbox_process")?;
        let process = self
            .require_sandbox_process(&request.sandbox_id, &request.process_id)
            .await?;
        Ok(wait_for_sandbox_process_terminal_status(&process).await)
    }

    pub(super) async fn cancel_sandbox_process(
        &self,
        request: CancelSandboxProcessRequest,
    ) -> Result<SandboxProcessStatus> {
        self.harness.check(self.owner).await?;
        self.ensure_full_sandbox_scope("cancel_sandbox_process")?;
        let process = self
            .require_sandbox_process(&request.sandbox_id, &request.process_id)
            .await?;
        process.stdin.lock().await.take();
        process
            .tasks
            .lock()
            .expect("sandbox process tasks poisoned")
            .take();
        push_sandbox_process_event(&process, SandboxProcessEventPayload::Cancelled).await;
        set_sandbox_process_status(&process, SandboxProcessStatus::Cancelled).await;
        Ok(SandboxProcessStatus::Cancelled)
    }

    pub(super) async fn run_in_sandbox(
        &self,
        request: RunInSandboxRequest,
    ) -> Result<Box<dyn SandboxProcess>> {
        self.harness.check(self.owner).await?;
        self.ensure_full_sandbox_scope("run_in_sandbox")?;
        let sandbox = self.load_sandbox(&request.id).await?;
        if !sandbox.running {
            bail!("sandbox is not running: {}", request.id);
        }
        if request.command.is_empty() {
            bail!("sandbox command must not be empty");
        }
        let (sandbox_handle, provider_state_event) = active_sandbox_handle(
            self.harness,
            &self.owner_dir,
            self.owner,
            &request.id,
            &sandbox,
        )
        .await?;
        if let Some(event) = provider_state_event {
            self.append_events(vec![event]).await?;
        }
        let parts = sandbox_handle
            .start_process(&SandboxCommand {
                argv: request.command.clone(),
                env: self.harness.command_env(
                    self.owner,
                    &sandbox.file_system_mounts,
                    request.env,
                )?,
                display_argv: Some(request.command),
                cwd: None,
                timeout: None,
            })
            .await
            .with_context(|| format!("failed to run command in sandbox {}", request.id))?;
        Ok(Box::new(LiveSandboxProcess::new(parts)))
    }

    pub(super) fn ensure_full_sandbox_scope(&self, operation: &str) -> Result<()> {
        if matches!(self.event_sink, BasicSandboxEventSink::Turn { .. }) {
            bail!("{operation} is not supported on a turn scope");
        }
        Ok(())
    }

    pub(super) async fn create_new_sandbox(
        &self,
        request: CreateSandboxRequest,
    ) -> Result<SandboxId> {
        self.harness.check(self.owner).await?;
        let prepared = prepare_sandbox_request(self.harness, self.owner, request).await?;
        let sandbox_id = format!("sandbox-{}", Uuid7::now());
        let sandbox = prepared.stored_sandbox(sandbox_id.clone());
        let (sandbox_handle, provider_state_event) = create_sandbox_handle(
            self.harness,
            &self.owner_dir,
            self.owner,
            &sandbox_id,
            &sandbox,
        )
        .await?;
        let _guard = self.harness.inner.write_lock.lock().await;
        self.persist_created_sandbox_locked(sandbox, sandbox_handle, provider_state_event)
            .await
    }

    pub(super) async fn create_new_sandbox_locked(
        &self,
        request: PreparedSandboxRequest,
    ) -> Result<SandboxId> {
        self.harness.check(self.owner).await?;
        let sandbox_id = format!("sandbox-{}", Uuid7::now());
        let sandbox = request.stored_sandbox(sandbox_id.clone());
        let (sandbox_handle, provider_state_event) = create_sandbox_handle(
            self.harness,
            &self.owner_dir,
            self.owner,
            &sandbox_id,
            &sandbox,
        )
        .await?;
        self.persist_created_sandbox_locked(sandbox, sandbox_handle, provider_state_event)
            .await
    }

    // The sandbox is acquired before the write lock is taken, while agent and
    // conversation deletion remove the owner's whole storage prefix under
    // that lock. Persisting without rechecking would resurrect the deleted
    // prefix and leave a live VM whose record no listing or reaper would ever
    // see again.
    pub(super) async fn owner_exists_locked(&self) -> Result<bool> {
        self.harness.check(self.owner).await?;
        Ok(self
            .harness
            .inner
            .storage
            .get_bytes_if_exists(self.owner_dir.join("record.json"))
            .await?
            .is_some())
    }

    pub(super) async fn persist_created_sandbox_locked(
        &self,
        mut sandbox: StoredSandbox,
        sandbox_handle: Arc<dyn ManagedSandboxHandle>,
        provider_state_event: Option<EventData>,
    ) -> Result<SandboxId> {
        sandbox.principal = self.harness.caller.as_ref().map(|c| c.principal.clone());
        self.harness.check(self.owner).await?;
        let sandbox_id = sandbox.id.clone();
        let latest_snapshot_id = sandbox.latest_snapshot_id;
        if !self.owner_exists_locked().await? {
            // This VM was created for the deleted owner, so it must not
            // outlive the failed persist.
            if let Err(error) = sandbox_handle.stop().await {
                tracing::warn!(%error, sandbox_id, "failed stopping sandbox whose owner was deleted");
            }
            bail!("sandbox owner was deleted while the sandbox was starting");
        }
        self.harness
            .inner
            .storage
            .put_json(
                self.sandboxes_dir().join(format!("{sandbox_id}.json")),
                &sandbox,
            )
            .await?;
        self.harness
            .inner
            .running_sandboxes
            .lock()
            .await
            .insert(sandbox_id.clone(), sandbox_handle);
        let policy = sandbox.policy();
        let enable_networking = policy.networking_enabled();
        let mut events = vec![
            EventData::SandboxCreated {
                sandbox_id: sandbox_id.clone(),
                name: sandbox.name,
                provider: sandbox.provider,
                image: sandbox.image,
                default_workdir: sandbox.default_workdir.unwrap_or_default(),
                file_system_mounts: sandbox.file_system_mounts,
                durable_file_systems: sandbox.durable_file_systems,
                policy: Some(policy),
                enable_networking,
                idle_seconds: sandbox.idle_seconds,
            },
            EventData::SandboxStarted {
                sandbox_id: sandbox_id.clone(),
                snapshot_id: latest_snapshot_id,
            },
        ];
        if let Some(event) = provider_state_event {
            events.push(event);
        }
        self.append_events_locked(events).await?;
        Ok(sandbox_id)
    }

    pub(super) async fn persist_attached_sandbox_locked(
        &self,
        mut sandbox: StoredSandbox,
        sandbox_handle: Arc<dyn ManagedSandboxHandle>,
        provider_state_event: Option<EventData>,
    ) -> Result<SandboxId> {
        sandbox.principal = self.harness.caller.as_ref().map(|c| c.principal.clone());
        self.harness.check(self.owner).await?;
        let sandbox_id = sandbox.id.clone();
        if !self.owner_exists_locked().await? {
            // An attached sandbox belongs to its external owner and keeps
            // running; only this attachment record is abandoned.
            bail!("sandbox owner was deleted while the sandbox was attaching");
        }
        let attachment = sandbox
            .attachment
            .clone()
            .expect("attached sandbox has an attachment descriptor");
        let default_workdir = sandbox.default_workdir.clone().unwrap_or_default();
        self.harness
            .inner
            .storage
            .put_json(
                self.sandboxes_dir().join(format!("{sandbox_id}.json")),
                &sandbox,
            )
            .await?;
        self.harness
            .inner
            .running_sandboxes
            .lock()
            .await
            .insert(sandbox_id.clone(), sandbox_handle);
        let mut events = vec![EventData::SandboxAttached {
            sandbox_id: sandbox_id.clone(),
            attachment,
            default_workdir,
        }];
        if let Some(event) = provider_state_event {
            events.push(event);
        }
        self.append_events_locked(events).await?;
        Ok(sandbox_id)
    }

    pub(super) async fn find_matching_sandbox(
        &self,
        request: &PreparedSandboxRequest,
    ) -> Result<Option<(SandboxId, StoredSandbox)>> {
        self.harness.check(self.owner).await?;
        match self.event_sink {
            BasicSandboxEventSink::None => {
                find_matching_stored_sandbox(
                    &self.harness.inner.storage,
                    &self.sandboxes_dir(),
                    request,
                    self.harness.caller.as_ref().map(|c| c.principal.as_str()),
                )
                .await
            }
            BasicSandboxEventSink::Conversation { .. } => {
                self.find_matching_conversation_sandbox(request).await
            }
            BasicSandboxEventSink::Turn { .. } => {
                bail!("create_sandbox is not supported on a turn scope")
            }
        }
    }

    pub(super) async fn find_matching_conversation_sandbox(
        &self,
        request: &PreparedSandboxRequest,
    ) -> Result<Option<(SandboxId, StoredSandbox)>> {
        self.harness.check(self.owner).await?;
        let Some(name) = &request.name else {
            return Ok(None);
        };
        let mut events = load_events(&self.harness.inner.storage, &self.owner_dir)
            .await?
            .into_iter()
            .filter(|event| event.data.kind() == EventKind::SANDBOX_CREATED)
            .collect::<Vec<_>>();
        events.sort_by_key(|event| event.id);

        for event in events.into_iter().rev() {
            let EventData::SandboxCreated {
                sandbox_id,
                name: event_name,
                provider,
                image,
                default_workdir,
                file_system_mounts,
                durable_file_systems,
                idle_seconds,
                ..
            } = event.data
            else {
                continue;
            };
            if event_name.as_ref() != Some(name) {
                continue;
            }
            let Some(sandbox) = self
                .harness
                .inner
                .storage
                .get_json_if_exists::<StoredSandbox>(
                    self.sandboxes_dir().join(format!("{sandbox_id}.json")),
                )
                .await?
            else {
                continue;
            };
            if self.harness.check_sandbox(&sandbox).is_err() {
                continue;
            }
            if !sandbox.running {
                continue;
            }
            if provider != request.provider
                || image != request.image
                || default_workdir != request.default_workdir.clone().unwrap_or_default()
                || file_system_mounts != request.file_system_mounts
                || durable_file_systems != request.durable_file_systems
                || sandbox.policy() != request.policy
                || sandbox.credentials != request.credentials
                || idle_seconds != request.idle_seconds
            {
                bail!("sandbox name {name:?} already exists with a different configuration");
            }
            return Ok(Some((sandbox_id, sandbox)));
        }
        Ok(None)
    }

    pub(super) async fn load_sandbox(&self, id: &str) -> Result<StoredSandbox> {
        self.harness.check(self.owner).await?;
        let sandbox = load_stored_sandbox(self.harness, &self.owner_dir, id).await?;
        self.harness.check_sandbox(&sandbox)?;
        Ok(sandbox)
    }

    #[cfg(feature = "basic-backend")]
    pub(super) async fn active_sandbox_handle(
        &self,
        id: &SandboxId,
        sandbox: &StoredSandbox,
    ) -> Result<Arc<dyn ManagedSandboxHandle>> {
        self.harness.check(self.owner).await?;
        let (handle, provider_state_event) =
            active_sandbox_handle(self.harness, &self.owner_dir, self.owner, id, sandbox).await?;
        if let Some(event) = provider_state_event {
            self.append_events(vec![event]).await?;
        }
        Ok(handle)
    }

    pub(super) async fn require_sandbox_process(
        &self,
        sandbox_id: &str,
        process_id: &str,
    ) -> Result<Arc<RunningSandboxProcess>> {
        self.harness.check(self.owner).await?;
        require_running_sandbox_process(self.harness, sandbox_id, process_id).await
    }

    pub(super) fn process_event_log(&self) -> Option<SandboxProcessEventLog> {
        match self.event_sink {
            BasicSandboxEventSink::None | BasicSandboxEventSink::Turn { .. } => None,
            BasicSandboxEventSink::Conversation { conversation_id } => {
                Some(SandboxProcessEventLog {
                    inner: Arc::clone(&self.harness.inner),
                    conversation_id,
                    conversation_dir: self.owner_dir.clone(),
                })
            }
        }
    }

    pub(super) async fn append_events(&self, data: Vec<EventData>) -> Result<()> {
        self.harness.check(self.owner).await?;
        if matches!(self.event_sink, BasicSandboxEventSink::None) {
            return Ok(());
        }
        let _guard = self.harness.inner.write_lock.lock().await;
        self.append_events_locked(data).await
    }

    pub(super) async fn append_events_locked(&self, data: Vec<EventData>) -> Result<()> {
        self.harness.check(self.owner).await?;
        match self.event_sink {
            BasicSandboxEventSink::None => Ok(()),
            BasicSandboxEventSink::Conversation { conversation_id } => {
                let mut record =
                    load_conversation_record(&self.harness.inner.storage, &self.owner_dir).await?;
                append_events_to_conversation(
                    &self.harness.inner,
                    &self.owner_dir,
                    conversation_id,
                    None,
                    None,
                    record.latest_event_id,
                    data,
                    &mut record,
                )
                .await?;
                Ok(())
            }
            BasicSandboxEventSink::Turn {
                conversation_id,
                session_id,
                turn_id,
                state,
            } => {
                let expected_head = state.lock().expect("turn state poisoned").latest_event_id;
                let mut record =
                    load_conversation_record(&self.harness.inner.storage, &self.owner_dir).await?;
                let add_result = append_events_to_conversation(
                    &self.harness.inner,
                    &self.owner_dir,
                    conversation_id,
                    Some(session_id),
                    Some(turn_id),
                    expected_head,
                    data,
                    &mut record,
                )
                .await?;
                state.lock().expect("turn state poisoned").latest_event_id =
                    Some(add_result.latest_event_id);
                Ok(())
            }
        }
    }
}

pub(super) async fn snapshot_sandbox_side_effect(
    harness: &BasicExoHarness,
    owner_dir: &Path,
    id: SandboxId,
) -> Result<(SnapshotId, EventData)> {
    let sandbox = load_stored_sandbox(harness, owner_dir, &id).await?;
    if sandbox.attachment.is_some() {
        bail!("attached sandboxes cannot be snapshotted");
    }
    // Capture the payload before taking the write lock. Backends may need to
    // talk to docker or pause the container, which can be slow.
    let handle = harness
        .inner
        .running_sandboxes
        .lock()
        .await
        .get(&id)
        .cloned()
        .ok_or_else(|| anyhow!("sandbox {id} is not running; start it before snapshotting"))?;
    let payload = handle.snapshot().await?;

    let _guard = harness.inner.write_lock.lock().await;
    let mut sandbox = load_stored_sandbox(harness, owner_dir, &id).await?;
    let snapshot_id = Uuid7::now();

    let manifest = StoredSnapshotManifest {
        snapshot_id,
        sandbox_id: id.clone(),
        format: payload.format,
        created_at: Utc::now(),
        payload_size_bytes: payload.bytes.len() as u64,
    };
    let snapshot_dir = owner_dir.join("snapshots").join(snapshot_id.to_string());
    let storage = &harness.inner.storage;
    futures::try_join!(
        storage.put_bytes(snapshot_dir.join("payload.bin"), payload.bytes.to_vec()),
        storage.put_json(snapshot_dir.join("manifest.json"), &manifest),
    )?;

    sandbox.latest_snapshot_id = Some(snapshot_id);
    harness
        .inner
        .storage
        .put_json(
            owner_dir.join("sandboxes").join(format!("{id}.json")),
            &sandbox,
        )
        .await?;
    Ok((
        snapshot_id,
        EventData::SandboxSnapshotted {
            sandbox_id: id,
            snapshot_id,
        },
    ))
}

pub(super) async fn start_sandbox_side_effect(
    harness: &BasicExoHarness,
    owner_dir: &Path,
    owner: ResourceScope,
    request: StartSandboxRequest,
) -> Result<EventData> {
    let payload = load_snapshot_payload(harness, owner_dir, request.snapshot_id).await?;

    // Keep the state transition and backend replacement behind the same
    // barrier as stop, terminate, and owner deletion. Otherwise one of those
    // operations can win after the new handle is acquired but before it is
    // registered, leaving a dead handle recorded as running.
    let _guard = harness.inner.write_lock.lock().await;
    let mut sandbox = load_stored_sandbox(harness, owner_dir, &request.id).await?;
    if sandbox.attachment.is_some() {
        bail!("attached sandboxes cannot be started from snapshots");
    }
    sandbox.running = true;
    sandbox.latest_snapshot_id = Some(request.snapshot_id);
    if let Some(idle_seconds) = request.idle_seconds {
        sandbox.idle_seconds = idle_seconds;
    }
    // Optional provider override: restore under a different backend (e.g.
    // teleport a Docker snapshot up to Daytona). Set before routing so the
    // restore targets the new backend and the new provider is persisted;
    // unsupported providers / snapshot formats error before dispatch.
    let previous_provider = sandbox.provider.clone();
    if let Some(provider) = request.provider {
        sandbox.provider = provider;
    }
    let cross_provider = sandbox.provider != previous_provider;
    let backend = harness
        .inner
        .sandbox_backend_for_provider(sandbox.provider.clone())
        .await?;
    ensure_snapshot_format_supported(backend.as_ref(), &sandbox.provider, &payload.format)?;

    // Two orders, chosen by whether the restore changes providers:
    //   - Cross-provider (teleport): make-before-break. Boot the new sandbox
    //     first and stop the old one only once it's up, so a failed restore
    //     leaves the source sandbox running and serving. Safe because the two
    //     handles live on different backends and can't collide.
    //   - Same provider: stop-then-boot. The backend replaces the warm
    //     container for this key itself during restore, and stopping the old
    //     handle after the new one exists would tear down the new container's
    //     warm-cache entry (both handles share the same sandbox ID).
    let sandbox_handle = if cross_provider {
        let sandbox_handle = backend
            .acquire_from_snapshot(sandbox_request(owner, &request.id, &sandbox, None), payload)
            .await?;
        let previous_handle = harness
            .inner
            .running_sandboxes
            .lock()
            .await
            .remove(&request.id);
        if let Some(previous_handle) = previous_handle
            && let Err(error) = previous_handle.stop().await
        {
            // The new sandbox is authoritative at this point; a failure to
            // reap the source shouldn't fail the restore.
            tracing::warn!(
                sandbox_id = %request.id,
                previous_provider = %previous_provider,
                error = format!("{error:#}"),
                "failed to stop the source sandbox after a cross-provider restore"
            );
        }
        sandbox_handle
    } else {
        let previous_handle = harness
            .inner
            .running_sandboxes
            .lock()
            .await
            .remove(&request.id);
        if let Some(previous_handle) = previous_handle {
            previous_handle.stop().await?;
        }
        backend
            .acquire_from_snapshot(sandbox_request(owner, &request.id, &sandbox, None), payload)
            .await?
    };

    // A restore can boot the sandbox from a different image than the one the
    // sandbox was created with (e.g. the docker backend loads the snapshot as
    // a new tag). Persist the effective image so every process — including
    // scheduler and adapter runners in other processes — derives the same
    // warm-sandbox spec and reattaches to this container instead of creating
    // a second one. Keep the originally requested image for named matching.
    if let Some(effective_image) = sandbox_handle.effective_image()
        && effective_image != sandbox.image
    {
        sandbox
            .requested_image
            .get_or_insert_with(|| sandbox.image.clone());
        sandbox.image = effective_image;
    }

    harness
        .inner
        .storage
        .put_json(
            owner_dir
                .join("sandboxes")
                .join(format!("{}.json", request.id)),
            &sandbox,
        )
        .await?;
    harness
        .inner
        .running_sandboxes
        .lock()
        .await
        .insert(request.id.clone(), sandbox_handle);
    Ok(EventData::SandboxStarted {
        sandbox_id: request.id,
        snapshot_id: Some(request.snapshot_id),
    })
}

pub(super) async fn load_snapshot_payload(
    harness: &BasicExoHarness,
    owner_dir: &Path,
    snapshot_id: SnapshotId,
) -> Result<SnapshotPayload> {
    // Load immutable snapshot metadata without holding the harness write lock;
    // payloads may live in a remote object store.
    let snapshot_dir = owner_dir.join("snapshots").join(snapshot_id.to_string());
    let storage = &harness.inner.storage;
    let (manifest_result, payload_result) = futures::join!(
        storage.get_json::<StoredSnapshotManifest>(snapshot_dir.join("manifest.json")),
        storage.get_bytes(snapshot_dir.join("payload.bin")),
    );
    let manifest = manifest_result.with_context(|| {
        format!(
            "loading snapshot manifest for {} (have you taken a snapshot?)",
            snapshot_id
        )
    })?;
    let payload_bytes =
        payload_result.with_context(|| format!("loading snapshot payload for {snapshot_id}"))?;
    Ok(SnapshotPayload {
        format: manifest.format,
        bytes: Bytes::from(payload_bytes),
    })
}

pub(super) fn ensure_snapshot_format_supported(
    backend: &dyn ManagedSandboxBackend,
    provider: &SandboxProvider,
    format: &SnapshotFormat,
) -> Result<()> {
    let supported = backend.consumable_snapshot_formats();
    if supported.contains(format) {
        return Ok(());
    }
    let supported = supported
        .iter()
        .map(SnapshotFormat::as_str)
        .collect::<Vec<_>>()
        .join(", ");
    let supported = if supported.is_empty() {
        "none".to_string()
    } else {
        supported
    };
    bail!(
        "sandbox provider {provider} cannot restore snapshot format {format}; supported formats: {supported}"
    )
}

pub(super) async fn load_stored_sandbox(
    harness: &BasicExoHarness,
    owner_dir: &Path,
    id: &str,
) -> Result<StoredSandbox> {
    harness
        .inner
        .storage
        .get_json_if_exists(owner_dir.join("sandboxes").join(format!("{id}.json")))
        .await?
        .ok_or_else(|| anyhow!("sandbox not found: {id}"))
}

pub(super) async fn prepare_sandbox_request(
    harness: &BasicExoHarness,
    scope: ResourceScope,
    request: CreateSandboxRequest,
) -> Result<PreparedSandboxRequest> {
    let image = if !request.image.trim().is_empty() {
        request.image.clone()
    } else if let Some(default) = harness
        .inner
        .binding_default_image(request.provider.clone())
        .await?
    {
        default
    } else {
        request.image.clone()
    };

    let policy = request
        .policy
        .or_else(|| harness.inner.sandbox_policy.clone())
        .unwrap_or_else(|| {
            if request.enable_networking.unwrap_or(true) {
                SandboxNetworkPolicy::Unrestricted.into()
            } else {
                SandboxNetworkPolicy::Disabled.into()
            }
        });
    let context = ScopedVaultContext { harness, scope };
    let credentials = futures::future::try_join_all(policy.credentials.iter().map(|binding| {
        let context = &context;
        async move {
            let reference = crate::vault::find_secret(context, &binding.name)
                .await?
                .with_context(|| format!("egress credential not found: {}", binding.name))?;
            Ok::<_, anyhow::Error>((binding.name.clone(), reference))
        }
    }))
    .await?
    .into_iter()
    .collect();
    Ok(PreparedSandboxRequest {
        credentials,
        name: request.name,
        provider: request.provider,
        image,
        resources: request.resources,
        default_workdir: request.default_workdir,
        file_system_mounts: request.file_system_mounts.unwrap_or_default(),
        durable_file_systems: request.durable_file_systems.unwrap_or_default(),
        policy,
        idle_seconds: request.idle_seconds.unwrap_or(60),
        tcp_ports: request.tcp_ports,
    })
}

pub(super) async fn find_matching_stored_sandbox(
    storage: &BasicObjectStore,
    sandboxes_dir: &Path,
    request: &PreparedSandboxRequest,
    principal: Option<&str>,
) -> Result<Option<(SandboxId, StoredSandbox)>> {
    let Some(name) = &request.name else {
        return Ok(None);
    };
    let mut sandboxes = storage
        .list_json_matching_suffix::<StoredSandbox>(sandboxes_dir, ".json")
        .await?;
    sandboxes.sort_by_key(|sandbox| sandbox.id.clone());
    for sandbox in sandboxes.into_iter().rev() {
        if principal.is_some_and(|p| sandbox.principal.as_deref() != Some(p)) {
            continue;
        }
        if sandbox.name.as_ref() != Some(name) {
            continue;
        }
        if !sandbox.running {
            continue;
        }
        // After a snapshot restore, `image` holds the restored tag; the
        // caller still asks for the image it originally configured, so match
        // against that.
        let comparable_image = sandbox.requested_image.as_ref().unwrap_or(&sandbox.image);
        if sandbox.provider != request.provider
            || comparable_image != &request.image
            || sandbox.resources != request.resources
            || sandbox.default_workdir != request.default_workdir
            || sandbox.file_system_mounts != request.file_system_mounts
            || sandbox.durable_file_systems != request.durable_file_systems
            || sandbox.policy() != request.policy
            || sandbox.credentials != request.credentials
            || sandbox.idle_seconds != request.idle_seconds
            || sandbox.tcp_ports != request.tcp_ports
        {
            bail!("sandbox name {name:?} already exists with a different configuration");
        }
        return Ok(Some((sandbox.id.clone(), sandbox)));
    }
    Ok(None)
}

pub(super) async fn active_sandbox_handle(
    harness: &BasicExoHarness,
    owner_dir: &Path,
    owner: ResourceScope,
    sandbox_id: &SandboxId,
    sandbox: &StoredSandbox,
) -> Result<(Arc<dyn ManagedSandboxHandle>, Option<EventData>)> {
    if let Some(handle) = harness
        .inner
        .running_sandboxes
        .lock()
        .await
        .get(sandbox_id)
        .cloned()
    {
        return Ok((handle, None));
    }

    let (handle, provider_state_event) =
        create_sandbox_handle(harness, owner_dir, owner, sandbox_id, sandbox).await?;
    let _guard = harness.inner.write_lock.lock().await;
    let current = match load_stored_sandbox(harness, owner_dir, sandbox_id).await {
        Ok(current) => current,
        Err(error) => {
            if let Err(stop_error) = handle.stop().await {
                return Err(error).context(format!(
                    "also failed to stop the unregistered sandbox handle: {stop_error:#}"
                ));
            }
            return Err(error);
        }
    };
    if !current.running {
        handle.stop().await?;
        bail!("sandbox is not running: {sandbox_id}");
    }
    let mut running = harness.inner.running_sandboxes.lock().await;
    if let Some(existing) = running.get(sandbox_id) {
        return Ok((Arc::clone(existing), None));
    }
    running.insert(sandbox_id.clone(), Arc::clone(&handle));
    Ok((handle, provider_state_event))
}

// The caller holds write_lock, so no owner or sandbox record can disappear
// between acquisition and registration.
pub(super) async fn active_sandbox_handle_locked(
    harness: &BasicExoHarness,
    owner_dir: &Path,
    owner: ResourceScope,
    sandbox_id: &SandboxId,
    sandbox: &StoredSandbox,
) -> Result<(Arc<dyn ManagedSandboxHandle>, Option<EventData>)> {
    if let Some(handle) = harness
        .inner
        .running_sandboxes
        .lock()
        .await
        .get(sandbox_id)
        .cloned()
    {
        return Ok((handle, None));
    }

    let (handle, provider_state_event) =
        create_sandbox_handle(harness, owner_dir, owner, sandbox_id, sandbox).await?;
    harness
        .inner
        .running_sandboxes
        .lock()
        .await
        .insert(sandbox_id.clone(), Arc::clone(&handle));
    Ok((handle, provider_state_event))
}

pub(super) async fn create_sandbox_handle(
    harness: &BasicExoHarness,
    owner_dir: &Path,
    owner: ResourceScope,
    sandbox_id: &SandboxId,
    sandbox: &StoredSandbox,
) -> Result<(Arc<dyn ManagedSandboxHandle>, Option<EventData>)> {
    let state_key = sandbox_provider_state_key(owner, sandbox_id, sandbox);
    let previous_state = load_sandbox_provider_state(
        harness,
        owner_dir,
        owner,
        sandbox_id,
        sandbox.provider.clone(),
        &state_key,
    )
    .await?;
    let backend = harness
        .inner
        .sandbox_backend_for_provider(sandbox.provider.clone())
        .await?;
    let request = sandbox_request(owner, sandbox_id, sandbox, previous_state.clone());
    let handle = match &sandbox.attachment {
        Some(attachment) => backend.attach(request, attachment.clone()).await?,
        None => backend.acquire(request).await?,
    };
    let provider_state_event = sandbox_provider_state_event(
        sandbox_id,
        sandbox.provider.clone(),
        state_key,
        previous_state,
        &handle,
    )?;
    Ok((handle, provider_state_event))
}

pub(super) fn sandbox_provider_state_key(
    owner: ResourceScope,
    sandbox_id: &SandboxId,
    sandbox: &StoredSandbox,
) -> String {
    let request = sandbox_request(owner, sandbox_id, sandbox, None);
    let owner_key = match owner {
        ResourceScope::Global => "global".to_owned(),
        ResourceScope::Agent { agent_id } => format!("agent:{agent_id}"),
        ResourceScope::Thread { thread_id, .. } => format!("thread:{thread_id}"),
    };
    format!(
        "{owner_key}:{sandbox_id}\n{}",
        sandbox_spec_hash(&request.spec)
    )
}

pub(super) async fn load_sandbox_provider_state(
    harness: &BasicExoHarness,
    owner_dir: &Path,
    owner: ResourceScope,
    sandbox_id: &SandboxId,
    provider: SandboxProvider,
    state_key: &str,
) -> Result<Option<Value>> {
    let ResourceScope::Thread { .. } = owner else {
        return Ok(None);
    };
    let mut events = load_events(&harness.inner.storage, owner_dir)
        .await?
        .into_iter()
        .filter(|event| event.data.kind() == EventKind::custom(SANDBOX_PROVIDER_STATE_EVENT))
        .collect::<Vec<_>>();
    events.sort_by_key(|event| event.id);
    for event in events.into_iter().rev() {
        let EventData::Custom {
            event_type,
            payload,
        } = event.data
        else {
            continue;
        };
        if event_type != SANDBOX_PROVIDER_STATE_EVENT {
            continue;
        }
        let Ok(payload) = serde_json::from_value::<SandboxProviderStatePayload>(payload) else {
            continue;
        };
        if payload.sandbox_id == *sandbox_id
            && payload.provider == provider
            && payload.state_key == state_key
        {
            return Ok(Some(payload.state));
        }
    }
    Ok(None)
}

pub(super) fn sandbox_provider_state_event(
    sandbox_id: &SandboxId,
    provider: SandboxProvider,
    state_key: String,
    previous_state: Option<Value>,
    handle: &Arc<dyn ManagedSandboxHandle>,
) -> Result<Option<EventData>> {
    let Some(state) = handle.provider_state() else {
        return Ok(None);
    };
    if previous_state.as_ref() == Some(&state) {
        return Ok(None);
    }
    Ok(Some(EventData::Custom {
        event_type: SANDBOX_PROVIDER_STATE_EVENT.to_string(),
        payload: serde_json::to_value(SandboxProviderStatePayload {
            sandbox_id: sandbox_id.clone(),
            provider,
            state_key,
            state,
        })?,
    }))
}

pub(super) async fn require_running_sandbox_process(
    harness: &BasicExoHarness,
    sandbox_id: &str,
    process_id: &str,
) -> Result<Arc<RunningSandboxProcess>> {
    let process = harness
        .inner
        .running_processes
        .lock()
        .await
        .get(process_id)
        .cloned()
        .ok_or_else(|| anyhow!("sandbox process not found: {process_id}"))?;
    if process.sandbox_id != sandbox_id {
        bail!("sandbox process {process_id} does not belong to sandbox {sandbox_id}");
    }
    Ok(process)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct StoredSandbox {
    #[serde(default)]
    pub(super) principal: Option<String>,
    pub(super) id: SandboxId,
    #[serde(default)]
    pub(super) name: Option<String>,
    pub(super) provider: SandboxProvider,
    pub(super) image: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) resources: Option<crate::SandboxResourceShape>,
    /// The image originally requested at creation, kept when a snapshot
    /// restore rewrites `image` to the restored tag. Named-sandbox matching
    /// compares against this so config-derived requests still resolve to the
    /// restored sandbox instead of erroring or spawning a duplicate.
    #[serde(default)]
    pub(super) requested_image: Option<String>,
    pub(super) default_workdir: Option<String>,
    pub(super) file_system_mounts: Vec<FileSystemMount>,
    #[serde(default)]
    pub(super) durable_file_systems: Vec<DurableFileSystem>,
    #[serde(flatten)]
    pub(super) network: StoredSandboxPolicy,
    #[serde(default)]
    pub(super) credentials: BTreeMap<String, SecretReference>,
    pub(super) idle_seconds: u64,
    #[serde(default)]
    pub(super) tcp_ports: Vec<u16>,
    pub(super) running: bool,
    pub(super) latest_snapshot_id: Option<SnapshotId>,
    #[serde(default)]
    pub(super) attachment: Option<SandboxAttachment>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged, deny_unknown_fields)]
pub(super) enum StoredSandboxPolicy {
    Policy { policy: crate::EgressPolicy },
    Legacy { enable_networking: bool },
}

impl StoredSandbox {
    pub(super) fn policy(&self) -> crate::EgressPolicy {
        match &self.network {
            StoredSandboxPolicy::Policy { policy } => policy.clone(),
            StoredSandboxPolicy::Legacy {
                enable_networking: true,
            } => SandboxNetworkPolicy::Unrestricted.into(),
            StoredSandboxPolicy::Legacy {
                enable_networking: false,
            } => SandboxNetworkPolicy::Disabled.into(),
        }
    }
}

impl From<StoredSandbox> for SandboxRecord {
    fn from(sandbox: StoredSandbox) -> Self {
        Self {
            id: sandbox.id,
            name: sandbox.name,
            provider: sandbox.provider,
            image: sandbox.image,
            running: sandbox.running,
        }
    }
}

#[derive(Debug, Clone)]
pub(super) struct PreparedSandboxRequest {
    pub(super) name: Option<String>,
    pub(super) provider: SandboxProvider,
    pub(super) image: String,
    pub(super) resources: Option<crate::SandboxResourceShape>,
    pub(super) default_workdir: Option<String>,
    pub(super) file_system_mounts: Vec<FileSystemMount>,
    pub(super) durable_file_systems: Vec<DurableFileSystem>,
    pub(super) policy: crate::EgressPolicy,
    pub(super) credentials: BTreeMap<String, SecretReference>,
    pub(super) idle_seconds: u64,
    pub(super) tcp_ports: Vec<u16>,
}

impl PreparedSandboxRequest {
    pub(super) fn stored_sandbox(&self, id: SandboxId) -> StoredSandbox {
        StoredSandbox {
            principal: None,
            id,
            name: self.name.clone(),
            provider: self.provider.clone(),
            image: self.image.clone(),
            resources: self.resources,
            requested_image: None,
            default_workdir: self.default_workdir.clone(),
            file_system_mounts: self.file_system_mounts.clone(),
            durable_file_systems: self.durable_file_systems.clone(),
            network: StoredSandboxPolicy::Policy {
                policy: self.policy.clone(),
            },
            credentials: self.credentials.clone(),
            idle_seconds: self.idle_seconds,
            tcp_ports: self.tcp_ports.clone(),
            running: true,
            latest_snapshot_id: None,
            attachment: None,
        }
    }
}

pub(super) struct PendingSandboxProcess {
    pub(super) record: SandboxProcessRecord,
    pub(super) process: Arc<RunningSandboxProcess>,
    pub(super) stdout: BoxAsyncRead,
    pub(super) stderr: BoxAsyncRead,
    pub(super) wait: BoxFuture<'static, Result<i32>>,
    pub(super) started_event: EventData,
    pub(super) provider_state_event: Option<EventData>,
}

pub(super) async fn prepare_sandbox_process(
    harness: &BasicExoHarness,
    owner_dir: &Path,
    owner: ResourceScope,
    event_log: Option<SandboxProcessEventLog>,
    request: StartSandboxProcessRequest,
) -> Result<PendingSandboxProcess> {
    let sandbox = load_stored_sandbox(harness, owner_dir, &request.sandbox_id).await?;
    if !sandbox.running {
        bail!("sandbox is not running: {}", request.sandbox_id);
    }
    if request.command.is_empty() {
        bail!("sandbox command must not be empty");
    }
    if request.mode != SandboxProcessMode::Exec {
        bail!("basic sandbox backend only supports exec-mode processes");
    }
    let (sandbox_handle, provider_state_event) =
        active_sandbox_handle(harness, owner_dir, owner, &request.sandbox_id, &sandbox).await?;
    let process_id = format!("process-{}", Uuid7::now());
    let sandbox_id = request.sandbox_id.clone();
    let command = request.command.clone();
    let cwd = request.cwd.clone();
    let mode = request.mode;
    let stdin_mode = request.stdin;
    let output = request.output;
    let lifecycle = request.lifecycle;
    let name = request.name.clone();
    let parts = sandbox_handle
        .start_process(&SandboxCommand {
            argv: command.clone(),
            env: harness.command_env(owner, &sandbox.file_system_mounts, request.env)?,
            display_argv: Some(command.clone()),
            cwd: cwd.clone(),
            timeout: None,
        })
        .await
        .with_context(|| format!("failed to start process in sandbox {}", request.sandbox_id))?;
    let SandboxProcessParts {
        stdout,
        stderr,
        stdin,
        wait,
    } = parts;
    let stdin = match stdin_mode {
        SandboxProcessStdin::Open => Some(stdin),
        SandboxProcessStdin::None => None,
    };
    let process = Arc::new(RunningSandboxProcess {
        event_log,
        sandbox_id: sandbox_id.clone(),
        process_id: process_id.clone(),
        stdin: AsyncMutex::new(stdin),
        events: AsyncMutex::new(Vec::new()),
        status: AsyncMutex::new(SandboxProcessStatus::Running),
        open_output_streams: AsyncMutex::new(2),
        output_drained: Notify::new(),
        tasks: Mutex::new(None),
        notify: Notify::new(),
    });
    Ok(PendingSandboxProcess {
        record: SandboxProcessRecord {
            id: process_id.clone(),
            sandbox_id: sandbox_id.clone(),
            name: name.clone(),
            status: SandboxProcessStatus::Running,
        },
        process,
        stdout,
        stderr,
        wait,
        started_event: EventData::SandboxProcessStarted {
            sandbox_id,
            process_id,
            name,
            command,
            cwd,
            mode,
            stdin: stdin_mode,
            output,
            lifecycle,
            status: SandboxProcessStatus::Running,
            provider_state: None,
        },
        provider_state_event,
    })
}

pub(super) async fn spawn_pending_sandbox_process(
    harness: &BasicExoHarness,
    pending: PendingSandboxProcess,
) -> Result<SandboxProcessRecord> {
    let PendingSandboxProcess {
        record,
        process,
        stdout,
        stderr,
        wait,
        ..
    } = pending;
    let process_id = record.id.clone();
    let mut tasks = crate::runtime_host::TaskGroup::new(harness.inner.host.clone());
    tasks.spawn(record_sandbox_process_output(
        Arc::clone(&process),
        SandboxProcessOutputStream::Stdout,
        stdout,
    ));
    tasks.spawn(record_sandbox_process_output(
        Arc::clone(&process),
        SandboxProcessOutputStream::Stderr,
        stderr,
    ));
    tasks.spawn(record_sandbox_process_exit(Arc::clone(&process), wait));
    *process
        .tasks
        .lock()
        .expect("sandbox process tasks poisoned") = Some(tasks);
    harness
        .inner
        .running_processes
        .lock()
        .await
        .insert(process_id, process);
    Ok(record)
}

pub(super) struct RunningSandboxProcess {
    pub(super) event_log: Option<SandboxProcessEventLog>,
    pub(super) sandbox_id: SandboxId,
    pub(super) process_id: SandboxProcessId,
    pub(super) stdin: AsyncMutex<Option<BoxAsyncWrite>>,
    pub(super) events: AsyncMutex<Vec<SandboxProcessEvent>>,
    pub(super) status: AsyncMutex<SandboxProcessStatus>,
    pub(super) open_output_streams: AsyncMutex<u8>,
    pub(super) output_drained: Notify,
    pub(super) tasks: Mutex<Option<crate::runtime_host::TaskGroup>>,
    pub(super) notify: Notify,
}

pub(super) struct SandboxProcessEventLog {
    pub(super) inner: Arc<BasicExoHarnessInner>,
    pub(super) conversation_id: ConversationId,
    pub(super) conversation_dir: PathBuf,
}

pub(super) enum SandboxProcessOutputStream {
    Stdout,
    Stderr,
}

pub(super) async fn record_sandbox_process_output(
    process: Arc<RunningSandboxProcess>,
    stream: SandboxProcessOutputStream,
    mut reader: BoxAsyncRead,
) {
    let mut buffer = vec![0; 8192];
    loop {
        match reader.read(&mut buffer).await {
            Ok(0) => {
                mark_sandbox_process_output_closed(&process).await;
                return;
            }
            Ok(length) => {
                let data = buffer[..length].to_vec();
                match stream {
                    SandboxProcessOutputStream::Stdout => {
                        push_sandbox_process_event(
                            &process,
                            SandboxProcessEventPayload::Stdout(data),
                        )
                        .await;
                    }
                    SandboxProcessOutputStream::Stderr => {
                        push_sandbox_process_event(
                            &process,
                            SandboxProcessEventPayload::Stderr(data),
                        )
                        .await;
                    }
                }
            }
            Err(error) => {
                let message = error.to_string();
                push_sandbox_process_event(
                    &process,
                    SandboxProcessEventPayload::Error(message.clone()),
                )
                .await;
                set_sandbox_process_status(&process, SandboxProcessStatus::Failed { message })
                    .await;
                mark_sandbox_process_output_closed(&process).await;
                return;
            }
        }
    }
}

pub(super) async fn record_sandbox_process_exit(
    process: Arc<RunningSandboxProcess>,
    wait: BoxFuture<'static, Result<i32>>,
) {
    let terminal = wait.await;
    wait_for_sandbox_process_output_drained(&process).await;
    if !sandbox_process_status(&process).await.is_running() {
        return;
    }
    match terminal {
        Ok(exit_code) => {
            push_sandbox_process_event(&process, SandboxProcessEventPayload::Exit(exit_code)).await;
            set_sandbox_process_status(&process, SandboxProcessStatus::Exited { exit_code }).await;
        }
        Err(error) => {
            let message = error.to_string();
            push_sandbox_process_event(
                &process,
                SandboxProcessEventPayload::Error(message.clone()),
            )
            .await;
            set_sandbox_process_status(
                &process,
                SandboxProcessStatus::Failed {
                    message: message.clone(),
                },
            )
            .await;
        }
    }
}

pub(super) async fn mark_sandbox_process_output_closed(process: &Arc<RunningSandboxProcess>) {
    let mut open_output_streams = process.open_output_streams.lock().await;
    if *open_output_streams > 0 {
        *open_output_streams -= 1;
    }
    let drained = *open_output_streams == 0;
    drop(open_output_streams);
    if drained {
        process.output_drained.notify_waiters();
    }
}

pub(super) async fn wait_for_sandbox_process_output_drained(process: &Arc<RunningSandboxProcess>) {
    loop {
        let notified = process.output_drained.notified();
        futures::pin_mut!(notified);
        notified.as_mut().enable();
        if *process.open_output_streams.lock().await == 0 {
            return;
        }
        notified.await;
    }
}

pub(super) enum SandboxProcessEventPayload {
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
    Exit(i32),
    Error(String),
    Cancelled,
}

pub(super) async fn push_sandbox_process_event(
    process: &Arc<RunningSandboxProcess>,
    payload: SandboxProcessEventPayload,
) {
    let event = push_sandbox_process_event_memory_only(process, payload).await;
    if let Err(error) = append_sandbox_process_data(
        process,
        vec![EventData::SandboxProcessEvent {
            sandbox_id: process.sandbox_id.clone(),
            process_id: process.process_id.clone(),
            event,
        }],
    )
    .await
    {
        push_sandbox_process_event_memory_only(
            process,
            SandboxProcessEventPayload::Error(format!(
                "failed to persist sandbox process event: {error}"
            )),
        )
        .await;
    }
}

pub(super) async fn push_sandbox_process_event_memory_only(
    process: &Arc<RunningSandboxProcess>,
    payload: SandboxProcessEventPayload,
) -> SandboxProcessEvent {
    let mut events = process.events.lock().await;
    let cursor = events.len() as u64 + 1;
    let event = match payload {
        SandboxProcessEventPayload::Stdout(data) => SandboxProcessEvent::Stdout { cursor, data },
        SandboxProcessEventPayload::Stderr(data) => SandboxProcessEvent::Stderr { cursor, data },
        SandboxProcessEventPayload::Exit(exit_code) => {
            SandboxProcessEvent::Exit { cursor, exit_code }
        }
        SandboxProcessEventPayload::Error(message) => {
            SandboxProcessEvent::Error { cursor, message }
        }
        SandboxProcessEventPayload::Cancelled => SandboxProcessEvent::Cancelled { cursor },
    };
    events.push(event.clone());
    drop(events);
    process.notify.notify_waiters();
    event
}

pub(super) async fn set_sandbox_process_status(
    process: &Arc<RunningSandboxProcess>,
    status: SandboxProcessStatus,
) {
    let append_result = append_sandbox_process_data(
        process,
        vec![EventData::SandboxProcessStateUpdated {
            sandbox_id: process.sandbox_id.clone(),
            process_id: process.process_id.clone(),
            status: status.clone(),
            provider_state: None,
        }],
    )
    .await;
    let mut current = process.status.lock().await;
    *current = status;
    drop(current);
    process.notify.notify_waiters();
    if let Err(error) = append_result {
        push_sandbox_process_event_memory_only(
            process,
            SandboxProcessEventPayload::Error(format!(
                "failed to persist sandbox process status: {error}"
            )),
        )
        .await;
    }
}

pub(super) async fn append_sandbox_process_data(
    process: &Arc<RunningSandboxProcess>,
    data: Vec<EventData>,
) -> Result<()> {
    let Some(event_log) = &process.event_log else {
        return Ok(());
    };
    let _guard = event_log.inner.write_lock.lock().await;
    let mut record =
        load_conversation_record(&event_log.inner.storage, &event_log.conversation_dir).await?;
    append_events_to_conversation(
        &event_log.inner,
        &event_log.conversation_dir,
        event_log.conversation_id,
        None,
        None,
        record.latest_event_id,
        data,
        &mut record,
    )
    .await?;
    Ok(())
}

pub(super) async fn sandbox_process_status(
    process: &Arc<RunningSandboxProcess>,
) -> SandboxProcessStatus {
    process.status.lock().await.clone()
}

pub(super) async fn wait_for_sandbox_process_terminal_status(
    process: &Arc<RunningSandboxProcess>,
) -> SandboxProcessStatus {
    loop {
        let notified = process.notify.notified();
        futures::pin_mut!(notified);
        notified.as_mut().enable();
        let status = sandbox_process_status(process).await;
        if !status.is_running() {
            return status;
        }
        notified.await;
    }
}

/// Sidecar JSON describing a snapshot payload.
///
/// Lives at `{conversation_dir}/snapshots/{snapshot_id}/manifest.json` alongside
/// the payload blob at `payload.bin`. The `format` controls how the payload is
/// interpreted on restore — only a backend that declares that format can
/// reconstruct a sandbox from it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct StoredSnapshotManifest {
    pub(super) snapshot_id: SnapshotId,
    pub(super) sandbox_id: SandboxId,
    #[serde(alias = "kind")]
    pub(super) format: SnapshotFormat,
    pub(super) created_at: DateTime<Utc>,
    pub(super) payload_size_bytes: u64,
}

pub(super) struct LiveSandboxProcess {
    pub(super) parts: Option<SandboxProcessParts>,
}

impl LiveSandboxProcess {
    pub(super) fn new(parts: SandboxProcessParts) -> Self {
        Self { parts: Some(parts) }
    }
}

impl SandboxProcess for LiveSandboxProcess {
    fn into_parts(mut self: Box<Self>) -> SandboxProcessParts {
        self.parts
            .take()
            .expect("live sandbox process parts already consumed")
    }
}

pub(super) fn sandbox_request(
    owner: ResourceScope,
    sandbox_id: &str,
    sandbox: &StoredSandbox,
    provider_state: Option<Value>,
) -> SandboxRequest {
    SandboxRequest {
        sandbox_id: sandbox_id.to_string(),
        scope: owner,
        spec: SandboxSpec {
            image: sandbox.image.clone(),
            resources: sandbox.resources,
            mounts: sandbox
                .file_system_mounts
                .iter()
                .map(|mount| SandboxMount {
                    host_path: PathBuf::from(&mount.host_path),
                    guest_path: mount.mount_path.clone(),
                    access: match mount.mode {
                        crate::FileSystemMountMode::ReadOnly => SandboxMountAccess::ReadOnly,
                        crate::FileSystemMountMode::ReadWrite => SandboxMountAccess::ReadWrite,
                    },
                    internal: mount.internal.unwrap_or(false),
                })
                .collect(),
            durable_file_systems: sandbox.durable_file_systems.clone(),
            policy: sandbox.policy(),
            default_workdir: sandbox
                .default_workdir
                .clone()
                .unwrap_or_else(|| SANDBOX_MAIN_MOUNT_DIR.to_string()),
            tcp_ports: sandbox.tcp_ports.clone(),
        },
        lifecycle: SandboxLifecycleConfig {
            idle_ttl: Some(std::time::Duration::from_secs(sandbox.idle_seconds)),
        },
        provider_state,
    }
}

impl BasicExoHarness {
    pub(super) fn check_sandbox(&self, sandbox: &StoredSandbox) -> Result<()> {
        if let Some(caller) = &self.caller {
            anyhow::ensure!(
                sandbox.principal.as_deref() == Some(&caller.principal),
                "sandbox belongs to another caller"
            );
        }
        Ok(())
    }
}

pub(super) async fn terminate_running_sandboxes(
    scope: &BasicScopedSandboxHandle<'_>,
) -> Result<()> {
    for sandbox in scope
        .harness
        .inner
        .storage
        .list_json_matching_suffix::<StoredSandbox>(scope.sandboxes_dir(), ".json")
        .await?
    {
        if sandbox.running && sandbox.attachment.is_none() {
            scope.terminate_sandbox(sandbox.id).await?;
        }
    }
    Ok(())
}

impl BasicExoHarnessInner {
    pub(super) async fn sandbox_backend_for_provider(
        self: &Arc<Self>,
        provider: SandboxProvider,
    ) -> Result<Arc<dyn ManagedSandboxBackend>> {
        if let Some(backend) = self.sandbox_backends.lock().await.get(&provider) {
            return Ok(Arc::clone(backend));
        }
        let registration = self.sandbox_registry.get(&provider).ok_or_else(|| {
            anyhow!("sandbox provider {provider:?} is not supported by this harness")
        })?;
        // Build without the cache lock so a slow build (secret I/O) doesn't
        // serialize other providers; a concurrent build loses to `or_insert`.
        let backend = (registration.factory)(self).await?;
        Ok(Arc::clone(
            self.sandbox_backends
                .lock()
                .await
                .entry(provider)
                .or_insert(backend),
        ))
    }
}

impl BasicExoHarnessInner {
    /// The configured default base image for `provider`, from the newest
    /// `Binding::Sandbox` for it. `None` when no such binding exists, so the
    /// backend applies its own intrinsic default.
    pub(super) async fn binding_default_image(
        &self,
        provider: SandboxProvider,
    ) -> Result<Option<String>> {
        let bindings = list_binding_records(&self.storage, Path::new("bindings")).await?;
        Ok(bindings.into_iter().rev().find_map(|record| {
            let Binding::Sandbox { config, .. } = record.binding else {
                return None;
            };
            (config.provider() == provider)
                .then(|| config.default_image().map(str::to_string))
                .flatten()
        }))
    }
}
