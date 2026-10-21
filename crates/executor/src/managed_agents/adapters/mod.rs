mod access;
mod definitions;
mod progress;
mod slack;
mod store;
#[cfg(test)]
mod tests;

use crate::{
    ExecutionStreamEvent, Runtime, SendRequest, TurnOptions,
    remote::{AuthServer, IdentityRegistry},
};
use anyhow::{Context, Result, ensure};
use exo_managed_agents::adapters::{AdapterDefinition, SlackTransport};
use exoharness::{AgentId, NewThreadRequest, TurnRecord, Uuid7};
use futures::{FutureExt, StreamExt};
use serde::{Deserialize, Serialize};
use slack_morphism::prelude::SlackStreamChunk;
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};
use store::{
    AdapterStore, Authority, DeliveryKind, Inbox, InboxError, IncomingMessage, Outbox, Route,
    digest,
};
use tokio_util::sync::CancellationToken;

const INVALID_PROGRESS_RETRY_LIMIT: u32 = 5;

#[derive(Clone)]
pub(crate) struct Deployment {
    pub id: String,
    pub agent: AgentId,
    pub definition: AdapterDefinition,
    pub fingerprint: String,
    pub connection: Arc<slack::SlackConnection>,
}

struct CachedCaller {
    runtime: Arc<Runtime>,
    last_used: std::time::Instant,
}

struct DesiredAdapter {
    agent: AgentId,
    definition: AdapterDefinition,
    reload: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Decision {
    inbox: String,
    turn: TurnRecord,
    approval_id: String,
    approved: bool,
    applied: bool,
    failure: Option<String>,
}

impl store::PendingRecord for Decision {
    fn pending(&self) -> bool {
        !self.applied && self.failure.is_none()
    }
}

/// One owner supervises a provider's adapter deployments. HTTP and Socket Mode
/// persist input through this service before acknowledging external receipt.
pub struct AdapterService {
    pub(crate) root: Arc<Runtime>,
    store: AdapterStore,
    pub(crate) identities: IdentityRegistry,
    auth: Option<Arc<AuthServer>>,
    only_agent: Option<AgentId>,
    deployments: Mutex<BTreeMap<String, Deployment>>,
    streams: Mutex<BTreeMap<String, crate::ExecutionStreamHandle>>,
    callers: Mutex<BTreeMap<String, CachedCaller>>,
    definitions: Arc<definitions::DefinitionCache>,
    admission: tokio::sync::Mutex<()>,
    routes: Mutex<BTreeMap<String, std::sync::Weak<tokio::sync::Mutex<()>>>>,
    notify: tokio::sync::Notify,
    receipt_notify: tokio::sync::Notify,
    stop: CancellationToken,
}

impl AdapterService {
    pub async fn new(
        root: Arc<Runtime>,
        auth: Option<Arc<AuthServer>>,
        only_agent: Option<AgentId>,
    ) -> Result<Arc<Self>> {
        let harness = root.exoharness_handle();
        let storage = harness.provider_storage()?;
        let identities = match &auth {
            Some(auth) => IdentityRegistry::authenticated(auth.clone(), harness.clone()),
            None => IdentityRegistry::open(harness.clone()).await?,
        };
        Ok(Arc::new(Self {
            root,
            store: AdapterStore::new(storage),
            identities,
            auth,
            only_agent,
            deployments: Mutex::default(),
            streams: Mutex::default(),
            callers: Mutex::default(),
            definitions: Arc::default(),
            admission: Default::default(),
            routes: Default::default(),
            notify: Default::default(),
            receipt_notify: Default::default(),
            stop: CancellationToken::new(),
        }))
    }
    pub async fn webhook_paths(&self) -> Result<Vec<String>> {
        let mut paths = Vec::new();
        for (
            _,
            DesiredAdapter {
                agent,
                definition: adapter,
                ..
            },
        ) in self.desired().await?
        {
            if matches!(&adapter, AdapterDefinition::Slack(config) if config.transport == SlackTransport::Http)
            {
                paths.push(if self.only_agent.is_some() {
                    format!("/exo/adapter/{}", adapter.name())
                } else {
                    format!("/exo/agent/{agent}/adapter/{}", adapter.name())
                });
            }
        }
        Ok(paths)
    }
    pub fn cancellation(&self) -> CancellationToken {
        self.stop.clone()
    }
    pub fn identities(&self) -> &IdentityRegistry {
        &self.identities
    }

    pub async fn recover_caller(&self, principal: &str) -> Result<Arc<Runtime>> {
        let authority = self
            .store
            .get::<Authority>(&AdapterStore::authority(principal))
            .await?
            .context("adapter caller authority is missing")?;
        self.caller_runtime(authority).await
    }
    async fn caller_runtime(&self, authority: Authority) -> Result<Arc<Runtime>> {
        let principal = access::principal(&authority)?;
        if let Some(caller) = self
            .callers
            .lock()
            .expect("adapter runtimes poisoned")
            .get_mut(&principal)
        {
            caller.last_used = std::time::Instant::now();
            return Ok(caller.runtime.clone());
        }
        let caller = access::caller(
            &self.root,
            &self.store,
            self.identities.clone(),
            self.definitions.clone(),
            authority,
        )
        .await?;
        let mut callers = self.callers.lock().expect("adapter runtimes poisoned");
        if let Some(caller) = callers.get_mut(&caller.principal) {
            caller.last_used = std::time::Instant::now();
            return Ok(caller.runtime.clone());
        }
        let principal = caller.principal.clone();
        let runtime = Arc::new(self.root.with_caller(caller)?);
        callers.insert(
            principal,
            CachedCaller {
                runtime: runtime.clone(),
                last_used: std::time::Instant::now(),
            },
        );
        Ok(runtime)
    }
    async fn shutdown_callers(&self) -> Result<()> {
        let callers = std::mem::take(&mut *self.callers.lock().expect("adapter runtimes poisoned"));
        futures::future::join_all(callers.values().map(|caller| caller.runtime.shutdown()))
            .await
            .into_iter()
            .collect()
    }
    async fn prune_callers(&self) {
        let expired = {
            let mut callers = self.callers.lock().expect("adapter runtimes poisoned");
            let keys = callers
                .iter()
                .filter(|(_, caller)| {
                    caller.last_used.elapsed() > std::time::Duration::from_secs(300)
                        && Arc::strong_count(&caller.runtime) == 1
                })
                .map(|(key, _)| key.clone())
                .collect::<Vec<_>>();
            keys.into_iter()
                .filter_map(|key| callers.remove(&key))
                .collect::<Vec<_>>()
        };
        futures::future::join_all(expired.into_iter().map(|caller| async move {
            if let Err(error) = caller.runtime.shutdown().await {
                tracing::warn!(%error, "shutting down idle adapter runtime");
            }
        }))
        .await;
    }
    fn touch_caller(&self, deployment: &Deployment, inbox: &Inbox) -> Result<()> {
        if let Some(thread) = inbox.thread {
            let principal = access::principal(&Authority {
                deployment: deployment.id.clone(),
                agent: deployment.agent,
                thread,
                adapter: deployment.definition.name().into(),
                source: (&inbox.message).into(),
                grant: inbox.grant.clone(),
            })?;
            if let Some(caller) = self
                .callers
                .lock()
                .expect("adapter runtimes poisoned")
                .get_mut(&principal)
            {
                caller.last_used = std::time::Instant::now();
            }
        }
        Ok(())
    }
    fn route_lock(&self, key: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut routes = self.routes.lock().expect("adapter routes poisoned");
        routes.retain(|_, lock| lock.strong_count() > 0);
        if let Some(lock) = routes.get(key).and_then(std::sync::Weak::upgrade) {
            return lock;
        }
        let lock = Arc::new(tokio::sync::Mutex::new(()));
        routes.insert(key.into(), Arc::downgrade(&lock));
        lock
    }
    fn check_scope(&self, agent: AgentId) -> Result<()> {
        ensure!(
            self.only_agent.is_none_or(|id| id == agent),
            "adapter is outside this server's scope"
        );
        Ok(())
    }
    pub(crate) fn deployment(&self, id: &str) -> Option<Deployment> {
        self.deployments
            .lock()
            .expect("adapter deployments poisoned")
            .get(id)
            .cloned()
    }
    pub async fn run(self: Arc<Self>) -> Result<()> {
        tokio::select! {
            biased;
            () = self.stop.cancelled() => {},
            () = self.run_work() => {},
            () = self.run_receipts() => {},
        }
        let deployments = std::mem::take(
            &mut *self
                .deployments
                .lock()
                .expect("adapter deployments poisoned"),
        );
        self.streams
            .lock()
            .expect("adapter streams poisoned")
            .clear();
        for (_, deployment) in deployments {
            deployment.connection.shutdown().await;
        }
        self.shutdown_callers().await
    }
    async fn run_receipts(&self) {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(2));
        loop {
            tokio::select! {
                () = self.receipt_notify.notified() => {},
                _ = interval.tick() => {},
            }
            let deployments = self
                .deployments
                .lock()
                .expect("adapter deployments poisoned")
                .values()
                .cloned()
                .collect::<Vec<_>>();
            for deployment in deployments {
                if let Err(error) = self.deliver_receipts(&deployment).await {
                    tracing::error!(adapter = deployment.definition.name(), %error, "adapter receipt retained for retry");
                }
            }
        }
    }
    async fn run_work(self: &Arc<Self>) {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(2));
        let mut progress = tokio::time::interval(std::time::Duration::from_millis(750));
        loop {
            tokio::select! {
                _ = interval.tick() => {
                    if let Err(error) = self.reconcile().await { tracing::error!(%error, "reconciling adapters"); }
                },
                () = self.notify.notified() => {},
                _ = progress.tick() => {},
            }
            let deployments = self
                .deployments
                .lock()
                .expect("adapter deployments poisoned")
                .values()
                .cloned()
                .collect::<Vec<_>>();
            for deployment in deployments {
                if let Err(error) = self.process(&deployment).await {
                    tracing::error!(adapter = deployment.definition.name(), %error, "adapter work retained for retry");
                }
            }
            self.prune_callers().await;
        }
    }
    async fn desired(&self) -> Result<BTreeMap<String, DesiredAdapter>> {
        let mut desired = BTreeMap::new();
        let agents = self.root.exoharness_handle().list_agents().await?;
        self.definitions
            .retain(&agents.iter().map(|agent| agent.record().id).collect());
        for agent in agents {
            if self.only_agent.is_some_and(|id| id != agent.record().id) {
                continue;
            }
            let definition = match self.definitions.load(agent.as_ref()).await {
                Ok(Some(definition)) => definition,
                Ok(None) => continue,
                Err(error) => {
                    tracing::error!(agent = %agent.record().id, %error, "adapter definition unavailable; retaining running deployments");
                    for deployment in self
                        .deployments
                        .lock()
                        .expect("adapter deployments poisoned")
                        .values()
                        .filter(|deployment| deployment.agent == agent.record().id)
                    {
                        desired.insert(
                            deployment.id.clone(),
                            DesiredAdapter {
                                agent: deployment.agent,
                                definition: deployment.definition.clone(),
                                reload: false,
                            },
                        );
                    }
                    continue;
                }
            };
            for definition in definition.frontmatter.adapters.iter().cloned() {
                let id = AdapterStore::deployment(agent.record().id, definition.name());
                desired.insert(
                    id,
                    DesiredAdapter {
                        agent: agent.record().id,
                        definition,
                        reload: true,
                    },
                );
            }
        }
        Ok(desired)
    }
    pub async fn reconcile(self: &Arc<Self>) -> Result<()> {
        let desired = self.desired().await?;
        let removed = {
            let mut current = self
                .deployments
                .lock()
                .expect("adapter deployments poisoned");
            let ids = current
                .keys()
                .filter(|id| !desired.contains_key(*id))
                .cloned()
                .collect::<Vec<_>>();
            ids.into_iter()
                .filter_map(|id| current.remove(&id))
                .collect::<Vec<_>>()
        };
        for deployment in removed {
            deployment.connection.shutdown().await;
        }
        for (
            id,
            DesiredAdapter {
                agent,
                definition,
                reload,
            },
        ) in desired
        {
            if !reload {
                continue;
            }
            if let Err(error) = self.reconcile_one(&id, agent, definition).await {
                tracing::error!(adapter = %id, %error, "Slack adapter awaiting configuration or connection");
            }
        }
        Ok(())
    }
    async fn reconcile_one(
        self: &Arc<Self>,
        id: &str,
        agent: AgentId,
        definition: AdapterDefinition,
    ) -> Result<()> {
        let AdapterDefinition::Slack(config) = &definition;
        let owner = if let Some(auth) = &self.auth {
            self.root
                .with_caller(auth.caller(auth.owner().await, false))?
                .exoharness_handle()
        } else {
            self.root.exoharness_handle()
        };
        let vault = exo_managed_agents::vaults::find_vault(owner.as_ref(), &config.vault).await?;
        let global =
            exoharness::vault::global_vault(self.root.exoharness_handle().as_ref()).await?;
        ensure!(
            vault.record().id != global.record().id,
            "Slack transport credentials need a separate vault"
        );
        let names = if config.transport == SlackTransport::Socket {
            ["bot-token", "app-token"]
        } else {
            ["bot-token", "signing-secret"]
        };
        let mut revisions = Vec::new();
        let metadata = vault.list_secrets().await?;
        for name in names {
            if let Some(secret) = metadata.iter().find(|s| s.name == name) {
                revisions.push((secret.id, secret.revision));
            }
        }
        let fingerprint = digest(&serde_json::to_string(&(&definition, &revisions))?);
        if self
            .deployment(id)
            .is_some_and(|d| d.fingerprint == fingerprint)
        {
            return Ok(());
        }
        if revisions.len() < 2 {
            let previous = self
                .deployments
                .lock()
                .expect("adapter deployments poisoned")
                .remove(id);
            if let Some(old) = previous {
                old.connection.shutdown().await;
            }
            tracing::debug!(adapter = definition.name(), "adapter awaiting credentials");
            return Ok(());
        }
        let mut credentials = BTreeMap::new();
        for name in names {
            let secret = metadata
                .iter()
                .find(|secret| secret.name == name)
                .context("adapter secret metadata is missing")?;
            let value = vault
                .get_secret(&secret.id)
                .await?
                .context("adapter secret is missing")?;
            credentials.insert(name.to_owned(), value.bearer_value().to_owned());
        }
        let connection =
            slack::SlackConnection::connect(self, id, config.clone(), &credentials).await?;
        let shared =
            exoharness::vault::global_vault(self.root.exoharness_handle().as_ref()).await?;
        let metadata = shared.list_secrets().await?;
        let reachable = metadata
            .iter()
            .filter(|secret| secret.policy.is_some())
            .map(|secret| (&secret.name, &secret.policy))
            .collect::<Vec<_>>();
        tracing::info!(adapter = definition.name(), shared_credentials = %serde_json::to_string(&reachable)?, "Slack callers can use these global credentials");
        let deployment = Deployment {
            id: id.to_owned(),
            agent,
            definition,
            fingerprint,
            connection,
        };
        let previous = self
            .deployments
            .lock()
            .expect("adapter deployments poisoned")
            .insert(id.to_owned(), deployment);
        if let Some(old) = previous {
            old.connection.shutdown().await;
        }
        Ok(())
    }
    async fn accept(
        &self,
        deployment: &Deployment,
        message: IncomingMessage,
        grant: Option<crate::remote::IdentityGrant>,
    ) -> Result<()> {
        let _guard = self.admission.lock().await;
        self.check_scope(deployment.agent)?;
        ensure!(
            message.account == deployment.connection.account,
            "adapter account mismatch"
        );
        let key = AdapterStore::inbox(&deployment.id, &message.id);
        let inbox = if let Some(existing) = self.store.get::<Inbox>(&key).await? {
            ensure!(
                existing.message == message,
                "adapter message ID was reused with different contents"
            );
            existing
        } else {
            let inbox = Inbox {
                accepted: Uuid7::now(),
                message,
                grant,
                thread: None,
                turn: None,
                cursor: None,
                final_text: None,
                progress: Default::default(),
                complete: false,
                error: None,
                observer_retry: None,
            };
            self.store.save(&key, &inbox).await?;
            inbox
        };
        self.track(deployment, &inbox.message).await?;
        self.queue_delivery(
            deployment,
            &inbox.message,
            "receipt",
            DeliveryKind::Reaction {
                message_ts: inbox.message.message_ts.clone(),
            },
        )
        .await?;
        self.receipt_notify.notify_one();
        self.notify.notify_one();
        Ok(())
    }
    async fn track(&self, deployment: &Deployment, message: &IncomingMessage) -> Result<()> {
        let key = AdapterStore::route(&deployment.id, message);
        if self.store.get::<Route>(&key).await?.is_none() {
            self.store.put(&key, &Route { thread: None }).await?;
        }
        Ok(())
    }
    async fn process(&self, deployment: &Deployment) -> Result<()> {
        let mut inbox = self
            .store
            .pending::<Inbox>(&format!("adapters/v1/{}/inbox", deployment.id))
            .await?;
        inbox.sort_by_key(|record| record.accepted);
        for mut record in inbox {
            if record.complete || record.message.account != deployment.connection.account {
                continue;
            }
            if let Err(error) = self.process_input(deployment, &mut record).await {
                tracing::warn!(input = %record.message.id, %error, "adapter input retained for retry");
            }
        }
        if let Err(error) = self.apply_decisions(deployment).await {
            tracing::warn!(adapter = deployment.definition.name(), %error, "adapter decisions retained for retry");
        }
        self.deliver(deployment).await?;
        Ok(())
    }
    async fn process_input(&self, deployment: &Deployment, record: &mut Inbox) -> Result<()> {
        self.touch_caller(deployment, record)?;
        self.restore_receipt(deployment, record).await?;
        if record.complete {
            self.finish_stream(deployment, record).await?;
            return self.save_inbox(deployment, record).await;
        }
        let connected = self
            .streams
            .lock()
            .expect("adapter streams poisoned")
            .contains_key(&stream_key(deployment, record));
        if !connected
            && record
                .observer_retry
                .as_ref()
                .is_some_and(|retry| retry.retry_at > chrono::Utc::now().timestamp())
        {
            return Ok(());
        }
        if !connected && let Err(error) = self.submit(deployment, record).await {
            if error.is::<crate::remote::IdentityAccessChanged>()
                || error.is::<access::AdapterAccessDenied>()
            {
                tracing::info!(input = %record.message.id, %error, "adapter access removed");
                record.error = Some(InboxError::AccessRemoved);
                record.complete = true;
                self.finish_stream(deployment, record).await?;
                self.save_inbox(deployment, record).await?;
                return Ok(());
            }
            // A queue write may have succeeded before the caller lost its
            // receipt. Retry with the same principal, session and key.
            tracing::warn!(input = %record.message.id, %error, "adapter submission retained for retry");
            let chunks = self.reconnecting(record);
            self.queue_stream(deployment, record, chunks, false).await?;
            self.save_inbox(deployment, record).await?;
            return Ok(());
        }
        self.drain_stream(deployment, record).await?;
        if record.complete {
            self.finish_stream(deployment, record).await?;
            self.save_inbox(deployment, record).await?;
        }
        Ok(())
    }
    async fn save_inbox(&self, deployment: &Deployment, inbox: &Inbox) -> Result<()> {
        self.store
            .save(
                &AdapterStore::inbox(&deployment.id, &inbox.message.id),
                inbox,
            )
            .await
    }
    async fn restore_receipt(&self, deployment: &Deployment, inbox: &mut Inbox) -> Result<()> {
        if let Some(outbox) = self
            .store
            .get::<Outbox>(&AdapterStore::outbox(
                &deployment.id,
                &format!("{}:stream", inbox.message.id),
            ))
            .await?
            && let Some(receipt) = outbox.receipt
            && receipt.cursor >= inbox.cursor
        {
            inbox.cursor = receipt.cursor;
            inbox.progress = receipt.progress;
            inbox.complete = receipt.complete;
            inbox.final_text = receipt.final_text;
            inbox.error = receipt.error;
            inbox.observer_retry = receipt.observer_retry;
        }
        Ok(())
    }
    async fn submit(&self, deployment: &Deployment, inbox: &mut Inbox) -> Result<()> {
        if inbox.complete {
            return Ok(());
        }
        if let Some(grant) = &inbox.grant {
            self.identities.validate(grant).await?;
        }
        let agent = self
            .root
            .exoharness_handle()
            .get_agent(&deployment.agent)
            .await?
            .context("adapter agent is missing")?;
        let route_key = AdapterStore::route(&deployment.id, &inbox.message);
        let route_lock = self.route_lock(&route_key);
        let route_guard = route_lock.lock().await;
        let thread_id = match self.store.get::<Route>(&route_key).await? {
            Some(Route {
                thread: Some(thread),
            }) => thread,
            _ => {
                let slug = format!("slack-{}", digest(&route_key));
                let existing = exo_managed_agents::list_threads(agent.as_ref())
                    .await?
                    .into_iter()
                    .find(|thread| thread.record().slug == slug);
                let thread = match existing {
                    Some(thread) => thread,
                    None => {
                        agent
                            .new_thread(NewThreadRequest {
                                slug: Some(slug),
                                ..Default::default()
                            })
                            .await?
                    }
                };
                let config = crate::load_agent_config(agent.as_ref()).await?;
                crate::harness_config::store_conversation_config(
                    thread.as_ref(),
                    &crate::ConversationConfig {
                        resources: config.resources.clone(),
                        sandbox_provider: Some(config.sandbox.provider.clone()),
                        ..Default::default()
                    },
                )
                .await?;
                // Admission only protects the durable route write. Thread
                // discovery and creation must not delay Slack acknowledgements.
                let _admission = self.admission.lock().await;
                self.store
                    .put(
                        &route_key,
                        &Route {
                            thread: Some(thread.record().id),
                        },
                    )
                    .await?;
                thread.record().id
            }
        };
        drop(route_guard);
        inbox.thread = Some(thread_id);
        let authority = Authority {
            deployment: deployment.id.clone(),
            agent: deployment.agent,
            thread: thread_id,
            adapter: deployment.definition.name().into(),
            source: (&inbox.message).into(),
            grant: inbox.grant.clone(),
        };
        let runtime = self.caller_runtime(authority).await?;
        let agent = runtime
            .exoharness_handle()
            .get_agent(&deployment.agent)
            .await?
            .context("adapter agent unavailable")?;
        let thread = agent
            .get_thread(&thread_id)
            .await?
            .context("adapter thread unavailable")?;
        let AdapterDefinition::Slack(config) = &deployment.definition;
        let selected = futures::future::try_join_all(config.turn_vaults.iter().map(|reference| {
            let state = runtime.exoharness_handle();
            async move {
                Ok::<_, anyhow::Error>(
                    exo_managed_agents::vaults::find_vault(state.as_ref(), reference)
                        .await?
                        .record()
                        .id,
                )
            }
        }))
        .await?;
        let thread = if selected
            .iter()
            .any(|id| !thread.record().vaults.contains(id))
        {
            thread.attach_vaults(selected).await?
        } else {
            thread
        };
        if let Some(turn) = &inbox.turn {
            let stream = runtime
                .watch_turn(thread.as_ref(), turn.clone(), inbox.cursor)
                .await?;
            self.streams
                .lock()
                .expect("adapter streams poisoned")
                .insert(stream_key(deployment, inbox), stream);
            return Ok(());
        }
        let input = format!(
            "[{} user {}]\n{}",
            deployment.definition.kind(),
            inbox.message.sender,
            inbox.message.text
        );
        let (turn, stream) = runtime
            .start_turn(
                agent,
                thread,
                SendRequest {
                    input: vec![crate::harness_helpers::user_message(&input)],
                    session_id: Some(inbox.accepted),
                },
                true,
                None,
                TurnOptions {
                    idempotency_key: Some(format!(
                        "adapter:{}/{}",
                        deployment.id, inbox.message.id
                    )),
                    retain_receipt: true,
                    ..Default::default()
                },
            )
            .await?;
        inbox.turn = Some(turn);
        self.save_inbox(deployment, inbox).await?;
        self.streams
            .lock()
            .expect("adapter streams poisoned")
            .insert(stream_key(deployment, inbox), stream);
        self.queue_stream(
            deployment,
            inbox,
            vec![progress::Progress::status("Queued")],
            false,
        )
        .await?;
        Ok(())
    }
    async fn drain_stream(&self, deployment: &Deployment, inbox: &mut Inbox) -> Result<bool> {
        let previous = store::StreamReceipt::from(&*inbox);
        let key = stream_key(deployment, inbox);
        let stream = self
            .streams
            .lock()
            .expect("adapter streams poisoned")
            .remove(&key);
        let Some(mut stream) = stream else {
            return Ok(false);
        };
        let mut progress = inbox.progress.clone();
        let mut chunks = Vec::new();
        let mut approvals = Vec::new();
        let mut completion = None;
        let mut checkpoint = inbox.cursor;
        let mut connected = true;
        for _ in 0..4096 {
            match stream.next().now_or_never() {
                Some(Some(Ok(ExecutionStreamEvent::Checkpoint { event_id }))) => {
                    checkpoint = Some(event_id);
                }
                Some(Some(Ok(ExecutionStreamEvent::Completed(result)))) => {
                    completion = Some(result);
                    connected = false;
                    break;
                }
                Some(Some(Ok(ExecutionStreamEvent::ApprovalRequested { turn, approval }))) => {
                    chunks.push(progress::Progress::status("Waiting for your approval"));
                    approvals.push((turn, approval));
                }
                Some(Some(Ok(event))) => match progress.event(event) {
                    Ok(updates) => chunks.extend(updates),
                    Err(error) => {
                        chunks.extend(self.observer_error(inbox, checkpoint, error));
                        connected = false;
                        break;
                    }
                },
                Some(Some(Err(error))) => {
                    chunks.extend(self.observer_error(inbox, checkpoint, error));
                    connected = false;
                    break;
                }
                Some(None) => {
                    chunks.extend(self.reconnecting(inbox));
                    connected = false;
                    break;
                }
                None => break,
            }
        }
        for (turn, approval) in approvals {
            self.queue_delivery(
                deployment,
                &inbox.message,
                &format!("approval:{}", approval.approval_id),
                DeliveryKind::Approval {
                    turn,
                    approval,
                    inbox: inbox.message.id.clone(),
                },
            )
            .await?;
        }
        inbox.progress = progress;
        if connected && checkpoint > inbox.cursor {
            inbox.observer_retry = None;
        }
        inbox.cursor = checkpoint;
        if let Some(result) = completion {
            inbox.observer_retry = None;
            inbox.cursor = Some(result.latest_event_id);
            inbox.final_text = inbox.progress.last_message().map(str::to_owned);
            inbox.complete = true;
        }
        // The output and its resume position are committed in one object.
        let changed = store::StreamReceipt::from(&*inbox) != previous;
        if changed || !chunks.is_empty() {
            self.queue_stream(deployment, inbox, chunks, false).await?;
        }
        if !inbox.complete && changed {
            self.save_inbox(deployment, inbox).await?;
        }
        if connected {
            self.streams
                .lock()
                .expect("adapter streams poisoned")
                .insert(key, stream);
        }
        Ok(connected)
    }

    async fn finish_stream(&self, deployment: &Deployment, inbox: &mut Inbox) -> Result<()> {
        let (text, failure) = match &inbox.error {
            Some(InboxError::AccessRemoved) => ("Access removed".into(), Some("Access removed")),
            Some(InboxError::TurnFailed(error)) => {
                (format!("Turn failed: {error}"), Some("Turn failed"))
            }
            Some(InboxError::InvalidProgress) => (
                "Unable to show turn updates. The turn remains saved in Exo; check the server logs.".into(),
                Some("Updates unavailable"),
            ),
            None => (
                inbox
                    .final_text
                    .clone()
                    .unwrap_or_else(|| "The turn finished without a text response.".into()),
                None,
            ),
        };
        let chunks = inbox.progress.finish(&text, failure);
        self.queue_stream(deployment, inbox, chunks, true).await?;
        self.streams
            .lock()
            .expect("adapter streams poisoned")
            .remove(&stream_key(deployment, inbox));
        self.queue_status(deployment, inbox, String::new()).await
    }

    fn observer_error(
        &self,
        inbox: &mut Inbox,
        checkpoint: Option<exoharness::EventId>,
        error: anyhow::Error,
    ) -> Vec<SlackStreamChunk> {
        if error.is::<crate::turn_queue::TurnFailed>() {
            inbox.error = Some(InboxError::TurnFailed(error.to_string()));
            inbox.complete = true;
            return vec![progress::Progress::status("Turn failed")];
        }
        // Advancing the committed cursor counts as progress;
        // retrying the same malformed event does not.
        if checkpoint > inbox.cursor {
            inbox.observer_retry = None;
        }
        let chunks = self.reconnecting(inbox);
        if error.is::<serde_json::Error>()
            && inbox
                .observer_retry
                .as_ref()
                .is_some_and(|retry| retry.attempts >= INVALID_PROGRESS_RETRY_LIMIT)
        {
            tracing::error!(input = %inbox.message.id, thread = ?inbox.thread, turn = ?inbox.turn, %error, "adapter progress is malformed; automatic observation stopped, turn retained");
            inbox.error = Some(InboxError::InvalidProgress);
            inbox.complete = true;
            return Vec::new();
        }
        tracing::warn!(input = %inbox.message.id, %error, "adapter observer disconnected; turn retained for recovery");
        chunks
    }

    fn reconnecting(&self, inbox: &mut Inbox) -> Vec<SlackStreamChunk> {
        let retry = inbox.observer_retry.get_or_insert(store::ObserverRetry {
            attempts: 0,
            retry_at: 0,
        });
        retry.attempts = retry.attempts.saturating_add(1);
        let delay = (2_i64.pow(retry.attempts.min(6))).min(60);
        retry.retry_at = chrono::Utc::now().timestamp() + delay;
        if retry.attempts == 1 {
            vec![progress::Progress::status("Reconnecting")]
        } else {
            Vec::new()
        }
    }

    async fn queue_stream(
        &self,
        deployment: &Deployment,
        inbox: &Inbox,
        chunks: Vec<SlackStreamChunk>,
        complete: bool,
    ) -> Result<()> {
        let id = format!("{}:stream", inbox.message.id);
        let key = AdapterStore::outbox(&deployment.id, &id);
        let mut delivery = self
            .store
            .get::<Outbox>(&key)
            .await?
            .unwrap_or_else(|| Outbox {
                id,
                account: inbox.message.account.clone(),
                channel: inbox.message.channel.clone(),
                reply_ts: inbox.message.reply_ts.clone(),
                kind: DeliveryKind::Stream {
                    chunks: Vec::new(),
                    complete: false,
                    sender: inbox.message.sender.clone(),
                    thread_ts: inbox
                        .message
                        .reply_ts
                        .as_ref()
                        .unwrap_or(&inbox.message.message_ts)
                        .clone(),
                },
                sent: false,
                failure: None,
                attempts: 0,
                retry_at: 0,
                external_id: None,
                receipt: None,
            });
        let receipt = store::StreamReceipt::from(inbox);
        ensure!(
            delivery
                .receipt
                .as_ref()
                .is_none_or(|saved| saved.cursor <= receipt.cursor),
            "cannot overwrite a newer adapter output receipt; restore it before writing"
        );
        if chunks.is_empty() && !complete && delivery.receipt.as_ref() == Some(&receipt) {
            return Ok(());
        }
        delivery.receipt = Some(receipt);
        let DeliveryKind::Stream {
            chunks: pending,
            complete: finished,
            ..
        } = &mut delivery.kind
        else {
            anyhow::bail!("stream outbox contains a different delivery kind");
        };
        if *finished {
            return Ok(());
        }
        for chunk in chunks {
            if let SlackStreamChunk::TaskUpdate { id, .. } = &chunk
                && let Some(existing) = pending.iter_mut().find(|previous| matches!(previous, SlackStreamChunk::TaskUpdate { id: previous, .. } if previous == id))
            {
                *existing = chunk;
            } else if let SlackStreamChunk::MarkdownText { text } = &chunk
                && let Some(SlackStreamChunk::MarkdownText { text: previous }) = pending.last_mut()
                && previous.chars().count() + text.chars().count() <= 4000
            {
                previous.push_str(text);
            } else {
                pending.push(chunk);
            }
        }
        *finished = complete;
        delivery.sent = false;
        self.store.save(&key, &delivery).await
    }
    async fn queue_delivery(
        &self,
        deployment: &Deployment,
        message: &IncomingMessage,
        suffix: &str,
        kind: DeliveryKind,
    ) -> Result<()> {
        let id = format!("{}:{suffix}", message.id);
        let key = AdapterStore::outbox(&deployment.id, &id);
        if self.store.get::<Outbox>(&key).await?.is_none() {
            self.store
                .save(
                    &key,
                    &Outbox {
                        id,
                        account: message.account.clone(),
                        channel: message.channel.clone(),
                        reply_ts: Some(
                            message
                                .reply_ts
                                .as_ref()
                                .unwrap_or(&message.message_ts)
                                .clone(),
                        ),
                        kind,
                        sent: false,
                        failure: None,
                        attempts: 0,
                        retry_at: 0,
                        external_id: None,
                        receipt: None,
                    },
                )
                .await?;
        }
        Ok(())
    }
    async fn queue_status(
        &self,
        deployment: &Deployment,
        inbox: &Inbox,
        text: String,
    ) -> Result<()> {
        let id = format!("{}:status", inbox.message.id);
        let key = AdapterStore::outbox(&deployment.id, &id);
        if let Some(existing) = self.store.get::<Outbox>(&key).await?
            && matches!(&existing.kind, DeliveryKind::Status { text: previous, .. } if previous == &text)
        {
            return Ok(());
        }
        self.store
            .save(
                &key,
                &Outbox {
                    id,
                    account: inbox.message.account.clone(),
                    channel: inbox.message.channel.clone(),
                    reply_ts: inbox.message.reply_ts.clone(),
                    kind: DeliveryKind::Status {
                        inbox: inbox.message.id.clone(),
                        thread_ts: inbox
                            .message
                            .reply_ts
                            .as_ref()
                            .unwrap_or(&inbox.message.message_ts)
                            .clone(),
                        text,
                    },
                    sent: false,
                    failure: None,
                    attempts: 0,
                    retry_at: 0,
                    external_id: None,
                    receipt: None,
                },
            )
            .await
    }
    async fn deliver(&self, deployment: &Deployment) -> Result<()> {
        let mut deliveries = self
            .store
            .pending::<Outbox>(&format!("adapters/v1/{}/outbox", deployment.id))
            .await?;
        deliveries.sort_by(|a, b| a.id.cmp(&b.id));
        for delivery in deliveries {
            if !matches!(delivery.kind, DeliveryKind::Reaction { .. })
                && let Err(error) = self.deliver_one(deployment, delivery).await
            {
                tracing::warn!(adapter = deployment.definition.name(), %error, "adapter delivery retained for retry");
            }
        }
        Ok(())
    }
    async fn deliver_receipts(&self, deployment: &Deployment) -> Result<()> {
        let deliveries = self
            .store
            .pending::<Outbox>(&format!("adapters/v1/{}/outbox", deployment.id))
            .await?;
        for delivery in deliveries {
            if matches!(delivery.kind, DeliveryKind::Reaction { .. })
                && let Err(error) = self.deliver_one(deployment, delivery).await
            {
                tracing::warn!(adapter = deployment.definition.name(), %error, "adapter receipt retained for retry");
            }
        }
        Ok(())
    }
    async fn deliver_one(&self, deployment: &Deployment, mut delivery: Outbox) -> Result<()> {
        if delivery.sent
            || delivery.failure.is_some()
            || delivery.account != deployment.connection.account
            || delivery.retry_at > chrono::Utc::now().timestamp()
        {
            return Ok(());
        }
        if matches!(&delivery.kind, DeliveryKind::Stream { chunks, complete: false, .. } if chunks.is_empty())
        {
            return Ok(());
        }
        if let DeliveryKind::Status { inbox, text, .. } = &delivery.kind
            && !text.is_empty()
            && self
                .store
                .get::<Inbox>(&AdapterStore::inbox(&deployment.id, inbox))
                .await?
                .is_some_and(|input| input.complete)
        {
            delivery.sent = true;
            self.store
                .save(
                    &AdapterStore::outbox(&deployment.id, &delivery.id),
                    &delivery,
                )
                .await?;
            return Ok(());
        }
        if let DeliveryKind::Approval { inbox, .. } = &delivery.kind
            && self
                .store
                .get::<Inbox>(&AdapterStore::inbox(&deployment.id, inbox))
                .await?
                .is_some_and(|input| input.complete)
        {
            delivery.sent = true;
            self.store
                .save(
                    &AdapterStore::outbox(&deployment.id, &delivery.id),
                    &delivery,
                )
                .await?;
            return Ok(());
        }
        let stream_had_id = delivery.external_id.is_some();
        match deployment.connection.send(&delivery).await {
            Ok(id) => {
                delivery.external_id = id;
                if let DeliveryKind::Stream {
                    chunks, complete, ..
                } = &mut delivery.kind
                {
                    let consumed = progress::batch(chunks).len();
                    chunks.drain(..consumed);
                    delivery.sent = *complete && stream_had_id && chunks.is_empty();
                    delivery.retry_at = 0;
                } else if matches!(&delivery.kind, DeliveryKind::Status { text, .. } if !text.is_empty())
                {
                    // Slack expires status indicators after two minutes.
                    delivery.retry_at = chrono::Utc::now().timestamp().saturating_add(60);
                } else {
                    delivery.sent = true;
                }
            }
            Err(error) => {
                delivery.attempts = delivery.attempts.saturating_add(1);
                let slack_error = error.downcast_ref::<slack_morphism::errors::SlackClientError>();
                if let Some(slack_morphism::errors::SlackClientError::ApiError(api)) = slack_error
                    && matches!(
                        api.code.as_str(),
                        "invalid_thread_ts" | "thread_not_found" | "message_not_found"
                    )
                {
                    delivery.failure = Some(api.code.clone());
                } else if matches!(
                    delivery.kind,
                    DeliveryKind::Reaction { .. } | DeliveryKind::Status { .. }
                ) {
                    delivery.sent = true;
                }
                let delay = match slack_error {
                    Some(slack_morphism::errors::SlackClientError::RateLimitError(limit)) => limit
                        .retry_after
                        .map(|delay| delay.as_secs().min(i64::MAX as u64) as i64),
                    _ => None,
                }
                .unwrap_or_else(|| i64::from(2u32.saturating_pow(delivery.attempts.min(8))));
                delivery.retry_at = chrono::Utc::now().timestamp().saturating_add(delay);
                tracing::warn!(adapter = deployment.definition.name(), delivery = %delivery.id, channel = %delivery.channel, terminal = delivery.failure.is_some(), %error, "adapter delivery failed");
            }
        }
        self.store
            .save(
                &AdapterStore::outbox(&deployment.id, &delivery.id),
                &delivery,
            )
            .await?;
        Ok(())
    }
    pub(crate) async fn accept_decision(
        &self,
        deployment: &Deployment,
        outbox_id: &str,
        sender: &str,
        channel: &str,
        message_ts: &str,
        approved: bool,
    ) -> Result<()> {
        let _guard = self.admission.lock().await;
        let outbox = self
            .store
            .get::<Outbox>(&AdapterStore::outbox(&deployment.id, outbox_id))
            .await?
            .context("approval delivery not found")?;
        ensure!(
            outbox.account == deployment.connection.account
                && outbox.channel == channel
                && outbox.external_id.as_deref() == Some(message_ts),
            "approval belongs to another Slack message"
        );
        let DeliveryKind::Approval {
            turn,
            approval,
            inbox,
        } = outbox.kind
        else {
            anyhow::bail!("message is not an approval request");
        };
        let source = self
            .store
            .get::<Inbox>(&AdapterStore::inbox(&deployment.id, &inbox))
            .await?
            .context("approval input missing")?;
        ensure!(source.message.sender == sender && source.turn.as_ref().is_some_and(|saved| saved.id == turn.id && saved.session_id == turn.session_id), "only the author of this pending turn may approve it");
        if let Some(grant) = &source.grant {
            self.identities.validate(grant).await?;
        }
        let key = format!(
            "adapters/v1/{}/decisions/{}.json",
            deployment.id,
            digest(outbox_id)
        );
        if let Some(previous) = self.store.get::<Decision>(&key).await? {
            ensure!(
                previous.approved == approved,
                "approval already has a different decision"
            );
            return Ok(());
        }
        ensure!(!source.complete, "this turn has finished");
        self.store
            .save(
                &key,
                &Decision {
                    inbox,
                    turn,
                    approval_id: approval.approval_id,
                    approved,
                    applied: false,
                    failure: None,
                },
            )
            .await?;
        self.notify.notify_one();
        Ok(())
    }
    async fn apply_decisions(&self, deployment: &Deployment) -> Result<()> {
        let prefix = format!("adapters/v1/{}/decisions", deployment.id);
        for mut decision in self.store.pending::<Decision>(&prefix).await? {
            if decision.applied {
                continue;
            }
            if decision.failure.is_some() {
                continue;
            }
            if let Err(error) = self.apply_decision(deployment, &mut decision).await {
                tracing::warn!(input = %decision.inbox, %error, "adapter decision rejected or retained for retry");
                if error.is::<crate::remote::IdentityAccessChanged>()
                    || error.is::<access::AdapterAccessDenied>()
                {
                    decision.failure = Some(error.to_string());
                    let id = format!("{}:approval:{}", decision.inbox, decision.approval_id);
                    if let Err(error) = self
                        .store
                        .save(&format!("{prefix}/{}.json", digest(&id)), &decision)
                        .await
                    {
                        tracing::warn!(%error, "saving rejected adapter decision");
                    }
                }
            }
        }
        Ok(())
    }
    async fn apply_decision(&self, deployment: &Deployment, decision: &mut Decision) -> Result<()> {
        let prefix = format!("adapters/v1/{}/decisions", deployment.id);
        let mut inbox = self
            .store
            .get::<Inbox>(&AdapterStore::inbox(&deployment.id, &decision.inbox))
            .await?
            .context("approval source missing")?;
        self.restore_receipt(deployment, &mut inbox).await?;
        let authority = Authority {
            deployment: deployment.id.clone(),
            agent: deployment.agent,
            thread: inbox.thread.context("approval thread missing")?,
            adapter: deployment.definition.name().into(),
            source: (&inbox.message).into(),
            grant: inbox.grant.clone(),
        };
        let runtime = self.caller_runtime(authority).await?;
        let applied = runtime
            .respond_to_pending_approval(
                deployment.agent,
                inbox.thread.context("approval thread missing")?,
                decision.turn.id,
                &exo_managed_agents::http::protocol::ApprovalResponseBody {
                    approval_id: decision.approval_id.clone(),
                    approved: decision.approved,
                    allow_for_tool: false,
                    session_id: decision.turn.session_id,
                },
            )
            .await?;
        if applied {
            inbox.observer_retry = None;
            self.queue_stream(
                deployment,
                &inbox,
                vec![progress::Progress::status("Waiting for model")],
                false,
            )
            .await?;
        }
        if !applied && !inbox.complete {
            return Ok(());
        }
        decision.applied = true;
        let id = format!("{}:approval:{}", decision.inbox, decision.approval_id);
        self.store
            .save(&format!("{prefix}/{}.json", digest(&id)), decision)
            .await?;
        Ok(())
    }
    pub async fn configuration(
        &self,
        agent: Option<&str>,
        name: &str,
    ) -> Result<(AgentId, AdapterDefinition)> {
        let handle = match agent {
            Some(reference) => {
                exo_managed_agents::find_agent(self.root.exoharness_handle().as_ref(), reference)
                    .await?
            }
            None => self
                .root
                .exoharness_handle()
                .get_agent(
                    &self
                        .only_agent
                        .context("an agent is required for this server")?,
                )
                .await?
                .context("agent missing")?,
        };
        self.check_scope(handle.record().id)?;
        let definition = match self.definitions.load(handle.as_ref()).await {
            Ok(definition) => definition.context("agent has no managed definition")?,
            Err(error) => {
                if let Some(deployment) =
                    self.deployment(&AdapterStore::deployment(handle.record().id, name))
                {
                    tracing::warn!(%error, "serving the retained adapter configuration until it can be reloaded");
                    return Ok((deployment.agent, deployment.definition));
                }
                return Err(error);
            }
        };
        let adapter = definition
            .frontmatter
            .adapters
            .iter()
            .find(|adapter| adapter.name() == name)
            .context("adapter not found")?
            .clone();
        Ok((handle.record().id, adapter))
    }
    pub async fn slack_webhook(
        &self,
        agent: Option<&str>,
        name: &str,
        body: &str,
        form: bool,
        signature: Option<&str>,
        timestamp: Option<&str>,
    ) -> Result<Option<String>> {
        use slack_morphism::prelude::*;
        let (agent, definition) = self.configuration(agent, name).await?;
        let AdapterDefinition::Slack(config) = definition;
        ensure!(
            config.transport == SlackTransport::Http,
            "adapter does not use HTTP"
        );
        let owner = if let Some(auth) = &self.auth {
            self.root
                .with_caller(auth.caller(auth.owner().await, false))?
                .exoharness_handle()
        } else {
            self.root.exoharness_handle()
        };
        let vault = exo_managed_agents::vaults::find_vault(owner.as_ref(), &config.vault).await?;
        let metadata = vault.list_secrets().await?;
        let signing = match metadata
            .into_iter()
            .find(|secret| secret.name == "signing-secret")
        {
            Some(secret) => Some(
                vault
                    .get_secret(&secret.id)
                    .await?
                    .context("Slack signing secret missing")?
                    .bearer_value()
                    .to_owned(),
            ),
            None => None,
        };
        #[derive(Deserialize)]
        #[serde(tag = "type", rename_all = "snake_case")]
        enum Probe {
            UrlVerification {
                challenge: String,
            },
            #[serde(other)]
            Other,
        }
        if let Some(secret) = &signing {
            slack_morphism::signature_verifier::SlackEventSignatureVerifier::new(
                &secret.clone().into(),
            )
            .verify(
                signature.unwrap_or_default(),
                body,
                timestamp.unwrap_or_default(),
            )?;
        } else {
            if !form && let Probe::UrlVerification { challenge } = serde_json::from_str(body)? {
                return Ok(Some(challenge));
            }
            anyhow::bail!("Slack adapter is awaiting credentials");
        }
        if !form && let Probe::UrlVerification { challenge } = serde_json::from_str(body)? {
            return Ok(Some(challenge));
        }
        let deployment = self
            .deployment(&AdapterStore::deployment(agent, name))
            .context("Slack adapter is not connected yet")?;
        let connection = &deployment.connection;
        if form {
            #[derive(Deserialize)]
            struct InteractionForm {
                payload: String,
            }
            let form: InteractionForm = serde_urlencoded::from_str(body)?;
            connection
                .interaction(self, &deployment, serde_json::from_str(&form.payload)?)
                .await?;
        } else {
            match serde_json::from_str::<SlackPushEvent>(body)? {
                SlackPushEvent::EventCallback(callback) => {
                    connection.push(self, &deployment, callback).await?
                }
                SlackPushEvent::AppRateLimited(_) => {
                    tracing::warn!("Slack event delivery was rate limited")
                }
                SlackPushEvent::UrlVerification(_) => unreachable!("handled URL verification"),
            }
        }
        Ok(None)
    }
}

fn stream_key(deployment: &Deployment, inbox: &Inbox) -> String {
    format!("{}/{}", deployment.id, inbox.message.id)
}
